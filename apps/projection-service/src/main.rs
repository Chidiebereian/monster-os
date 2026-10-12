//! projection-service
//!
//! Builds per-sport, per-stat player projections from decades of real game logs
//! and writes them where the rest of MonsterOS already knows how to consume
//! them: `player_projections` (for the Projections UI) and `posterior_features`
//! (so simulation-engine can draw the distribution via
//! `/simulate?model_id=...&parameter=...`).
//!
//! Phase 1 implements NFL passing touchdowns from nflverse weekly player stats
//! (1999-present). The model is an empirical-Bayes Gamma-Poisson with recency
//! weighting: the league rate is the prior, a player's own recency-weighted
//! history is the likelihood, and the posterior predictive for the next game is
//! Negative-Binomial (mean = posterior rate, variance = rate*(1 + 1/beta)).
//! This is a genuine Bayesian model over real data — no fabricated numbers.
//!
//! Endpoints:
//!   GET  /healthz
//!   POST /ingest/nfl/pass_td            refit from nflverse and store
//!   GET  /projections/nfl/pass_td       leaderboard of projections
//!   GET  /projections/nfl/pass_td/{id}  one player's projection + recent form

use std::collections::HashMap;
use std::sync::Arc;

use actix_web::{web, App, HttpResponse, HttpServer, Responder};
use common::healthz;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_postgres::{types::Json, NoTls};

const NFLVERSE_PLAYER_STATS: &str =
    "https://github.com/nflverse/nflverse-data/releases/download/player_stats/player_stats.csv";
const MODEL_ID: &str = "nfl-passtd-v1";

struct State {
    db: Arc<tokio_postgres::Client>,
    http: reqwest::Client,
    nflverse_url: String,
}

fn internal(msg: impl std::fmt::Display) -> HttpResponse {
    eprintln!("projection-service: {msg}");
    HttpResponse::InternalServerError().json(json!({ "error": msg.to_string() }))
}
fn bad_request(msg: impl Into<String>) -> HttpResponse {
    HttpResponse::BadRequest().json(json!({ "error": msg.into() }))
}

// --------------------------------------------------------------------------
// Model: empirical-Bayes Gamma-Poisson with exponential recency weighting.
// --------------------------------------------------------------------------

/// One player's game-by-game stat, oldest first.
struct PlayerHistory {
    display_name: String,
    position: String,
    team: String,
    games: Vec<GameStat>,
}

#[derive(Clone)]
struct GameStat {
    season: i32,
    week: String,
    week_num: f64, // sortable: season*100 + week
    opponent: String,
    stats: Value, // full line we store
    value: f64,   // the modeled stat (passing_tds)
}

struct Projection {
    proj_mean: f64,
    proj_sd: f64,
    dispersion: f64, // Gamma shape alpha (NB size)
    career_mean: f64,
    recent_mean: f64,
    sample_games: i32,
}

/// Posterior predictive for the next game. `league_rate` is the EB prior mean,
/// `prior_k` the prior strength in pseudo-games, `halflife` the recency
/// half-life in games.
fn fit_gamma_poisson(values_oldest_first: &[f64], league_rate: f64, prior_k: f64, halflife: f64) -> Projection {
    let n = values_oldest_first.len();
    let newest = (n - 1) as f64;
    let (mut sw, mut swy) = (0.0f64, 0.0f64);
    for (i, &y) in values_oldest_first.iter().enumerate() {
        // age in games counted back from the most recent game.
        let age = newest - i as f64;
        let w = 0.5f64.powf(age / halflife);
        sw += w;
        swy += w * y;
    }
    // Gamma(alpha0, beta0) prior with mean = league_rate and strength prior_k.
    let alpha0 = league_rate * prior_k;
    let beta0 = prior_k;
    let alpha = alpha0 + swy;
    let beta = beta0 + sw;
    let rate = alpha / beta; // posterior mean of lambda
    // Posterior predictive (next game) is NB: mean = rate, var = rate*(1 + 1/beta).
    let var = rate * (1.0 + 1.0 / beta);
    let career_mean = values_oldest_first.iter().sum::<f64>() / n as f64;
    let recent_k = n.min(17);
    let recent_mean = values_oldest_first[n - recent_k..].iter().sum::<f64>() / recent_k as f64;
    Projection {
        proj_mean: rate,
        proj_sd: var.sqrt(),
        dispersion: alpha,
        career_mean,
        recent_mean,
        sample_games: n as i32,
    }
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
}
fn d_min_games() -> usize { 8 }
fn d_prior_k() -> f64 { 6.0 }
fn d_halflife() -> f64 { 34.0 }

