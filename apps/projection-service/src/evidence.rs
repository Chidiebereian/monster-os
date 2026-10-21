//! Backtest evidence for AI Picks grade caps (`pick_evidence`, source
//! 'backtest'), regenerated automatically.
//!
//! A row only applies while its `detail.engine_version` is the champion's engine
//! code, so when an engine's code changes (a new MODEL_VERSION) its picks drop to
//! "unproven" until this worker has re-run full-season walk-forward backtests on
//! the new code. The autopilot runs one engine at a time in its slow loop.
//!
//! Each backtest uses the learning loop's own `evaluate` (inputs strictly before
//! each game) on every completed regular-season game of the last five seasons,
//! scoring each forecast against the baseline it carries: real lines (closing
//! spread / total) for 'market' rows, a naive model (player's own prior rate,
//! league rate) for 'baseline' rows. A season counts as better / worse when the
//! paired Brier difference is beyond 2 standard errors.

use std::collections::BTreeMap;

use actix_web::{web, HttpResponse, Responder};
use chrono::{NaiveDate, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::learning::{self, base};
use crate::State;

fn dberr(e: &tokio_postgres::Error) -> String {
    e.as_db_error().map(|d| d.message().to_string()).unwrap_or_else(|| e.to_string())
}

/// Engines whose forecasts price picks.
pub const PICK_ENGINES: [&str; 6] = ["nba", "nhl", "nfl", "nba_team", "nhl_team", "nfl_team"];
const SIMS: usize = 2_000;
const DRAWS: usize = 20;
const MIN_GAMES: usize = 100;

/// Forecast market -> (pick market, comparison).
pub(crate) fn pick_market(engine: &str, market: &str) -> Option<(&'static str, &'static str)> {
    Some(match (base(engine), market) {
        ("nba", "home_win") | ("nfl", "home_win") => ("moneyline", "market"),
        ("nba", "home_cover") | ("nfl", "home_cover") => ("spread", "market"),
        ("nba", "total_over") | ("nfl", "total_over") => ("total", "market"),
        ("nba", "pts_over_avg") => ("pts", "baseline"),
        ("nba", "reb_over_avg") => ("reb", "baseline"),
        ("nba", "ast_over_avg") => ("ast", "baseline"),
        ("nba", "fg3m_over_avg") => ("fg3m", "baseline"),
        ("nfl", "qb_pass_yds_over_avg") => ("pass_yds", "baseline"),
        ("nfl", "rush_yds_over_avg") => ("rush_yds", "baseline"),
        ("nfl", "rec_yds_over_avg") => ("rec_yds", "baseline"),
        ("nfl", "receptions_over_avg") => ("receptions", "baseline"),
        ("nhl", "home_win") => ("moneyline", "baseline"),
        ("nhl", "total_over_5_5") => ("total", "baseline"),
        ("nhl", "points_1plus") => ("points", "baseline"),
        ("nhl", "sog_over_avg") => ("sog", "baseline"),
        ("nhl", "goalie_saves_over_avg") => ("saves", "baseline"),
        _ => return None,
    })
}

/// The last five season labels (the current one included; seasons with too few games are skipped).
fn seasons(engine: &str, today: NaiveDate) -> Vec<i32> {
    let y = chrono::Datelike::year(&today);
    let m = chrono::Datelike::month(&today) as i32;
    match base(engine) {
        // NHL 20252026 style; a season starts in October
        "nhl" => { let s = if m >= 10 { y } else { y - 1 }; (0..5).map(|k| (s - k) * 10000 + (s - k + 1)).collect() }
        // NBA labelled by the year it ends
        "nba" => { let s = if m >= 10 { y + 1 } else { y }; (0..5).map(|k| s - k).collect() }
        _ => { let s = if m >= 9 { y } else { y - 1 }; (0..5).map(|k| s - k).collect() }
    }
}

#[derive(Default)]
struct Acc { seasons: Vec<i32>, n: usize, better: i32, worse: i32, sum: f64, var_n2: f64, baseline: String }

/// Paired Brier differences (model - baseline) per pick market for one season's games.
async fn season_diffs(state: &State, engine: &str, season: i32, cfg: &Value) -> Result<(usize, BTreeMap<(&'static str, &'static str), (Vec<f64>, String)>), String> {
    let games = learning::season_games(state, engine, season).await?;
    let mut out: BTreeMap<(&'static str, &'static str), (Vec<f64>, String)> = BTreeMap::new();
    let mut used = 0;
    for id in &games {
        let seed = learning::fnv(&[engine, id, "evidence"]);
        let Ok(Some(ge)) = learning::evaluate(state, engine, id, cfg, SIMS, DRAWS, seed).await else { continue };
        used += 1;
        // a moneyline compared with "the market" needs that game's real spread
        let has_line = ge.forecasts.iter().any(|f| f.market == "home_cover");
        for f in &ge.forecasts {
            let Some((pm, cmp)) = pick_market(engine, f.market) else { continue };
            if pm == "moneyline" && cmp == "market" && !has_line { continue; }
            let Some(y) = ge.outcomes.y(f) else { continue };
            let e = out.entry((pm, cmp)).or_insert_with(|| (Vec::new(), f.baseline.to_string()));
            e.0.push((f.p_model - y).powi(2) - (f.p_base - y).powi(2));
        }
    }
    Ok((used, out))
}

pub(crate) async fn run(state: &State, engine: &str) -> Result<Value, String> {
    let db = &state.db;
    let (version, cfg) = learning::champion(db, engine).await?;
    let code = learning::engine_of(engine);
    let started = Utc::now();
    let mut acc: BTreeMap<(&'static str, &'static str), Acc> = BTreeMap::new();
    let mut per_season = Vec::new();
    for season in seasons(engine, (Utc::now() - chrono::Duration::hours(5)).date_naive()) {
        let (used, diffs) = season_diffs(state, engine, season, &cfg).await?;
        if used < MIN_GAMES { per_season.push(json!({ "season": season, "games": used, "skipped": "too few completed games" })); continue; }
        let mut s = serde_json::Map::new();
        for ((pm, cmp), (d, baseline)) in diffs {
            let n = d.len();
            if n < 30 { continue; }
            let mean = d.iter().sum::<f64>() / n as f64;
            let se = (d.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n as f64 - 1.0) / n as f64).sqrt();
            let a = acc.entry((pm, cmp)).or_default();
            a.seasons.push(season); a.n += n; a.sum += mean * n as f64; a.var_n2 += (se * n as f64).powi(2); a.baseline = baseline;
            if mean < -2.0 * se { a.better += 1; }
            if mean > 2.0 * se { a.worse += 1; }
            s.insert(format!("{pm}:{cmp}"), json!({ "n": n, "brier_diff": (mean * 1e4).round() / 1e4, "se": (se * 1e4).round() / 1e4 }));
        }
        per_season.push(json!({ "season": season, "games": used, "markets": s }));
    }
    let mut rows = 0;
    for ((pm, cmp), a) in &acc {
        let metric = a.sum / a.n as f64;
        let se = a.var_n2.sqrt() / a.n as f64;
        let detail = json!({ "engine_version": code, "registry_version": version, "seasons": a.seasons, "baseline": a.baseline,
            "source_note": format!("automatic full-season walk-forward backtests, {SIMS} sims per game, run {}", started.format("%Y-%m-%d")) });
        rows += db.execute("INSERT INTO pick_evidence (engine, market, source, comparison, n, seasons, better, worse, metric, se, detail, updated_at) \
            VALUES ($1,$2,'backtest',$3,$4,$5,$6,$7,$8,$9,$10,now()) ON CONFLICT (engine, market, source, comparison) DO UPDATE SET n=EXCLUDED.n, seasons=EXCLUDED.seasons, \
            better=EXCLUDED.better, worse=EXCLUDED.worse, metric=EXCLUDED.metric, se=EXCLUDED.se, detail=EXCLUDED.detail, updated_at=now()",
            &[&engine, pm, cmp, &(a.n as i32), &(a.seasons.len() as i32), &a.better, &a.worse, &metric, &se, &detail]).await.map_err(|e| dberr(&e))?;
    }
    // an engine with no testable market still gets a marker row, so it is not re-run every cycle
    if acc.is_empty() {
        let detail = json!({ "engine_version": code, "registry_version": version, "seasons": [], "note": "no testable markets" });
        db.execute("INSERT INTO pick_evidence (engine, market, source, comparison, n, detail) VALUES ($1,'_none','backtest','baseline',0,$2) \
            ON CONFLICT (engine, market, source, comparison) DO UPDATE SET detail=EXCLUDED.detail, updated_at=now()", &[&engine, &detail]).await.map_err(|e| dberr(&e))?;
    }
    Ok(json!({ "engine": engine, "engine_version": code, "rows": rows, "seasons": per_season, "secs": (Utc::now() - started).num_seconds() }))
}

