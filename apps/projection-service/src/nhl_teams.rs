//! NHL team model (Mack-style log-linear Poisson on goals), mirroring the NFL
//! team model: mu = exp(intercept + off[team] - def[opp] + home_adv*is_home),
//! fit by IRLS coordinate-Newton on the recency-weighted Poisson log-likelihood
//! with a ridge prior. Goals drawn Negative-Binomial (dispersion from residuals,
//! ~Poisson when goals aren't overdispersed). A matchup is simulated 100k times
//! for win / total / puckline; regulation ties are broken by an OT winner drawn
//! in proportion to each side's expected goals (OT/SO modeled as a single golden
//! goal — the full ES/ST/OT/SO decomposition in Mack's Stan reference is the
//! documented next step). Data: MoneyPuck team gameByGame (goalsFor, ~2008-).

use std::collections::HashMap;

use actix_web::{web, HttpResponse, Responder};
use futures::stream::{self, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::State;

const NHL_API: &str = "https://api-web.nhle.com/v1";
const MP_TEAM: &str = "https://moneypuck.com/moneypuck/playerData/careers/gameByGame/regular/teams";
pub const TEAM_MODEL: &str = "nhl-team-goals-v1";

fn internal(m: impl std::fmt::Display) -> HttpResponse {
    eprintln!("projection-service(nhl_teams): {m}");
    HttpResponse::InternalServerError().json(json!({ "error": m.to_string() }))
}
fn bad_request(m: impl Into<String>) -> HttpResponse {
    HttpResponse::BadRequest().json(json!({ "error": m.into() }))
}

struct Rng(u64);
impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn uniform(&mut self) -> f64 { ((self.next_u64() >> 11) as f64 + 1.0) / (1u64 << 53) as f64 }
    fn normal(&mut self) -> f64 {
        let (u1, u2) = (self.uniform(), self.uniform());
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }
    fn gamma(&mut self, shape: f64, scale: f64) -> f64 {
        if shape < 1.0 {
            let g = self.gamma(shape + 1.0, 1.0);
            return scale * g * self.uniform().powf(1.0 / shape);
        }
        let d = shape - 1.0 / 3.0;
        let c = 1.0 / (9.0 * d).sqrt();
        loop {
            let x = self.normal();
            let v = (1.0 + c * x).powi(3);
            if v <= 0.0 { continue; }
            if self.uniform().ln() < 0.5 * x * x + d - d * v + d * v.ln() { return scale * d * v; }
        }
    }
    fn poisson(&mut self, lambda: f64) -> f64 {
        if lambda <= 0.0 { return 0.0; }
        if lambda > 30.0 { return (lambda + lambda.sqrt() * self.normal()).round().max(0.0); }
        let l = (-lambda).exp();
        let (mut k, mut p) = (0.0f64, 1.0f64);
        loop { k += 1.0; p *= self.uniform(); if p <= l { return k - 1.0; } }
    }
    fn negbin(&mut self, mean: f64, r: f64) -> f64 {
        if mean <= 0.0 { return 0.0; }
        let lambda = self.gamma(r, mean / r);
        self.poisson(lambda)
    }
}

struct Obs { y: f64, team: usize, opp: usize, home: f64, w: f64 }

#[derive(Deserialize)]
pub struct IngestQuery {
    #[serde(default = "d_halflife")] halflife_seasons: f64,
    #[serde(default = "d_epochs")] epochs: usize,
    #[serde(default = "d_ridge")] ridge: f64,
    #[serde(default = "d_concurrency")] concurrency: usize,
}
fn d_halflife() -> f64 { 2.0 }
fn d_epochs() -> usize { 80 }
fn d_ridge() -> f64 { 0.5 }
fn d_concurrency() -> usize { 12 }

