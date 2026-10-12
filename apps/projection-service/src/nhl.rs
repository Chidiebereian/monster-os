//! NHL skater player projections.
//!
//! Data: current rosters from the official NHL API (api-web.nhle.com) to
//! enumerate active skaters, then each skater's game-by-game history from
//! MoneyPuck (careers/gameByGame, ~2008-present, with xG and the counting
//! stats). Each stat is fit with the same empirical-Bayes recency-weighted model
//! the NFL side uses (count -> Gamma-Poisson/Negative-Binomial, continuous ->
//! Gamma) and written to player_projections + posterior_features (model_id
//! nhl-{stat}-v1), so simulation-engine draws the true distribution at 100k with
//! exact Over/Under/Push. Real data in, Bayesian model, distribution out.
//!
//! Scope now: skater counting stats (shots, goals, assists, points, hits, PIM)
//! + time on ice. Goalie models (saves/GSAx), xG/RAPM/GAR, the NHL team model,
//! schedule/game filter, and the full market catalog are the documented next
//! phases — not claimed done.

use std::collections::HashMap;

use actix_web::{web, HttpResponse, Responder};
use futures::stream::{self, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_postgres::types::Json;

use crate::{fit_continuous, fit_count, Family, Projection, State, StatDef};

const NHL_API: &str = "https://api-web.nhle.com/v1";
const MONEYPUCK_GAMELOG: &str =
    "https://moneypuck.com/moneypuck/playerData/careers/gameByGame/regular/skaters";

// Canonical per-game keys we compute from MoneyPuck and model on. `opportunity`
// is toi_seconds (>=1 means the player was on the ice that game).
pub(crate) const NHL_STATS: &[StatDef] = &[
    StatDef { key: "shots",   label: "Shots on Goal",   column: "shots",   opportunity: "toi_seconds", family: Family::Count,      unit: "SOG" },
    StatDef { key: "goals",   label: "Goals",           column: "goals",   opportunity: "toi_seconds", family: Family::Count,      unit: "G" },
    StatDef { key: "assists", label: "Assists",         column: "assists", opportunity: "toi_seconds", family: Family::Count,      unit: "A" },
    StatDef { key: "points",  label: "Points",          column: "points",  opportunity: "toi_seconds", family: Family::Count,      unit: "P" },
    StatDef { key: "hits",    label: "Hits",            column: "hits",    opportunity: "toi_seconds", family: Family::Count,      unit: "hits" },
    StatDef { key: "pim",     label: "Penalty Minutes", column: "pim",     opportunity: "toi_seconds", family: Family::Count,      unit: "PIM" },
    StatDef { key: "toi",     label: "Time on Ice",     column: "toi",     opportunity: "toi_seconds", family: Family::Continuous, unit: "min" },
];

// Goalie props. saves = shots-on-goal faced minus goals allowed; a game counts
// only if the goalie played a meaningful share (toi_seconds filter at parse).
pub(crate) const GOALIE_STATS: &[StatDef] = &[
    StatDef { key: "saves",         label: "Saves",         column: "saves",         opportunity: "toi_seconds", family: Family::Count, unit: "SV" },
    StatDef { key: "shots_against", label: "Shots Against", column: "shots_against", opportunity: "toi_seconds", family: Family::Count, unit: "SA" },
    StatDef { key: "goals_against", label: "Goals Against", column: "goals_against", opportunity: "toi_seconds", family: Family::Count, unit: "GA" },
];

fn stat_def(key: &str) -> Option<&'static StatDef> {
    NHL_STATS.iter().chain(GOALIE_STATS).find(|s| s.key == key)
}
fn model_id(stat: &str) -> String {
    format!("nhl-{stat}-v1")
}
fn internal(msg: impl std::fmt::Display) -> HttpResponse {
    eprintln!("projection-service(nhl): {msg}");
    HttpResponse::InternalServerError().json(json!({ "error": msg.to_string() }))
}
fn bad_request(msg: impl Into<String>) -> HttpResponse {
    HttpResponse::BadRequest().json(json!({ "error": msg.into() }))
}

