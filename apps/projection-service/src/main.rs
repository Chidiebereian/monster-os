//! projection-service
//!
//! Per-sport, per-stat player projections from decades of real game logs,
//! written where the rest of MonsterOS already consumes them: `player_projections`
//! (Projections UI) and `posterior_features` (so simulation-engine draws the
//! distribution via `/simulate?model_id=&parameter=&n=100000`).
//!
//! Phase: NFL player validation set from nflverse weekly player stats (1999-present).
//! Each stat is fit with a family-appropriate empirical-Bayes model, recency-weighted
//! (exponential half-life in games); the league rate/mean is the prior and the
//! player's own recency-weighted history is the likelihood:
//!   - COUNT stats (TDs, attempts, completions, INTs, carries, receptions, targets):
//!     Gamma-Poisson -> Negative-Binomial posterior predictive.
//!   - CONTINUOUS stats (passing/rushing/receiving yards): recency-weighted Gamma,
//!     mean EB-shrunk toward the league mean, predictive sd inflated for parameter
//!     uncertainty.
//! Real data in, Bayesian model, distribution out — nothing fabricated. Opportunity-
//! based joint game simulation (predict snaps/targets/carries before yards) and a
//! family-aware simulation draw are the documented next phases; today the simulation
//! engine draws the stored mean/sd (Normal approximation).

use std::collections::HashMap;
use std::sync::Arc;

use actix_web::{web, App, HttpResponse, HttpServer, Responder};
use common::healthz;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_postgres::{types::Json, NoTls};

mod hitrates;
mod market;
mod team_model;
mod player_backtest;
mod nba;
mod nba_teams;
mod nhl;
mod nhl_teams;
mod teams;

const NFLVERSE_PLAYER_STATS: &str =
    "https://github.com/nflverse/nflverse-data/releases/download/player_stats/player_stats.csv";
// Current-season per-season files (newer nflverse schema: `team`,
// `passing_interceptions`). The cumulative file above lags by ~a season, so we
// merge these in. Override with NFLVERSE_CURRENT_URLS (comma-separated).
const NFL_CURRENT_SEASON_URLS: &[&str] = &[
    "https://github.com/nflverse/nflverse-data/releases/download/stats_player/stats_player_week_2025.csv",
    "https://github.com/nflverse/nflverse-data/releases/download/stats_player/stats_player_week_2026.csv",
];

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Family {
    Count,
    Continuous,
}
impl Family {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Family::Count => "count",
            Family::Continuous => "continuous",
        }
    }
    pub(crate) fn model_type(&self) -> &'static str {
        match self {
            Family::Count => "negative_binomial",
            Family::Continuous => "bayesian",
        }
    }
}

/// One projectable stat. `column` is the nflverse value; `opportunity` is the
/// column whose value must be >= 1 for a game to count (so a QB's rushing isn't
/// modeled from games he never ran, etc.).
pub(crate) struct StatDef {
    pub(crate) key: &'static str,
    pub(crate) label: &'static str,
    pub(crate) column: &'static str,
    pub(crate) opportunity: &'static str,
    pub(crate) family: Family,
    pub(crate) unit: &'static str,
}