/// Fetch nflverse weekly player stats, keep QB regular-season passing games,
/// fit each QB, and persist players, game logs and projections.
async fn ingest_nfl_passtd(state: web::Data<State>, q: web::Query<IngestQuery>) -> impl Responder {
    let q = q.into_inner();
    // 1. download
    let bytes = match state.http.get(&state.nflverse_url).send().await.and_then(|r| r.error_for_status()) {
        Ok(r) => match r.bytes().await {
            Ok(b) => b,
            Err(e) => return internal(format!("nflverse body: {e}")),
        },
        Err(e) => return internal(format!("nflverse fetch: {e}")),
    };

    // 2. parse the columns we need by header name.
    let mut rdr = csv::ReaderBuilder::new().has_headers(true).from_reader(bytes.as_ref());
    let headers = match rdr.headers() {
        Ok(h) => h.clone(),
        Err(e) => return internal(format!("csv headers: {e}")),
    };
    let col = |name: &str| headers.iter().position(|h| h == name);
    let (Some(i_pid), Some(i_name), Some(i_pos), Some(i_team), Some(i_season), Some(i_week), Some(i_type), Some(i_opp), Some(i_att), Some(i_tds)) = (
        col("player_id"), col("player_display_name"), col("position"), col("recent_team"),
        col("season"), col("week"), col("season_type"), col("opponent_team"),
        col("attempts"), col("passing_tds"),
    ) else {
        return internal("nflverse schema changed: a required column is missing");
    };
    let i_yds = col("passing_yards");
    let i_int = col("interceptions");
    let i_cmp = col("completions");

    let mut players: HashMap<String, PlayerHistory> = HashMap::new();
    let (mut league_tds, mut league_games) = (0.0f64, 0.0f64);
    let getf = |r: &csv::StringRecord, i: usize| r.get(i).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);

    for rec in rdr.records() {
        let r = match rec {
            Ok(r) => r,
            Err(e) => return internal(format!("csv row: {e}")),
        };
        if r.get(i_pos) != Some("QB") || r.get(i_type) != Some("REG") {
            continue;
        }
        let attempts = getf(&r, i_att);
        if attempts < 1.0 {
            continue; // only games the player actually threw in
        }
        let pid = r.get(i_pid).unwrap_or("").to_string();
        if pid.is_empty() {
            continue;
        }
        let season: i32 = r.get(i_season).and_then(|s| s.parse().ok()).unwrap_or(0);
        let week = r.get(i_week).unwrap_or("").to_string();
        let week_sort: f64 = season as f64 * 100.0 + week.parse::<f64>().unwrap_or(0.0);
        let tds = getf(&r, i_tds);
        let stats = json!({
            "passing_tds": tds,
            "passing_yards": i_yds.map(|i| getf(&r, i)),
            "interceptions": i_int.map(|i| getf(&r, i)),
            "completions": i_cmp.map(|i| getf(&r, i)),
            "attempts": attempts,
        });
        let entry = players.entry(pid).or_insert_with(|| PlayerHistory {
            display_name: r.get(i_name).unwrap_or("").to_string(),
            position: "QB".into(),
            team: r.get(i_team).unwrap_or("").to_string(),
            games: Vec::new(),
        });
        entry.team = r.get(i_team).unwrap_or(&entry.team).to_string(); // most-recent wins (rows are chronological-ish; refined on sort)
        entry.games.push(GameStat {
            season,
            week,
            week_num: week_sort,
            opponent: r.get(i_opp).unwrap_or("").to_string(),
            stats,
            value: tds,
        });
        league_tds += tds;
        league_games += 1.0;
    }

    if league_games == 0.0 {
        return internal("no QB regular-season passing games parsed");
    }
    let league_rate = league_tds / league_games;

    // 3. register the model (posterior_features FKs models.id).
    if let Err(e) = state
        .db
        .execute(
            "INSERT INTO models (id, name, sport, version, type, status) \
             VALUES ($1, 'NFL passing TDs (Gamma-Poisson EB, recency-weighted)', 'nfl', '1', 'negative_binomial', 'active') \
             ON CONFLICT (id) DO NOTHING",
            &[&MODEL_ID],
        )
        .await
    {
        return internal(format!("register model: {e}"));
    }

    // 4. fit + persist each qualifying player.
    let mut fitted = 0usize;
    let mut sample: Vec<Value> = Vec::new();
    for (pid, mut hist) in players.into_iter() {
        hist.games.sort_by(|a, b| a.week_num.total_cmp(&b.week_num));
        let n = hist.games.len();
        if n < q.min_games {
            continue;
        }
        // most-recent team = last game's team context
        if let Some(last) = hist.games.last() {
            let _ = last; // team already tracked; keep last opponent context out of player dim
        }
        let values: Vec<f64> = hist.games.iter().map(|g| g.value).collect();
        let proj = fit_gamma_poisson(&values, league_rate, q.prior_k, q.halflife);
        let (first_season, last_season) = (hist.games.first().unwrap().season, hist.games.last().unwrap().season);

        if let Err(e) = persist_player(&state.db, &pid, &hist, n as i32, first_season, last_season).await {
            return internal(e);
        }
        if let Err(e) = persist_game_logs(&state.db, &pid, &hist).await {
            return internal(e);
        }
        if let Err(e) = persist_projection(&state.db, &pid, &proj).await {
            return internal(e);
        }
        if sample.len() < 10 {
            sample.push(json!({
                "player_id": pid, "name": hist.display_name,
                "proj_mean": round3(proj.proj_mean), "proj_sd": round3(proj.proj_sd),
                "career_mean": round3(proj.career_mean), "games": proj.sample_games,
            }));
        }
        fitted += 1;
    }

    sample.sort_by(|a, b| b["proj_mean"].as_f64().unwrap_or(0.0).total_cmp(&a["proj_mean"].as_f64().unwrap_or(0.0)));
    HttpResponse::Ok().json(json!({
        "sport": "nfl", "stat": "passing_tds", "model_id": MODEL_ID,
        "league_rate": round3(league_rate), "qb_games_parsed": league_games as i64,
        "players_fitted": fitted, "min_games": q.min_games,
        "prior_k": q.prior_k, "halflife_games": q.halflife,
        "sample_top": sample,
    }))
}