struct Skater {
    id: i64,
    name: String,
    position: String,
    team: String,
}
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
    #[serde(default = "d_min_games")]
    min_games: usize,
    #[serde(default = "d_prior_k")]
    prior_k: f64,
    #[serde(default = "d_halflife")]
    halflife: f64,
    /// cap number of skaters (for a quick partial ingest); 0 = all
    #[serde(default)]
    limit: usize,
    #[serde(default = "d_concurrency")]
    concurrency: usize,
}
fn d_min_games() -> usize { 10 }
fn d_prior_k() -> f64 { 8.0 }
fn d_halflife() -> f64 { 60.0 } // games; ~ a season of recency
fn d_concurrency() -> usize { 12 }

pub async fn ingest_nhl(state: web::Data<State>, q: web::Query<IngestQuery>) -> impl Responder {
    let q = q.into_inner();
    let http = &state.http;

    // 1. team abbreviations from the current standings
    let standings: Value = match http.get(format!("{NHL_API}/standings/now")).send().await.and_then(|r| r.error_for_status()) {
        Ok(r) => match r.json().await { Ok(v) => v, Err(e) => return internal(format!("standings json: {e}")) },
        Err(e) => return internal(format!("standings fetch: {e}")),
    };
    let teams: Vec<String> = standings["standings"].as_array().map(|a| {
        a.iter().filter_map(|t| t["teamAbbrev"]["default"].as_str().map(|s| s.to_string())).collect()
    }).unwrap_or_default();
    if teams.is_empty() {
        return internal("no teams from standings");
    }

    // 2. rosters -> skaters (concurrent)
    let roster_results: Vec<Vec<Skater>> = stream::iter(teams.clone())
        .map(|abbr| {
            let http = http.clone();
            async move {
                let url = format!("{NHL_API}/roster/{abbr}/current");
                let Ok(resp) = http.get(&url).send().await else { return Vec::new() };
                let Ok(v) = resp.json::<Value>().await else { return Vec::new() };
                let mut out = Vec::new();
                for group in ["forwards", "defensemen"] {
                    if let Some(arr) = v[group].as_array() {
                        for p in arr {
                            if let Some(id) = p["id"].as_i64() {
                                let fname = p["firstName"]["default"].as_str().unwrap_or("");
                                let lname = p["lastName"]["default"].as_str().unwrap_or("");
                                out.push(Skater {
                                    id,
                                    name: format!("{fname} {lname}").trim().to_string(),
                                    position: p["positionCode"].as_str().unwrap_or("").to_string(),
                                    team: abbr.clone(),
                                });
                            }
                        }
                    }
                }
                out
            }
        })
        .buffer_unordered(q.concurrency)
        .collect()
        .await;
    let mut skaters: Vec<Skater> = roster_results.into_iter().flatten().collect();
    // dedupe by id
    skaters.sort_by_key(|s| s.id);
    skaters.dedup_by_key(|s| s.id);
    if q.limit > 0 && skaters.len() > q.limit {
        skaters.truncate(q.limit);
    }
    let total_skaters = skaters.len();

    // 3. MoneyPuck game logs per skater (concurrent)
    let players: Vec<(i64, Player)> = stream::iter(skaters)
        .map(|sk| {
            let http = http.clone();
            async move {
                let url = format!("{MONEYPUCK_GAMELOG}/{}.csv", sk.id);
                let Ok(resp) = http.get(&url).send().await else { return None };
                let Ok(resp) = resp.error_for_status() else { return None };
                let Ok(text) = resp.text().await else { return None };
                parse_moneypuck(&text, &sk).map(|p| (sk.id, p))
            }
        })
        .buffer_unordered(q.concurrency)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .flatten()
        .collect();

    // 4. league baselines per stat
    let mut league: HashMap<&str, f64> = HashMap::new();
    for s in NHL_STATS {
        let (mut sum, mut cnt) = (0.0, 0.0);
        for (_, p) in &players {
            for g in &p.games {
                if *g.vals.get(s.opportunity).unwrap_or(&0.0) >= 1.0 {
                    sum += *g.vals.get(s.column).unwrap_or(&0.0);
                    cnt += 1.0;
                }
            }
        }
        league.insert(s.key, if cnt > 0.0 { sum / cnt } else { 0.0 });
    }

    // 5. register models
    for s in NHL_STATS {
        if let Err(e) = state.db.execute(
            "INSERT INTO models (id, name, sport, version, type, status) VALUES ($1, $2, 'nhl', '1', $3, 'active') \
             ON CONFLICT (id) DO NOTHING",
            &[&model_id(s.key), &format!("NHL {} (EB, recency-weighted)", s.label), &s.family.model_type()],
        ).await {
            return internal(format!("register model {}: {e}", s.key));
        }
    }

    // 6. fit + persist
    let mut fitted_players = 0usize;
    let mut fitted_by_stat: HashMap<&str, usize> = HashMap::new();
    for (pid, mut p) in players {
        p.games.sort_by(|a, b| a.sort.total_cmp(&b.sort));
        let n = p.games.len();
        if n == 0 {
            continue;
        }
        let (first, last) = (p.games.first().unwrap().season, p.games.last().unwrap().season);
        if let Err(e) = persist_player(&state.db, pid, &p, n as i32, first, last).await {
            return internal(e);
        }
        if let Err(e) = persist_game_logs(&state.db, pid, &p).await {
            return internal(e);
        }
        for s in NHL_STATS {
            let vals: Vec<f64> = p.games.iter()
                .filter(|g| *g.vals.get(s.opportunity).unwrap_or(&0.0) >= 1.0)
                .map(|g| *g.vals.get(s.column).unwrap_or(&0.0))
                .collect();
            if vals.len() < q.min_games {
                continue;
            }
            let base = *league.get(s.key).unwrap_or(&0.0);
            let proj = match s.family {
                Family::Count => fit_count(&vals, base, q.prior_k, q.halflife),
                Family::Continuous => fit_continuous(&vals, base, q.prior_k, q.halflife),
            };
            if let Err(e) = persist_projection(&state.db, pid, s, &proj).await {
                return internal(e);
            }
            *fitted_by_stat.entry(s.key).or_insert(0) += 1;
        }
        fitted_players += 1;
    }

    let summary: Vec<Value> = NHL_STATS.iter().map(|s| json!({
        "stat": s.key, "label": s.label, "family": s.family.as_str(),
        "league_mean": (league.get(s.key).unwrap_or(&0.0) * 1000.0).round() / 1000.0,
        "players_fitted": fitted_by_stat.get(s.key).copied().unwrap_or(0),
    })).collect();
    HttpResponse::Ok().json(json!({
        "sport": "nhl", "skaters_rostered": total_skaters, "players_with_logs": fitted_players,
        "min_games": q.min_games, "prior_k": q.prior_k, "halflife_games": q.halflife,
        "stats": summary,
    }))
}