pub(crate) const NFL_STATS: &[StatDef] = &[
    StatDef { key: "pass_td",      label: "Passing TDs",      column: "passing_tds",    opportunity: "attempts", family: Family::Count,      unit: "TDs" },
    StatDef { key: "pass_yds",     label: "Passing Yards",    column: "passing_yards",  opportunity: "attempts", family: Family::Continuous, unit: "yds" },
    StatDef { key: "pass_att",     label: "Pass Attempts",    column: "attempts",       opportunity: "attempts", family: Family::Count,      unit: "att" },
    StatDef { key: "completions",  label: "Completions",      column: "completions",    opportunity: "attempts", family: Family::Count,      unit: "comp" },
    StatDef { key: "interceptions",label: "Interceptions",    column: "interceptions",  opportunity: "attempts", family: Family::Count,      unit: "INT" },
    StatDef { key: "rush_yds",     label: "Rushing Yards",    column: "rushing_yards",  opportunity: "carries",  family: Family::Continuous, unit: "yds" },
    StatDef { key: "carries",      label: "Carries",          column: "carries",        opportunity: "carries",  family: Family::Count,      unit: "att" },
    StatDef { key: "rush_td",      label: "Rushing TDs",      column: "rushing_tds",    opportunity: "carries",  family: Family::Count,      unit: "TDs" },
    StatDef { key: "rec_yds",      label: "Receiving Yards",  column: "receiving_yards",opportunity: "targets",  family: Family::Continuous, unit: "yds" },
    StatDef { key: "receptions",   label: "Receptions",       column: "receptions",     opportunity: "targets",  family: Family::Count,      unit: "rec" },
    StatDef { key: "targets",      label: "Targets",          column: "targets",        opportunity: "targets",  family: Family::Count,      unit: "tgt" },
    StatDef { key: "rec_td",       label: "Receiving TDs",    column: "receiving_tds",  opportunity: "targets",  family: Family::Count,      unit: "TDs" },
];

fn stat_def(key: &str) -> Option<&'static StatDef> {
    NFL_STATS.iter().find(|s| s.key == key)
}
fn model_id(stat: &str) -> String {
    format!("nfl-{stat}-v1")
}

pub struct State {
    pub db: Arc<tokio_postgres::Client>,
    pub http: reqwest::Client,
    nflverse_url: String,
}

fn internal(msg: impl std::fmt::Display) -> HttpResponse {
    eprintln!("projection-service: {msg}");
    HttpResponse::InternalServerError().json(json!({ "error": msg.to_string() }))
}
fn bad_request(msg: impl Into<String>) -> HttpResponse {
    HttpResponse::BadRequest().json(json!({ "error": msg.into() }))
}
fn round3(x: f64) -> f64 { (x * 1000.0).round() / 1000.0 }

// --------------------------------------------------------------------------
// Models
// --------------------------------------------------------------------------

pub(crate) struct Projection {
    pub(crate) proj_mean: f64,
    pub(crate) proj_sd: f64,
    pub(crate) dispersion: f64,
    pub(crate) career_mean: f64,
    pub(crate) recent_mean: f64,
    pub(crate) sample_games: i32,
}

/// Recency weight for a game `age` games back from the most recent, given the
/// half-life in games.
fn recency_weight(age: f64, halflife: f64) -> f64 {
    0.5f64.powf(age / halflife)
}

/// COUNT: empirical-Bayes Negative-Binomial. The mean is shrunk toward the
/// league rate; the predictive variance is the player's EMPIRICAL recency-
/// weighted game-to-game variance (inflated for parameter uncertainty), floored
/// at the Poisson minimum (var >= mean). This captures real overdispersion for
/// high-variance counts (e.g. NBA points) that the pure Gamma-Poisson predictive
/// badly underestimated. The NB size `r` is back-solved so a draw matches.
pub(crate) fn fit_count(values_oldest_first: &[f64], league_rate: f64, prior_k: f64, halflife: f64) -> Projection {
    let n = values_oldest_first.len();
    let newest = (n - 1) as f64;
    let (mut sw, mut swy) = (0.0f64, 0.0f64);
    for (i, &y) in values_oldest_first.iter().enumerate() {
        let w = recency_weight(newest - i as f64, halflife);
        sw += w;
        swy += w * y;
    }
    let wmean = if sw > 0.0 { swy / sw } else { league_rate };
    let mut swv = 0.0;
    for (i, &y) in values_oldest_first.iter().enumerate() {
        swv += recency_weight(newest - i as f64, halflife) * (y - wmean).powi(2);
    }
    let game_var = if sw > 0.0 { swv / sw } else { wmean };
    // EB shrinkage of the mean toward the league baseline
    let mean = (prior_k * league_rate + sw * wmean) / (prior_k + sw);
    // predictive var = game-to-game var + parameter uncertainty, never below Poisson
    let pred_var = (game_var * (1.0 + 1.0 / (prior_k + sw))).max(mean.max(1e-6));
    // Negative-Binomial size that reproduces this mean/variance (var = mean + mean^2/r)
    let dispersion = if pred_var > mean + 1e-9 { (mean * mean / (pred_var - mean)).clamp(0.1, 1e6) } else { 1e6 };
    Projection {
        proj_mean: mean,
        proj_sd: pred_var.sqrt(),
        dispersion,
        career_mean: values_oldest_first.iter().sum::<f64>() / n as f64,
        recent_mean: recent_mean(values_oldest_first),
        sample_games: n as i32,
    }
}

