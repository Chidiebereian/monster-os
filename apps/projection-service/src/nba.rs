//! NBA player projections.
//!
//! Data: stats.nba.com `leaguegamelog` (one bulk call per season for every
//! player-game), parsed into per-player game logs across seasons. Each stat is
//! fit with the shared empirical-Bayes recency-weighted model (count ->
//! Gamma-Poisson/Negative-Binomial, continuous -> Gamma) and written to
//! player_projections + posterior_features (model_id nba-{stat}-v1), so
//! simulation-engine draws the true distribution at 100k with exact
//! Over/Under/Push. Combo props (PRA, PR, PA, RA) are derived per game so they
//! stay consistent with their components.
//!
//! Scope now: counting stats + minutes + the common combos. Joint possession/
//! lineup/minutes simulation, shot-level and tracking models, and the full prop
//! catalog from the spec are documented next phases.

use std::collections::HashMap;

use actix_web::{web, HttpResponse, Responder};
use futures::stream::{self, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_postgres::types::Json;

use crate::{fit_continuous, fit_count, Family, Projection, State, StatDef};

pub const NBA_STATS: &[StatDef] = &[
    StatDef { key: "points",     label: "Points",      column: "pts",     opportunity: "min_played", family: Family::Count,      unit: "PTS" },
    StatDef { key: "rebounds",   label: "Rebounds",    column: "reb",     opportunity: "min_played", family: Family::Count,      unit: "REB" },
    StatDef { key: "assists",    label: "Assists",     column: "ast",     opportunity: "min_played", family: Family::Count,      unit: "AST" },
    StatDef { key: "threes",     label: "3-Pointers",  column: "threes",  opportunity: "min_played", family: Family::Count,      unit: "3PM" },
    StatDef { key: "steals",     label: "Steals",      column: "stl",     opportunity: "min_played", family: Family::Count,      unit: "STL" },
    StatDef { key: "blocks",     label: "Blocks",      column: "blk",     opportunity: "min_played", family: Family::Count,      unit: "BLK" },
    StatDef { key: "turnovers",  label: "Turnovers",   column: "tov",     opportunity: "min_played", family: Family::Count,      unit: "TOV" },
    StatDef { key: "minutes",    label: "Minutes",     column: "minutes", opportunity: "min_played", family: Family::Continuous, unit: "MIN" },
    StatDef { key: "pra",        label: "Pts+Reb+Ast", column: "pra",     opportunity: "min_played", family: Family::Count,      unit: "PRA" },
    StatDef { key: "pr",         label: "Pts+Reb",     column: "pr",      opportunity: "min_played", family: Family::Count,      unit: "P+R" },
    StatDef { key: "pa",         label: "Pts+Ast",     column: "pa",      opportunity: "min_played", family: Family::Count,      unit: "P+A" },
    StatDef { key: "ra",         label: "Reb+Ast",     column: "ra",      opportunity: "min_played", family: Family::Count,      unit: "R+A" },
];

fn stat_def(key: &str) -> Option<&'static StatDef> { NBA_STATS.iter().find(|s| s.key == key) }
fn model_id(stat: &str) -> String { format!("nba-{stat}-v1") }
fn internal(m: impl std::fmt::Display) -> HttpResponse {
    eprintln!("projection-service(nba): {m}");
    HttpResponse::InternalServerError().json(json!({ "error": m.to_string() }))
}
fn bad_request(m: impl Into<String>) -> HttpResponse { HttpResponse::BadRequest().json(json!({ "error": m.into() })) }

// hoopR-data NBA player box scores (gzipped CSV, one file per season). Reqwest-
// friendly (raw.githubusercontent). Coverage currently ends at the 2022-23
// season (upstream frozen); a current-season source is a documented follow-up.
const HOOPR_PLAYER_BOX: &str =
    "https://raw.githubusercontent.com/sportsdataverse/hoopR-data/main/nba/player_box/csv/player_box_{Y}.csv.gz";

struct GameRow {
    season: i32,
    game_id: String,
    date: Option<chrono::NaiveDate>,
    sort: f64,
    opponent: String,
    vals: HashMap<&'static str, f64>,
}
struct Player {
    name: String,
    position: String,
    team: String,
    games: Vec<GameRow>,
}

#[derive(Deserialize)]
pub struct IngestQuery {
    #[serde(default = "d_from")] from: i32,  // first season start year
    #[serde(default = "d_to")] to: i32,      // last season start year
    #[serde(default = "d_min_games")] min_games: usize,
    #[serde(default = "d_prior_k")] prior_k: f64,
    #[serde(default = "d_halflife")] halflife: f64,
    #[serde(default = "d_concurrency")] concurrency: usize,
}
fn d_from() -> i32 { 2002 }
fn d_to() -> i32 { 2023 }
fn d_min_games() -> usize { 15 }
fn d_prior_k() -> f64 { 10.0 }
fn d_halflife() -> f64 { 60.0 }
fn d_concurrency() -> usize { 6 }

pub async fn ingest_nba(state: web::Data<State>, q: web::Query<IngestQuery>) -> impl Responder {
    use std::io::Read;
    let q = q.into_inner();
    let years: Vec<i32> = (q.from..=q.to).collect();
    let http = &state.http;

    // one gzipped CSV per season, fetched concurrently
    let fetched: Vec<Vec<u8>> = stream::iter(years.clone())
        .map(|y| {
            let http = http.clone();
            async move {
                let url = HOOPR_PLAYER_BOX.replace("{Y}", &y.to_string());
                http.get(&url).send().await.ok()?.error_for_status().ok()?.bytes().await.ok().map(|b| b.to_vec())
            }
        })
        .buffer_unordered(q.concurrency).collect::<Vec<_>>().await.into_iter().flatten().collect();

    let mut players: HashMap<String, Player> = HashMap::new();
    for gz in &fetched {
        // decompress to bytes then lossy-decode (NBA names can carry non-UTF-8 bytes)
        let mut buf = Vec::new();
        if flate2::read::GzDecoder::new(gz.as_slice()).read_to_end(&mut buf).is_err() {
            continue;
        }
        let text = String::from_utf8_lossy(&buf);
        let mut rdr = csv::ReaderBuilder::new().has_headers(true).from_reader(text.as_bytes());
        let Ok(headers) = rdr.headers() else { continue };
        let idx: HashMap<String, usize> = headers.iter().enumerate().map(|(i, h)| (h.to_string(), i)).collect();
        let col = |r: &csv::StringRecord, name: &str| idx.get(name).and_then(|&i| r.get(i)).unwrap_or("").to_string();
        let gf = |r: &csv::StringRecord, name: &str| idx.get(name).and_then(|&i| r.get(i)).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
        for rec in rdr.records().flatten() {
            if col(&rec, "season_type") != "2" { continue; } // regular season
            if col(&rec, "did_not_play").eq_ignore_ascii_case("true") { continue; }
            let minutes = gf(&rec, "minutes");
            if minutes < 1.0 { continue; }
            let pid = col(&rec, "athlete_id");
            if pid.is_empty() { continue; }
            let (pts, reb, ast, threes, stl, blk, tov) =
                (gf(&rec, "points"), gf(&rec, "rebounds"), gf(&rec, "assists"), gf(&rec, "three_point_field_goals_made"), gf(&rec, "steals"), gf(&rec, "blocks"), gf(&rec, "turnovers"));
            let season: i32 = col(&rec, "season").parse().unwrap_or(0);
            let game_id = col(&rec, "game_id");
            let date_str = col(&rec, "game_date");
            let date = chrono::NaiveDate::parse_from_str(&date_str, "%Y-%m-%d").ok();
            let mut vals: HashMap<&'static str, f64> = HashMap::new();
            vals.insert("pts", pts); vals.insert("reb", reb); vals.insert("ast", ast);
            vals.insert("threes", threes); vals.insert("stl", stl); vals.insert("blk", blk); vals.insert("tov", tov);
            vals.insert("minutes", minutes); vals.insert("min_played", minutes);
            vals.insert("pra", pts + reb + ast); vals.insert("pr", pts + reb); vals.insert("pa", pts + ast); vals.insert("ra", reb + ast);
            let name = col(&rec, "athlete_display_name");
            let team = col(&rec, "team_abbreviation");
            let pos = col(&rec, "athlete_position_abbreviation");
            let entry = players.entry(pid).or_insert_with(|| Player { name, position: pos, team: team.clone(), games: Vec::new() });
            entry.team = team;
            let sort = date_str.replace('-', "").parse::<f64>().unwrap_or(season as f64);
            entry.games.push(GameRow { season, game_id, date, sort, opponent: col(&rec, "opponent_team_abbreviation"), vals });
        }
    }
    if players.is_empty() {
        return internal("no player-game rows parsed (hoopR source unavailable)");
    }

    // league baselines
    let mut league: HashMap<&str, f64> = HashMap::new();
    for s in NBA_STATS {
        let (mut sum, mut cnt) = (0.0, 0.0);
        for p in players.values() {
            for g in &p.games {
                if *g.vals.get(s.opportunity).unwrap_or(&0.0) >= 1.0 { sum += *g.vals.get(s.column).unwrap_or(&0.0); cnt += 1.0; }
            }
        }
        league.insert(s.key, if cnt > 0.0 { sum / cnt } else { 0.0 });
    }
    for s in NBA_STATS {
        if let Err(e) = state.db.execute(
            "INSERT INTO models (id, name, sport, version, type, status) VALUES ($1, $2, 'nba', '1', $3, 'active') ON CONFLICT (id) DO NOTHING",
            &[&model_id(s.key), &format!("NBA {} (EB, recency-weighted)", s.label), &s.family.model_type()],
        ).await { return internal(format!("register model {}: {e}", s.key)); }
    }

    let mut fitted = 0usize;
    let mut by_stat: HashMap<&str, usize> = HashMap::new();
    for (pid, mut p) in players {
        // dedupe by game_id, sort oldest-first
        let mut best: HashMap<String, GameRow> = HashMap::new();
        for g in p.games.drain(..) { best.entry(g.game_id.clone()).or_insert(g); }
        p.games = best.into_values().collect();
        p.games.sort_by(|a, b| a.sort.total_cmp(&b.sort));
        if p.games.is_empty() { continue; }
        let (first, last) = (p.games.first().unwrap().season, p.games.last().unwrap().season);
        if let Err(e) = persist_player(&state.db, &pid, &p, p.games.len() as i32, first, last).await { return internal(e); }
        if let Err(e) = persist_game_logs(&state.db, &pid, &p).await { return internal(e); }
        for s in NBA_STATS {
            let vals: Vec<f64> = p.games.iter().filter(|g| *g.vals.get(s.opportunity).unwrap_or(&0.0) >= 1.0).map(|g| *g.vals.get(s.column).unwrap_or(&0.0)).collect();
            if vals.len() < q.min_games { continue; }
            let base = *league.get(s.key).unwrap_or(&0.0);
            let proj = match s.family { Family::Count => fit_count(&vals, base, q.prior_k, q.halflife), Family::Continuous => fit_continuous(&vals, base, q.prior_k, q.halflife) };
            if let Err(e) = persist_projection(&state.db, &pid, s, &proj).await { return internal(e); }
            *by_stat.entry(s.key).or_insert(0) += 1;
        }
        fitted += 1;
    }

    let summary: Vec<Value> = NBA_STATS.iter().map(|s| json!({
        "stat": s.key, "label": s.label, "family": s.family.as_str(),
        "league_mean": (league.get(s.key).unwrap_or(&0.0)*1000.0).round()/1000.0,
        "players_fitted": by_stat.get(s.key).copied().unwrap_or(0),
    })).collect();
    HttpResponse::Ok().json(json!({ "sport": "nba", "seasons": format!("{}-{}", q.from, q.to), "players_fitted": fitted, "stats": summary }))
}

async fn persist_player(db: &tokio_postgres::Client, pid: &str, p: &Player, games: i32, first: i32, last: i32) -> Result<(), String> {
    db.execute(
        "INSERT INTO sports_players (sport, player_id, display_name, position, team, first_season, last_season, games) \
         VALUES ('nba', $1, $2, $3, $4, $5, $6, $7) \
         ON CONFLICT (sport, player_id) DO UPDATE SET display_name=EXCLUDED.display_name, team=EXCLUDED.team, \
           first_season=EXCLUDED.first_season, last_season=EXCLUDED.last_season, games=EXCLUDED.games, updated_at=now()",
        &[&pid, &p.name, &p.position, &p.team, &first, &last, &games],
    ).await.map(|_| ()).map_err(|e| format!("persist player {pid}: {e}"))
}

async fn persist_game_logs(db: &tokio_postgres::Client, pid: &str, p: &Player) -> Result<(), String> {
    let seasons: Vec<i32> = p.games.iter().map(|g| g.season).collect();
    let weeks: Vec<&str> = p.games.iter().map(|g| g.game_id.as_str()).collect();
    let dates: Vec<Option<chrono::NaiveDate>> = p.games.iter().map(|g| g.date).collect();
    let teams: Vec<&str> = p.games.iter().map(|_| p.team.as_str()).collect();
    let opps: Vec<&str> = p.games.iter().map(|g| g.opponent.as_str()).collect();
    let stats: Vec<Value> = p.games.iter().map(|g| Value::Object(g.vals.iter().map(|(k, v)| ((*k).to_string(), json!(v))).collect())).collect();
    let stats_json: Vec<Json<&Value>> = stats.iter().map(Json).collect();
    db.execute(
        "INSERT INTO player_game_logs (sport, player_id, season, week, game_date, team, opponent, stats) \
         SELECT 'nba', $1, * FROM UNNEST($2::int[], $3::text[], $4::date[], $5::text[], $6::text[], $7::jsonb[]) \
         ON CONFLICT (sport, player_id, season, week) DO UPDATE SET game_date=EXCLUDED.game_date, team=EXCLUDED.team, opponent=EXCLUDED.opponent, stats=EXCLUDED.stats",
        &[&pid, &seasons, &weeks, &dates, &teams, &opps, &stats_json],
    ).await.map(|_| ()).map_err(|e| format!("persist logs {pid}: {e}"))
}

async fn persist_projection(db: &tokio_postgres::Client, pid: &str, s: &StatDef, p: &Projection) -> Result<(), String> {
    let mid = model_id(s.key);
    db.execute(
        "INSERT INTO player_projections (sport, player_id, stat, game_id, model_id, proj_mean, proj_sd, dispersion, sample_games, career_mean, recent_mean, family, unit) \
         VALUES ('nba', $1, $2, '', $3, $4, $5, $6, $7, $8, $9, $10, $11) \
         ON CONFLICT (sport, player_id, stat, game_id) DO UPDATE SET model_id=EXCLUDED.model_id, proj_mean=EXCLUDED.proj_mean, proj_sd=EXCLUDED.proj_sd, \
           dispersion=EXCLUDED.dispersion, sample_games=EXCLUDED.sample_games, career_mean=EXCLUDED.career_mean, recent_mean=EXCLUDED.recent_mean, family=EXCLUDED.family, unit=EXCLUDED.unit, as_of=now()",
        &[&pid, &s.key, &mid, &p.proj_mean, &p.proj_sd, &p.dispersion, &p.sample_games, &p.career_mean, &p.recent_mean, &s.family.as_str(), &s.unit],
    ).await.map_err(|e| format!("persist projection {pid}/{}: {e}", s.key))?;
    let q05 = (p.proj_mean - 1.645 * p.proj_sd).max(0.0);
    let q95 = p.proj_mean + 1.645 * p.proj_sd;
    db.execute(
        "INSERT INTO posterior_features (model_id, parameter, mean, sd, q05, q95, rhat, ess) VALUES ($1, $2, $3, $4, $5, $6, 1.0, NULL) \
         ON CONFLICT (model_id, parameter) DO UPDATE SET mean=EXCLUDED.mean, sd=EXCLUDED.sd, q05=EXCLUDED.q05, q95=EXCLUDED.q95",
        &[&mid, &format!("{}:{pid}", s.key), &p.proj_mean, &p.proj_sd, &q05, &q95],
    ).await.map(|_| ()).map_err(|e| format!("persist posterior {pid}/{}: {e}", s.key))
}

#[derive(Deserialize)]
pub struct FitQuery {
    #[serde(default = "d_min_games")] min_games: usize,
    #[serde(default = "d_prior_k")] prior_k: f64,
    #[serde(default = "d_halflife")] halflife: f64,
}

/// Fit all NBA stats from whatever game logs are already stored (loaded by the
/// Python stats.nba.com ingester). Keeps the Bayesian modeling in Rust while the
/// acquisition of current-season data lives in Python (stats.nba.com is TLS-
/// fingerprint-blocked for the Rust client).
pub async fn fit_nba(state: web::Data<State>, q: web::Query<FitQuery>) -> impl Responder {
    let q = q.into_inner();
    let rows = match state.db.query(
        "SELECT player_id, stats FROM player_game_logs WHERE sport='nba' \
         ORDER BY player_id, game_date NULLS LAST, week",
        &[],
    ).await {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    if rows.is_empty() {
        return internal("no NBA game logs stored; run the Python ingester first");
    }
    // group by player (rows are ordered by player_id, date)
    let mut players: Vec<(String, Vec<Value>)> = Vec::new();
    for r in &rows {
        let pid: String = r.get("player_id");
        let stats: Json<Value> = r.get("stats");
        match players.last_mut() {
            Some((p, v)) if *p == pid => v.push(stats.0),
            _ => players.push((pid, vec![stats.0])),
        }
    }
    // league baselines
    let mut league: HashMap<&str, f64> = HashMap::new();
    for s in NBA_STATS {
        let (mut sum, mut cnt) = (0.0, 0.0);
        for (_, games) in &players {
            for g in games {
                if g.get(s.opportunity).and_then(|v| v.as_f64()).unwrap_or(0.0) >= 1.0 {
                    sum += g.get(s.column).and_then(|v| v.as_f64()).unwrap_or(0.0);
                    cnt += 1.0;
                }
            }
        }
        league.insert(s.key, if cnt > 0.0 { sum / cnt } else { 0.0 });
    }
    for s in NBA_STATS {
        if let Err(e) = state.db.execute(
            "INSERT INTO models (id, name, sport, version, type, status) VALUES ($1, $2, 'nba', '1', $3, 'active') ON CONFLICT (id) DO NOTHING",
            &[&model_id(s.key), &format!("NBA {} (EB, recency-weighted)", s.label), &s.family.model_type()],
        ).await { return internal(format!("register {}: {e}", s.key)); }
    }
    let mut fitted = 0usize;
    let mut by_stat: HashMap<&str, usize> = HashMap::new();
    for (pid, games) in &players {
        for s in NBA_STATS {
            let vals: Vec<f64> = games.iter()
                .filter(|g| g.get(s.opportunity).and_then(|v| v.as_f64()).unwrap_or(0.0) >= 1.0)
                .map(|g| g.get(s.column).and_then(|v| v.as_f64()).unwrap_or(0.0))
                .collect();
            if vals.len() < q.min_games { continue; }
            let base = *league.get(s.key).unwrap_or(&0.0);
            let proj = match s.family { Family::Count => fit_count(&vals, base, q.prior_k, q.halflife), Family::Continuous => fit_continuous(&vals, base, q.prior_k, q.halflife) };
            if let Err(e) = persist_projection(&state.db, pid, s, &proj).await { return internal(e); }
            *by_stat.entry(s.key).or_insert(0) += 1;
        }
        fitted += 1;
    }
    let summary: Vec<Value> = NBA_STATS.iter().map(|s| json!({
        "stat": s.key, "league_mean": (league.get(s.key).unwrap_or(&0.0)*1000.0).round()/1000.0, "players_fitted": by_stat.get(s.key).copied().unwrap_or(0),
    })).collect();
    HttpResponse::Ok().json(json!({ "sport": "nba", "players_seen": players.len(), "players_fitted": fitted, "stats": summary }))
}

// --- read endpoints ---
#[derive(Deserialize)]
pub struct ListQuery { limit: Option<i64>, min_games: Option<i32>, q: Option<String>, teams: Option<String> }

pub async fn list_stat(state: web::Data<State>, path: web::Path<String>, query: web::Query<ListQuery>) -> impl Responder {
    let stat = path.into_inner();
    let Some(def) = stat_def(&stat) else { return bad_request("unknown stat"); };
    let limit = query.limit.unwrap_or(50).clamp(1, 500);
    let min_games = query.min_games.unwrap_or(0);
    let search = query.q.clone().unwrap_or_default();
    let teams: Option<Vec<String>> = query.teams.as_ref().and_then(|s| {
        let v: Vec<String> = s.split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect();
        if v.is_empty() { None } else { Some(v) }
    });
    let min_last_season: Option<i32> = if teams.is_some() {
        state.db.query_opt("SELECT max(last_season)::int - 1 AS c FROM sports_players WHERE sport='nba'", &[]).await.ok().flatten().and_then(|r| r.get::<_, Option<i32>>("c"))
    } else { None };
    let sql = "SELECT p.player_id, pl.display_name, pl.team, pl.position, pl.last_season, p.proj_mean, p.proj_sd, p.dispersion, \
                      p.sample_games, p.career_mean, p.recent_mean, p.market_line, p.market_implied, p.model_id, p.family, p.unit \
               FROM player_projections p JOIN sports_players pl ON pl.sport=p.sport AND pl.player_id=p.player_id \
               WHERE p.sport='nba' AND p.stat=$1 AND p.sample_games >= $2 AND ($3='' OR pl.display_name ILIKE '%'||$3||'%') \
                 AND ($5::text[] IS NULL OR pl.team = ANY($5)) AND ($6::int IS NULL OR pl.last_season >= $6) \
               ORDER BY p.proj_mean DESC LIMIT $4";
    match state.db.query(sql, &[&def.key, &min_games, &search, &limit, &teams, &min_last_season]).await {
        Ok(rows) => {
            let out: Vec<Value> = rows.iter().map(proj_row_json).collect();
            HttpResponse::Ok().json(json!({ "sport": "nba", "stat": def.key, "label": def.label, "unit": def.unit, "family": def.family.as_str(), "model_id": model_id(def.key), "count": out.len(), "projections": out }))
        }
        Err(e) => internal(e),
    }
}

pub async fn get_player_stat(state: web::Data<State>, path: web::Path<(String, String)>) -> impl Responder {
    let (stat, pid) = path.into_inner();
    let Some(def) = stat_def(&stat) else { return bad_request("unknown stat"); };
    let row = match state.db.query_opt(
        "SELECT p.player_id, pl.display_name, pl.team, pl.position, pl.last_season, p.proj_mean, p.proj_sd, p.dispersion, p.sample_games, p.career_mean, p.recent_mean, p.market_line, p.market_implied, p.model_id, p.family, p.unit \
         FROM player_projections p JOIN sports_players pl ON pl.sport=p.sport AND pl.player_id=p.player_id WHERE p.sport='nba' AND p.stat=$1 AND p.player_id=$2",
        &[&def.key, &pid],
    ).await {
        Ok(Some(r)) => r,
        Ok(None) => return bad_request("no projection for that player/stat"),
        Err(e) => return internal(e),
    };
    let logs = state.db.query(
        "SELECT season, week, opponent, game_date, stats FROM player_game_logs WHERE sport='nba' AND player_id=$1 ORDER BY game_date DESC NULLS LAST LIMIT 20",
        &[&pid],
    ).await.unwrap_or_default();
    let recent: Vec<Value> = logs.iter().map(|r| {
        let stats: Json<Value> = r.get("stats");
        json!({ "season": r.get::<_, i32>("season"), "date": r.get::<_, Option<chrono::NaiveDate>>("game_date").map(|d| d.to_string()),
                "opponent": r.get::<_, String>("opponent"), "value": stats.0.get(def.column).cloned().unwrap_or(Value::Null) })
    }).collect();
    HttpResponse::Ok().json(json!({ "stat": def.key, "unit": def.unit, "projection": proj_row_json(&row), "recent_games": recent }))
}

pub async fn list_stats(_state: web::Data<State>) -> impl Responder {
    let out: Vec<Value> = NBA_STATS.iter().map(|s| json!({ "key": s.key, "label": s.label, "unit": s.unit, "family": s.family.as_str(), "model_id": model_id(s.key) })).collect();
    HttpResponse::Ok().json(json!({ "sport": "nba", "stats": out }))
}

fn proj_row_json(r: &tokio_postgres::Row) -> Value {
    let f = |c: &str| r.get::<_, Option<f64>>(c);
    json!({
        "player_id": r.get::<_, String>("player_id"), "name": r.get::<_, String>("display_name"),
        "team": r.get::<_, Option<String>>("team"), "position": r.get::<_, Option<String>>("position"),
        "last_season": r.get::<_, Option<i32>>("last_season"),
        "proj_mean": f("proj_mean"), "proj_sd": f("proj_sd"), "dispersion": f("dispersion"),
        "sample_games": r.get::<_, Option<i32>>("sample_games"), "career_mean": f("career_mean"), "recent_mean": f("recent_mean"),
        "market_line": f("market_line"), "market_implied": f("market_implied"), "model_id": r.get::<_, Option<String>>("model_id"),
        "family": r.get::<_, Option<String>>("family"), "unit": r.get::<_, Option<String>>("unit"),
    })
}