pub async fn ingest_teams(state: web::Data<State>, q: web::Query<IngestQuery>) -> impl Responder {
    let q = q.into_inner();
    let http = &state.http;
    let standings: Value = match http.get(format!("{NHL_API}/standings/now")).send().await.and_then(|r| r.error_for_status()) {
        Ok(r) => r.json().await.unwrap_or(Value::Null),
        Err(e) => return internal(format!("standings: {e}")),
    };
    let teams: Vec<String> = standings["standings"].as_array().map(|a| {
        a.iter().filter_map(|t| t["teamAbbrev"]["default"].as_str().map(|s| s.to_string())).collect()
    }).unwrap_or_default();
    if teams.is_empty() { return internal("no teams"); }

    // fetch per-team game logs concurrently -> (team, opp, home, season, goals)
    let rows: Vec<(String, String, bool, i32, f64, String)> = stream::iter(teams.clone())
        .map(|abbr| {
            let http = http.clone();
            async move {
                let Ok(resp) = http.get(format!("{MP_TEAM}/{abbr}.csv")).send().await else { return Vec::new() };
                let Ok(resp) = resp.error_for_status() else { return Vec::new() };
                let Ok(text) = resp.text().await else { return Vec::new() };
                parse_team(&text)
            }
        })
        .buffer_unordered(q.concurrency).collect::<Vec<_>>().await.into_iter().flatten().collect();
    if rows.len() < 100 { return internal("not enough team-game rows"); }

    // Persist historical games into sports_games (home perspective + away score)
    // so the walk-forward backtest has NHL history to train/test on.
    if let Err(e) = store_games(&state.db, &rows).await { return internal(e); }

    // team index
    let mut idx: HashMap<String, usize> = HashMap::new();
    for (t, o, _, _, _, _) in &rows {
        let n = idx.len(); idx.entry(t.clone()).or_insert(n);
        let n = idx.len(); idx.entry(o.clone()).or_insert(n);
    }
    let tn = idx.len();
    let names: Vec<String> = { let mut v = vec![String::new(); tn]; for (k, &i) in &idx { v[i] = k.clone(); } v };
    let max_season = rows.iter().map(|r| r.3).max().unwrap_or(0) as f64;

    let obs: Vec<Obs> = rows.iter().map(|(t, o, home, season, goals, _)| Obs {
        y: *goals, team: idx[t], opp: idx[o], home: if *home { 1.0 } else { 0.0 },
        w: 0.5f64.powf((max_season - *season as f64) / q.halflife_seasons),
    }).collect();
    let m = obs.len();

    let mut as_off: Vec<Vec<usize>> = vec![Vec::new(); tn];
    let mut as_def: Vec<Vec<usize>> = vec![Vec::new(); tn];
    for (i, o) in obs.iter().enumerate() { as_off[o.team].push(i); as_def[o.opp].push(i); }

    let wtot: f64 = obs.iter().map(|o| o.w).sum();
    let wmean: f64 = obs.iter().map(|o| o.y * o.w).sum::<f64>() / wtot;
    let mut off = vec![0.0f64; tn];
    let mut def = vec![0.0f64; tn];
    let mut home_adv = 0.05f64;
    let mut intercept = wmean.max(0.5).ln();
    let eta = |off: &[f64], def: &[f64], ha: f64, ic: f64, o: &Obs| (ic + off[o.team] - def[o.opp] + ha * o.home).clamp(-2.0, 3.0);

    for _ in 0..q.epochs {
        let (mut g, mut h) = (0.0, 0.0);
        for o in &obs { let mu = eta(&off, &def, home_adv, intercept, o).exp(); g += o.w * (o.y - mu); h += o.w * mu; }
        if h > 0.0 { intercept += g / h; }
        let (mut g, mut h) = (0.0, 0.0);
        for o in &obs { if o.home > 0.0 { let mu = eta(&off, &def, home_adv, intercept, o).exp(); g += o.w * (o.y - mu); h += o.w * mu; } }
        if h > 0.0 { home_adv += g / h; }
        for ti in 0..tn {
            let (mut g, mut h) = (-q.ridge * off[ti], q.ridge);
            for &i in &as_off[ti] { let o = &obs[i]; let mu = eta(&off, &def, home_adv, intercept, o).exp(); g += o.w * (o.y - mu); h += o.w * mu; }
            if h > 0.0 { off[ti] += g / h; }
        }
        for ti in 0..tn {
            let (mut g, mut h) = (-q.ridge * def[ti], q.ridge);
            for &i in &as_def[ti] { let o = &obs[i]; let mu = eta(&off, &def, home_adv, intercept, o).exp(); g += o.w * (mu - o.y); h += o.w * mu; }
            if h > 0.0 { def[ti] += g / h; }
        }
        let moff: f64 = off.iter().sum::<f64>() / tn as f64;
        let mdef: f64 = def.iter().sum::<f64>() / tn as f64;
        for v in &mut off { *v -= moff; }
        for v in &mut def { *v -= mdef; }
        intercept += moff - mdef;
    }
    let (mut a, mut b, mut c) = (0.0, 0.0, 0.0);
    for o in &obs { let mu = eta(&off, &def, home_adv, intercept, o).exp(); a += o.w * (o.y - mu).powi(2); b += o.w * mu; c += o.w * mu * mu; }
    let dispersion = if a > b { (c / (a - b)).clamp(1.0, 1e6) } else { 1e6 };

    // persist
    if let Err(e) = store(&state.db, &names, &off, &def, home_adv, intercept, dispersion).await { return internal(e); }

    let mut net: Vec<(String, f64)> = names.iter().enumerate().map(|(i, n)| (n.clone(), off[i] + def[i])).collect();
    net.sort_by(|a, b| b.1.total_cmp(&a.1));
    HttpResponse::Ok().json(json!({
        "model_id": TEAM_MODEL, "teams": tn, "fit_on_rows": m,
        "home_advantage": home_adv, "intercept": intercept, "mean_goals": (intercept.exp()*1000.0).round()/1000.0,
        "dispersion": dispersion, "halflife_seasons": q.halflife_seasons,
        "top_net": net.iter().take(6).map(|(t, r)| json!({"team": t, "net": (r*1000.0).round()/1000.0})).collect::<Vec<_>>(),
    }))
}