/// CONTINUOUS: recency-weighted Gamma. Mean EB-shrunk toward the league mean,
/// game-to-game spread from the weighted variance, predictive sd inflated by the
/// parameter uncertainty 1/(prior_k + effective n). dispersion = Gamma shape.
pub(crate) fn fit_continuous(values_oldest_first: &[f64], league_mean: f64, prior_k: f64, halflife: f64) -> Projection {
    let n = values_oldest_first.len();
    let newest = (n - 1) as f64;
    let (mut sw, mut swy) = (0.0f64, 0.0f64);
    for (i, &y) in values_oldest_first.iter().enumerate() {
        let w = recency_weight(newest - i as f64, halflife);
        sw += w;
        swy += w * y;
    }
    let wmean = if sw > 0.0 { swy / sw } else { league_mean };
    // weighted variance (game-to-game spread)
    let mut swv = 0.0;
    for (i, &y) in values_oldest_first.iter().enumerate() {
        let w = recency_weight(newest - i as f64, halflife);
        swv += w * (y - wmean).powi(2);
    }
    let game_sd = if sw > 0.0 { (swv / sw).sqrt() } else { 0.0 };
    // EB shrinkage of the mean toward the league baseline
    let mean = (prior_k * league_mean + sw * wmean) / (prior_k + sw);
    // predictive sd: game-to-game spread inflated for parameter uncertainty
    let pred_sd = game_sd * (1.0 + 1.0 / (prior_k + sw)).sqrt();
    // Gamma shape (method of moments) as the dispersion summary
    let shape = if pred_sd > 0.0 { (mean / pred_sd).powi(2) } else { 0.0 };
    Projection {
        proj_mean: mean,
        proj_sd: pred_sd.max(1e-6),
        dispersion: shape,
        career_mean: values_oldest_first.iter().sum::<f64>() / n as f64,
        recent_mean: recent_mean(values_oldest_first),
        sample_games: n as i32,
    }
}

pub(crate) fn recent_mean(values: &[f64]) -> f64 {
    let n = values.len();
    let k = n.min(17);
    values[n - k..].iter().sum::<f64>() / k as f64
}

// --------------------------------------------------------------------------
// Ingestion
// --------------------------------------------------------------------------

#[derive(Deserialize)]
struct IngestQuery {
    #[serde(default = "d_min_games")]
    min_games: usize,
    #[serde(default = "d_prior_k")]
    prior_k: f64,
    #[serde(default = "d_halflife")]
    halflife: f64,
    /// comma-separated stat keys; empty = all NFL stats
    stats: Option<String>,
}
fn d_min_games() -> usize { 8 }
fn d_prior_k() -> f64 { 6.0 }
fn d_halflife() -> f64 { 34.0 }

/// A player's full per-game history: one row per game with every stat column we
/// parsed plus the opportunity columns, oldest-first after sorting.
struct Player {
    display_name: String,
    position: String,
    team: String,
    games: Vec<GameRow>,
}
struct GameRow {
    season: i32,
    week: String,
    week_sort: f64,
    opponent: String,
    vals: HashMap<&'static str, f64>, // column -> value (only columns we track)
}