fn round3(x: f64) -> f64 { (x * 1000.0).round() / 1000.0 }

async fn persist_player(
    db: &tokio_postgres::Client,
    pid: &str,
    hist: &PlayerHistory,
    games: i32,
    first_season: i32,
    last_season: i32,
) -> Result<(), String> {
    db.execute(
        "INSERT INTO sports_players (sport, player_id, display_name, position, team, first_season, last_season, games) \
         VALUES ('nfl', $1, $2, $3, $4, $5, $6, $7) \
         ON CONFLICT (sport, player_id) DO UPDATE SET \
           display_name = EXCLUDED.display_name, position = EXCLUDED.position, team = EXCLUDED.team, \
           first_season = EXCLUDED.first_season, last_season = EXCLUDED.last_season, games = EXCLUDED.games, \
           updated_at = now()",
        &[&pid, &hist.display_name, &hist.position, &hist.team, &first_season, &last_season, &games],
    )
    .await
    .map(|_| ())
    .map_err(|e| format!("persist player {pid}: {e}"))
}

async fn persist_game_logs(db: &tokio_postgres::Client, pid: &str, hist: &PlayerHistory) -> Result<(), String> {
    // One batched upsert per player via UNNEST.
    let seasons: Vec<i32> = hist.games.iter().map(|g| g.season).collect();
    let weeks: Vec<&str> = hist.games.iter().map(|g| g.week.as_str()).collect();
    let teams: Vec<&str> = hist.games.iter().map(|_| hist.team.as_str()).collect();
    let opps: Vec<&str> = hist.games.iter().map(|g| g.opponent.as_str()).collect();
    let stats: Vec<Json<&Value>> = hist.games.iter().map(|g| Json(&g.stats)).collect();
    db.execute(
        "INSERT INTO player_game_logs (sport, player_id, season, week, team, opponent, stats) \
         SELECT 'nfl', $1, * FROM UNNEST($2::int[], $3::text[], $4::text[], $5::text[], $6::jsonb[]) \
         ON CONFLICT (sport, player_id, season, week) DO UPDATE SET \
           team = EXCLUDED.team, opponent = EXCLUDED.opponent, stats = EXCLUDED.stats",
        &[&pid, &seasons, &weeks, &teams, &opps, &stats],
    )
    .await
    .map(|_| ())
    .map_err(|e| format!("persist logs {pid}: {e}"))
}

