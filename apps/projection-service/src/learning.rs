//! Adaptive learning loop shared by every sport engine.
//!
//!   freeze  — before a game starts, the champion model's 100k-game prediction is
//!             written to an immutable ledger together with its scoreable
//!             forecasts (each with a baseline computed from data before the game);
//!   score   — after the game, each forecast gets its outcome exactly once;
//!   registry— candidate configurations are validated against the champion on the
//!             same games and seeds (paired walk-forward), promoted only through a
//!             statistical gate, and can be rolled back; every step is audited.
//!
//! The backtests use the same `Forecast` / `Outcomes` definitions, so live and
//! historical scores are directly comparable.

use std::collections::{BTreeMap, HashMap};
use actix_web::{web, HttpResponse, Responder};
use chrono::{DateTime, NaiveDate, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_postgres::types::Json;
use crate::State;

fn internal(m: impl std::fmt::Display) -> HttpResponse {
    eprintln!("projection-service(learning): {m}");
    HttpResponse::InternalServerError().json(json!({ "error": m.to_string() }))
}
fn bad_request(m: impl Into<String>) -> HttpResponse { HttpResponse::BadRequest().json(json!({ "error": m.into() })) }
fn unprocessable(m: impl Into<String>) -> HttpResponse { HttpResponse::UnprocessableEntity().json(json!({ "error": m.into() })) }
fn dberr(e: &tokio_postgres::Error) -> String {
    match e.as_db_error() { Some(d) => format!("{}: {}", d.code().code(), d.message()), None => e.to_string() }
}
fn r4(x: f64) -> f64 { (x * 10000.0).round() / 10000.0 }

/// Declares an engine's tunable hyper-parameters once: field, default, bounds, note.
/// A registry config overrides any subset; unknown keys are rejected at registration.
macro_rules! tuning {
    ($name:ident { $($f:ident: $d:expr, $lo:expr, $hi:expr, $note:expr;)* }) => {
        #[derive(Clone, Debug)]
        pub struct $name { $(pub $f: f64,)* }
        impl Default for $name { fn default() -> Self { $name { $($f: $d,)* } } }
        impl $name {
            #[allow(dead_code)]
            pub fn from_cfg(c: &serde_json::Value) -> Self {
                let d = Self::default();
                $name { $($f: c.get(stringify!($f)).and_then(|v| v.as_f64()).unwrap_or(d.$f),)* }
            }
            pub fn describe(&self) -> serde_json::Value {
                serde_json::json!({ $(stringify!($f): { "default": $d, "min": $lo, "max": $hi, "note": $note },)* })
            }
        }
    };
}
pub(crate) use tuning;

pub const SPORTS: [&str; 4] = ["nfl", "mlb", "nba", "nhl"];
/// Every engine in the registry: the four simulators, plus the team-points
/// engines (`{sport}_team`), which share their sport's schedule and results.
pub const ENGINES: [&str; 7] = ["nfl", "mlb", "nba", "nhl", "nfl_team", "nba_team", "nhl_team"];
/// The sport whose schedule, results and games an engine uses.
pub fn base(engine: &str) -> &str { engine.strip_suffix("_team").unwrap_or(engine) }
pub fn is_team(engine: &str) -> bool { engine.ends_with("_team") }
/// Production predictions are always at least this many simulated games.
pub const LEDGER_N: usize = 100_000;

// ---------------------------------------------------------------------------
// Forecasts and outcomes
// ---------------------------------------------------------------------------

/// One scoreable binary forecast: y = actual(stat) > line.
#[derive(Clone, Debug)]
pub struct Forecast {
    pub market: &'static str,
    pub baseline: &'static str,
    pub subject: String,
    pub name: String,
    pub stat: &'static str,
    pub line: f64,
    pub p_model: f64,
    pub p_base: f64,
    /// actual == line is a push and voids the forecast (market lines on whole numbers)
    pub push_void: bool,
}

impl Forecast {
    pub fn game(market: &'static str, baseline: &'static str, stat: &'static str, line: f64, p_model: f64, p_base: f64) -> Self {
        Forecast { market, baseline, subject: "game".into(), name: "game".into(), stat, line, p_model, p_base, push_void: false }
    }
    #[allow(clippy::too_many_arguments)]
    pub fn player(market: &'static str, baseline: &'static str, id: impl ToString, name: impl Into<String>, stat: &'static str, line: f64, p_model: f64, p_base: f64) -> Self {
        Forecast { market, baseline, subject: id.to_string(), name: name.into(), stat, line, p_model, p_base, push_void: false }
    }
    pub fn pushes(mut self) -> Self { self.push_void = true; self }
    fn key(&self) -> (&'static str, &str) { (self.market, self.subject.as_str()) }
}

/// What happened. Players appear only if they played (props are conditional on playing).
#[derive(Default, Debug)]
pub struct Outcomes {
    pub game: HashMap<&'static str, f64>,
    pub players: HashMap<String, HashMap<&'static str, f64>>,
}

impl Outcomes {
    pub fn value(&self, f: &Forecast) -> Option<f64> {
        if f.subject == "game" { self.game.get(f.stat).copied() } else { self.players.get(&f.subject).and_then(|s| s.get(f.stat).copied()) }
    }
    /// The binary outcome, or None when void (did not play, or a push).
    pub fn y(&self, f: &Forecast) -> Option<f64> {
        let v = self.value(f)?;
        if f.push_void && (v - f.line).abs() < 1e-9 { return None; }
        Some((v > f.line) as i32 as f64)
    }
}

/// Add-one smoothed frequency of `x > line` in a player's prior games.
pub fn freq_over(past: &[f64], line: f64) -> f64 {
    (past.iter().filter(|x| **x > line).count() as f64 + 1.0) / (past.len() as f64 + 2.0)
}

/// Paired Brier record (model vs baseline) for one market.
#[derive(Default, Clone)]
pub struct Pair { n: f64, m: f64, b: f64, lm: f64, lb: f64, d: Vec<f64>, cal: [(f64, f64, f64); 10] }

impl Pair {
    pub fn add(&mut self, pm: f64, pb: f64, y: f64) {
        let (pm, pb) = (pm.clamp(1e-4, 1.0 - 1e-4), pb.clamp(1e-4, 1.0 - 1e-4));
        let (bm, bb) = ((pm - y).powi(2), (pb - y).powi(2));
        let ll = |p: f64| -(y * p.ln() + (1.0 - y) * (1.0 - p).ln());
        self.n += 1.0; self.m += bm; self.b += bb; self.lm += ll(pm); self.lb += ll(pb); self.d.push(bm - bb);
        let k = ((pm * 10.0) as usize).min(9);
        self.cal[k].0 += 1.0; self.cal[k].1 += pm; self.cal[k].2 += y;
    }
    pub(crate) fn mean_se(d: &[f64]) -> (f64, f64) {
        let n = d.len().max(1) as f64;
        let md = d.iter().sum::<f64>() / n;
        let sd = (d.iter().map(|x| (x - md).powi(2)).sum::<f64>() / (n - 1.0).max(1.0)).sqrt();
        (md, sd / n.sqrt())
    }
    pub fn report(&self, baseline: &str) -> Value {
        let n = self.n.max(1.0);
        let (md, se) = Self::mean_se(&self.d);
        let ece = self.cal.iter().map(|c| if c.0 > 0.0 { (c.1 / c.0 - c.2 / c.0).abs() * c.0 } else { 0.0 }).sum::<f64>() / n;
        json!({ "n": self.n, "baseline": baseline, "brier_model": r4(self.m / n), "brier_baseline": r4(self.b / n), "brier_diff": r4(md), "brier_diff_se": r4(se),
                "log_loss_model": r4(self.lm / n), "log_loss_baseline": r4(self.lb / n), "ece_model": r4(ece) })
    }
}

/// Per-market scorecard over many games.
#[derive(Default)]
pub struct Scorecard { m: BTreeMap<&'static str, (&'static str, Pair)> }

impl Scorecard {
    pub fn add(&mut self, f: &Forecast, y: f64) {
        self.m.entry(f.market).or_insert_with(|| (f.baseline, Pair::default())).1.add(f.p_model, f.p_base, y);
    }
    /// Score every forecast that has an outcome; returns how many were scored.
    pub fn add_all(&mut self, fs: &[Forecast], o: &Outcomes) -> usize {
        let mut k = 0;
        for f in fs { if let Some(y) = o.y(f) { self.add(f, y); k += 1; } }
        k
    }
    pub fn report(&self) -> Value {
        Value::Object(self.m.iter().map(|(k, (b, p))| (k.to_string(), p.report(b))).collect())
    }
}

/// One evaluated game: forecasts frozen before the game, outcomes after, and
/// engine diagnostics as (name, simulated, actual).
pub struct GameEval { pub forecasts: Vec<Forecast>, pub outcomes: Outcomes, pub diag: Vec<(&'static str, f64, f64)> }

/// A served prediction ready for the ledger.
pub struct Prediction {
    pub payload: Value,
    pub forecasts: Vec<Forecast>,
    pub date: NaiveDate,
    pub start: Option<DateTime<Utc>>,
    pub engine: &'static str,
    pub n: usize,
}

/// A game on the schedule.
pub struct SlateGame { pub id: String, pub date: NaiveDate, pub start: Option<DateTime<Utc>>, pub label: String }

// ---------------------------------------------------------------------------
// Sport dispatch
// ---------------------------------------------------------------------------

pub(crate) async fn slate(state: &State, sport: &str, date: NaiveDate) -> Result<Vec<SlateGame>, String> {
    match base(sport) {
        "nhl" => crate::nhl_game::slate(&state.db, date).await,
        "nba" => crate::nba_game::slate(&state.db, date).await,
        "nfl" => crate::nfl_game::slate(&state.db, date).await,
        "mlb" => crate::mlb::ledger_slate(state, date).await,
        _ => Err(format!("unknown sport {sport}")),
    }
}

async fn predict(state: &State, sport: &str, id: &str, lineup: &str, cfg: &Value, seed: u64) -> Result<Prediction, String> {
    if is_team(sport) { return crate::team_engine::predict(state, base(sport), id, cfg, seed, LEDGER_N).await; }
    match sport {
        "nhl" => crate::nhl_game::predict(state, id, lineup, cfg, seed, LEDGER_N).await,
        "nba" => crate::nba_game::predict(state, id, lineup, cfg, seed, LEDGER_N).await,
        "nfl" => crate::nfl_game::predict(state, id, lineup, cfg, seed, LEDGER_N).await,
        "mlb" => crate::mlb::predict(state, id, lineup, cfg, seed, LEDGER_N).await,
        _ => Err(format!("unknown sport {sport}")),
    }
}

async fn outcomes(state: &State, sport: &str, id: &str) -> Result<Option<Outcomes>, String> {
    game_outcomes(&state.db, sport, id).await
}

/// Final outcomes of a game (None until final), for any engine of the sport.
pub(crate) async fn game_outcomes(db: &tokio_postgres::Client, sport: &str, id: &str) -> Result<Option<Outcomes>, String> {
    match base(sport) {
        "nhl" => crate::nhl_game::outcomes(db, id).await,
        "nba" => crate::nba_game::outcomes(db, id).await,
        "nfl" => crate::nfl_game::outcomes(db, id).await,
        "mlb" => crate::mlb::outcomes(db, id).await,
        _ => Err(format!("unknown sport {sport}")),
    }
}

/// Walk-forward evaluation of one historical game under a configuration.
pub(crate) async fn evaluate(state: &State, sport: &str, id: &str, cfg: &Value, n: usize, draws: usize, seed: u64) -> Result<Option<GameEval>, String> {
    evaluate_lineup(state, sport, id, cfg, n, draws, seed, "projected").await
}

/// As `evaluate`, with the simulators' lineup source ("projected", or "actual" for hindsight lineups).
pub(crate) async fn evaluate_lineup(state: &State, sport: &str, id: &str, cfg: &Value, n: usize, draws: usize, seed: u64, lineup: &str) -> Result<Option<GameEval>, String> {
    if is_team(sport) {
        let o = outcomes(state, sport, id).await?;
        return crate::team_engine::evaluate(state, base(sport), id, cfg, n, seed, o).await;
    }
    match sport {
        "nhl" => crate::nhl_game::evaluate(state, id, lineup, cfg, n, draws, seed).await,
        "nba" => crate::nba_game::evaluate(state, id, lineup, cfg, n, draws, seed).await,
        "nfl" => crate::nfl_game::evaluate(state, id, lineup, cfg, n, draws, seed).await,
        "mlb" => crate::mlb::evaluate(state, id, lineup, cfg, n, draws, seed).await,
        _ => Err(format!("unknown sport {sport}")),
    }
}

/// Completed regular-season games of a season in date order, evenly thinned to `games`.
pub(crate) async fn eval_games(state: &State, sport: &str, season: i32, games: usize) -> Result<Vec<String>, String> {
    let all = season_games(state, sport, season).await?;
    let step = (all.len() as f64 / games.max(1) as f64).max(1.0);
    Ok((0..games.min(all.len())).map(|i| all[(i as f64 * step) as usize].clone()).collect())
}

/// Every completed regular-season game of a season that the engine can be tested on, in date order.
pub(crate) async fn season_games(state: &State, sport: &str, season: i32) -> Result<Vec<String>, String> {
    Ok(match base(sport) {
        "nhl" => crate::nhl_game::backtest_games(&state.db, season).await?,
        "nba" => crate::nba_game::backtest_games(&state.db, season).await?,
        "nfl" => crate::nfl_game::backtest_games(&state.db, season).await?,
        "mlb" => crate::mlb::backtest_games(&state.db, season).await?,
        _ => return Err(format!("unknown sport {sport}")),
    })
}

pub(crate) fn engine_of(sport: &str) -> &'static str {
    if is_team(sport) { return crate::team_engine::engine(base(sport)); }
    match sport {
        "nhl" => crate::nhl_game::MODEL_VERSION,
        "nba" => crate::nba_game::MODEL_VERSION,
        "nfl" => crate::nfl_game::MODEL_VERSION,
        "mlb" => crate::mlb::SIM_MODEL,
        _ => "unknown",
    }
}

/// Hyper-parameters each engine accepts in a registry config, with defaults.
pub(crate) fn tunables(sport: &str) -> Value {
    if is_team(sport) { return crate::team_engine::tunables(base(sport)); }
    match sport {
        "nhl" => crate::nhl_game::Tuning::default().describe(),
        "nba" => crate::nba_game::Tuning::default().describe(),
        "nfl" => crate::nfl_game::Tuning::default().describe(),
        "mlb" => crate::mlb::Tuning::default().describe(),
        _ => json!({}),
    }
}

pub(crate) fn fnv(parts: &[&str]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for p in parts { for b in p.bytes().chain(std::iter::once(0xff)) { h ^= b as u64; h = h.wrapping_mul(0x100000001b3); } }
    h
}

fn canonical(cfg: &Value) -> String {
    // serde_json maps are ordered (BTreeMap) without the preserve_order feature
    serde_json::to_string(cfg).unwrap_or_default()
}

pub(crate) fn version_for(sport: &str, cfg: &Value) -> String {
    let empty = cfg.as_object().map(|m| m.is_empty()).unwrap_or(true);
    if empty { format!("{}+default", engine_of(sport)) } else { format!("{}+cfg.{:08x}", engine_of(sport), fnv(&[&canonical(cfg)]) as u32) }
}

pub(crate) async fn audit(db: &tokio_postgres::Client, sport: &str, action: &str, version: Option<&str>, detail: Value) {
    if let Err(e) = db.execute("INSERT INTO learning_audit (sport, action, version, detail) VALUES ($1,$2,$3,$4)",
        &[&sport, &action, &version, &Json(detail)]).await { eprintln!("projection-service(learning): audit: {}", dberr(&e)); }
}

/// The sport's champion (version, config); registers the engine default as the
/// first champion when the registry is empty.
pub(crate) async fn champion(db: &tokio_postgres::Client, sport: &str) -> Result<(String, Value), String> {
    if let Some(r) = db.query_opt("SELECT version, config, engine FROM model_registry WHERE sport=$1 AND status='champion'", &[&sport]).await.map_err(|e| dberr(&e))? {
        let (old, c, engine): (String, Json<Value>, String) = (r.get(0), r.get(1), r.get(2));
        if engine == engine_of(sport) {
            return Ok((old, c.0));
        }
        // The engine code changed (new MODEL_VERSION): the champion's
        // configuration carries over under a version that names the new
        // engine, so ledger rows never attribute new code to an old version.
        // One statement; the single-champion constraint is checked at its end.
        let v = version_for(sport, &c.0);
        db.execute("WITH old AS (UPDATE model_registry SET status='retired', retired_at=now() WHERE sport=$1 AND version=$2 RETURNING config) \
                    INSERT INTO model_registry (sport, version, engine, config, status, parent, notes, promoted_at) \
                    SELECT $1, $3, $4, config, 'champion', $2, 'engine upgrade: champion configuration on the new engine', now() FROM old \
                    ON CONFLICT (sport, version) DO UPDATE SET status='champion', promoted_at=now(), retired_at=NULL",
            &[&sport, &old, &v, &engine_of(sport)]).await.map_err(|e| dberr(&e))?;
        audit(db, sport, "engine_upgrade", Some(&v), json!({ "from": old, "from_engine": engine, "engine": engine_of(sport) })).await;
        return Ok((v, c.0));
    }
    let v = version_for(sport, &json!({}));
    let n = db.execute("INSERT INTO model_registry (sport, version, engine, config, status, notes, promoted_at) VALUES ($1,$2,$3,'{}','champion',$4, now()) ON CONFLICT (sport, version) DO NOTHING",
        &[&sport, &v, &engine_of(sport), &"initial champion: engine defaults"]).await.map_err(|e| dberr(&e))?;
    if n > 0 { audit(db, sport, "register", Some(&v), json!({ "status": "champion", "reason": "registry was empty; engine defaults become the first champion" })).await; }
    Ok((v, json!({})))
}

fn check_sport(s: &str) -> Result<(), HttpResponse> {
    if ENGINES.contains(&s) { Ok(()) } else { Err(bad_request(format!("sport must be one of {ENGINES:?}"))) }
}

// ---------------------------------------------------------------------------
// Freeze
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct FreezeQuery { date: Option<String>, lineup: Option<String>, game: Option<String> }

/// POST /learning/{sport}/freeze?date=YYYY-MM-DD&lineup=confirmed[&game=id]
///
/// Freezes the champion's prediction for every game on `date` that has not
/// started. A game is never frozen after its scheduled start; when the source
/// has no start time the game must be on a later calendar day (US Eastern).
pub async fn freeze(state: web::Data<State>, path: web::Path<String>, q: web::Query<FreezeQuery>) -> impl Responder {
    let sport = path.into_inner();
    if let Err(r) = check_sport(&sport) { return r; }
    let today = (Utc::now() - chrono::Duration::hours(5)).date_naive();
    let date = match q.date.as_deref() { Some(d) => match NaiveDate::parse_from_str(d, "%Y-%m-%d") { Ok(d) => d, Err(_) => return bad_request("date must be YYYY-MM-DD") }, None => today };
    let lineup = q.lineup.clone().unwrap_or_else(|| "confirmed".into());
    if !matches!(lineup.as_str(), "projected" | "confirmed") { return bad_request("lineup must be projected or confirmed (actual is hindsight and cannot be frozen)"); }
    match freeze_slate(&state, &sport, date, &lineup, q.game.as_deref()).await {
        Ok(v) => HttpResponse::Ok().json(v),
        Err(e) => unprocessable(e),
    }
}

async fn freeze_slate(state: &State, sport: &str, date: NaiveDate, lineup: &str, only: Option<&str>) -> Result<Value, String> {
    let db = &state.db;
    let (version, cfg) = champion(db, sport).await?;
    let games = slate(state, sport, date).await?;
    let mut frozen = Vec::new();
    let mut skipped = Vec::new();
    for g in games.iter().filter(|g| only.map(|o| o == g.id).unwrap_or(true)) {
        match freeze_game(state, sport, g, lineup, &version, &cfg).await? {
            Ok(f) => frozen.push(f),
            Err(reason) => skipped.push(json!({ "game_id": g.id, "game": g.label, "reason": reason })),
        }
    }
    let lineup = if is_team(sport) { "none" } else { lineup };
    let out = json!({ "sport": sport, "date": date, "model_version": version, "lineup": lineup, "frozen": frozen, "skipped": skipped });
    if !frozen.is_empty() { audit(db, sport, "freeze", Some(&version), json!({ "date": date, "games": frozen.len(), "lineup": lineup })).await; }
    Ok(out)
}

/// Freeze one version's prediction for one game. The outer error is a database
/// failure; the inner one is why this game was skipped (started, already
/// frozen, too few valid simulations, ...). Team engines ignore lineups and
/// freeze once, as lineup mode "none".
pub(crate) async fn freeze_game(state: &State, sport: &str, g: &SlateGame, lineup: &str, version: &str, cfg: &Value) -> Result<Result<Value, String>, String> {
    let db = &state.db;
    let lineup = if is_team(sport) { "none" } else { lineup };
    let today = (Utc::now() - chrono::Duration::hours(5)).date_naive();
    let open = match g.start { Some(s) => s > Utc::now() + chrono::Duration::minutes(2), None => g.date > today };
    if !open { return Ok(Err("started or start time unknown on game day".into())); }
    if db.query_opt("SELECT 1 FROM prediction_ledger WHERE sport=$1 AND game_id=$2 AND model_version=$3 AND lineup_mode=$4", &[&sport, &g.id, &version, &lineup])
        .await.map_err(|e| dberr(&e))?.is_some() {
        return Ok(Err("already frozen for this version".into()));
    }
    let seed = fnv(&[sport, &g.id, version, lineup]);
    let p = match predict(state, sport, &g.id, lineup, cfg, seed).await { Ok(p) => p, Err(e) => return Ok(Err(e)) };
    if p.n < LEDGER_N { return Ok(Err(format!("only {} valid simulations", p.n))); }
    let start = p.start.or(g.start);
    if let Some(s) = start { if s <= Utc::now() { return Ok(Err("started while simulating".into())); } }
    let fc: Vec<Value> = p.forecasts.iter().filter(|f| f.p_model.is_finite() && f.p_base.is_finite() && f.line.is_finite()).map(|f| json!({ "market": f.market, "subject": f.subject, "name": f.name, "stat": f.stat,
        "line": f.line, "p_model": f.p_model, "p_base": f.p_base, "baseline": f.baseline, "push_void": f.push_void })).collect();
    let cutoff = p.date.pred_opt().unwrap_or(p.date);
    let row = db.query_one(
        "WITH l AS (INSERT INTO prediction_ledger (sport, game_id, game_date, game_start, model_version, engine, lineup_mode, seed, n_sims, data_cutoff, payload_sha256, payload) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10, encode(sha256(convert_to($11::jsonb::text, 'UTF8')), 'hex'), $11) RETURNING id), \
         f AS (INSERT INTO prediction_forecasts (ledger_id, market, subject, name, stat, line, p_model, p_base, baseline, push_void) \
             SELECT l.id, x.market, x.subject, x.name, x.stat, x.line, x.p_model, x.p_base, x.baseline, x.push_void FROM l, \
             jsonb_to_recordset($12) AS x(market text, subject text, name text, stat text, line float8, p_model float8, p_base float8, baseline text, push_void boolean) RETURNING 1) \
         SELECT (SELECT id FROM l), (SELECT count(*) FROM f)",
        &[&sport, &g.id, &p.date, &start, &version, &p.engine, &lineup, &(seed as i64), &(p.n as i32), &cutoff, &Json(&p.payload), &Json(Value::Array(fc))],
    ).await.map_err(|e| dberr(&e))?;
    let (id, nf): (i64, i64) = (row.get(0), row.get(1));
    Ok(Ok(json!({ "ledger_id": id, "game_id": g.id, "game": g.label, "start": start, "forecasts": nf, "n_sims": p.n, "version": version, "lineup": lineup })))
}

// ---------------------------------------------------------------------------
// Score
// ---------------------------------------------------------------------------

/// POST /learning/{sport}/score — writes outcomes for every frozen forecast whose game is final.
pub async fn score(state: web::Data<State>, path: web::Path<String>) -> impl Responder {
    let sport = path.into_inner();
    if let Err(r) = check_sport(&sport) { return r; }
    match score_sport(&state, &sport).await { Ok(v) => HttpResponse::Ok().json(v), Err(e) => internal(e) }
}

pub(crate) async fn score_sport(state: &State, sport: &str) -> Result<Value, String> {
    let db = &state.db;
    let rows = db.query("SELECT DISTINCT l.id, l.game_id FROM prediction_ledger l JOIN prediction_forecasts f ON f.ledger_id = l.id \
                         WHERE l.sport=$1 AND f.scored_at IS NULL ORDER BY l.id", &[&sport]).await.map_err(|e| dberr(&e))?;
    let mut by_game: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    for r in &rows { by_game.entry(r.get(1)).or_default().push(r.get(0)); }
    let (mut games, mut scored, mut voided, mut pending) = (0, 0u64, 0u64, 0);
    for (gid, ids) in &by_game {
        let Some(o) = outcomes(state, sport, gid).await? else { pending += 1; continue };
        games += 1;
        for id in ids {
            let fs = db.query("SELECT market, subject, stat, line, push_void FROM prediction_forecasts WHERE ledger_id=$1 AND scored_at IS NULL", &[id]).await.map_err(|e| dberr(&e))?;
            for f in &fs {
                let (market, subject, stat, line, push_void): (String, String, String, f64, bool) = (f.get(0), f.get(1), f.get(2), f.get(3), f.get(4));
                let v = if subject == "game" { o.game.get(stat.as_str()).copied() } else { o.players.get(&subject).and_then(|s| s.get(stat.as_str()).copied()) };
                let (actual, y, void): (Option<f64>, Option<i16>, Option<&str>) = match v {
                    Some(v) if push_void && (v - line).abs() < 1e-9 => (Some(v), None, Some("push at the line")),
                    Some(v) => (Some(v), Some((v > line) as i16), None),
                    None => (None, None, Some(if subject == "game" { "no outcome for this statistic" } else { "did not play (props are conditional on playing)" })),
                };
                db.execute("UPDATE prediction_forecasts SET actual=$4, y=$5, void_reason=$6, scored_at=now() WHERE ledger_id=$1 AND market=$2 AND subject=$3",
                    &[id, &market, &subject, &actual, &y, &void]).await.map_err(|e| dberr(&e))?;
                if void.is_some() { voided += 1 } else { scored += 1 }
            }
        }
    }
    let out = json!({ "sport": sport, "games_scored": games, "forecasts_scored": scored, "forecasts_void": voided, "games_pending": pending });
    if games > 0 { audit(db, sport, "score", None, out.clone()).await; }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Cycle: refresh results, score, freeze upcoming
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CycleQuery { #[serde(default = "d_days")] days_ahead: i64, lineup: Option<String>, #[serde(default)] skip_ingest: bool }
fn d_days() -> i64 { 1 }

/// POST /learning/{sport}/cycle — ingest the last few days' results, score the
/// ledger, then freeze today's (not yet started) and the next `days_ahead` slates.
pub async fn cycle(state: web::Data<State>, path: web::Path<String>, q: web::Query<CycleQuery>) -> impl Responder {
    let sport = path.into_inner();
    if let Err(r) = check_sport(&sport) { return r; }
    let today = (Utc::now() - chrono::Duration::hours(5)).date_naive();
    let mut ingest = Value::Null;
    if !q.skip_ingest {
        ingest = ingest_recent(&state, &sport, today, q.days_ahead).await;
    }
    let scored = match score_sport(&state, &sport).await { Ok(v) => v, Err(e) => json!({ "error": e }) };
    let lineup = q.lineup.clone().unwrap_or_else(|| "confirmed".into());
    let mut frozen = Vec::new();
    for d in 0..=q.days_ahead.clamp(0, 7) {
        let date = today + chrono::Duration::days(d);
        frozen.push(match freeze_slate(&state, &sport, date, &lineup, None).await { Ok(v) => v, Err(e) => json!({ "date": date, "error": e }) });
    }
    HttpResponse::Ok().json(json!({ "sport": sport, "ingest": ingest, "score": scored, "freeze": frozen }))
}

/// Refresh the sport's recent results and upcoming schedule through the
/// service's own ingest endpoints (the last three days through `days_ahead`).
pub(crate) async fn ingest_recent(state: &State, sport: &str, today: NaiveDate, days_ahead: i64) -> Value {
    let port = std::env::var("PORT").unwrap_or_else(|_| "8121".into());
    let from = today - chrono::Duration::days(3);
    let to = today + chrono::Duration::days(days_ahead.clamp(0, 7));
    let url = match base(sport) {
        "nhl" => format!("http://127.0.0.1:{port}/ingest/nhl/api?from={from}&to={to}"),
        "nba" => format!("http://127.0.0.1:{port}/ingest/nba/espn?from={from}&to={to}"),
        "mlb" => format!("http://127.0.0.1:{port}/ingest/mlb?from={}&to={}", today.format("%Y"), today.format("%Y")),
        _ => format!("http://127.0.0.1:{port}/ingest/nfl/pbp?seasons={}", crate::nfl_game::season_of(today)),
    };
    match state.http.post(&url).timeout(std::time::Duration::from_secs(1800)).send().await {
        Ok(r) => { let s = r.status(); json!({ "url": url, "status": s.as_u16(), "ok": s.is_success() }) }
        Err(e) => json!({ "url": url, "error": e.to_string(), "ok": false }),
    }
}

// ---------------------------------------------------------------------------
// Read side: ledger, summary, registry, audit
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct ListQuery { sport: Option<String>, #[serde(default = "d_limit")] limit: i64, version: Option<String> }
fn d_limit() -> i64 { 50 }

/// GET /learning/ledger?sport=&limit=
pub async fn ledger(state: web::Data<State>, q: web::Query<ListQuery>) -> impl Responder {
    let rows = match state.db.query(
        "SELECT l.id, l.sport, l.game_id, l.game_date::text, l.game_start::text, l.frozen_at::text, l.model_version, l.lineup_mode, l.n_sims, l.payload_sha256, \
                l.payload->>'away', l.payload->>'home', \
                count(f.*), count(f.scored_at), count(f.y), avg((f.p_model - f.y)^2), avg((f.p_base - f.y)^2) \
         FROM prediction_ledger l LEFT JOIN prediction_forecasts f ON f.ledger_id = l.id \
         WHERE ($1::text IS NULL OR l.sport = $1 OR l.sport = $1 || '_team') GROUP BY l.id ORDER BY l.game_date DESC, l.id DESC LIMIT $2",
        &[&q.sport, &q.limit.clamp(1, 500)]).await { Ok(r) => r, Err(e) => return internal(dberr(&e)) };
    let out: Vec<Value> = rows.iter().map(|r| {
        let (away, home): (Option<String>, Option<String>) = (r.get(10), r.get(11));
        json!({ "id": r.get::<_, i64>(0), "sport": r.get::<_, String>(1), "game_id": r.get::<_, String>(2), "game_date": r.get::<_, String>(3),
            "game_start": r.get::<_, Option<String>>(4), "frozen_at": r.get::<_, String>(5), "model_version": r.get::<_, String>(6), "lineup": r.get::<_, String>(7),
            "n_sims": r.get::<_, i32>(8), "sha256": r.get::<_, String>(9), "away": away, "home": home,
            "forecasts": r.get::<_, i64>(12), "scored": r.get::<_, i64>(13), "resolved": r.get::<_, i64>(14),
            "brier_model": r.get::<_, Option<f64>>(15).map(r4), "brier_baseline": r.get::<_, Option<f64>>(16).map(r4) })
    }).collect();
    HttpResponse::Ok().json(json!({ "ledger": out }))
}

/// GET /learning/ledger/{id} — the frozen payload and every forecast with its outcome.
pub async fn ledger_entry(state: web::Data<State>, path: web::Path<i64>) -> impl Responder {
    let id = path.into_inner();
    let db = &state.db;
    let Some(l) = (match db.query_opt("SELECT sport, game_id, game_date::text, game_start::text, frozen_at::text, model_version, engine, lineup_mode, seed, n_sims, data_cutoff::text, payload_sha256, payload, \
                                         payload_sha256 = encode(sha256(convert_to(payload::text, 'UTF8')), 'hex') FROM prediction_ledger WHERE id=$1", &[&id]).await {
        Ok(r) => r, Err(e) => return internal(dberr(&e)) }) else { return HttpResponse::NotFound().json(json!({ "error": "no such ledger entry" })) };
    let fs = match db.query("SELECT market, subject, name, stat, line, p_model, p_base, baseline, actual, y, void_reason, scored_at::text FROM prediction_forecasts WHERE ledger_id=$1 ORDER BY market, name", &[&id]).await {
        Ok(r) => r, Err(e) => return internal(dberr(&e)) };
    let payload: Json<Value> = l.get(12);
    HttpResponse::Ok().json(json!({
        "id": id, "sport": l.get::<_, String>(0), "game_id": l.get::<_, String>(1), "game_date": l.get::<_, String>(2), "game_start": l.get::<_, Option<String>>(3),
        "frozen_at": l.get::<_, String>(4), "model_version": l.get::<_, String>(5), "engine": l.get::<_, String>(6), "lineup": l.get::<_, String>(7),
        "seed": l.get::<_, i64>(8), "n_sims": l.get::<_, i32>(9), "data_cutoff": l.get::<_, String>(10), "sha256": l.get::<_, String>(11), "hash_verified": l.get::<_, bool>(13),
        "forecasts": fs.iter().map(|f| json!({ "market": f.get::<_, String>(0), "subject": f.get::<_, String>(1), "name": f.get::<_, String>(2), "stat": f.get::<_, String>(3),
            "line": f.get::<_, f64>(4), "p_model": r4(f.get(5)), "p_base": r4(f.get(6)), "baseline": f.get::<_, String>(7), "actual": f.get::<_, Option<f64>>(8),
            "y": f.get::<_, Option<i16>>(9), "void_reason": f.get::<_, Option<String>>(10), "scored_at": f.get::<_, Option<String>>(11) })).collect::<Vec<_>>(),
        "payload": payload.0,
    }))
}

/// GET /learning/summary?sport=&version= — live (ledger) performance per market,
/// next to the latest walk-forward backtests.
pub async fn summary(state: web::Data<State>, q: web::Query<ListQuery>) -> impl Responder {
    let db = &state.db;
    let rows = match db.query(
        "SELECT l.sport, l.model_version, f.market, f.baseline, f.p_model, f.p_base, f.y::float8 \
         FROM prediction_forecasts f JOIN prediction_ledger l ON l.id = f.ledger_id \
         WHERE f.y IS NOT NULL AND ($1::text IS NULL OR l.sport=$1 OR l.sport=$1 || '_team') AND ($2::text IS NULL OR l.model_version=$2)",
        &[&q.sport, &q.version]).await { Ok(r) => r, Err(e) => return internal(dberr(&e)) };
    let mut live: BTreeMap<(String, String), BTreeMap<String, (String, Pair)>> = BTreeMap::new();
    for r in &rows {
        let e = live.entry((r.get(0), r.get(1))).or_default().entry(r.get(2)).or_insert_with(|| (r.get(3), Pair::default()));
        e.1.add(r.get(4), r.get(5), r.get(6));
    }
    let live: Vec<Value> = live.into_iter().map(|((s, v), m)| json!({ "sport": s, "model_version": v,
        "markets": Value::Object(m.into_iter().map(|(k, (b, p))| (k, p.report(&b))).collect()) })).collect();
    let counts = match db.query("SELECT l.sport, count(DISTINCT l.id), count(f.*), count(f.y), count(f.void_reason), min(l.game_date)::text, max(l.game_date)::text \
                                 FROM prediction_ledger l LEFT JOIN prediction_forecasts f ON f.ledger_id=l.id WHERE ($1::text IS NULL OR l.sport=$1 OR l.sport=$1 || '_team') GROUP BY 1", &[&q.sport]).await {
        Ok(r) => r, Err(e) => return internal(dberr(&e)) };
    let counts: Vec<Value> = counts.iter().map(|r| json!({ "sport": r.get::<_, String>(0), "predictions": r.get::<_, i64>(1), "forecasts": r.get::<_, i64>(2),
        "resolved": r.get::<_, i64>(3), "void": r.get::<_, i64>(4), "first_game": r.get::<_, Option<String>>(5), "last_game": r.get::<_, Option<String>>(6) })).collect();
    let bt = match db.query("SELECT key, params, fitted_at::text FROM sim_model_params WHERE key LIKE '%:backtest:%' \
                             UNION ALL SELECT 'mlb:' || key, params, fitted_at::text FROM mlb_model_params WHERE key LIKE 'backtest:%' ORDER BY 1", &[]).await {
        Ok(r) => r, Err(e) => return internal(dberr(&e)) };
    let backtests: Vec<Value> = bt.iter().filter(|r| q.sport.as_deref().map(|s| r.get::<_, String>(0).starts_with(s)).unwrap_or(true)).map(|r| {
        let p: Json<Value> = r.get(1);
        json!({ "key": r.get::<_, String>(0), "run_at": r.get::<_, String>(2), "season": p.0["season"], "games": p.0.get("games_simulated").cloned().unwrap_or(Value::Null),
                "lineup": p.0.get("lineup").cloned().unwrap_or(Value::Null), "markets": p.0.get("binary").cloned().unwrap_or(Value::Null),
                "model_version": p.0.get("model_version").cloned().unwrap_or(Value::Null) })
    }).collect();
    HttpResponse::Ok().json(json!({ "live": live, "counts": counts, "backtests": backtests,
        "note": "Live = frozen pregame predictions scored after the game. Brier diff = model − baseline (negative is better); ± is one standard error of the paired difference." }))
}

/// GET /learning/registry?sport=
pub async fn registry(state: web::Data<State>, q: web::Query<ListQuery>) -> impl Responder {
    for s in ENGINES { if q.sport.as_deref().map(|x| x == base(s)).unwrap_or(true) { if let Err(e) = champion(&state.db, s).await { return internal(e) } } }
    let rows = match state.db.query("SELECT sport, version, engine, config, status, parent, evidence, notes, created_at::text, promoted_at::text, retired_at::text \
                                     FROM model_registry WHERE ($1::text IS NULL OR sport=$1 OR sport=$1 || '_team') ORDER BY sport, created_at DESC", &[&q.sport]).await {
        Ok(r) => r, Err(e) => return internal(dberr(&e)) };
    let out: Vec<Value> = rows.iter().map(|r| {
        let (c, e): (Json<Value>, Option<Json<Value>>) = (r.get(3), r.get(6));
        json!({ "sport": r.get::<_, String>(0), "version": r.get::<_, String>(1), "engine": r.get::<_, String>(2), "config": c.0, "status": r.get::<_, String>(4),
            "parent": r.get::<_, Option<String>>(5), "evidence": e.map(|x| x.0), "notes": r.get::<_, Option<String>>(7), "created_at": r.get::<_, String>(8),
            "promoted_at": r.get::<_, Option<String>>(9), "retired_at": r.get::<_, Option<String>>(10), "current_engine": engine_of(&r.get::<_, String>(0)) })
    }).collect();
    let tun: serde_json::Map<String, Value> = ENGINES.iter().map(|s| (s.to_string(), tunables(s))).collect();
    HttpResponse::Ok().json(json!({ "registry": out, "tunables": tun }))
}

/// GET /learning/audit?sport=&limit=
pub async fn audit_log(state: web::Data<State>, q: web::Query<ListQuery>) -> impl Responder {
    let rows = match state.db.query("SELECT id, at::text, sport, action, version, detail FROM learning_audit WHERE ($1::text IS NULL OR sport=$1 OR sport=$1 || '_team') ORDER BY id DESC LIMIT $2",
        &[&q.sport, &q.limit.clamp(1, 1000)]).await { Ok(r) => r, Err(e) => return internal(dberr(&e)) };
    HttpResponse::Ok().json(json!({ "audit": rows.iter().map(|r| { let d: Json<Value> = r.get(5); json!({ "id": r.get::<_, i64>(0), "at": r.get::<_, String>(1),
        "sport": r.get::<_, String>(2), "action": r.get::<_, String>(3), "version": r.get::<_, Option<String>>(4), "detail": d.0 }) }).collect::<Vec<_>>() }))
}

// ---------------------------------------------------------------------------
// Candidates, paired evaluation, promotion, rollback
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CandidateBody { config: Value, notes: Option<String> }

/// POST /learning/{sport}/candidates  {"config": {...}, "notes": "..."}
pub async fn register(state: web::Data<State>, path: web::Path<String>, body: web::Json<CandidateBody>) -> impl Responder {
    let sport = path.into_inner();
    if let Err(r) = check_sport(&sport) { return r; }
    let Some(obj) = body.config.as_object() else { return bad_request("config must be an object") };
    let allowed = tunables(&sport);
    for (k, v) in obj {
        let Some(spec) = allowed.get(k) else { return bad_request(format!("unknown tunable {k}; allowed: {}", allowed.as_object().map(|m| m.keys().cloned().collect::<Vec<_>>().join(", ")).unwrap_or_default())) };
        let (Some(x), Some(lo), Some(hi)) = (v.as_f64(), spec["min"].as_f64(), spec["max"].as_f64()) else { return bad_request(format!("{k} must be a number")) };
        if x < lo || x > hi { return bad_request(format!("{k} must be within [{lo}, {hi}]")); }
    }
    let (champ, _) = match champion(&state.db, &sport).await { Ok(c) => c, Err(e) => return internal(e) };
    let v = version_for(&sport, &body.config);
    if v == champ { return bad_request("that configuration is the current champion"); }
    match state.db.execute("INSERT INTO model_registry (sport, version, engine, config, status, parent, notes) VALUES ($1,$2,$3,$4,'candidate',$5,$6) \
                            ON CONFLICT (sport, version) DO NOTHING",
        &[&sport, &v, &engine_of(&sport), &Json(&body.config), &champ, &body.notes]).await {
        Ok(0) => return bad_request(format!("{v} is already registered")),
        Ok(_) => {}
        Err(e) => return internal(dberr(&e)),
    }
    audit(&state.db, &sport, "register", Some(&v), json!({ "status": "candidate", "config": body.config, "parent": champ, "notes": body.notes })).await;
    HttpResponse::Ok().json(json!({ "sport": sport, "version": v, "status": "candidate", "parent": champ }))
}

#[derive(Deserialize)]
pub struct EvalQuery {
    version: String,
    season: Option<i32>,
    #[serde(default = "d_eval_games")] games: usize,
    #[serde(default = "d_eval_n")] n: usize,
    #[serde(default = "d_eval_draws")] draws: usize,
    /// simulators' lineup source: projected (default, what the tuner uses) or actual (hindsight)
    lineup: Option<String>,
}
fn d_eval_games() -> usize { 300 }
fn d_eval_n() -> usize { 2000 }
fn d_eval_draws() -> usize { 20 }

/// Promotion gate on paired (candidate − champion) Brier differences, per market.
/// Promote only if at least one market improves by ≥ 2 SE and none degrades by ≥ 2 SE.
const GATE_Z: f64 = 2.0;
const GATE_MIN_N: f64 = 200.0;

/// POST /learning/{sport}/evaluate?version=&season=&games=
///
/// Paired walk-forward evaluation: champion and candidate simulate the same
/// historical games with the same seeds, using only data before each game.
pub async fn evaluate_candidate(state: web::Data<State>, path: web::Path<String>, q: web::Query<EvalQuery>) -> impl Responder {
    let sport = path.into_inner();
    if let Err(r) = check_sport(&sport) { return r; }
    let db = &state.db;
    let Some(row) = (match db.query_opt("SELECT config, status FROM model_registry WHERE sport=$1 AND version=$2", &[&sport, &q.version]).await {
        Ok(r) => r, Err(e) => return internal(dberr(&e)) }) else { return bad_request("unknown version") };
    let (cand_cfg, status): (Json<Value>, String) = (row.get(0), row.get(1));
    if status != "candidate" { return bad_request(format!("{} is {status}, not a candidate", q.version)); }
    let (champ, champ_cfg) = match champion(db, &sport).await { Ok(c) => c, Err(e) => return internal(e) };
    let season = q.season.unwrap_or(match base(&sport) { "nhl" => 20252026, "nba" => 2026, _ => 2025 });
    let picked = match eval_games(&state, &sport, season, q.games).await { Ok(p) => p, Err(e) => return internal(e) };
    if picked.is_empty() { return unprocessable("no completed games to evaluate on"); }
    let lineup = if q.lineup.as_deref() == Some("actual") { "actual" } else { "projected" };
    let mut report = paired_eval(&state, &sport, &champ_cfg, &cand_cfg.0, &picked, q.n, q.draws, lineup).await;
    report["lineup"] = json!(lineup);
    report["candidate"] = json!(q.version);
    report["champion"] = json!(champ);
    report["season"] = json!(season);
    let pass = report["gate"]["pass"].as_bool().unwrap_or(false);
    if let Err(e) = db.execute("UPDATE model_registry SET evidence=$3 WHERE sport=$1 AND version=$2", &[&sport, &q.version, &Json(&report)]).await { return internal(dberr(&e)); }
    audit(db, &sport, "evaluate", Some(&q.version), json!({ "champion": champ, "season": season, "games": report["games"], "pass": pass, "improves": report["gate"]["improves"], "degrades": report["gate"]["degrades"] })).await;
    HttpResponse::Ok().json(report)
}

/// Champion and candidate simulate the same games with the same seeds; the
/// report has per-market paired Brier differences (candidate − champion) and
/// the promotion gate.
pub(crate) async fn paired_eval(state: &State, sport: &str, champ_cfg: &Value, cand_cfg: &Value, games: &[String], n: usize, draws: usize, lineup: &str) -> Value {
    let t0 = std::time::Instant::now();
    let (mut sc_champ, mut sc_cand) = (Scorecard::default(), Scorecard::default());
    let mut paired: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    let mut used = 0;
    for gid in games {
        let seed = fnv(&["eval", sport, gid]);
        let a = match evaluate_lineup(state, sport, gid, champ_cfg, n, draws, seed, lineup).await { Ok(Some(e)) => e, Ok(None) => continue, Err(e) => { eprintln!("eval {gid}: {e}"); continue } };
        let b = match evaluate_lineup(state, sport, gid, cand_cfg, n, draws, seed, lineup).await { Ok(Some(e)) => e, Ok(None) => continue, Err(e) => { eprintln!("eval {gid}: {e}"); continue } };
        used += 1;
        sc_champ.add_all(&a.forecasts, &a.outcomes);
        sc_cand.add_all(&b.forecasts, &b.outcomes);
        let bmap: HashMap<(&str, &str), &Forecast> = b.forecasts.iter().map(|f| (f.key(), f)).collect();
        for fa in &a.forecasts {
            let (Some(fb), Some(y)) = (bmap.get(&fa.key()), a.outcomes.y(fa)) else { continue };
            if (fb.line - fa.line).abs() > 1e-9 { continue; }
            let c = |p: f64| (p.clamp(1e-4, 1.0 - 1e-4) - y).powi(2);
            paired.entry(fa.market.to_string()).or_default().push(c(fb.p_model) - c(fa.p_model));
        }
    }
    let (markets, improves, degrades) = gate(&paired);
    let pass = !improves.is_empty() && degrades.is_empty();
    json!({
        "sport": sport, "games": used, "sims_per_game": n, "param_draws": draws, "secs": t0.elapsed().as_secs(),
        "paired": markets, "champion_scores": sc_champ.report(), "candidate_scores": sc_cand.report(),
        "gate": { "rule": format!("promote if ≥1 market improves by ≥{GATE_Z} SE and none degrades by ≥{GATE_Z} SE (markets with ≥{GATE_MIN_N} paired forecasts)"),
                  "improves": improves, "degrades": degrades, "pass": pass },
        "evaluated_at": Utc::now(),
    })
}

/// Per-market (mean, SE, z) of paired differences (new − old Brier) and the
/// markets that improve / degrade by at least GATE_Z standard errors.
pub(crate) fn gate(paired: &BTreeMap<String, Vec<f64>>) -> (serde_json::Map<String, Value>, Vec<String>, Vec<String>) {
    let (mut improves, mut degrades) = (Vec::new(), Vec::new());
    let markets = paired.iter().map(|(m, d)| {
        let (md, se) = Pair::mean_se(d);
        let z = if se > 0.0 { md / se } else { 0.0 };
        let n = d.len() as f64;
        if n >= GATE_MIN_N && z <= -GATE_Z { improves.push(m.clone()); }
        if n >= GATE_MIN_N && z >= GATE_Z { degrades.push(m.clone()); }
        (m.clone(), json!({ "n": n, "brier_diff_candidate_minus_champion": r4(md), "se": r4(se), "z": (z * 100.0).round() / 100.0 }))
    }).collect();
    (markets, improves, degrades)
}

#[derive(Deserialize)]
pub struct PromoteQuery { version: String, reason: Option<String> }

/// POST /learning/{sport}/promote?version= — only a candidate whose stored
/// evaluation passed the gate, against the current champion, can be promoted.
pub async fn promote(state: web::Data<State>, path: web::Path<String>, q: web::Query<PromoteQuery>) -> impl Responder {
    let sport = path.into_inner();
    if let Err(r) = check_sport(&sport) { return r; }
    let db = &state.db;
    let (champ, _) = match champion(db, &sport).await { Ok(c) => c, Err(e) => return internal(e) };
    let Some(row) = (match db.query_opt("SELECT status, evidence FROM model_registry WHERE sport=$1 AND version=$2", &[&sport, &q.version]).await {
        Ok(r) => r, Err(e) => return internal(dberr(&e)) }) else { return bad_request("unknown version") };
    let (status, ev): (String, Option<Json<Value>>) = (row.get(0), row.get(1));
    if status != "candidate" && status != "shadow" { return bad_request(format!("{} is {status}", q.version)); }
    let Some(ev) = ev.map(|e| e.0) else { return unprocessable("no evaluation on record; POST /learning/{sport}/evaluate first") };
    if ev["champion"].as_str() != Some(champ.as_str()) { return unprocessable(format!("evaluation was against {}, but the champion is now {champ}; re-evaluate", ev["champion"])); }
    if ev["gate"]["pass"].as_bool() != Some(true) {
        let _ = db.execute("UPDATE model_registry SET status='rejected', retired_at=now() WHERE sport=$1 AND version=$2", &[&sport, &q.version]).await;
        audit(db, &sport, "reject", Some(&q.version), json!({ "champion": champ, "gate": ev["gate"] })).await;
        return unprocessable(format!("{} failed the promotion gate and was rejected", q.version));
    }
    if let Err(e) = swap_to(db, &sport, &champ, &q.version, "retired").await { return internal(e); }
    audit(db, &sport, "promote", Some(&q.version), json!({ "from": champ, "reason": q.reason, "gate": ev["gate"] })).await;
    HttpResponse::Ok().json(json!({ "sport": sport, "champion": q.version, "retired": champ }))
}

/// `to` becomes champion; `from` (the champion) gets `old_status`: retired,
/// rejected (after a rollback), or shadow (kept running as the rollback
/// reference). One statement: the single-champion constraint is checked at its end.
pub(crate) async fn swap_to(db: &tokio_postgres::Client, sport: &str, from: &str, to: &str, old_status: &str) -> Result<(), String> {
    db.execute("UPDATE model_registry SET status = CASE WHEN version=$2 THEN $4 ELSE 'champion' END, \
                    retired_at = CASE WHEN version=$2 THEN now() ELSE NULL END, \
                    promoted_at = CASE WHEN version=$3 THEN now() ELSE promoted_at END \
                WHERE sport=$1 AND version IN ($2, $3)", &[&sport, &from, &to, &old_status]).await.map_err(|e| dberr(&e))?;
    Ok(())
}

/// POST /learning/{sport}/rollback?reason= — restore the most recently retired champion.
pub async fn rollback(state: web::Data<State>, path: web::Path<String>, q: web::Query<RollbackQuery>) -> impl Responder {
    let sport = path.into_inner();
    if let Err(r) = check_sport(&sport) { return r; }
    let db = &state.db;
    let (champ, _) = match champion(db, &sport).await { Ok(c) => c, Err(e) => return internal(e) };
    let prev: Option<String> = match db.query_opt("SELECT version FROM model_registry WHERE sport=$1 AND status IN ('retired','shadow') AND promoted_at IS NOT NULL AND engine=$2 ORDER BY retired_at DESC NULLS LAST LIMIT 1", &[&sport, &engine_of(&sport)]).await {
        Ok(r) => r.map(|r| r.get(0)), Err(e) => return internal(dberr(&e)) };
    // (a champion retired by an engine upgrade ran code that no longer exists)
    let Some(prev) = prev else { return unprocessable("no previous champion on the current engine to roll back to") };
    if let Err(e) = swap_to(db, &sport, &champ, &prev, "retired").await { return internal(e); }
    audit(db, &sport, "rollback", Some(&prev), json!({ "from": champ, "reason": q.reason })).await;
    HttpResponse::Ok().json(json!({ "sport": sport, "champion": prev, "retired": champ }))
}

#[derive(Deserialize)]
pub struct RollbackQuery { reason: Option<String> }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_matches_hand_computation() {
        let mut p = Pair::default();
        p.add(0.7, 0.5, 1.0);
        p.add(0.2, 0.5, 0.0);
        let r = p.report("b");
        assert!((r["brier_model"].as_f64().unwrap() - (0.09 + 0.04) / 2.0).abs() < 1e-9);
        assert!((r["brier_baseline"].as_f64().unwrap() - 0.25).abs() < 1e-9);
    }

    #[test]
    fn outcomes_lookup_and_voids() {
        let mut o = Outcomes::default();
        o.game.insert("total", 7.0);
        o.players.entry("42".into()).or_default().insert("sog", 3.0);
        let g = Forecast::game("total_over_5_5", "x", "total", 5.5, 0.6, 0.5);
        let p = Forecast::player("sog_over_avg", "x", 42, "A", "sog", 2.5, 0.5, 0.5);
        let missing = Forecast::player("sog_over_avg", "x", 7, "B", "sog", 2.5, 0.5, 0.5);
        assert_eq!(o.value(&g), Some(7.0));
        assert_eq!(o.value(&p), Some(3.0));
        assert_eq!(o.value(&missing), None);
        let mut s = Scorecard::default();
        assert_eq!(s.add_all(&[g, p, missing], &o), 2);
    }

    #[test]
    fn versions_are_stable_and_distinct() {
        assert!(version_for("nhl", &json!({})).ends_with("+default"));
        let a = version_for("nhl", &json!({ "a": 1.0, "b": 2.0 }));
        let b = version_for("nhl", &json!({ "b": 2.0, "a": 1.0 }));
        assert_eq!(a, b);
        assert_ne!(a, version_for("nhl", &json!({ "a": 1.5, "b": 2.0 })));
    }

    #[test]
    fn freq_is_add_one_smoothed() {
        assert!((freq_over(&[], 0.5) - 0.5).abs() < 1e-12);
        assert!((freq_over(&[1.0, 0.0, 2.0], 0.5) - 3.0 / 5.0).abs() < 1e-12);
    }
}