async fn ingest_nfl(state: web::Data<State>, q: web::Query<IngestQuery>) -> impl Responder {
    let q = q.into_inner();
    let selected: Vec<&StatDef> = match &q.stats {
        Some(s) if !s.trim().is_empty() => {
            let keys: Vec<&str> = s.split(',').map(|k| k.trim()).collect();
            let defs: Vec<&StatDef> = keys.iter().filter_map(|k| stat_def(k)).collect();
            if defs.len() != keys.len() {
                return bad_request("unknown stat key; see NFL_STATS");
            }
            defs
        }
        _ => NFL_STATS.iter().collect(),
    };

    // columns we need to read = every stat column + every opportunity column
    let mut need_cols: Vec<&'static str> = Vec::new();
    for s in NFL_STATS {
        for c in [s.column, s.opportunity] {
            if !need_cols.contains(&c) {
                need_cols.push(c);
            }
        }
    }

    // 1. sources: cumulative 1999-2024 file (old schema) + current-season files
    //    (newer schema). Parse each with alias-aware column lookup and merge.
    let mut sources: Vec<String> = vec![state.nflverse_url.clone()];
    match std::env::var("NFLVERSE_CURRENT_URLS") {
        Ok(v) if !v.trim().is_empty() => sources.extend(v.split(',').map(|s| s.trim().to_string())),
        _ => sources.extend(NFL_CURRENT_SEASON_URLS.iter().map(|s| s.to_string())),
    }
    // canonical need-column -> accepted header names (schema differences)
    fn aliases(name: &str) -> Vec<&str> {
        match name {
            "interceptions" => vec!["interceptions", "passing_interceptions"],
            other => vec![other],
        }
    }

    let mut players: HashMap<String, Player> = HashMap::new();
    let getf = |r: &csv::StringRecord, i: Option<usize>| i.and_then(|i| r.get(i)).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);

    for url in &sources {
        let bytes = match state.http.get(url).send().await.and_then(|r| r.error_for_status()) {
            Ok(r) => match r.bytes().await { Ok(b) => b, Err(e) => return internal(format!("fetch body {url}: {e}")) },
            Err(e) => {
                if url == &state.nflverse_url {
                    return internal(format!("nflverse fetch: {e}"));
                }
                eprintln!("projection-service: skipping current-season source {url}: {e}");
                continue;
            }
        };
        let mut rdr = csv::ReaderBuilder::new().has_headers(true).from_reader(bytes.as_ref());
        let headers = match rdr.headers() { Ok(h) => h.clone(), Err(e) => return internal(format!("csv headers {url}: {e}")) };
        let find = |names: &[&str]| names.iter().find_map(|n| headers.iter().position(|h| h == *n));
        let (Some(i_pid), Some(i_name), Some(i_pos), Some(i_team), Some(i_season), Some(i_week), Some(i_type), Some(i_opp)) = (
            find(&["player_id"]), find(&["player_display_name"]), find(&["position"]),
            find(&["recent_team", "team"]), find(&["season"]), find(&["week"]),
            find(&["season_type"]), find(&["opponent_team", "opponent"]),
        ) else {
            return internal(format!("schema: a required dimension column is missing in {url}"));
        };
        let mut col_idx: HashMap<&'static str, Option<usize>> = HashMap::new();
        for &c in &need_cols {
            col_idx.insert(c, find(&aliases(c)));
        }
        for rec in rdr.records() {
            let r = match rec { Ok(r) => r, Err(e) => return internal(format!("csv row {url}: {e}")) };
            if r.get(i_type) != Some("REG") {
                continue;
            }
            let pid = r.get(i_pid).unwrap_or("").to_string();
            if pid.is_empty() {
                continue;
            }
            let mut vals: HashMap<&'static str, f64> = HashMap::new();
            for (&c, &i) in &col_idx {
                vals.insert(c, getf(&r, i));
            }
            let season: i32 = r.get(i_season).and_then(|s| s.parse().ok()).unwrap_or(0);
            let week = r.get(i_week).unwrap_or("").to_string();
            let week_sort = season as f64 * 100.0 + week.parse::<f64>().unwrap_or(0.0);
            let entry = players.entry(pid).or_insert_with(|| Player {
                display_name: r.get(i_name).unwrap_or("").to_string(),
                position: r.get(i_pos).unwrap_or("").to_string(),
                team: r.get(i_team).unwrap_or("").to_string(),
                games: Vec::new(),
            });
            entry.position = r.get(i_pos).unwrap_or(&entry.position).to_string();
            entry.team = r.get(i_team).unwrap_or(&entry.team).to_string();
            entry.games.push(GameRow { season, week, week_sort, opponent: r.get(i_opp).unwrap_or("").to_string(), vals });
        }
    }
    if players.is_empty() {
        return internal("no regular-season rows parsed");
    }

    // Dedupe each player's games by (season, week) — nflverse occasionally has a
    // duplicate row (e.g. a mid-game injury split); keep the one with the most
    // actual opportunity — then sort oldest-first.
    let opp_sum = |g: &GameRow| {
        g.vals.get("attempts").copied().unwrap_or(0.0)
            + g.vals.get("carries").copied().unwrap_or(0.0)
            + g.vals.get("targets").copied().unwrap_or(0.0)
    };
    for p in players.values_mut() {
        let mut best: HashMap<(i32, String), GameRow> = HashMap::new();
        for g in p.games.drain(..) {
            let key = (g.season, g.week.clone());
            match best.get(&key) {
                Some(existing) if opp_sum(existing) >= opp_sum(&g) => {}
                _ => { best.insert(key, g); }
            }
        }
        p.games = best.into_values().collect();
        p.games.sort_by(|a, b| a.week_sort.total_cmp(&b.week_sort));
    }

    // 3. league baselines per selected stat (mean over qualifying games)
    let mut league: HashMap<&str, f64> = HashMap::new();
    for s in &selected {
        let (mut sum, mut cnt) = (0.0, 0.0);
        for p in players.values() {
            for g in &p.games {
                if *g.vals.get(s.opportunity).unwrap_or(&0.0) >= 1.0 {
                    sum += *g.vals.get(s.column).unwrap_or(&0.0);
                    cnt += 1.0;
                }
            }
        }
        league.insert(s.key, if cnt > 0.0 { sum / cnt } else { 0.0 });
    }

    // 4. register models
    for s in &selected {
        if let Err(e) = state.db.execute(
            "INSERT INTO models (id, name, sport, version, type, status) VALUES ($1, $2, 'nfl', '1', $3, 'active') \
             ON CONFLICT (id) DO NOTHING",
            &[&model_id(s.key), &format!("NFL {} (EB, recency-weighted)", s.label), &s.family.model_type()],
        ).await {
            return internal(format!("register model {}: {e}", s.key));
        }
    }

    // 5. persist players + logs once; fit + persist each selected stat
    let mut fitted_by_stat: HashMap<&str, usize> = HashMap::new();
    for (pid, p) in &players {
        if let Err(e) = persist_player(&state.db, pid, p).await {
            return internal(e);
        }
        if let Err(e) = persist_game_logs(&state.db, pid, p).await {
            return internal(e);
        }
        for s in &selected {
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
    }

    let summary: Vec<Value> = selected.iter().map(|s| json!({
        "stat": s.key, "label": s.label, "family": s.family.as_str(),
        "league_mean": round3(*league.get(s.key).unwrap_or(&0.0)),
        "players_fitted": fitted_by_stat.get(s.key).copied().unwrap_or(0),
    })).collect();
    HttpResponse::Ok().json(json!({
        "sport": "nfl", "players_parsed": players.len(),
        "min_games": q.min_games, "prior_k": q.prior_k, "halflife_games": q.halflife,
        "stats": summary,
    }))
}