async fn persist_projection(db: &tokio_postgres::Client, pid: &str, p: &Projection) -> Result<(), String> {
    db.execute(
        "INSERT INTO player_projections \
           (sport, player_id, stat, game_id, model_id, proj_mean, proj_sd, dispersion, sample_games, career_mean, recent_mean) \
         VALUES ('nfl', $1, 'passing_tds', '', $2, $3, $4, $5, $6, $7, $8) \
         ON CONFLICT (sport, player_id, stat, game_id) DO UPDATE SET \
           model_id = EXCLUDED.model_id, proj_mean = EXCLUDED.proj_mean, proj_sd = EXCLUDED.proj_sd, \
           dispersion = EXCLUDED.dispersion, sample_games = EXCLUDED.sample_games, \
           career_mean = EXCLUDED.career_mean, recent_mean = EXCLUDED.recent_mean, as_of = now()",
        &[&pid, &MODEL_ID, &p.proj_mean, &p.proj_sd, &p.dispersion, &p.sample_games, &p.career_mean, &p.recent_mean],
    )
    .await
    .map_err(|e| format!("persist projection {pid}: {e}"))?;
    // Also expose as a posterior so simulation-engine can draw the distribution.
    let q05 = (p.proj_mean - 1.645 * p.proj_sd).max(0.0);
    let q95 = p.proj_mean + 1.645 * p.proj_sd;
    db.execute(
        "INSERT INTO posterior_features (model_id, parameter, mean, sd, q05, q95, rhat, ess) \
         VALUES ($1, $2, $3, $4, $5, $6, 1.0, NULL) \
         ON CONFLICT (model_id, parameter) DO UPDATE SET \
           mean = EXCLUDED.mean, sd = EXCLUDED.sd, q05 = EXCLUDED.q05, q95 = EXCLUDED.q95",
        &[&MODEL_ID, &format!("pass_td:{pid}"), &p.proj_mean, &p.proj_sd, &q05, &q95],
    )
    .await
    .map(|_| ())
    .map_err(|e| format!("persist posterior {pid}: {e}"))
}

// --------------------------------------------------------------------------
// Read endpoints
// --------------------------------------------------------------------------

#[derive(Deserialize)]
struct ListQuery {
    limit: Option<i64>,
    min_games: Option<i32>,
    q: Option<String>,
}