/// Normalize MoneyPuck's dotted team codes to the standard NHL abbreviations
/// so a team's games aren't split across two identities.
pub(crate) fn norm(t: &str) -> String {
    match t {
        "T.B" => "TBL", "N.J" => "NJD", "S.J" => "SJS", "L.A" => "LAK",
        other => other,
    }.to_string()
}

fn parse_team(text: &str) -> Vec<(String, String, bool, i32, f64, String)> {
    let mut rdr = csv::ReaderBuilder::new().has_headers(true).from_reader(text.as_bytes());
    let Ok(headers) = rdr.headers() else { return Vec::new() };
    let headers = headers.clone();
    let idx = |n: &str| headers.iter().position(|h| h == n);
    let (Some(i_sit), Some(i_team), Some(i_opp), Some(i_home), Some(i_season), Some(i_gf)) =
        (idx("situation"), idx("playerTeam"), idx("opposingTeam"), idx("home_or_away"), idx("season"), idx("goalsFor"))
    else { return Vec::new() };
    let i_date = idx("gameDate");
    let mut out = Vec::new();
    for rec in rdr.records().flatten() {
        if rec.get(i_sit) != Some("all") { continue; }
        let goals = rec.get(i_gf).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
        let season: i32 = rec.get(i_season).and_then(|s| s.parse().ok()).unwrap_or(0);
        let home = rec.get(i_home).map(|s| s.eq_ignore_ascii_case("HOME")).unwrap_or(false);
        let date = i_date.and_then(|i| rec.get(i)).unwrap_or("").to_string();
        out.push((norm(rec.get(i_team).unwrap_or("")), norm(rec.get(i_opp).unwrap_or("")), home, season, goals, date));
    }
    out
}