async fn persist_player(db: &tokio_postgres::Client, pid: &str, p: &Player) -> Result<(), String> {
    let (first, last) = (p.games.first().map(|g| g.season).unwrap_or(0), p.games.last().map(|g| g.season).unwrap_or(0));
    db.execute(
        "INSERT INTO sports_players (sport, player_id, display_name, position, team, first_season, last_season, games) \
         VALUES ('nfl', $1, $2, $3, $4, $5, $6, $7) \
         ON CONFLICT (sport, player_id) DO UPDATE SET display_name=EXCLUDED.display_name, position=EXCLUDED.position, \
           team=EXCLUDED.team, first_season=EXCLUDED.first_season, last_season=EXCLUDED.last_season, games=EXCLUDED.games, updated_at=now()",
        &[&pid, &p.display_name, &p.position, &p.team, &first, &last, &(p.games.len() as i32)],
    ).await.map(|_| ()).map_err(|e| format!("persist player {pid}: {e}"))
}

async fn persist_game_logs(db: &tokio_postgres::Client, pid: &str, p: &Player) -> Result<(), String> {
    let seasons: Vec<i32> = p.games.iter().map(|g| g.season).collect();
    let weeks: Vec<&str> = p.games.iter().map(|g| g.week.as_str()).collect();
    let teams: Vec<&str> = p.games.iter().map(|_| p.team.as_str()).collect();
    let opps: Vec<&str> = p.games.iter().map(|g| g.opponent.as_str()).collect();
    let stats: Vec<Value> = p.games.iter().map(|g| {
        Value::Object(g.vals.iter().map(|(k, v)| ((*k).to_string(), json!(v))).collect())
    }).collect();
    let stats_json: Vec<Json<&Value>> = stats.iter().map(Json).collect();
    db.execute(
        "INSERT INTO player_game_logs (sport, player_id, season, week, team, opponent, stats) \
         SELECT 'nfl', $1, * FROM UNNEST($2::int[], $3::text[], $4::text[], $5::text[], $6::jsonb[]) \
         ON CONFLICT (sport, player_id, season, week) DO UPDATE SET team=EXCLUDED.team, opponent=EXCLUDED.opponent, stats=EXCLUDED.stats",
        &[&pid, &seasons, &weeks, &teams, &opps, &stats_json],
    ).await.map(|_| ()).map_err(|e| format!("persist logs {pid}: {e}"))
}