/// The next pick engine whose champion code has no backtest evidence (and did not fail in the last day).
pub(crate) async fn due(state: &State) -> Option<String> {
    for e in PICK_ENGINES {
        let code = learning::engine_of(e);
        let have = state.db.query_opt("SELECT 1 FROM pick_evidence WHERE engine=$1 AND source='backtest' AND detail->>'engine_version'=$2 LIMIT 1", &[&e, &code]).await.ok().flatten().is_some();
        if have { continue; }
        let failed = state.db.query_opt("SELECT 1 FROM learning_jobs WHERE job=$1 AND NOT last_ok AND last_run > now() - interval '1 day'", &[&format!("evidence:{e}")]).await.ok().flatten().is_some();
        if !failed { return Some(e.to_string()); }
    }
    None
}

#[derive(Deserialize)]
pub struct RunQ { engine: String }

/// POST /picks/evidence/run?engine= — run now (blocks until done; minutes for a sport).
pub async fn run_http(state: web::Data<State>, q: web::Query<RunQ>) -> impl Responder {
    if !PICK_ENGINES.contains(&q.engine.as_str()) { return HttpResponse::BadRequest().json(json!({ "error": format!("engine must be one of {PICK_ENGINES:?}") })); }
    match run(&state, &q.engine).await {
        Ok(v) => HttpResponse::Ok().json(v),
        Err(e) => HttpResponse::InternalServerError().json(json!({ "error": e })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seasons_follow_each_calendar() {
        let d = NaiveDate::from_ymd_opt(2026, 10, 21).unwrap();
        assert_eq!(seasons("nhl", d), vec![20262027, 20252026, 20242025, 20232024, 20222023]);
        assert_eq!(seasons("nba_team", d), vec![2027, 2026, 2025, 2024, 2023]);
        assert_eq!(seasons("nfl", d), vec![2026, 2025, 2024, 2023, 2022]);
        let spring = NaiveDate::from_ymd_opt(2027, 3, 1).unwrap();
        assert_eq!(seasons("nhl", spring)[0], 20262027);
        assert_eq!(seasons("nfl", spring)[0], 2026);
    }

    #[test]
    fn markets_map_per_sport() {
        assert_eq!(pick_market("nba_team", "home_cover"), Some(("spread", "market")));
        assert_eq!(pick_market("nhl", "home_win"), Some(("moneyline", "baseline")));
        assert_eq!(pick_market("nfl", "anytime_td"), None);
    }
}