/// Parse a MoneyPuck gameByGame CSV (all-situation rows) into canonical per-game stats.
fn parse_moneypuck(text: &str, sk: &Skater) -> Option<Player> {
    let mut rdr = csv::ReaderBuilder::new().has_headers(true).from_reader(text.as_bytes());
    let headers = rdr.headers().ok()?.clone();
    let idx = |n: &str| headers.iter().position(|h| h == n);
    let (i_sit, i_season, i_date, i_opp, i_gid) =
        (idx("situation")?, idx("season")?, idx("gameDate")?, idx("opposingTeam")?, idx("gameId")?);
    let i_toi = idx("icetime")?;
    let col = |n: &str| idx(n);
    let (i_g, i_sog, i_pts, i_pa, i_sa, i_hits, i_pim) = (
        col("I_F_goals"), col("I_F_shotsOnGoal"), col("I_F_points"),
        col("I_F_primaryAssists"), col("I_F_secondaryAssists"), col("I_F_hits"), col("I_F_penalityMinutes"),
    );
    let getf = |r: &csv::StringRecord, i: Option<usize>| i.and_then(|i| r.get(i)).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);

    let mut games = Vec::new();
    for rec in rdr.records().flatten() {
        if rec.get(i_sit) != Some("all") {
            continue;
        }
        let toi = getf(&rec, Some(i_toi)); // seconds
        let season: i32 = rec.get(i_season).and_then(|s| s.parse().ok()).unwrap_or(0);
        let date_str = rec.get(i_date).unwrap_or(""); // YYYYMMDD
        let date = chrono::NaiveDate::parse_from_str(date_str, "%Y%m%d").ok();
        let sort = date_str.parse::<f64>().unwrap_or(season as f64);
        let mut vals: HashMap<&'static str, f64> = HashMap::new();
        vals.insert("goals", getf(&rec, i_g));
        vals.insert("shots", getf(&rec, i_sog));
        vals.insert("points", getf(&rec, i_pts));
        vals.insert("assists", getf(&rec, i_pa) + getf(&rec, i_sa));
        vals.insert("hits", getf(&rec, i_hits));
        vals.insert("pim", getf(&rec, i_pim));
        vals.insert("toi", toi / 60.0);
        vals.insert("toi_seconds", toi);
        games.push(GameRow {
            season,
            game_id: rec.get(i_gid).unwrap_or("").to_string(),
            date,
            sort,
            opponent: rec.get(i_opp).unwrap_or("").to_string(),
            vals,
        });
    }
    if games.is_empty() {
        return None;
    }
    Some(Player { name: sk.name.clone(), position: sk.position.clone(), team: sk.team.clone(), games })
}