/// Pair the per-team MoneyPuck rows into one game row (home + away score) and
/// persist completed games into sports_games for backtesting.
async fn store_games(db: &tokio_postgres::Client, rows: &[(String, String, bool, i32, f64, String)]) -> Result<(), String> {
    let mut games: HashMap<(String, String, String), (i32, Option<f64>, Option<f64>)> = HashMap::new();
    for (team, opp, home, season, goals, date) in rows {
        if date.is_empty() { continue; }
        if *home {
            let e = games.entry((date.clone(), team.clone(), opp.clone())).or_insert((*season, None, None));
            e.0 = *season; e.1 = Some(*goals);
        } else {
            let e = games.entry((date.clone(), opp.clone(), team.clone())).or_insert((*season, None, None));
            e.0 = *season; e.2 = Some(*goals);
        }
    }
    let (mut gid, mut season, mut week, mut gtype, mut gdate, mut home_v, mut away_v, mut hs, mut as_, mut status) =
        (vec![], vec![], vec![], vec![], vec![], vec![], vec![], vec![], vec![], vec![]);
    for ((date, h, a), (s, hsc, asc)) in &games {
        let (Some(hsc), Some(asc)) = (hsc, asc) else { continue };
        gid.push(format!("mp_{date}_{a}_{h}"));
        season.push(*s);
        week.push(date.clone());
        gtype.push("REG".to_string());
        gdate.push(chrono::NaiveDate::parse_from_str(date, "%Y%m%d").or_else(|_| chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")).ok());
        home_v.push(h.clone()); away_v.push(a.clone());
        hs.push(*hsc as i32); as_.push(*asc as i32);
        status.push("final".to_string());
    }
    if gid.is_empty() { return Ok(()); }
    db.execute(
        "INSERT INTO sports_games (sport, game_id, season, week, game_type, game_date, home_team, away_team, home_score, away_score, status) \
         SELECT 'nhl', * FROM UNNEST($1::text[],$2::int[],$3::text[],$4::text[],$5::date[],$6::text[],$7::text[],$8::int[],$9::int[],$10::text[]) \
         ON CONFLICT (sport, game_id) DO UPDATE SET home_score=EXCLUDED.home_score, away_score=EXCLUDED.away_score, status=EXCLUDED.status, game_date=EXCLUDED.game_date, season=EXCLUDED.season",
        &[&gid, &season, &week, &gtype, &gdate, &home_v, &away_v, &hs, &as_, &status],
    ).await.map(|_| ()).map_err(|e| format!("store nhl games: {e}"))
}

async fn store(db: &tokio_postgres::Client, names: &[String], off: &[f64], def: &[f64], ha: f64, ic: f64, disp: f64) -> Result<(), String> {
    db.execute(
        "INSERT INTO models (id, name, sport, version, type, status) VALUES ($1, 'NHL team goals (log-linear Poisson)', 'nhl', '1', 'poisson', 'active') ON CONFLICT (id) DO NOTHING",
        &[&TEAM_MODEL],
    ).await.map_err(|e| format!("register: {e}"))?;
    db.execute("DELETE FROM team_components WHERE model_id=$1", &[&TEAM_MODEL]).await.map_err(|e| format!("clear: {e}"))?;
    for (i, name) in names.iter().enumerate() {
        db.execute("INSERT INTO team_components (sport, team, component, rating, model_id, as_of) VALUES ('nhl',$1,'goals_off',$2,$3,now())",
            &[name, &off[i], &TEAM_MODEL]).await.map_err(|e| format!("off {name}: {e}"))?;
        db.execute("INSERT INTO team_components (sport, team, component, rating, model_id, as_of) VALUES ('nhl',$1,'goals_def',$2,$3,now())",
            &[name, &def[i], &TEAM_MODEL]).await.map_err(|e| format!("def {name}: {e}"))?;
    }
    for (k, v) in [("home_adv", ha), ("intercept", ic), ("dispersion", disp)] {
        db.execute("INSERT INTO team_model_params (model_id, key, value) VALUES ($1,$2,$3) ON CONFLICT (model_id, key) DO UPDATE SET value=EXCLUDED.value, as_of=now()",
            &[&TEAM_MODEL, &k, &v]).await.map_err(|e| format!("param {k}: {e}"))?;
    }
    Ok(())
}

/// Ingest the NHL schedule (per-team club schedule) into sports_games so the
/// Player tab can filter by upcoming game.
pub async fn ingest_schedule(state: web::Data<State>) -> impl Responder {
    let http = &state.http;
    let standings: Value = match http.get(format!("{NHL_API}/standings/now")).send().await.and_then(|r| r.error_for_status()) {
        Ok(r) => r.json().await.unwrap_or(Value::Null),
        Err(e) => return internal(format!("standings: {e}")),
    };
    let teams: Vec<String> = standings["standings"].as_array().map(|a| {
        a.iter().filter_map(|t| t["teamAbbrev"]["default"].as_str().map(|s| s.to_string())).collect()
    }).unwrap_or_default();

    let per_team: Vec<Vec<Value>> = stream::iter(teams)
        .map(|abbr| {
            let http = http.clone();
            async move {
                let Ok(resp) = http.get(format!("{NHL_API}/club-schedule-season/{abbr}/now")).send().await else { return Vec::new() };
                let Ok(v) = resp.json::<Value>().await else { return Vec::new() };
                v["games"].as_array().cloned().unwrap_or_default()
            }
        })
        .buffer_unordered(12).collect::<Vec<_>>().await;

    // dedupe by game id
    let mut seen: HashMap<String, Value> = HashMap::new();
    for g in per_team.into_iter().flatten() {
        if let Some(id) = g["id"].as_i64() {
            seen.entry(id.to_string()).or_insert(g);
        }
    }
    if seen.is_empty() { return internal("no schedule games"); }

    let (mut gid, mut season, mut week, mut gtype, mut date, mut home, mut away, mut hs, mut as_, mut status) =
        (vec![], vec![], vec![], vec![], vec![], vec![], vec![], vec![], vec![], vec![]);
    for (id, g) in &seen {
        let state_str = g["gameState"].as_str().unwrap_or("");
        let is_final = state_str == "FINAL" || state_str == "OFF";
        let gd = g["gameDate"].as_str().unwrap_or("");
        gid.push(id.clone());
        // season like 20262027 -> start year 2026
        season.push(g["season"].as_i64().map(|s| (s / 10000) as i32).unwrap_or(0));
        week.push(gd.to_string()); // NHL has no weeks; use the date as the per-game key suffix
        gtype.push(match g["gameType"].as_i64() { Some(3) => "POST".to_string(), _ => "REG".to_string() });
        date.push(chrono::NaiveDate::parse_from_str(gd, "%Y-%m-%d").ok());
        home.push(g["homeTeam"]["abbrev"].as_str().unwrap_or("").to_string());
        away.push(g["awayTeam"]["abbrev"].as_str().unwrap_or("").to_string());
        hs.push(if is_final { g["homeTeam"]["score"].as_i64().map(|x| x as i32) } else { None });
        as_.push(if is_final { g["awayTeam"]["score"].as_i64().map(|x| x as i32) } else { None });
        status.push(if is_final { "final".to_string() } else { "scheduled".to_string() });
    }
    let r = state.db.execute(
        "INSERT INTO sports_games (sport, game_id, season, week, game_type, game_date, home_team, away_team, home_score, away_score, status) \
         SELECT 'nhl', * FROM UNNEST($1::text[],$2::int[],$3::text[],$4::text[],$5::date[],$6::text[],$7::text[],$8::int[],$9::int[],$10::text[]) \
         ON CONFLICT (sport, game_id) DO UPDATE SET season=EXCLUDED.season, week=EXCLUDED.week, game_type=EXCLUDED.game_type, \
           game_date=EXCLUDED.game_date, home_team=EXCLUDED.home_team, away_team=EXCLUDED.away_team, home_score=EXCLUDED.home_score, \
           away_score=EXCLUDED.away_score, status=EXCLUDED.status, updated_at=now()",
        &[&gid, &season, &week, &gtype, &date, &home, &away, &hs, &as_, &status],
    ).await;
    match r {
        Ok(_) => HttpResponse::Ok().json(json!({ "sport": "nhl", "games_ingested": seen.len() })),
        Err(e) => internal(format!("store schedule: {e}")),
    }
}

#[derive(Deserialize)]
pub struct GamesQuery { season: Option<i32>, status: Option<String>, limit: Option<i64> }
pub async fn list_games(state: web::Data<State>, q: web::Query<GamesQuery>) -> impl Responder {
    let limit = q.limit.unwrap_or(400).clamp(1, 2000);
    let sql = "SELECT game_id, season, week, game_type, game_date, home_team, away_team, home_score, away_score, status \
               FROM sports_games WHERE sport='nhl' AND ($1::int IS NULL OR season=$1) AND ($2::text IS NULL OR status=$2) \
               ORDER BY game_date ASC NULLS LAST, game_id LIMIT $3";
    match state.db.query(sql, &[&q.season, &q.status, &limit]).await {
        Ok(rows) => {
            let out: Vec<Value> = rows.iter().map(|r| json!({
                "game_id": r.get::<_, String>("game_id"), "season": r.get::<_, Option<i32>>("season"),
                "week": r.get::<_, Option<String>>("week"), "game_type": r.get::<_, Option<String>>("game_type"),
                "date": r.get::<_, Option<chrono::NaiveDate>>("game_date").map(|d| d.to_string()),
                "home": r.get::<_, Option<String>>("home_team"), "away": r.get::<_, Option<String>>("away_team"),
                "home_score": r.get::<_, Option<i32>>("home_score"), "away_score": r.get::<_, Option<i32>>("away_score"),
                "status": r.get::<_, Option<String>>("status"),
            })).collect();
            HttpResponse::Ok().json(json!({ "count": out.len(), "games": out }))
        }
        Err(e) => internal(e),
    }
}

pub async fn list_ratings(state: web::Data<State>) -> impl Responder {
    let sql = "SELECT o.team, o.rating AS off, d.rating AS def FROM team_components o \
               JOIN team_components d ON d.sport=o.sport AND d.team=o.team AND d.component='goals_def' AND d.model_id=o.model_id \
               WHERE o.sport='nhl' AND o.component='goals_off' AND o.model_id=$1 ORDER BY (o.rating+d.rating) DESC";
    match state.db.query(sql, &[&TEAM_MODEL]).await {
        Ok(rows) => {
            let out: Vec<Value> = rows.iter().map(|r| {
                let (off, def): (f64, f64) = (r.get("off"), r.get("def"));
                json!({ "team": r.get::<_, String>("team"), "off": off, "def": def, "net": off + def })
            }).collect();
            HttpResponse::Ok().json(json!({ "model_id": TEAM_MODEL, "count": out.len(), "teams": out }))
        }
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
pub struct MatchupQuery {
    home: String,
    away: String,
    n: Option<usize>,
    seed: Option<u64>,
    puckline: Option<f64>, // home puckline, e.g. -1.5 (home must win by 2+)
    total: Option<f64>,
}

pub async fn matchup(state: web::Data<State>, q: web::Query<MatchupQuery>) -> impl Responder {
    let n = q.n.unwrap_or(100_000).clamp(1000, 1_000_000);
    let rating = |team: &str, comp: &str| {
        let db = &state.db; let team = team.to_string(); let comp = comp.to_string();
        async move {
            db.query_opt("SELECT rating FROM team_components WHERE sport='nhl' AND team=$1 AND component=$2 AND model_id=$3",
                &[&team, &comp, &TEAM_MODEL]).await.ok().flatten().map(|r| r.get::<_, f64>("rating"))
        }
    };
    let (oh, dh, oa, da) = (rating(&q.home, "goals_off").await, rating(&q.home, "goals_def").await, rating(&q.away, "goals_off").await, rating(&q.away, "goals_def").await);
    let (Some(oh), Some(dh), Some(oa), Some(da)) = (oh, dh, oa, da) else {
        return bad_request("unknown team(s); run /ingest/nhl/teams and use codes from /teams/nhl");
    };
    let param = |k: &str| {
        let db = &state.db; let k = k.to_string();
        async move { db.query_opt("SELECT value FROM team_model_params WHERE model_id=$1 AND key=$2", &[&TEAM_MODEL, &k]).await.ok().flatten().map(|r| r.get::<_, f64>("value")) }
    };
    let home_adv = param("home_adv").await.unwrap_or(0.0);
    let intercept = param("intercept").await.unwrap_or(1.1);
    let dispersion = param("dispersion").await.unwrap_or(30.0);
    let mu_home = (intercept + oh - da + home_adv).clamp(-2.0, 3.0).exp();
    let mu_away = (intercept + oa - dh).clamp(-2.0, 3.0).exp();
    let seed = q.seed.unwrap_or_else(|| std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1));
    let (puckline, total) = (q.puckline, q.total);

    let out = web::block(move || {
        let mut rng = Rng(seed);
        let (mut hw, mut aw, mut cover, mut over) = (0u64, 0u64, 0u64, 0u64);
        let (mut sh, mut sa) = (0.0, 0.0);
        let mut margins = Vec::with_capacity(n);
        let mut totals = Vec::with_capacity(n);
        let p_home_ot = mu_home / (mu_home + mu_away);
        for _ in 0..n {
            let mut hp = rng.negbin(mu_home, dispersion);
            let mut ap = rng.negbin(mu_away, dispersion);
            if (hp - ap).abs() < 0.5 {
                // regulation tie -> OT/SO golden goal
                if rng.uniform() < p_home_ot { hp += 1.0; } else { ap += 1.0; }
            }
            sh += hp; sa += ap;
            if hp > ap { hw += 1; } else { aw += 1; }
            let margin = hp - ap;
            let tot = hp + ap;
            if let Some(pl) = puckline { if margin + pl > 0.0 { cover += 1; } }
            if let Some(t) = total { if tot > t { over += 1; } }
            margins.push(margin); totals.push(tot);
        }
        margins.sort_by(|a, b| a.total_cmp(b));
        totals.sort_by(|a, b| a.total_cmp(b));
        let qf = |v: &[f64], p: f64| v[((p * (v.len() - 1) as f64) as usize).min(v.len() - 1)];
        let nf = n as f64;
        json!({
            "proj_home_goals": (sh/nf*100.0).round()/100.0, "proj_away_goals": (sa/nf*100.0).round()/100.0,
            "home_win_prob": hw as f64/nf, "away_win_prob": aw as f64/nf,
            "proj_total": ((sh+sa)/nf*100.0).round()/100.0, "median_total": qf(&totals,0.5), "total_p10": qf(&totals,0.1), "total_p90": qf(&totals,0.9),
            "median_margin": qf(&margins,0.5), "margin_p10": qf(&margins,0.1), "margin_p90": qf(&margins,0.9),
            "cover_prob": puckline.map(|_| cover as f64/nf), "over_prob": total.map(|_| over as f64/nf),
            "margin_hist": hist(&margins, 24), "total_hist": hist(&totals, 24),
        })
    }).await;
    match out {
        Ok(mut v) => {
            v["home"] = json!(q.home); v["away"] = json!(q.away);
            v["mu_home"] = json!((mu_home*100.0).round()/100.0); v["mu_away"] = json!((mu_away*100.0).round()/100.0);
            v["n"] = json!(n);
            HttpResponse::Ok().json(v)
        }
        Err(_) => HttpResponse::InternalServerError().finish(),
    }
}

fn hist(sorted: &[f64], bins: usize) -> Value {
    let q = |p: f64| sorted[((p * (sorted.len() - 1) as f64) as usize).min(sorted.len() - 1)];
    let (lo, hi) = (q(0.01), q(0.99));
    let span = (hi - lo).max(1e-9);
    let w = span / bins as f64;
    let mut counts = vec![0u32; bins];
    for &x in sorted { if x < lo || x > hi { continue; } let b = (((x - lo) / w) as usize).min(bins - 1); counts[b] += 1; }
    let total: u32 = counts.iter().sum();
    let density: Vec<f64> = counts.iter().map(|&c| if total > 0 { c as f64 / total as f64 } else { 0.0 }).collect();
    json!({ "lo": lo, "hi": hi, "bin_width": w, "density": density })
}
