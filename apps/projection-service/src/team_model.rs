//! Generic walk-forward backtest for the team models (all sports). For each test
//! season it fits team ratings on PRIOR seasons only (no leakage) from the
//! completed games in sports_games, then scores predictions against actual
//! results. Straight-up accuracy, Brier, and calibration are always reported;
//! ATS / totals vs the closing line only when the sport stores market lines
//! (NFL). Reuses the same log-linear Poisson att/def + home-advantage structure.

use std::collections::HashMap;

use actix_web::{web, HttpResponse, Responder};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::State;

struct Obs { y: f64, team: usize, opp: usize, home: f64, w: f64 }
struct Fit { off: Vec<f64>, def: Vec<f64>, home_adv: f64, intercept: f64, dispersion: f64 }

fn fit(obs: &[Obs], tn: usize, epochs: usize, ridge: f64) -> Fit {
    let mut as_off: Vec<Vec<usize>> = vec![Vec::new(); tn];
    let mut as_def: Vec<Vec<usize>> = vec![Vec::new(); tn];
    for (i, o) in obs.iter().enumerate() { as_off[o.team].push(i); as_def[o.opp].push(i); }
    let wtot: f64 = obs.iter().map(|o| o.w).sum();
    let wmean: f64 = obs.iter().map(|o| o.y * o.w).sum::<f64>() / wtot.max(1e-9);
    let mut off = vec![0.0f64; tn];
    let mut def = vec![0.0f64; tn];
    let mut home_adv = 0.03f64;
    let mut intercept = wmean.max(0.5).ln();
    let eta = |off: &[f64], def: &[f64], ha: f64, ic: f64, o: &Obs| (ic + off[o.team] - def[o.opp] + ha * o.home).clamp(-2.0, 6.0);
    for _ in 0..epochs {
        let (mut g, mut h) = (0.0, 0.0);
        for o in obs { let mu = eta(&off, &def, home_adv, intercept, o).exp(); g += o.w * (o.y - mu); h += o.w * mu; }
        if h > 0.0 { intercept += g / h; }
        let (mut g, mut h) = (0.0, 0.0);
        for o in obs { if o.home > 0.0 { let mu = eta(&off, &def, home_adv, intercept, o).exp(); g += o.w * (o.y - mu); h += o.w * mu; } }
        if h > 0.0 { home_adv += g / h; }
        for ti in 0..tn {
            let (mut g, mut h) = (-ridge * off[ti], ridge);
            for &i in &as_off[ti] { let o = &obs[i]; let mu = eta(&off, &def, home_adv, intercept, o).exp(); g += o.w * (o.y - mu); h += o.w * mu; }
            if h > 0.0 { off[ti] += g / h; }
        }
        for ti in 0..tn {
            let (mut g, mut h) = (-ridge * def[ti], ridge);
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
    for o in obs { let mu = eta(&off, &def, home_adv, intercept, o).exp(); a += o.w * (o.y - mu).powi(2); b += o.w * mu; c += o.w * mu * mu; }
    let dispersion = if a > b { (c / (a - b)).clamp(1.0, 1e6) } else { 1e6 };
    Fit { off, def, home_adv, intercept, dispersion }
}

fn erf(x: f64) -> f64 {
    let t = 1.0 / (1.0 + 0.3275911 * x.abs());
    let y = 1.0 - (((((1.061405429 * t - 1.453152027) * t) + 1.421413741) * t - 0.284496736) * t + 0.254829592) * t * (-x * x).exp();
    if x >= 0.0 { y } else { -y }
}
fn normal_cdf(z: f64) -> f64 { 0.5 * (1.0 + erf(z / std::f64::consts::SQRT_2)) }
fn r3(x: f64) -> f64 { (x * 1000.0).round() / 1000.0 }

struct Game { season: i32, home: usize, away: usize, hs: f64, as_: f64, spread: Option<f64>, total: Option<f64> }

#[derive(Deserialize)]
pub struct BtQuery {
    from: Option<i32>, to: Option<i32>,
    #[serde(default = "d_halflife")] halflife_seasons: f64,
    #[serde(default = "d_epochs")] epochs: usize,
    #[serde(default = "d_ridge")] ridge: f64,
}
fn d_halflife() -> f64 { 2.0 }
fn d_epochs() -> usize { 60 }
fn d_ridge() -> f64 { 0.5 }

pub async fn backtest(state: web::Data<State>, path: web::Path<String>, q: web::Query<BtQuery>) -> impl Responder {
    let sport = path.into_inner();
    if !matches!(sport.as_str(), "nfl" | "nhl" | "nba") {
        return HttpResponse::BadRequest().json(json!({ "error": "backtest supports nfl, nhl, nba" }));
    }
    let q = q.into_inner();
    let rows = match state.db.query(
        "SELECT season, home_team, away_team, home_score, away_score, spread_line, total_line FROM sports_games \
         WHERE sport=$1 AND game_type='REG' AND home_score IS NOT NULL AND away_score IS NOT NULL",
        &[&sport],
    ).await { Ok(r) => r, Err(e) => return HttpResponse::InternalServerError().json(json!({ "error": e.to_string() })) };
    if rows.len() < 300 { return HttpResponse::InternalServerError().json(json!({ "error": "not enough stored games; ingest teams/schedule first" })); }

    let mut idx: HashMap<String, usize> = HashMap::new();
    let mut games: Vec<Game> = Vec::new();
    for r in &rows {
        let (h, a): (String, String) = (r.get("home_team"), r.get("away_team"));
        for t in [&h, &a] { let n = idx.len(); idx.entry(t.clone()).or_insert(n); }
        games.push(Game {
            season: r.get::<_, Option<i32>>("season").unwrap_or(0), home: idx[&h], away: idx[&a],
            hs: r.get::<_, i32>("home_score") as f64, as_: r.get::<_, i32>("away_score") as f64,
            spread: r.get("spread_line"), total: r.get("total_line"),
        });
    }
    let tn = idx.len();
    let seasons: Vec<i32> = { let mut s: Vec<i32> = games.iter().map(|g| g.season).collect(); s.sort(); s.dedup(); s };
    let default_from = *seasons.get(seasons.len().saturating_sub(5)).unwrap_or(&0);
    let default_to = *seasons.last().unwrap_or(&0);
    let (from, to) = (q.from.unwrap_or(default_from), q.to.unwrap_or(default_to));

    let (mut n, mut su, mut brier) = (0u32, 0u32, 0.0f64);
    let (mut ats_w, mut ats_l, mut ev_w, mut ev_l, mut tot_w, mut tot_l) = (0u32, 0u32, 0u32, 0u32, 0u32, 0u32);
    let mut cal = vec![(0.0f64, 0u32, 0u32); 10];
    let mut per_season: Vec<Value> = Vec::new();
    const IMPLIED_110: f64 = 0.5238;
    let mut have_lines = false;

    for s in from..=to {
        let train: Vec<Obs> = {
            let max_prior = games.iter().filter(|g| g.season < s).map(|g| g.season).max();
            let Some(mx) = max_prior else { continue };
            games.iter().filter(|g| g.season < s).flat_map(|g| {
                let w = 0.5f64.powf((mx - g.season) as f64 / q.halflife_seasons);
                [Obs { y: g.hs, team: g.home, opp: g.away, home: 1.0, w }, Obs { y: g.as_, team: g.away, opp: g.home, home: 0.0, w }]
            }).collect()
        };
        let test: Vec<&Game> = games.iter().filter(|g| g.season == s).collect();
        if train.len() < 200 || test.is_empty() { continue; }
        let f = fit(&train, tn, q.epochs, q.ridge);
        let (mut sn, mut ssu, mut saw, mut sal) = (0u32, 0u32, 0u32, 0u32);
        for g in test {
            let mu_h = (f.intercept + f.off[g.home] - f.def[g.away] + f.home_adv).clamp(-2.0, 6.0).exp();
            let mu_a = (f.intercept + f.off[g.away] - f.def[g.home]).clamp(-2.0, 6.0).exp();
            let var = |mu: f64| mu + mu * mu / f.dispersion;
            let msd = (var(mu_h) + var(mu_a)).sqrt().max(1e-6);
            let mmean = mu_h - mu_a;
            let p_home = normal_cdf(mmean / msd);
            let actual_margin = g.hs - g.as_;
            let home_won = if actual_margin > 0.0 { 1.0 } else { 0.0 };
            n += 1; sn += 1;
            if (p_home >= 0.5) as i32 as f64 == home_won { su += 1; ssu += 1; }
            brier += (p_home - home_won).powi(2);
            cal[((p_home * 10.0) as usize).min(9)].0 += p_home;
            cal[((p_home * 10.0) as usize).min(9)].1 += 1;
            cal[((p_home * 10.0) as usize).min(9)].2 += home_won as u32;
            if let Some(line) = g.spread {
                have_lines = true;
                if (actual_margin - line).abs() > 1e-9 {
                    let cover_home = actual_margin > line;
                    let p_cover = normal_cdf((mmean - line) / msd);
                    let pick_home = p_cover >= 0.5;
                    let correct = pick_home == cover_home;
                    if correct { ats_w += 1; saw += 1; } else { ats_l += 1; sal += 1; }
                    let edge_p = if pick_home { p_cover } else { 1.0 - p_cover };
                    if edge_p > IMPLIED_110 { if correct { ev_w += 1; } else { ev_l += 1; } }
                }
            }
            if let Some(tl) = g.total {
                let at = g.hs + g.as_;
                if (at - tl).abs() > 1e-9 {
                    let p_over = normal_cdf((mu_h + mu_a - tl) / msd);
                    if (p_over >= 0.5) == (at > tl) { tot_w += 1; } else { tot_l += 1; }
                }
            }
        }
        let at = saw + sal;
        per_season.push(json!({ "season": s, "games": sn, "train_games": train.len() / 2, "su_acc": r3(ssu as f64 / sn.max(1) as f64),
            "ats_record": if at > 0 { format!("{saw}-{sal}") } else { "—".into() }, "ats_pct": if at > 0 { r3(saw as f64 / at as f64) } else { 0.0 } }));
    }
    if n == 0 { return HttpResponse::InternalServerError().json(json!({ "error": "no testable games in range" })); }
    let calib: Vec<Value> = cal.iter().enumerate().filter(|(_, c)| c.1 > 0).map(|(i, c)| json!({
        "bucket": format!("{}0-{}0%", i, i + 1), "n": c.1, "predicted": r3(c.0 / c.1 as f64), "actual": r3(c.2 as f64 / c.1 as f64) })).collect();
    let (ats_tot, ev_tot, tot_tot) = (ats_w + ats_l, ev_w + ev_l, tot_w + tot_l);
    HttpResponse::Ok().json(json!({
        "model": format!("{sport}-team-points"), "method": "walk-forward (train on prior seasons only)",
        "test_seasons": format!("{from}-{to}"), "games": n,
        "straight_up_accuracy": r3(su as f64 / n as f64), "brier_score": r3(brier / n as f64),
        "has_market_lines": have_lines,
        "ats_vs_closing_line": { "record": if ats_tot > 0 { format!("{ats_w}-{ats_l}") } else { "n/a".into() }, "pct": if ats_tot > 0 { r3(ats_w as f64 / ats_tot as f64) } else { 0.0 } },
        "plus_ev_subset": { "record": if ev_tot > 0 { format!("{ev_w}-{ev_l}") } else { "n/a".into() }, "pct": if ev_tot > 0 { r3(ev_w as f64 / ev_tot as f64) } else { 0.0 }, "note": "model edge > -110 breakeven; >52.4% beats the vig" },
        "totals_vs_closing_line": { "record": if tot_tot > 0 { format!("{tot_w}-{tot_l}") } else { "n/a".into() }, "pct": if tot_tot > 0 { r3(tot_w as f64 / tot_tot as f64) } else { 0.0 } },
        "calibration": calib, "by_season": per_season,
    }))
}