/// Enumerate goalies from current rosters, pull their MoneyPuck game logs, and
/// fit the goalie prop models (saves / shots-against / goals-against).
pub async fn ingest_goalies(state: web::Data<State>, q: web::Query<IngestQuery>) -> impl Responder {
    let q = q.into_inner();
    let http = &state.http;
    let standings: Value = match http.get(format!("{NHL_API}/standings/now")).send().await.and_then(|r| r.error_for_status()) {
        Ok(r) => match r.json().await { Ok(v) => v, Err(e) => return internal(format!("standings json: {e}")) },
        Err(e) => return internal(format!("standings fetch: {e}")),
    };
    let teams: Vec<String> = standings["standings"].as_array().map(|a| {
        a.iter().filter_map(|t| t["teamAbbrev"]["default"].as_str().map(|s| s.to_string())).collect()
    }).unwrap_or_default();

    let roster_results: Vec<Vec<Skater>> = stream::iter(teams)
        .map(|abbr| {
            let http = http.clone();
            async move {
                let Ok(resp) = http.get(format!("{NHL_API}/roster/{abbr}/current")).send().await else { return Vec::new() };
                let Ok(v) = resp.json::<Value>().await else { return Vec::new() };
                v["goalies"].as_array().map(|arr| arr.iter().filter_map(|p| {
                    let id = p["id"].as_i64()?;
                    let fname = p["firstName"]["default"].as_str().unwrap_or("");
                    let lname = p["lastName"]["default"].as_str().unwrap_or("");
                    Some(Skater { id, name: format!("{fname} {lname}").trim().to_string(), position: "G".into(), team: abbr.clone() })
                }).collect()).unwrap_or_default()
            }
        })
        .buffer_unordered(q.concurrency).collect().await;
    let mut goalies: Vec<Skater> = roster_results.into_iter().flatten().collect();
    goalies.sort_by_key(|s| s.id);
    goalies.dedup_by_key(|s| s.id);
    if q.limit > 0 && goalies.len() > q.limit { goalies.truncate(q.limit); }
    let total = goalies.len();

    let players: Vec<(i64, Player)> = stream::iter(goalies)
        .map(|sk| {
            let http = http.clone();
            async move {
                let url = format!("https://moneypuck.com/moneypuck/playerData/careers/gameByGame/regular/goalies/{}.csv", sk.id);
                let Ok(resp) = http.get(&url).send().await else { return None };
                let Ok(resp) = resp.error_for_status() else { return None };
                let Ok(text) = resp.text().await else { return None };
                parse_goalie(&text, &sk).map(|p| (sk.id, p))
            }
        })
        .buffer_unordered(q.concurrency).collect::<Vec<_>>().await.into_iter().flatten().collect();

    let mut league: HashMap<&str, f64> = HashMap::new();
    for s in GOALIE_STATS {
        let (mut sum, mut cnt) = (0.0, 0.0);
        for (_, p) in &players {
            for g in &p.games {
                if *g.vals.get(s.opportunity).unwrap_or(&0.0) >= 1.0 {
                    sum += *g.vals.get(s.column).unwrap_or(&0.0);
                    cnt += 1.0;
                }
            }
        }
        league.insert(s.key, if cnt > 0.0 { sum / cnt } else { 0.0 });
    }
    for s in GOALIE_STATS {
        if let Err(e) = state.db.execute(
            "INSERT INTO models (id, name, sport, version, type, status) VALUES ($1, $2, 'nhl', '1', $3, 'active') ON CONFLICT (id) DO NOTHING",
            &[&model_id(s.key), &format!("NHL {} (EB, recency-weighted)", s.label), &s.family.model_type()],
        ).await { return internal(format!("register model {}: {e}", s.key)); }
    }

    let mut fitted = 0usize;
    let mut by_stat: HashMap<&str, usize> = HashMap::new();
    for (pid, mut p) in players {
        p.games.sort_by(|a, b| a.sort.total_cmp(&b.sort));
        if p.games.is_empty() { continue; }
        let (first, last) = (p.games.first().unwrap().season, p.games.last().unwrap().season);
        if let Err(e) = persist_player(&state.db, pid, &p, p.games.len() as i32, first, last).await { return internal(e); }
        if let Err(e) = persist_game_logs(&state.db, pid, &p).await { return internal(e); }
        for s in GOALIE_STATS {
            let vals: Vec<f64> = p.games.iter().filter(|g| *g.vals.get(s.opportunity).unwrap_or(&0.0) >= 1.0).map(|g| *g.vals.get(s.column).unwrap_or(&0.0)).collect();
            if vals.len() < q.min_games { continue; }
            let base = *league.get(s.key).unwrap_or(&0.0);
            let proj = fit_count(&vals, base, q.prior_k, q.halflife);
            if let Err(e) = persist_projection(&state.db, pid, s, &proj).await { return internal(e); }
            *by_stat.entry(s.key).or_insert(0) += 1;
        }
        fitted += 1;
    }
    let summary: Vec<Value> = GOALIE_STATS.iter().map(|s| json!({
        "stat": s.key, "label": s.label, "league_mean": (league.get(s.key).unwrap_or(&0.0)*1000.0).round()/1000.0,
        "players_fitted": by_stat.get(s.key).copied().unwrap_or(0),
    })).collect();
    HttpResponse::Ok().json(json!({ "sport": "nhl", "goalies_rostered": total, "goalies_with_logs": fitted, "stats": summary }))
}