async fn persist_projection(db: &tokio_postgres::Client, pid: &str, s: &StatDef, p: &Projection) -> Result<(), String> {
    let mid = model_id(s.key);
    db.execute(
        "INSERT INTO player_projections \
           (sport, player_id, stat, game_id, model_id, proj_mean, proj_sd, dispersion, sample_games, career_mean, recent_mean, family, unit) \
         VALUES ('nfl', $1, $2, '', $3, $4, $5, $6, $7, $8, $9, $10, $11) \
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

// --------------------------------------------------------------------------
// Read endpoints
// --------------------------------------------------------------------------

#[derive(Deserialize)]
struct ListQuery {
    limit: Option<i64>,
    min_games: Option<i32>,
    q: Option<String>,
    /// comma-separated team codes to filter to (e.g. a game's two teams)
    teams: Option<String>,
    /// only players whose last season is >= this (current roster when a game is picked)
    min_last_season: Option<i32>,
}

async fn list_stat(state: web::Data<State>, path: web::Path<String>, query: web::Query<ListQuery>) -> impl Responder {
    let stat = path.into_inner();
    let Some(def) = stat_def(&stat) else { return bad_request("unknown stat"); };
    let limit = query.limit.unwrap_or(50).clamp(1, 500);
    let min_games = query.min_games.unwrap_or(0);
    let search = query.q.clone().unwrap_or_default();
    let teams: Option<Vec<String>> = query.teams.as_ref().and_then(|s| {
        let v: Vec<String> = s.split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect();
        if v.is_empty() { None } else { Some(v) }
    });
    // When filtering to a game's teams, default to the current roster relative to
    // the freshest player data we hold (max last_season - 1), since the player
    // feed can lag the schedule feed.
    let min_last_season: Option<i32> = match (query.min_last_season, &teams) {
        (Some(s), _) => Some(s),
        (None, Some(_)) => state
            .db
            .query_opt("SELECT max(last_season)::int - 1 AS cutoff FROM sports_players WHERE sport='nfl'", &[])
            .await
            .ok()
            .flatten()
            .and_then(|r| r.get::<_, Option<i32>>("cutoff")),
        (None, None) => None,
    };
    let sql = "SELECT p.player_id, pl.display_name, pl.team, pl.position, pl.last_season, \
                      p.proj_mean, p.proj_sd, p.dispersion, p.sample_games, p.career_mean, p.recent_mean, \
                      p.market_line, p.market_implied, p.model_id, p.family, p.unit \
               FROM player_projections p JOIN sports_players pl ON pl.sport=p.sport AND pl.player_id=p.player_id \
               WHERE p.sport='nfl' AND p.stat=$1 AND p.sample_games >= $2 AND ($3='' OR pl.display_name ILIKE '%'||$3||'%') \
                 AND ($5::text[] IS NULL OR pl.team = ANY($5)) \
                 AND ($6::int IS NULL OR pl.last_season >= $6) \
               ORDER BY p.proj_mean DESC LIMIT $4";
    match state.db.query(sql, &[&def.key, &min_games, &search, &limit, &teams, &min_last_season]).await {
        Ok(rows) => {
            let out: Vec<Value> = rows.iter().map(proj_row_json).collect();
            HttpResponse::Ok().json(json!({ "sport": "nfl", "stat": def.key, "label": def.label, "unit": def.unit, "family": def.family.as_str(), "model_id": model_id(def.key), "count": out.len(), "projections": out }))
        }
        Err(e) => internal(e),
    }
}

async fn get_player_stat(state: web::Data<State>, path: web::Path<(String, String)>) -> impl Responder {
    let (stat, pid) = path.into_inner();
    let Some(def) = stat_def(&stat) else { return bad_request("unknown stat"); };
    let row = match state.db.query_opt(
        "SELECT p.player_id, pl.display_name, pl.team, pl.position, pl.last_season, p.proj_mean, p.proj_sd, p.dispersion, \
                p.sample_games, p.career_mean, p.recent_mean, p.market_line, p.market_implied, p.model_id, p.family, p.unit \
         FROM player_projections p JOIN sports_players pl ON pl.sport=p.sport AND pl.player_id=p.player_id \
         WHERE p.sport='nfl' AND p.stat=$1 AND p.player_id=$2",
        &[&def.key, &pid],
    ).await {
        Ok(Some(r)) => r,
        Ok(None) => return bad_request("no projection for that player/stat"),
        Err(e) => return internal(e),
    };
    let logs = state.db.query(
        "SELECT season, week, opponent, stats FROM player_game_logs WHERE sport='nfl' AND player_id=$1 \
         ORDER BY season DESC, CASE WHEN week ~ '^[0-9]+$' THEN week::int ELSE 0 END DESC LIMIT 20",
        &[&pid],
    ).await.unwrap_or_default();
    let recent: Vec<Value> = logs.iter().map(|r| {
        let stats: Json<Value> = r.get("stats");
        json!({ "season": r.get::<_, i32>("season"), "week": r.get::<_, String>("week"),
                "opponent": r.get::<_, String>("opponent"), "value": stats.0.get(def.column).cloned().unwrap_or(Value::Null) })
    }).collect();
    HttpResponse::Ok().json(json!({ "stat": def.key, "unit": def.unit, "projection": proj_row_json(&row), "recent_games": recent }))
}

async fn list_stats(_state: web::Data<State>) -> impl Responder {
    let out: Vec<Value> = NFL_STATS.iter().map(|s| json!({
        "key": s.key, "label": s.label, "unit": s.unit, "family": s.family.as_str(), "model_id": model_id(s.key),
    })).collect();
    HttpResponse::Ok().json(json!({ "sport": "nfl", "stats": out }))
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

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8121);
    let url = std::env::var("DATABASE_URL")
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "DATABASE_URL must be set"))?;
    let (client, connection) = tokio_postgres::connect(&url, NoTls)
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    actix_web::rt::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("projection-service: postgres connection closed: {e}");
        }
    });
    let state = web::Data::new(State {
        db: Arc::new(client),
        http: reqwest::Client::builder().timeout(std::time::Duration::from_secs(120))
            .user_agent("Mozilla/5.0 (MonsterOS projection-service)").build()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?,
        nflverse_url: std::env::var("NFLVERSE_PLAYER_STATS_URL").ok().filter(|v| !v.is_empty())
            .unwrap_or_else(|| NFLVERSE_PLAYER_STATS.to_string()),
    });
    println!("projection-service listening on 0.0.0.0:{port}");
    HttpServer::new(move || {
        App::new()
            .app_data(state.clone())
            .route("/healthz", web::get().to(healthz))
            .route("/ingest/nfl", web::post().to(ingest_nfl))
            .route("/stats/nfl", web::get().to(list_stats))
            .route("/projections/nfl/{stat}", web::get().to(list_stat))
            .route("/projections/nfl/{stat}/{player_id}", web::get().to(get_player_stat))
            .route("/ingest/nfl/teams", web::post().to(teams::ingest_teams))
            .route("/teams/nfl", web::get().to(teams::list_ratings))
            .route("/games/nfl", web::get().to(teams::list_games))
            .route("/matchup/nfl", web::get().to(teams::matchup))
            .route("/backtest/{sport}", web::get().to(team_model::backtest))
            .route("/backtest/player/{sport}/{stat}", web::get().to(player_backtest::backtest_q))
            .route("/market/{sport}/games", web::get().to(market::games))
            .route("/hitrates/{sport}/player/{player_id}", web::get().to(hitrates::player))
            .route("/hitrates/{sport}/team/{team}", web::get().to(hitrates::team))
            .route("/cheatsheet/{sport}/props", web::get().to(hitrates::cheatsheet_props))
            .route("/cheatsheet/{sport}/games", web::get().to(hitrates::cheatsheet_games))
            .route("/market/{sport}/props/{stat}", web::get().to(market::props))
            .route("/ingest/nhl", web::post().to(nhl::ingest_nhl))
            .route("/ingest/nhl/goalies", web::post().to(nhl::ingest_goalies))
            .route("/stats/nhl", web::get().to(nhl::list_stats))
            .route("/projections/nhl/{stat}", web::get().to(nhl::list_stat))
            .route("/projections/nhl/{stat}/{player_id}", web::get().to(nhl::get_player_stat))
            .route("/ingest/nhl/teams", web::post().to(nhl_teams::ingest_teams))
            .route("/ingest/nhl/schedule", web::post().to(nhl_teams::ingest_schedule))
            .route("/teams/nhl", web::get().to(nhl_teams::list_ratings))
            .route("/games/nhl", web::get().to(nhl_teams::list_games))
            .route("/matchup/nhl", web::get().to(nhl_teams::matchup))
            .route("/ingest/nba", web::post().to(nba::ingest_nba))
            .route("/fit/nba", web::post().to(nba::fit_nba))
            .route("/stats/nba", web::get().to(nba::list_stats))
            .route("/projections/nba/{stat}", web::get().to(nba::list_stat))
            .route("/projections/nba/{stat}/{player_id}", web::get().to(nba::get_player_stat))
            .route("/fit/nba/teams", web::post().to(nba_teams::fit_teams))
            .route("/teams/nba", web::get().to(nba_teams::list_ratings))
            .route("/games/nba", web::get().to(nba_teams::list_games))
            .route("/matchup/nba", web::get().to(nba_teams::matchup))
    })
    .bind(("0.0.0.0", port))?
    .run()
    .await
}
