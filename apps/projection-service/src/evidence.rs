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
//! league rate) for 'baseline' rows. The engine's probability is blended toward
//! that comparison (blend.rs) with a weight fitted on the other seasons, and a
//! season counts as better / worse when the paired Brier difference is beyond 2
//! standard errors. Evidence is re-run every 30 days, and whenever the engine's
//! code or the blending method changes.

use std::collections::BTreeMap;

use actix_web::{web, HttpResponse, Responder};
use chrono::{NaiveDate, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::blend;
use crate::learning::{self, base};
use crate::State;

fn dberr(e: &tokio_postgres::Error) -> String {
    e.as_db_error().map(|d| d.message().to_string()).unwrap_or_else(|| e.to_string())
}

/// Engines whose forecasts price picks.
pub const PICK_ENGINES: [&str; 7] = ["nba", "nhl", "nfl", "mlb", "nba_team", "nhl_team", "nfl_team"];
const SIMS: usize = 2_000;
const DRAWS: usize = 20;
const MIN_GAMES: usize = 100;
/// Backtest evidence is re-run this often even without a code change.
const REFRESH_DAYS: i32 = 30;

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
        ("nba", "stl_over_avg") => ("stl", "baseline"),
        ("nba", "blk_over_avg") => ("blk", "baseline"),
        ("nba", "tov_over_avg") => ("tov", "baseline"),
        ("nba", "pra_over_avg") => ("pra", "baseline"),
        ("nfl", "qb_pass_yds_over_avg") => ("pass_yds", "baseline"),
        ("nfl", "rush_yds_over_avg") => ("rush_yds", "baseline"),
        ("nfl", "rec_yds_over_avg") => ("rec_yds", "baseline"),
        ("nfl", "receptions_over_avg") => ("receptions", "baseline"),
        ("nhl", "home_win") => ("moneyline", "baseline"),
        ("nhl", "total_over_5_5") => ("total", "baseline"),
        ("nhl", "points_1plus") => ("points", "baseline"),
        ("nhl", "sog_over_avg") => ("sog", "baseline"),
        ("nhl", "goalie_saves_over_avg") => ("saves", "baseline"),
        ("nhl", "goals_1plus") => ("goals", "baseline"),
        ("nhl", "assists_1plus") => ("assists", "baseline"),
        ("nhl", "blocks_over_avg") => ("blocks", "baseline"),
        ("nhl", "home_puckline") | ("nhl", "away_puckline") => ("spread", "baseline"),
        // the rest of the NBA and NFL prop menus
        ("nba", "pr_over_avg") => ("pr", "baseline"),
        ("nba", "pa_over_avg") => ("pa", "baseline"),
        ("nba", "ra_over_avg") => ("ra", "baseline"),
        ("nba", "stocks_over_avg") => ("stocks", "baseline"),
        ("nba", "fgm_over_avg") => ("fgm", "baseline"),
        ("nba", "fga_over_avg") => ("fga", "baseline"),
        ("nba", "fg3a_over_avg") => ("fg3a", "baseline"),
        ("nba", "ftm_over_avg") => ("ftm", "baseline"),
        ("nba", "fta_over_avg") => ("fta", "baseline"),
        ("nba", "oreb_over_avg") => ("oreb", "baseline"),
        ("nba", "dreb_over_avg") => ("dreb", "baseline"),
        ("nba", "double_double_yes") => ("double_double", "baseline"),
        ("nba", "triple_double_yes") => ("triple_double", "baseline"),
        ("nfl", "pass_att_over_avg") => ("pass_att", "baseline"),
        ("nfl", "completions_over_avg") => ("completions", "baseline"),
        ("nfl", "pass_td_over_avg") => ("pass_td", "baseline"),
        ("nfl", "interceptions_over_0_5") => ("interceptions", "baseline"),
        ("nfl", "pass_rush_yds_over_avg") => ("pass_rush_yds", "baseline"),
        ("nfl", "qb_rush_yds_over_avg") => ("rush_yds", "baseline"),
        ("nfl", "carries_over_avg") => ("carries", "baseline"),
        ("nfl", "targets_over_avg") => ("targets", "baseline"),
        ("nfl", "rush_rec_yds_over_avg") => ("rush_rec_yds", "baseline"),
        ("nfl", "anytime_td" | "tds_over_1_5") => ("tds", "baseline"),
        ("nfl", "kick_pts_over_avg") => ("kick_pts", "baseline"),
        ("nfl", "fgm_over_avg") => ("fgm", "baseline"),
        ("mlb", "home_win") => ("moneyline", "baseline"),
        ("mlb", "total_over_8_5") => ("total", "baseline"),
        // team totals and the first segment (segments::forecasts): their lines are
        // derived from the full-game market or are league-standard, never a real
        // quoted line, so they count as baseline evidence (grade cap B+)
        (_, "h1_home_win") => ("moneyline_1h", "baseline"),
        (_, "h1_home_cover") => ("spread_1h", "baseline"),
        (_, "h1_total_over" | "h1_total_over_1_5" | "h1_total_over_4_5") => ("total_1h", "baseline"),
        (_, "h1_home_total_over" | "h1_home_total_over_0_5" | "h1_home_total_over_2_5") => ("team_total_home_1h", "baseline"),
        (_, "h1_away_total_over" | "h1_away_total_over_0_5" | "h1_away_total_over_2_5") => ("team_total_away_1h", "baseline"),
        (_, "home_total_over" | "home_total_over_2_5" | "home_total_over_4_5") => ("team_total_home", "baseline"),
        (_, "away_total_over" | "away_total_over_2_5" | "away_total_over_4_5") => ("team_total_away", "baseline"),
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

/// (pick market, comparison) -> (baseline description, [(season, p_model, p_base, y)]).
type Obs = BTreeMap<(&'static str, &'static str), (String, Vec<(i32, f64, f64, f64)>)>;

/// One season's forecasts and outcomes per pick market.
async fn season_obs(state: &State, engine: &str, season: i32, cfg: &Value, out: &mut Obs) -> Result<usize, String> {
    let games = learning::season_games(state, engine, season).await?;
    let mut used = 0;
    for id in &games {
        let seed = learning::fnv(&[engine, id, "evidence"]);
        let Ok(Some(ge)) = learning::evaluate(state, engine, id, cfg, SIMS, DRAWS, seed).await else { continue };
        used += 1;
        // a moneyline compared with "the market" needs that game's real spread
        let has_line = ge.forecasts.iter().any(|f| f.market == "home_cover");
        let mut keep = Vec::new();
        for f in &ge.forecasts {
            let Some((pm, cmp)) = pick_market(engine, f.market) else { continue };
            if pm == "moneyline" && cmp == "market" && !has_line { continue; }
            let Some(y) = ge.outcomes.y(f) else { continue };
            out.entry((pm, cmp)).or_insert_with(|| (f.baseline.to_string(), Vec::new())).1.push((season, f.p_model, f.p_base, y));
            keep.push(json!({ "market": pm, "subject": f.subject, "line": f.line, "comparison": cmp, "p_model": f.p_model, "p_base": f.p_base, "y": y,
                "kind": if f.subject == "game" { "game" } else { "prop" }, "features": { "forecast_market": f.market, "baseline": f.baseline, "stat": f.stat } }));
        }
        save_examples(state, engine, season, id, &keep).await;
    }
    Ok(used)
}

/// Every backtest forecast also goes to the immutable ML dataset (migration
/// 0035), so research can train on whole seasons instead of the aggregates
/// pick_evidence keeps. A row already stored for this engine version is left as is.
async fn save_examples(state: &State, engine: &str, season: i32, game: &str, rows: &[Value]) {
    if rows.is_empty() { return; }
    let code = learning::engine_of(engine);
    let r = state.db.execute(
        "INSERT INTO ml_examples (source, sport, engine, engine_version, season, game_id, game_date, kind, market, subject, line, comparison, p_model, p_base, y, features) \
         SELECT 'backtest', $1, $2, $3, $4, $5, (SELECT game_date FROM sports_games WHERE game_id=$5 LIMIT 1), x.kind, x.market, x.subject, x.line, x.comparison, x.p_model, x.p_base, x.y, x.features \
         FROM jsonb_to_recordset($6) AS x(kind text, market text, subject text, line float8, comparison text, p_model float8, p_base float8, y float8, features jsonb) \
         ON CONFLICT DO NOTHING",
        &[&base(engine), &engine, &code, &season, &game, &tokio_postgres::types::Json(Value::Array(rows.to_vec()))]).await;
    if let Err(e) = r { eprintln!("ml_examples backtest {engine} {game}: {}", dberr(&e)); }
}

/// Full-season walk-forward backtests of the champion over the last five
/// seasons. Each pick market's rows record how the engine's probability, blended
/// toward the comparison (blend.rs) with a weight fitted on the OTHER seasons,
/// did against the comparison alone: real closing lines ('market') or the naive
/// model ('baseline'). The weight fitted on all seasons is the one picks use.
pub(crate) async fn run(state: &State, engine: &str) -> Result<Value, String> {
    let db = &state.db;
    let (version, cfg) = learning::champion(db, engine).await?;
    let code = learning::engine_of(engine);
    let started = Utc::now();
    let mut obs: Obs = BTreeMap::new();
    let mut per_season = Vec::new();
    for season in seasons(engine, (Utc::now() - chrono::Duration::hours(5)).date_naive()) {
        let mut one: Obs = BTreeMap::new();
        let used = season_obs(state, engine, season, &cfg, &mut one).await?;
        if used < MIN_GAMES { per_season.push(json!({ "season": season, "games": used, "skipped": "too few completed games" })); continue; }
        per_season.push(json!({ "season": season, "games": used }));
        for (k, (b, v)) in one { obs.entry(k).or_insert_with(|| (b, Vec::new())).1.extend(v); }
    }
    let mut rows = 0;
    let mut summary = serde_json::Map::new();
    for ((pm, cmp), (baseline, v)) in &obs {
        // seasons with too few forecasts of this market are left out
        let mut per: BTreeMap<i32, usize> = BTreeMap::new();
        for x in v { *per.entry(x.0).or_default() += 1; }
        let v: Vec<(i32, f64, f64, f64)> = v.iter().filter(|x| per[&x.0] >= 30).copied().collect();
        let oos = blend::out_of_sample(&v);
        if oos.is_empty() { continue; }
        let (mut better, mut worse) = (0, 0);
        let mut seasons_used = Vec::new();
        let mut seasons_json = Vec::new();
        for (season, (d, w)) in &oos {
            let (m, se) = blend::mean_se(d);
            if m < -2.0 * se { better += 1; }
            if m > 2.0 * se { worse += 1; }
            seasons_used.push(*season);
            let raw: Vec<f64> = v.iter().filter(|x| x.0 == *season).map(|(_, p, b, y)| (p - y).powi(2) - (b - y).powi(2)).collect();
            let (rm, _) = blend::mean_se(&raw);
            seasons_json.push(json!({ "season": season, "n": d.len(), "w": w, "brier_diff": (m * 1e4).round() / 1e4, "se": (se * 1e4).round() / 1e4, "raw_brier_diff": (rm * 1e4).round() / 1e4 }));
        }
        let all: Vec<f64> = oos.values().flat_map(|(d, _)| d.iter().copied()).collect();
        let (metric, se) = blend::mean_se(&all);
        let raw: Vec<f64> = v.iter().map(|(_, p, b, y)| (p - y).powi(2) - (b - y).powi(2)).collect();
        let (raw_metric, raw_se) = blend::mean_se(&raw);
        let w = blend::fit_w(&v.iter().map(|(_, p, b, y)| (*p, *b, *y)).collect::<Vec<_>>());
        let detail = json!({ "engine_version": code, "registry_version": version, "method": blend::METHOD, "w": w, "seasons": seasons_used, "baseline": baseline,
            "per_season": seasons_json, "raw_model_metric": raw_metric, "raw_model_se": raw_se,
            "source_note": format!("automatic full-season walk-forward backtests, {SIMS} sims per game; model blended toward the {} with a weight fitted on the other seasons, run {}",
                if *cmp == "market" { "closing line" } else { "naive baseline" }, started.format("%Y-%m-%d")) });
        summary.insert(format!("{pm}:{cmp}"), json!({ "w": w, "better": better, "worse": worse, "metric": (metric * 1e4).round() / 1e4, "raw_metric": (raw_metric * 1e4).round() / 1e4 }));
        rows += db.execute("INSERT INTO pick_evidence (engine, market, source, comparison, n, seasons, better, worse, metric, se, detail, updated_at) \
            VALUES ($1,$2,'backtest',$3,$4,$5,$6,$7,$8,$9,$10,now()) ON CONFLICT (engine, market, source, comparison) DO UPDATE SET n=EXCLUDED.n, seasons=EXCLUDED.seasons, \
            better=EXCLUDED.better, worse=EXCLUDED.worse, metric=EXCLUDED.metric, se=EXCLUDED.se, detail=EXCLUDED.detail, updated_at=now()",
            &[&engine, pm, cmp, &(v.len() as i32), &(seasons_used.len() as i32), &better, &worse, &metric, &se, &detail]).await.map_err(|e| dberr(&e))?;
    }
    // an engine with no testable market still gets a marker row, so it is not re-run every cycle
    if rows == 0 {
        let detail = json!({ "engine_version": code, "registry_version": version, "method": blend::METHOD, "seasons": [], "note": "no testable markets" });
        db.execute("INSERT INTO pick_evidence (engine, market, source, comparison, n, detail) VALUES ($1,'_none','backtest','baseline',0,$2) \
            ON CONFLICT (engine, market, source, comparison) DO UPDATE SET detail=EXCLUDED.detail, updated_at=now()", &[&engine, &detail]).await.map_err(|e| dberr(&e))?;
    }
    Ok(json!({ "engine": engine, "engine_version": code, "rows": rows, "markets": summary, "seasons": per_season, "secs": (Utc::now() - started).num_seconds() }))
}

/// The next pick engine whose champion code has no current backtest evidence (and did not fail in the last day).
pub(crate) async fn due(state: &State) -> Option<String> {
    for e in PICK_ENGINES {
        let code = learning::engine_of(e);
        // current code, current method, and refreshed within REFRESH_DAYS (new games accumulate in season)
        let have = state.db.query_opt("SELECT 1 FROM pick_evidence WHERE engine=$1 AND source='backtest' AND detail->>'engine_version'=$2 \
            AND detail->>'method'=$3 AND updated_at > now() - make_interval(days => $4) LIMIT 1", &[&e, &code, &blend::METHOD, &REFRESH_DAYS]).await.ok().flatten().is_some();
        if have { continue; }
        let failed = state.db.query_opt("SELECT 1 FROM learning_jobs WHERE job=$1 AND NOT last_ok AND last_run > now() - interval '1 day'", &[&format!("evidence:{e}")]).await.ok().flatten().is_some();
        if !failed { return Some(e.to_string()); }
    }
    None
}

#[derive(Deserialize)]
pub struct RunQ { engine: String, #[serde(default)] wait: bool }

/// POST /picks/evidence/run?engine=[&wait=true] — run now. By default the run is
/// started on the server and the call returns at once (a full sport takes half an
/// hour, longer than a client should hold a connection; a dropped connection would
/// cancel it). Results land in pick_evidence and the `evidence:{engine}` job row.
/// `wait=true` blocks and returns the summary.
pub async fn run_http(state: web::Data<State>, q: web::Query<RunQ>) -> impl Responder {
    if !PICK_ENGINES.contains(&q.engine.as_str()) { return HttpResponse::BadRequest().json(json!({ "error": format!("engine must be one of {PICK_ENGINES:?}") })); }
    if !q.wait {
        let (st, engine) = (state.clone(), q.engine.clone());
        actix_web::rt::spawn(async move {
            let v = run(&st, &engine).await;
            crate::autopilot::mark(&st.db, &format!("evidence:{engine}"), v.is_ok(), &v.unwrap_or_else(|e| json!({ "error": e }))).await;
        });
        return HttpResponse::Accepted().json(json!({ "engine": q.engine, "started": true, "note": "running on the server; poll pick_evidence or /learning jobs" }));
    }
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
        assert_eq!(pick_market("nfl", "anytime_td"), Some(("tds", "baseline")), "anytime TD is touchdowns over 0.5");
        assert_eq!(pick_market("nfl", "rush_rec_yds_over_avg"), Some(("rush_rec_yds", "baseline")));
        assert_eq!(pick_market("nba", "double_double_yes"), Some(("double_double", "baseline")));
        assert_eq!(pick_market("nfl", "qb_rush_td"), None);
        assert_eq!(pick_market("nhl", "away_puckline"), Some(("spread", "baseline")));
        assert_eq!(pick_market("nba", "blk_over_avg"), Some(("blk", "baseline")));
    }
}
