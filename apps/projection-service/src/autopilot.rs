//! Learning autopilot: runs the learning loop inside the service, unattended.
//!
//! Every 10 minutes (the "tick"):
//!   1. ingest  — recent results and the upcoming schedule (NBA/NHL every 30
//!                minutes, MLB/NFL every 6 hours);
//!   2. score   — every frozen forecast whose game is final;
//!   3. guard   — shadow models are compared with the champion on the same live
//!                forecasts: promoted, rejected, or (after a promotion) used to
//!                roll the new champion back;
//!   4. freeze  — the champion and every shadow, for games starting within 36
//!                hours with projected lineups, and again inside 90 minutes of
//!                the start with confirmed lineups. NFL has no start times, so it
//!                is frozen in the days before (confirmed: the day before).
//!
//! Once a day per engine, in the early-morning window (08:00–14:00 UTC), the
//! tuner proposes one hyper-parameter change, tests it against the champion on
//! the same past games and seeds (search season), confirms it on the most
//! recent season it has not seen (holdout), and makes a passing candidate a
//! shadow. Nothing is promoted on backtests alone.
//!
//! Every action is in `learning_audit`; `learning_jobs` records when each job
//! last ran, so restarts do not repeat work. `POST /learning/autopilot?paused=true`
//! stops everything; `LEARNING_AUTOPILOT=off` keeps it from starting at all.

use std::collections::{BTreeMap, HashMap};