/// Parse a MoneyPuck goalie gameByGame CSV into per-game saves/shots/goals.
/// Only games with >= 25 min of ice time count (approximate starts).
fn parse_goalie(text: &str, sk: &Skater) -> Option<Player> {
    let mut rdr = csv::ReaderBuilder::new().has_headers(true).from_reader(text.as_bytes());
    let headers = rdr.headers().ok()?.clone();
    let idx = |n: &str| headers.iter().position(|h| h == n);
    let (i_sit, i_season, i_date, i_opp, i_toi, i_ongoal, i_goals) =
        (idx("situation")?, idx("season")?, idx("gameDate")?, idx("opposingTeam")?, idx("icetime")?, idx("ongoal")?, idx("goals")?);
    let getf = |r: &csv::StringRecord, i: usize| r.get(i).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
    let mut games = Vec::new();
    for rec in rdr.records().flatten() {
        if rec.get(i_sit) != Some("all") { continue; }
        let toi = getf(&rec, i_toi);
        if toi < 1500.0 { continue; } // ~25+ min => a real appearance / start
        let ongoal = getf(&rec, i_ongoal);
        let goals = getf(&rec, i_goals);
        let season: i32 = rec.get(i_season).and_then(|s| s.parse().ok()).unwrap_or(0);
        let date_str = rec.get(i_date).unwrap_or("");
        let date = chrono::NaiveDate::parse_from_str(date_str, "%Y%m%d").ok();
        let mut vals: HashMap<&'static str, f64> = HashMap::new();
        vals.insert("saves", (ongoal - goals).max(0.0));
        vals.insert("shots_against", ongoal);
        vals.insert("goals_against", goals);
        vals.insert("toi_seconds", toi);
        games.push(GameRow {
            season, game_id: format!("{season}-{date_str}"), date, sort: date_str.parse().unwrap_or(season as f64),
            opponent: rec.get(i_opp).unwrap_or("").to_string(), vals,
        });
    }
    if games.is_empty() { return None; }
    Some(Player { name: sk.name.clone(), position: "G".into(), team: sk.team.clone(), games })
}

