//! Walk-forward out-of-sample backtest of the PLAYER models. For each test game
//! we fit the empirical-Bayes recency-weighted model on that player's PRIOR
//! games only, project the stat, and compare to the actual result. Reports MAE,
//! RMSE, bias and predictive-interval coverage (80% and 90%), plus a career-
//! average-to-date baseline so you can see whether the model beats the naive
//! average. No historical prop lines are stored, so this measures predictive
//! QUALITY (calibration/accuracy), not ATS vs a market.

use actix_web::{web, HttpResponse, Responder};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_postgres::types::Json;

use crate::{fit_continuous, fit_count, Family, State, StatDef, NFL_STATS};

pub(crate) fn stat_def(sport: &str, key: &str) -> Option<&'static StatDef> {
    let regs: &[&[StatDef]] = match sport {
        "nfl" => &[NFL_STATS],
        "nhl" => &[crate::nhl::NHL_STATS, crate::nhl::GOALIE_STATS],
        "nba" => &[crate::nba::NBA_STATS],
        "mlb" => &[crate::mlb::MLB_BATTER_STATS, crate::mlb::MLB_PITCHER_STATS],
        _ => &[],
    };
    regs.iter().flat_map(|r| r.iter()).find(|s| s.key == key)
}

#[derive(Deserialize)]
pub struct Query {
    #[serde(default = "d_min_train")] min_train: usize,
    #[serde(default = "d_test_games")] test_games: usize,
    #[serde(default = "d_prior_k")] prior_k: f64,
    #[serde(default = "d_halflife")] halflife: f64,
    #[serde(default = "d_min_games")] min_games: usize,
}
fn d_min_train() -> usize { 12 }
fn d_test_games() -> usize { 15 }
fn d_prior_k() -> f64 { 8.0 }
fn d_halflife() -> f64 { 40.0 }
fn d_min_games() -> usize { 30 }

pub async fn backtest_q(state: web::Data<State>, path: web::Path<(String, String)>, q: web::Query<Query>) -> impl Responder {
    let (sport, stat) = path.into_inner();
    let Some(def) = stat_def(&sport, &stat) else { return HttpResponse::BadRequest().json(json!({ "error": "unknown sport/stat" })); };
    let q = q.into_inner();

    let rows = match state.db.query(
        "SELECT player_id, stats FROM player_game_logs WHERE sport=$1 ORDER BY player_id, game_date NULLS LAST, week",
        &[&sport],
    ).await { Ok(r) => r, Err(e) => return HttpResponse::InternalServerError().json(json!({ "error": e.to_string() })) };
    if rows.is_empty() { return HttpResponse::InternalServerError().json(json!({ "error": "no game logs" })); }

    // group by player -> the stat's per-game values (only games with opportunity)
    let mut players: Vec<(String, Vec<f64>)> = Vec::new();
    for r in &rows {
        let pid: String = r.get("player_id");
        let s: Json<Value> = r.get("stats");
        let opp = s.0.get(def.opportunity).and_then(|v| v.as_f64()).unwrap_or(0.0);
        let val = s.0.get(def.column).and_then(|v| v.as_f64()).unwrap_or(0.0);
        match players.last_mut() {
            Some((p, v)) if *p == pid => { if opp >= 1.0 { v.push(val); } }
            _ => players.push((pid, if opp >= 1.0 { vec![val] } else { Vec::new() })),
        }
    }

    // league baseline mean (for the model's prior) over all qualifying games
    let (mut lsum, mut lcnt) = (0.0, 0.0);
    for (_, v) in &players { for &x in v { lsum += x; lcnt += 1.0; } }
    let league = if lcnt > 0.0 { lsum / lcnt } else { 0.0 };

    let (mut n, mut m_abs, mut m_sq, mut m_bias, mut cov80, mut cov90) = (0u32, 0.0f64, 0.0f64, 0.0f64, 0u32, 0u32);
    let (mut b_abs, mut b_sq) = (0.0f64, 0.0f64);
    let mut pcount = 0u32;

    for (_pid, vals) in &players {
        if vals.len() < q.min_train + 1 || vals.len() < q.min_games { continue; }
        pcount += 1;
        let start = vals.len().saturating_sub(q.test_games).max(q.min_train);
        for i in start..vals.len() {
            let prefix = &vals[..i];
            if prefix.len() < q.min_train { continue; }
            let proj = match def.family {
                Family::Count => fit_count(prefix, league, q.prior_k, q.halflife),
                Family::Continuous => fit_continuous(prefix, league, q.prior_k, q.halflife),
            };
            let actual = vals[i];
            let err = actual - proj.proj_mean;
            n += 1;
            m_abs += err.abs(); m_sq += err * err; m_bias += err;
            let (lo80, hi80) = ((proj.proj_mean - 1.2816 * proj.proj_sd).max(0.0), proj.proj_mean + 1.2816 * proj.proj_sd);
            let (lo90, hi90) = ((proj.proj_mean - 1.6449 * proj.proj_sd).max(0.0), proj.proj_mean + 1.6449 * proj.proj_sd);
            if actual >= lo80 && actual <= hi80 { cov80 += 1; }
            if actual >= lo90 && actual <= hi90 { cov90 += 1; }
            // baseline: career average to date
            let base = prefix.iter().sum::<f64>() / prefix.len() as f64;
            let be = actual - base;
            b_abs += be.abs(); b_sq += be * be;
        }
    }
    if n == 0 { return HttpResponse::InternalServerError().json(json!({ "error": "no testable player-games; lower min_games/min_train" })); }
    let nf = n as f64;
    let r3 = |x: f64| (x * 1000.0).round() / 1000.0;
    HttpResponse::Ok().json(json!({
        "sport": sport, "stat": def.key, "label": def.label, "unit": def.unit, "family": def.family.as_str(),
        "method": "walk-forward per player (train on prior games only)",
        "players": pcount, "samples": n,
        "model": { "mae": r3(m_abs / nf), "rmse": r3((m_sq / nf).sqrt()), "bias": r3(m_bias / nf),
                   "coverage_80": r3(cov80 as f64 / nf), "coverage_90": r3(cov90 as f64 / nf) },
        "baseline_career_avg": { "mae": r3(b_abs / nf), "rmse": r3((b_sq / nf).sqrt()) },
        "note": "coverage near 0.80 / 0.90 = well-calibrated intervals; model MAE below baseline = recency-weighted model beats the naive average. No historical prop lines stored, so this is predictive quality, not ATS.",
    }))
}