use actix_web::{web, HttpResponse, Responder};
use chrono::{DateTime, Duration, NaiveDate, Timelike, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_postgres::types::Json;

use crate::learning::{self, base, is_team, SlateGame, ENGINES, SPORTS};
use crate::State;

const TICK_SECS: u64 = 600;
const TUNER_SECS: u64 = 900;
/// Games starting within this many hours get a projected-lineup freeze.
const PROJECTED_HOURS: i64 = 36;
/// ... and a confirmed-lineup freeze inside this many minutes of the start.
const CONFIRMED_MINUTES: i64 = 90;
const TUNE_WINDOW_UTC: std::ops::Range<u32> = 8..14;
const TUNE_GAMES: usize = 300;
const TUNE_N: usize = 2000;
const TUNE_DRAWS: usize = 20;

fn dberr(e: &tokio_postgres::Error) -> String {
    match e.as_db_error() { Some(d) => format!("{}: {}", d.code().code(), d.message()), None => e.to_string() }
}
fn today_et() -> NaiveDate { (Utc::now() - Duration::hours(5)).date_naive() }

/// Live games a shadow needs (paired with the champion) before it can be promoted.
fn shadow_games(engine: &str) -> i64 {
    match base(engine) { "nfl" => 30, _ => 50 }
}

fn ingest_every_minutes(sport: &str) -> i64 {
    match sport { "nba" | "nhl" => 30, _ => 360 }
}

pub fn spawn(state: web::Data<State>) {
    if std::env::var("LEARNING_AUTOPILOT").map(|v| matches!(v.to_ascii_lowercase().as_str(), "off" | "0" | "false")).unwrap_or(false) {
        println!("projection-service: learning autopilot disabled (LEARNING_AUTOPILOT=off)");
        return;
    }
    println!("projection-service: learning autopilot on (tick {TICK_SECS}s, tuner window {TUNE_WINDOW_UTC:?} UTC)");
    let s2 = state.clone();
    actix_web::rt::spawn(async move {
        actix_web::rt::time::sleep(std::time::Duration::from_secs(60)).await;
        loop {
            if !paused(&state.db).await {
                let v = tick(&state).await;
                if v["frozen"].as_u64().unwrap_or(0) > 0 || v["errors"].as_array().map(|a| !a.is_empty()).unwrap_or(false) {
                    println!("autopilot tick: {}", v);
                }
            }
            actix_web::rt::time::sleep(std::time::Duration::from_secs(TICK_SECS)).await;
        }
    });
    actix_web::rt::spawn(async move {
        actix_web::rt::time::sleep(std::time::Duration::from_secs(300)).await;
        loop {
            // NBA player impact ratings: one fit per in-season month, backfilled
            // over the seasons the backtests cover (nba_impact.rs)
            if !paused(&s2.db).await && due(&s2.db, "fit:nba_impact", 360).await {
                let today = today_et();
                let v = crate::nba_impact::ensure(&s2.db, today - Duration::days(365 * 6), today).await;
                let fitted = v.as_ref().map(|x| x.len()).unwrap_or(0);
                if fitted > 0 { println!("autopilot nba impact: {fitted} monthly fits"); }
                mark(&s2.db, "fit:nba_impact", v.is_ok(), &json!({ "fitted": v.unwrap_or_else(|e| vec![json!({ "error": e })]) })).await;
            }
            // AI Picks evidence first: an engine whose code changed re-earns its grade caps
            if !paused(&s2.db).await {
                if let Some(engine) = crate::evidence::due(&s2).await {
                    let job = format!("evidence:{engine}");
                    let v = match crate::evidence::run(&s2, &engine).await { Ok(v) => v, Err(e) => json!({ "error": e }) };
                    let ok = v.get("error").is_none();
                    println!("autopilot evidence {engine}: {}", if ok { format!("{} rows in {} s", v["rows"], v["secs"]) } else { v["error"].to_string() });
                    mark(&s2.db, &job, ok, &v).await;
                    actix_web::rt::time::sleep(std::time::Duration::from_secs(60)).await;
                    continue;
                }
            }
            if !paused(&s2.db).await && TUNE_WINDOW_UTC.contains(&Utc::now().hour()) {
                if let Some(engine) = next_to_tune(&s2).await {
                    let v = tune(&s2, &engine).await;
                    println!("autopilot tune {engine}: {}", v["outcome"]);
                }
            }
            actix_web::rt::time::sleep(std::time::Duration::from_secs(TUNER_SECS)).await;
        }
    });
}

// ---------------------------------------------------------------------------
// Bookkeeping
// ---------------------------------------------------------------------------

async fn paused(db: &tokio_postgres::Client) -> bool {
    db.query_opt("SELECT (value->>'paused')::boolean FROM learning_settings WHERE key='autopilot'", &[]).await
        .ok().flatten().and_then(|r| r.get::<_, Option<bool>>(0)).unwrap_or(false)
}

async fn last_run(db: &tokio_postgres::Client, job: &str) -> Option<DateTime<Utc>> {
    db.query_opt("SELECT last_run FROM learning_jobs WHERE job=$1", &[&job]).await.ok().flatten().map(|r| r.get(0))
}

async fn due(db: &tokio_postgres::Client, job: &str, minutes: i64) -> bool {
    last_run(db, job).await.map(|t| Utc::now() - t >= Duration::minutes(minutes)).unwrap_or(true)
}

async fn mark(db: &tokio_postgres::Client, job: &str, ok: bool, detail: &Value) {
    if let Err(e) = db.execute("INSERT INTO learning_jobs (job, last_run, last_ok, detail) VALUES ($1, now(), $2, $3) \
                                ON CONFLICT (job) DO UPDATE SET last_run=now(), last_ok=EXCLUDED.last_ok, detail=EXCLUDED.detail",
        &[&job, &ok, &Json(detail)]).await { eprintln!("autopilot: mark {job}: {}", dberr(&e)); }
}

// ---------------------------------------------------------------------------
// The tick: ingest, score, guard, freeze
// ---------------------------------------------------------------------------

pub async fn tick(state: &State) -> Value {
    let db = &state.db;
    let today = today_et();
    let mut errors: Vec<Value> = Vec::new();
    let mut ingested = Vec::new();
    for sport in SPORTS {
        let job = format!("ingest:{sport}");
        if !due(db, &job, ingest_every_minutes(sport)).await { continue; }
        let v = learning::ingest_recent(state, sport, today, 2).await;
        let ok = v["ok"].as_bool().unwrap_or(false);
        if !ok { errors.push(json!({ "job": job, "detail": v })); }
        mark(db, &job, ok, &v).await;
        ingested.push(sport);
    }
    let mut scored = 0u64;
    for e in ENGINES {
        match learning::score_sport(state, e).await {
            Ok(v) => scored += v["forecasts_scored"].as_u64().unwrap_or(0),
            Err(err) => errors.push(json!({ "job": format!("score:{e}"), "error": err })),
        }
    }
    let mut guarded = Vec::new();
    for e in ENGINES {
        match guard(state, e).await {
            Ok(v) => if !v.is_empty() { guarded.extend(v) },
            Err(err) => errors.push(json!({ "job": format!("guard:{e}"), "error": err })),
        }
    }
    let (frozen, skipped_errors) = freeze_due(state, today).await;
    errors.extend(skipped_errors);
    // AI Picks: fetch lines and (re)price every 20 minutes per sport; settle every tick
    let mut picks_run = Vec::new();
    for sport in crate::picks::SPORTS {
        let job = format!("picks:{sport}");
        if due(db, &job, 20).await {
            let v = crate::picks::run(state, sport, false).await;
            let ok = v["ok"].as_bool().unwrap_or(false);
            if !ok { errors.push(json!({ "job": job, "detail": { "lines": v["lines"]["providers"]["espn"]["error"], "build": v["build"]["error"], "settle": v["settle"]["error"] } })); }
            picks_run.push(json!({ "sport": sport, "published": v["build"]["published"], "settled": v["settle"]["settled"] }));
            mark(db, &job, ok, &v).await;
        } else if let Err(e) = crate::picks::settle(state, sport).await {
            errors.push(json!({ "job": format!("picks-settle:{sport}"), "error": e }));
        }
    }
    let out = json!({ "at": Utc::now(), "ingested": ingested, "forecasts_scored": scored, "guard": guarded, "frozen": frozen, "picks": picks_run, "errors": errors });
    mark(db, "tick", errors.is_empty(), &out).await;
    out
}

/// Freeze the champion and every shadow of every engine for games inside the windows.
async fn freeze_due(state: &State, today: NaiveDate) -> (u64, Vec<Value>) {
    let db = &state.db;
    let now = Utc::now();
    let mut slates: HashMap<&str, Vec<SlateGame>> = HashMap::new();
    let mut errors = Vec::new();
    for sport in SPORTS {
        let mut games = Vec::new();
        for d in 0..=6 {
            let date = today + Duration::days(d);
            // only NFL looks a week ahead (no start times); others need today and tomorrow
            if sport != "nfl" && d > 1 { break; }
            match learning::slate(state, sport, date).await {
                Ok(g) => games.extend(g),
                Err(e) => { errors.push(json!({ "job": format!("slate:{sport}"), "date": date, "error": e })); break; }
            }
        }
        slates.insert(sport, games);
    }
    let mut frozen = 0u64;
    for engine in ENGINES {
        let versions = match versions_to_freeze(db, engine).await { Ok(v) => v, Err(e) => { errors.push(json!({ "job": format!("freeze:{engine}"), "error": e })); continue } };
        let Some(games) = slates.get(base(engine)) else { continue };
        let mut by_version: BTreeMap<String, u64> = BTreeMap::new();
        for g in games {
            let (projected, confirmed) = match g.start {
                Some(s) => (s - now <= Duration::hours(PROJECTED_HOURS), s - now <= Duration::minutes(CONFIRMED_MINUTES)),
                // NFL: a game day can only be frozen on an earlier day
                None => (g.date > today && g.date <= today + Duration::days(6), g.date == today + Duration::days(1)),
            };
            let mut modes = Vec::new();
            if projected { modes.push("projected"); }
            if confirmed && !is_team(engine) { modes.push("confirmed"); }
            for (version, cfg) in &versions {
                for lineup in &modes {
                    match learning::freeze_game(state, engine, g, lineup, version, cfg).await {
                        Ok(Ok(_)) => { frozen += 1; *by_version.entry(version.clone()).or_default() += 1; }
                        Ok(Err(_skip)) => {}
                        Err(e) => errors.push(json!({ "job": format!("freeze:{engine}"), "game": g.id, "error": e })),
                    }
                }
            }
        }
        for (v, n) in by_version {
            learning::audit(db, engine, "freeze", Some(&v), json!({ "games": n, "by": "autopilot" })).await;
        }
    }
    (frozen, errors)
}

/// Champion first, then every shadow (fresh candidates and rollback references).
async fn versions_to_freeze(db: &tokio_postgres::Client, engine: &str) -> Result<Vec<(String, Value)>, String> {
    let mut out = vec![learning::champion(db, engine).await?];
    for r in db.query("SELECT version, config FROM model_registry WHERE sport=$1 AND status='shadow' AND engine=$2 ORDER BY created_at",
        &[&engine, &learning::engine_of(engine)]).await.map_err(|e| dberr(&e))? {
        let c: Json<Value> = r.get(1);
        out.push((r.get(0), c.0));
    }
    Ok(out)
}

/// Paired live Brier differences (b − a) per market, over forecasts both
/// versions froze for the same game, lineup mode, subject and line, once scored.
pub async fn live_paired(db: &tokio_postgres::Client, engine: &str, a: &str, b: &str) -> Result<(i64, BTreeMap<String, Vec<f64>>), String> {
    let rows = db.query(
        "SELECT fa.market, fa.p_model, fb.p_model, fa.y::float8, la.game_id \
         FROM prediction_ledger la \
         JOIN prediction_ledger lb ON lb.sport = la.sport AND lb.game_id = la.game_id AND lb.lineup_mode = la.lineup_mode \
         JOIN prediction_forecasts fa ON fa.ledger_id = la.id \
         JOIN prediction_forecasts fb ON fb.ledger_id = lb.id AND fb.market = fa.market AND fb.subject = fa.subject AND fb.line = fa.line \
         WHERE la.sport = $1 AND la.model_version = $2 AND lb.model_version = $3 AND fa.y IS NOT NULL AND fb.y IS NOT NULL",
        &[&engine, &a, &b]).await.map_err(|e| dberr(&e))?;
    let mut games = std::collections::HashSet::new();
    let mut d: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for r in &rows {
        let (pa, pb, y): (f64, f64, f64) = (r.get(1), r.get(2), r.get(3));
        let c = |p: f64| (p.clamp(1e-4, 1.0 - 1e-4) - y).powi(2);
        d.entry(r.get(0)).or_default().push(c(pb) - c(pa));
        games.insert(r.get::<_, String>(4));
    }
    Ok((games.len() as i64, d))
}

/// Promote, reject or roll back based on paired live forecasts.
async fn guard(state: &State, engine: &str) -> Result<Vec<Value>, String> {
    let db = &state.db;
    let (champ, _) = learning::champion(db, engine).await?;
    let shadows = db.query("SELECT version, promoted_at IS NOT NULL FROM model_registry WHERE sport=$1 AND status='shadow' AND engine=$2",
        &[&engine, &learning::engine_of(engine)]).await.map_err(|e| dberr(&e))?;
    let need = shadow_games(engine);
    let mut out = Vec::new();
    for r in &shadows {
        let (v, watch): (String, bool) = (r.get(0), r.get(1));
        let (games, paired) = live_paired(db, engine, &champ, &v).await?;
        let (markets, better, worse) = learning::gate(&paired); // "better" = the shadow beats the champion
        let live = json!({ "champion": champ, "shadow": v, "games": games, "markets": markets, "shadow_better": better, "shadow_worse": worse });
        if !watch {
            if !worse.is_empty() && games >= need / 2 {
                db.execute("UPDATE model_registry SET status='rejected', retired_at=now(), evidence = coalesce(evidence,'{}'::jsonb) || jsonb_build_object('live', $3::jsonb) \
                            WHERE sport=$1 AND version=$2", &[&engine, &v, &Json(&live)]).await.map_err(|e| dberr(&e))?;
                learning::audit(db, engine, "shadow_reject", Some(&v), json!({ "reason": "worse than the champion on live forecasts", "live": live })).await;
                out.push(json!({ "engine": engine, "action": "shadow_reject", "version": v }));
            } else if games >= need && worse.is_empty() {
                // the champion stays running as a shadow: the rollback reference
                learning::swap_to(db, engine, &champ, &v, "shadow").await?;
                db.execute("UPDATE model_registry SET evidence = coalesce(evidence,'{}'::jsonb) || jsonb_build_object('live', $3::jsonb) WHERE sport=$1 AND version=$2",
                    &[&engine, &v, &Json(&live)]).await.map_err(|e| dberr(&e))?;
                learning::audit(db, engine, "auto_promote", Some(&v), json!({ "from": champ, "live": live,
                    "rule": format!("{need}+ paired live games and no market worse by 2 SE; the backtest search and holdout gates were passed before shadowing") })).await;
                out.push(json!({ "engine": engine, "action": "auto_promote", "version": v, "from": champ }));
                return Ok(out); // the champion changed; remaining shadows are compared next tick
            }
        } else if !better.is_empty() {
            // the previous champion beats the current one on live forecasts
            learning::swap_to(db, engine, &champ, &v, "rejected").await?;
            learning::audit(db, engine, "auto_rollback", Some(&v), json!({ "from": champ, "reason": "previous champion better on live forecasts by 2 SE", "live": live })).await;
            out.push(json!({ "engine": engine, "action": "auto_rollback", "to": v, "from": champ }));
            return Ok(out);
        } else if games >= 2 * need {
            db.execute("UPDATE model_registry SET status='retired', retired_at=now() WHERE sport=$1 AND version=$2", &[&engine, &v]).await.map_err(|e| dberr(&e))?;
            learning::audit(db, engine, "watch_end", Some(&v), json!({ "champion": champ, "games": games, "reason": "new champion held up; rollback reference retired" })).await;
            out.push(json!({ "engine": engine, "action": "watch_end", "version": v }));
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Tuner: propose, search, hold out, shadow
// ---------------------------------------------------------------------------

async fn next_to_tune(state: &State) -> Option<String> {
    for e in ENGINES {
        if !due(&state.db, &format!("tune:{e}"), 20 * 60).await { continue; }
        // one fresh shadow at a time per engine
        let busy = state.db.query_opt("SELECT 1 FROM model_registry WHERE sport=$1 AND status='shadow' AND promoted_at IS NULL", &[&e]).await.ok().flatten().is_some();
        if busy { mark(&state.db, &format!("tune:{e}"), true, &json!({ "outcome": "skipped: a shadow is already being tested live" })).await; continue; }
        return Some(e.to_string());
    }
    None
}

/// Season labels, most recent first.
fn recent_seasons(engine: &str, today: NaiveDate) -> Vec<i32> {
    let y = chrono::Datelike::year(&today);
    match base(engine) {
        "nhl" => (0..5).map(|k| (y - k) * 10000 + (y - k + 1)).collect(),
        "nba" => (0..5).map(|k| y + 1 - k).collect(),
        _ => (0..5).map(|k| y - k).collect(),
    }
}

/// One knob changed by ×1.5 or ÷1.5 (within its bounds) from the champion's
/// configuration; deterministic per engine and day; skips versions already tried.
async fn propose(state: &State, engine: &str, champ_cfg: &Value, day: NaiveDate) -> Result<Option<(Value, String)>, String> {
    let spec = learning::tunables(engine);
    let Some(spec) = spec.as_object() else { return Ok(None) };
    let keys: Vec<&String> = spec.keys().collect();
    if keys.is_empty() { return Ok(None); }
    let h = learning::fnv(&["propose", engine, &day.to_string()]);
    for attempt in 0..(4 * keys.len() as u64) {
        let key = keys[((h >> 3).wrapping_add(attempt) % keys.len() as u64) as usize];
        let s = &spec[key.as_str()];
        let (d, lo, hi) = (s["default"].as_f64().unwrap_or(1.0), s["min"].as_f64().unwrap_or(f64::MIN), s["max"].as_f64().unwrap_or(f64::MAX));
        let cur = champ_cfg.get(key.as_str()).and_then(|v| v.as_f64()).unwrap_or(d);
        let up = (h.wrapping_add(attempt / keys.len() as u64)) % 2 == 0;
        let raw = if up { cur * 1.5 } else { cur / 1.5 };
        let new = { let x = raw.clamp(lo, hi); if x == 0.0 { 0.0 } else { let m = 10f64.powf(x.abs().log10().floor() - 2.0); (x / m).round() * m } };
        if (new - cur).abs() < 1e-9 * cur.abs().max(1.0) { continue; }
        let mut cfg = champ_cfg.as_object().cloned().unwrap_or_default();
        if (new - d).abs() < 1e-12 { cfg.remove(key.as_str()); } else { cfg.insert(key.clone(), json!(new)); }
        let cfg = Value::Object(cfg);
        let v = learning::version_for(engine, &cfg);
        let tried = state.db.query_opt("SELECT 1 FROM model_registry WHERE sport=$1 AND version=$2", &[&engine, &v]).await.map_err(|e| dberr(&e))?.is_some();
        if !tried { return Ok(Some((cfg, format!("{key}: {cur} -> {new}")))); }
    }
    Ok(None)
}

pub async fn tune(state: &State, engine: &str) -> Value {
    let job = format!("tune:{engine}");
    let out = tune_inner(state, engine).await.unwrap_or_else(|e| json!({ "outcome": "error", "error": e }));
    mark(&state.db, &job, out["outcome"] != "error", &out).await;
    out
}

async fn tune_inner(state: &State, engine: &str) -> Result<Value, String> {
    let db = &state.db;
    let today = today_et();
    let (champ, champ_cfg) = learning::champion(db, engine).await?;
    let mut seasons = Vec::new();
    for s in recent_seasons(engine, today) {
        let g = learning::season_games(state, engine, s).await?;
        if g.len() >= 200 { seasons.push((s, g)); }
        if seasons.len() == 2 { break; }
    }
    if seasons.len() < 2 { return Ok(json!({ "outcome": "skipped: need two seasons with 200+ testable games" })); }
    let (hold_season, hold_all) = (seasons[0].0, &seasons[0].1);
    let search_season = seasons[1].0;
    let Some((cfg, change)) = propose(state, engine, &champ_cfg, today).await? else {
        return Ok(json!({ "outcome": "skipped: no untried change left" }));
    };
    let v = learning::version_for(engine, &cfg);
    db.execute("INSERT INTO model_registry (sport, version, engine, config, status, parent, notes) VALUES ($1,$2,$3,$4,'candidate',$5,$6) ON CONFLICT (sport, version) DO NOTHING",
        &[&engine, &v, &learning::engine_of(engine), &Json(&cfg), &champ, &format!("autopilot: {change}")]).await.map_err(|e| dberr(&e))?;
    learning::audit(db, engine, "register", Some(&v), json!({ "status": "candidate", "config": cfg, "parent": champ, "by": "autopilot", "change": change })).await;

    // 1. search: a season the holdout does not touch
    let search_games = learning::eval_games(state, engine, search_season, TUNE_GAMES).await?;
    let mut search = learning::paired_eval(state, engine, &champ_cfg, &cfg, &search_games, TUNE_N, TUNE_DRAWS, "projected").await;
    search["season"] = json!(search_season);
    let s_pass = search["gate"]["pass"].as_bool().unwrap_or(false);
    let mut evidence = json!({ "champion": champ, "change": change, "search": search });
    if !s_pass {
        evidence["gate"] = json!({ "pass": false, "stage": "search", "rule": search["gate"]["rule"] });
        reject(db, engine, &v, &evidence).await?;
        return Ok(json!({ "outcome": "rejected at search", "version": v, "change": change }));
    }
    // 2. holdout: the most recent games of the latest season; a market that
    // improved in the search must improve again, and none may degrade
    let hold_games: Vec<String> = hold_all.iter().rev().take(TUNE_GAMES).rev().cloned().collect();
    let mut hold = learning::paired_eval(state, engine, &champ_cfg, &cfg, &hold_games, TUNE_N, TUNE_DRAWS, "projected").await;
    hold["season"] = json!(hold_season);
    let s_imp: Vec<String> = search["gate"]["improves"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
    let h_imp: Vec<String> = hold["gate"]["improves"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
    let h_deg = hold["gate"]["degrades"].as_array().map(|a| a.len()).unwrap_or(0);
    let confirmed: Vec<&String> = s_imp.iter().filter(|m| h_imp.contains(m)).collect();
    let pass = !confirmed.is_empty() && h_deg == 0;
    evidence["holdout"] = hold;
    evidence["gate"] = json!({ "pass": pass, "stage": "holdout", "improves": confirmed, "degrades": evidence["holdout"]["gate"]["degrades"],
        "rule": "search: ≥1 market better by 2 SE, none worse by 2 SE; holdout: a search-improved market better again by 2 SE, none worse; then live shadow" });
    if !pass {
        reject(db, engine, &v, &evidence).await?;
        return Ok(json!({ "outcome": "rejected at holdout", "version": v, "change": change }));
    }
    db.execute("UPDATE model_registry SET status='shadow', evidence=$3 WHERE sport=$1 AND version=$2", &[&engine, &v, &Json(&evidence)]).await.map_err(|e| dberr(&e))?;
    learning::audit(db, engine, "auto_shadow", Some(&v), json!({ "champion": champ, "change": change, "improves": confirmed,
        "next": format!("frozen next to the champion until {} paired live games", shadow_games(engine)) })).await;
    Ok(json!({ "outcome": "shadow", "version": v, "change": change, "improves": confirmed }))
}

async fn reject(db: &tokio_postgres::Client, engine: &str, v: &str, evidence: &Value) -> Result<(), String> {
    db.execute("UPDATE model_registry SET status='rejected', retired_at=now(), evidence=$3 WHERE sport=$1 AND version=$2", &[&engine, &v, &Json(evidence)]).await.map_err(|e| dberr(&e))?;
    learning::audit(db, engine, "auto_reject", Some(v), json!({ "change": evidence["change"], "gate": evidence["gate"] })).await;
    Ok(())
}

// ---------------------------------------------------------------------------
// HTTP: status, pause, run now
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct PauseQuery { paused: Option<bool> }

/// GET /learning/autopilot — schedule, last runs, shadows' live progress.
/// POST /learning/autopilot?paused=true|false — pause or resume everything.
pub async fn status(state: web::Data<State>, q: web::Query<PauseQuery>, req: actix_web::HttpRequest) -> impl Responder {
    let db = &state.db;
    if req.method() == actix_web::http::Method::POST {
        let Some(p) = q.paused else { return HttpResponse::BadRequest().json(json!({ "error": "paused=true|false" })) };
        if let Err(e) = db.execute("INSERT INTO learning_settings (key, value) VALUES ('autopilot', $1) ON CONFLICT (key) DO UPDATE SET value=EXCLUDED.value, updated_at=now()",
            &[&Json(json!({ "paused": p }))]).await { return HttpResponse::InternalServerError().json(json!({ "error": dberr(&e) })) }
        learning::audit(db, "all", if p { "autopilot_pause" } else { "autopilot_resume" }, None, json!({})).await;
    }
    let jobs: Vec<Value> = match db.query("SELECT job, last_run::text, last_ok, detail FROM learning_jobs ORDER BY job", &[]).await {
        Ok(r) => r.iter().map(|r| { let d: Json<Value> = r.get(3); json!({ "job": r.get::<_, String>(0), "last_run": r.get::<_, String>(1), "ok": r.get::<_, bool>(2), "detail": d.0 }) }).collect(),
        Err(e) => return HttpResponse::InternalServerError().json(json!({ "error": dberr(&e) })),
    };
    let mut shadows = Vec::new();
    for e in ENGINES {
        let Ok((champ, _)) = learning::champion(db, e).await else { continue };
        let Ok(rows) = db.query("SELECT version, promoted_at IS NOT NULL, notes FROM model_registry WHERE sport=$1 AND status='shadow'", &[&e]).await else { continue };
        for r in rows {
            let v: String = r.get(0);
            let Ok((games, paired)) = live_paired(db, e, &champ, &v).await else { continue };
            let (markets, better, worse) = learning::gate(&paired);
            shadows.push(json!({ "engine": e, "version": v, "role": if r.get::<_, bool>(1) { "rollback reference (previous champion)" } else { "candidate on trial" },
                "notes": r.get::<_, Option<String>>(2), "champion": champ, "live_games": games, "needed": shadow_games(e), "markets": markets, "shadow_better": better, "shadow_worse": worse }));
        }
    }
    let enabled = !std::env::var("LEARNING_AUTOPILOT").map(|v| matches!(v.to_ascii_lowercase().as_str(), "off" | "0" | "false")).unwrap_or(false);
    HttpResponse::Ok().json(json!({
        "enabled": enabled, "paused": paused(db).await, "now": Utc::now(), "jobs": jobs, "shadows": shadows,
        "schedule": {
            "tick_minutes": TICK_SECS / 60,
            "ingest": "NBA/NHL every 30 min, MLB/NFL every 6 h (last 3 days + next 2)",
            "freeze": format!("champion + shadows: projected lineup within {PROJECTED_HOURS} h of the start, confirmed lineup within {CONFIRMED_MINUTES} min (NFL: the days before; confirmed the day before)"),
            "tune": format!("one proposal per engine per day, {:02}:00–{:02}:00 UTC; search season + holdout of the latest {TUNE_GAMES} games, {TUNE_N} sims x {TUNE_DRAWS} draws", TUNE_WINDOW_UTC.start, TUNE_WINDOW_UTC.end),
            "promote": "a shadow is promoted after enough paired live games with no market worse by 2 SE; the old champion stays as a shadow and is restored if the new one falls 2 SE behind",
            "picks": "AI Picks every 20 min per sport: ESPN lines each run, The Odds API at most every 6 h and SportsGameOdds every 4 h within daily budgets; reprice from the champions' frozen forecasts; settle finished games every tick",
            "nba_impact": "NBA player impact ratings (ridge adjusted plus-minus) fitted as of the first of each in-season month; checked every 6 h",
            "evidence": "when a champion's engine code has no backtest evidence, re-run five seasons of walk-forward backtests (one engine at a time) so AI Picks grade caps follow the code",
        },
    }))
}

#[derive(Deserialize)]
pub struct RunQuery { job: String, engine: Option<String> }

/// POST /learning/autopilot/run?job=tick | job=tune&engine=nhl — run a job now.
pub async fn run_now(state: web::Data<State>, q: web::Query<RunQuery>) -> impl Responder {
    match q.job.as_str() {
        "tick" => HttpResponse::Ok().json(tick(&state).await),
        "tune" => {
            let Some(e) = q.engine.as_deref().filter(|e| ENGINES.contains(e)) else { return HttpResponse::BadRequest().json(json!({ "error": format!("engine must be one of {ENGINES:?}") })) };
            HttpResponse::Ok().json(tune(&state, e).await)
        }
        _ => HttpResponse::BadRequest().json(json!({ "error": "job must be tick or tune" })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn season_labels() {
        let d = NaiveDate::from_ymd_opt(2026, 10, 20).unwrap();
        assert_eq!(recent_seasons("nhl", d)[0], 20262027);
        assert_eq!(recent_seasons("nhl_team", d)[1], 20252026);
        assert_eq!(recent_seasons("nba", d)[0], 2027);
        assert_eq!(recent_seasons("mlb", d)[0], 2026);
    }
}