async fn persist_player(db: &tokio_postgres::Client, pid: i64, p: &Player, games: i32, first: i32, last: i32) -> Result<(), String> {
    db.execute(
        "INSERT INTO sports_players (sport, player_id, display_name, position, team, first_season, last_season, games) \
         VALUES ('nhl', $1, $2, $3, $4, $5, $6, $7) \
         ON CONFLICT (sport, player_id) DO UPDATE SET display_name=EXCLUDED.display_name, position=EXCLUDED.position, \
           team=EXCLUDED.team, first_season=EXCLUDED.first_season, last_season=EXCLUDED.last_season, games=EXCLUDED.games, updated_at=now()",
        &[&pid.to_string(), &p.name, &p.position, &p.team, &first, &last, &games],
    ).await.map(|_| ()).map_err(|e| format!("persist player {pid}: {e}"))
}

async fn persist_game_logs(db: &tokio_postgres::Client, pid: i64, p: &Player) -> Result<(), String> {
    let seasons: Vec<i32> = p.games.iter().map(|g| g.season).collect();
    let weeks: Vec<&str> = p.games.iter().map(|g| g.game_id.as_str()).collect();
    let dates: Vec<Option<chrono::NaiveDate>> = p.games.iter().map(|g| g.date).collect();
    let teams: Vec<&str> = p.games.iter().map(|_| p.team.as_str()).collect();
    let opps: Vec<&str> = p.games.iter().map(|g| g.opponent.as_str()).collect();
    let stats: Vec<Value> = p.games.iter().map(|g| {
        Value::Object(g.vals.iter().map(|(k, v)| ((*k).to_string(), json!(v))).collect())
    }).collect();
    let stats_json: Vec<Json<&Value>> = stats.iter().map(Json).collect();
    let pid = pid.to_string();
    db.execute(
        "INSERT INTO player_game_logs (sport, player_id, season, week, game_date, team, opponent, stats) \
         SELECT 'nhl', $1, * FROM UNNEST($2::int[], $3::text[], $4::date[], $5::text[], $6::text[], $7::jsonb[]) \
         ON CONFLICT (sport, player_id, season, week) DO UPDATE SET game_date=EXCLUDED.game_date, team=EXCLUDED.team, \
           opponent=EXCLUDED.opponent, stats=EXCLUDED.stats",
        &[&pid, &seasons, &weeks, &dates, &teams, &opps, &stats_json],
    ).await.map(|_| ()).map_err(|e| format!("persist logs {pid}: {e}"))
}