async fn list_nfl_passtd(state: web::Data<State>, query: web::Query<ListQuery>) -> impl Responder {
    let limit = query.limit.unwrap_or(50).clamp(1, 500);
    let min_games = query.min_games.unwrap_or(0);
    let search = query.q.clone().unwrap_or_default();
    let sql = "SELECT p.player_id, pl.display_name, pl.team, pl.last_season, \
                      p.proj_mean, p.proj_sd, p.dispersion, p.sample_games, p.career_mean, p.recent_mean, \
                      p.market_line, p.market_implied, p.model_id \
               FROM player_projections p \
               JOIN sports_players pl ON pl.sport = p.sport AND pl.player_id = p.player_id \
               WHERE p.sport = 'nfl' AND p.stat = 'passing_tds' AND p.sample_games >= $1 \
                 AND ($2 = '' OR pl.display_name ILIKE '%' || $2 || '%') \
               ORDER BY p.proj_mean DESC LIMIT $3";
    match state.db.query(sql, &[&min_games, &search, &limit]).await {
        Ok(rows) => {
            let out: Vec<Value> = rows.iter().map(proj_row_json).collect();
            HttpResponse::Ok().json(json!({ "sport": "nfl", "stat": "passing_tds", "model_id": MODEL_ID, "count": out.len(), "projections": out }))
        }
        Err(e) => internal(e),
    }
}

async fn get_nfl_passtd(state: web::Data<State>, pid: web::Path<String>) -> impl Responder {
    let pid = pid.into_inner();
    let proj = state
        .db
        .query_opt(
            "SELECT p.player_id, pl.display_name, pl.team, pl.last_season, \
                    p.proj_mean, p.proj_sd, p.dispersion, p.sample_games, p.career_mean, p.recent_mean, \
                    p.market_line, p.market_implied, p.model_id \
             FROM player_projections p JOIN sports_players pl \
               ON pl.sport = p.sport AND pl.player_id = p.player_id \
             WHERE p.sport='nfl' AND p.stat='passing_tds' AND p.player_id = $1",
            &[&pid],
        )
        .await;
    let row = match proj {
        Ok(Some(r)) => r,
        Ok(None) => return bad_request("no projection for that player"),
        Err(e) => return internal(e),
    };
    // recent game-by-game form (last 20).
    let logs = state
        .db
        .query(
            "SELECT season, week, opponent, stats FROM player_game_logs \
             WHERE sport='nfl' AND player_id=$1 ORDER BY season DESC, (week ~ '^[0-9]+$') DESC, \
                   CASE WHEN week ~ '^[0-9]+$' THEN week::int ELSE 0 END DESC LIMIT 20",
            &[&pid],
        )
        .await
        .unwrap_or_default();
    let recent: Vec<Value> = logs
        .iter()
        .map(|r| {
            let stats: Json<Value> = r.get("stats");
            json!({ "season": r.get::<_, i32>("season"), "week": r.get::<_, String>("week"),
                    "opponent": r.get::<_, String>("opponent"), "passing_tds": stats.0["passing_tds"] })
        })
        .collect();
    HttpResponse::Ok().json(json!({ "projection": proj_row_json(&row), "recent_games": recent }))
}

fn proj_row_json(r: &tokio_postgres::Row) -> Value {
    let f = |c: &str| r.get::<_, Option<f64>>(c);
    json!({
        "player_id": r.get::<_, String>("player_id"),
        "name": r.get::<_, String>("display_name"),
        "team": r.get::<_, Option<String>>("team"),
        "last_season": r.get::<_, Option<i32>>("last_season"),
        "proj_mean": f("proj_mean"),
        "proj_sd": f("proj_sd"),
        "dispersion": f("dispersion"),
        "sample_games": r.get::<_, Option<i32>>("sample_games"),
        "career_mean": f("career_mean"),
        "recent_mean": f("recent_mean"),
        "market_line": f("market_line"),
        "market_implied": f("market_implied"),
        "model_id": r.get::<_, Option<String>>("model_id"),
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
        http: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?,
        nflverse_url: std::env::var("NFLVERSE_PLAYER_STATS_URL")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| NFLVERSE_PLAYER_STATS.to_string()),
    });
    println!("projection-service listening on 0.0.0.0:{port}");
    HttpServer::new(move || {
        App::new()
            .app_data(state.clone())
            .route("/healthz", web::get().to(healthz))
            .route("/ingest/nfl/pass_td", web::post().to(ingest_nfl_passtd))
            .route("/projections/nfl/pass_td", web::get().to(list_nfl_passtd))
            .route("/projections/nfl/pass_td/{player_id}", web::get().to(get_nfl_passtd))
    })
    .bind(("0.0.0.0", port))?
    .run()
    .await
}