async fn persist_projection(db: &tokio_postgres::Client, pid: i64, s: &StatDef, p: &Projection) -> Result<(), String> {
    let mid = model_id(s.key);
    let pid = pid.to_string();
    db.execute(
        "INSERT INTO player_projections \
           (sport, player_id, stat, game_id, model_id, proj_mean, proj_sd, dispersion, sample_games, career_mean, recent_mean, family, unit) \
         VALUES ('nhl', $1, $2, '', $3, $4, $5, $6, $7, $8, $9, $10, $11) \
         ON CONFLICT (sport, player_id, stat, game_id) DO UPDATE SET model_id=EXCLUDED.model_id, proj_mean=EXCLUDED.proj_mean, \
           proj_sd=EXCLUDED.proj_sd, dispersion=EXCLUDED.dispersion, sample_games=EXCLUDED.sample_games, career_mean=EXCLUDED.career_mean, \
           recent_mean=EXCLUDED.recent_mean, family=EXCLUDED.family, unit=EXCLUDED.unit, as_of=now()",
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

// --- read endpoints (mirror the NFL ones, sport='nhl') ---

#[derive(Deserialize)]
pub struct ListQuery {
    limit: Option<i64>,
    min_games: Option<i32>,
    q: Option<String>,
    teams: Option<String>,
}

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
    // when filtered to a game's teams, restrict to the current roster (freshest data)
    let min_last_season: Option<i32> = if teams.is_some() {
        state.db.query_opt("SELECT max(last_season)::int - 1 AS c FROM sports_players WHERE sport='nhl'", &[])
            .await.ok().flatten().and_then(|r| r.get::<_, Option<i32>>("c"))
    } else { None };
    let sql = "SELECT p.player_id, pl.display_name, pl.team, pl.position, pl.last_season, \
                      p.proj_mean, p.proj_sd, p.dispersion, p.sample_games, p.career_mean, p.recent_mean, \
                      p.market_line, p.market_implied, p.model_id, p.family, p.unit \
               FROM player_projections p JOIN sports_players pl ON pl.sport=p.sport AND pl.player_id=p.player_id \
               WHERE p.sport='nhl' AND p.stat=$1 AND p.sample_games >= $2 AND ($3='' OR pl.display_name ILIKE '%'||$3||'%') \
                 AND ($5::text[] IS NULL OR pl.team = ANY($5)) AND ($6::int IS NULL OR pl.last_season >= $6) \
               ORDER BY p.proj_mean DESC LIMIT $4";
    match state.db.query(sql, &[&def.key, &min_games, &search, &limit, &teams, &min_last_season]).await {
        Ok(rows) => {
            let out: Vec<Value> = rows.iter().map(proj_row_json).collect();
            HttpResponse::Ok().json(json!({ "sport": "nhl", "stat": def.key, "label": def.label, "unit": def.unit, "family": def.family.as_str(), "model_id": model_id(def.key), "count": out.len(), "projections": out }))
        }
        Err(e) => internal(e),
    }
}

pub async fn get_player_stat(state: web::Data<State>, path: web::Path<(String, String)>) -> impl Responder {
    let (stat, pid) = path.into_inner();
    let Some(def) = stat_def(&stat) else { return bad_request("unknown stat"); };
    let row = match state.db.query_opt(
        "SELECT p.player_id, pl.display_name, pl.team, pl.position, pl.last_season, p.proj_mean, p.proj_sd, p.dispersion, \
                p.sample_games, p.career_mean, p.recent_mean, p.market_line, p.market_implied, p.model_id, p.family, p.unit \
         FROM player_projections p JOIN sports_players pl ON pl.sport=p.sport AND pl.player_id=p.player_id \
         WHERE p.sport='nhl' AND p.stat=$1 AND p.player_id=$2",
        &[&def.key, &pid],
    ).await {
        Ok(Some(r)) => r,
        Ok(None) => return bad_request("no projection for that player/stat"),
        Err(e) => return internal(e),
    };
    let logs = state.db.query(
        "SELECT season, week, opponent, game_date, stats FROM player_game_logs WHERE sport='nhl' AND player_id=$1 \
         ORDER BY game_date DESC NULLS LAST LIMIT 20",
        &[&pid],
    ).await.unwrap_or_default();
    let recent: Vec<Value> = logs.iter().map(|r| {
        let stats: Json<Value> = r.get("stats");
        json!({ "season": r.get::<_, i32>("season"),
                "date": r.get::<_, Option<chrono::NaiveDate>>("game_date").map(|d| d.to_string()),
                "opponent": r.get::<_, String>("opponent"), "value": stats.0.get(def.column).cloned().unwrap_or(Value::Null) })
    }).collect();
    HttpResponse::Ok().json(json!({ "stat": def.key, "unit": def.unit, "projection": proj_row_json(&row), "recent_games": recent }))
}

pub async fn list_stats(_state: web::Data<State>) -> impl Responder {
    let out: Vec<Value> = NHL_STATS.iter().chain(GOALIE_STATS).map(|s| json!({
        "key": s.key, "label": s.label, "unit": s.unit, "family": s.family.as_str(), "model_id": model_id(s.key),
    })).collect();
    HttpResponse::Ok().json(json!({ "sport": "nhl", "stats": out }))
}

fn proj_row_json(r: &tokio_postgres::Row) -> Value {
    let f = |c: &str| r.get::<_, Option<f64>>(c);
    json!({
        "player_id": r.get::<_, String>("player_id"),
        "name": r.get::<_, String>("display_name"),
        "team": r.get::<_, Option<String>>("team"),
        "position": r.get::<_, Option<String>>("position"),
        "last_season": r.get::<_, Option<i32>>("last_season"),
        "proj_mean": f("proj_mean"), "proj_sd": f("proj_sd"), "dispersion": f("dispersion"),
        "sample_games": r.get::<_, Option<i32>>("sample_games"),
        "career_mean": f("career_mean"), "recent_mean": f("recent_mean"),
        "market_line": f("market_line"), "market_implied": f("market_implied"),
        "model_id": r.get::<_, Option<String>>("model_id"),
        "family": r.get::<_, Option<String>>("family"), "unit": r.get::<_, Option<String>>("unit"),
    })
}
