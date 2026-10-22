//! NBA player impact: game-level adjusted plus-minus.
//!
//! There is no play-by-play or plus-minus in the warehouse, so stints are not
//! available; who played and for how long is. Each game is one observation:
//!
//!   home margin = home court + sum_i x_i * rating_i,
//!   x_i = 5 x player i's share of his team's minutes (home +, away -)
//!
//! Over the three seasons before the fit date, each game weighted by
//! 0.5^(age in years), with home court unpenalised:
//!   pass 1: ridge toward 0 (LAMBDA)
//!   prior : pass-1 ratings regressed on per-36 box lines and log minutes
//!           (minutes weighted), bounded to [-5, 7]; barely-seen players <= 0
//!   pass 2: ridge toward the prior (LAMBDA_PRIOR)
//! giving each player's margin per full game on the floor.
//!
//! Chosen on 2,295 games of 2022-23 and 2025-26 with closing spreads, by how
//! well the lineup gap (who plays vs the team's last ten games) explains the
//! line: R^2 0.057 for plain ridge, 0.088 with the box prior. The result beyond
//! the line stays at zero (+0.04 +/- 0.19 per point): the market already prices
//! lineups. The simulator, which only redistributes box-score rates, did not.
//!
//! Fits are taken as of the first of each month (games strictly before it), so
//! a walk-forward backtest of any game uses only earlier games.
//!
//!   POST /fit/nba/impact?from=YYYY-MM-DD&to=YYYY-MM-DD   monthly fits (missing ones)
//!   GET  /nba/impact?date=YYYY-MM-DD&team=ABBR           ratings in force on a date

use std::collections::HashMap;

use actix_web::{web, HttpResponse, Responder};
use chrono::{Datelike, NaiveDate};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_postgres::types::Json;

use crate::State;

pub const LAMBDA: f64 = 10.0;
/// Second pass: shrink toward the box-score prior instead of 0.
pub const LAMBDA_PRIOR: f64 = 30.0;
const YEARS: i64 = 3;
/// Stored with each fit; a fit made by another method is refitted.
const METHOD: &str = "apm-box-prior-v2";

fn dberr(e: &tokio_postgres::Error) -> String {
    e.as_db_error().map(|d| d.message().to_string()).unwrap_or_else(|| e.to_string())
}
fn key(asof: NaiveDate) -> String { format!("nba:impact:{asof}") }

/// Solve A x = b in place for symmetric positive-definite A (n x n, row-major).
fn cholesky_solve(a: &mut [f64], b: &mut [f64], n: usize) -> Result<(), String> {
    for j in 0..n {
        let mut d = a[j * n + j];
        for k in 0..j { d -= a[j * n + k] * a[j * n + k]; }
        if d <= 0.0 { return Err("ridge system is not positive definite".into()); }
        let d = d.sqrt();
        a[j * n + j] = d;
        for i in (j + 1)..n {
            let mut s = a[i * n + j];
            for k in 0..j { s -= a[i * n + k] * a[j * n + k]; }
            a[i * n + j] = s / d;
        }
    }
    for i in 0..n { let mut s = b[i]; for k in 0..i { s -= a[i * n + k] * b[k]; } b[i] = s / a[i * n + i]; }
    for i in (0..n).rev() { let mut s = b[i]; for k in (i + 1)..n { s -= a[k * n + i] * b[k]; } b[i] = s / a[i * n + i]; }
    Ok(())
}

/// One game: home margin and each player's signed x.
pub(crate) struct Obs { pub y: f64, pub w: f64, pub x: Vec<(usize, f64)> }

/// Weighted normal equations X'WX, X'Wy (last column: home court).
fn normal(obs: &[Obs], n: usize) -> (Vec<f64>, Vec<f64>) {
    let m = n + 1;
    let mut a = vec![0.0; m * m];
    let mut b = vec![0.0; m];
    for o in obs {
        let mut x = o.x.clone();
        x.push((n, 1.0));
        for &(i, xi) in &x {
            b[i] += o.w * xi * o.y;
            for &(j, xj) in &x { a[i * m + j] += o.w * xi * xj; }
        }
    }
    (a, b)
}

/// Ridge solve toward `prior` (zeros when None): (ratings, home court).
fn solve(a: &[f64], b: &[f64], n: usize, lambda: f64, prior: Option<&[f64]>) -> Result<(Vec<f64>, f64), String> {
    let m = n + 1;
    let (mut a, mut b) = (a.to_vec(), b.to_vec());
    for i in 0..n {
        a[i * m + i] += lambda;
        if let Some(p) = prior { b[i] += lambda * p[i]; }
    }
    a[n * m + n] += 1e-6;
    cholesky_solve(&mut a, &mut b, m)?;
    let hca = b[n];
    b.truncate(n);
    Ok((b, hca))
}

/// Ridge fit toward 0: (ratings, home court).
#[allow(dead_code)]
pub(crate) fn ridge(obs: &[Obs], n: usize, lambda: f64) -> Result<(Vec<f64>, f64), String> {
    let (a, b) = normal(obs, n);
    solve(&a, &b, n, lambda, None)
}

/// Box-score prior: the pass-1 ratings regressed on each player's per-36 box
/// line and log minutes (weighted by minutes); barely-seen players get no credit.
fn box_prior(pass1: &[f64], minutes: &[f64], stats: &[[f64; NSTAT]]) -> Result<Vec<f64>, String> {
    let k = NSTAT + 2;
    let feat = |i: usize| -> Vec<f64> {
        let mut f = vec![1.0];
        f.extend(stats[i].iter().map(|s| s / minutes[i].max(1.0) * 36.0));
        f.push((1.0 + minutes[i]).ln());
        f
    };
    let (mut a, mut b) = (vec![0.0; k * k], vec![0.0; k]);
    for i in 0..pass1.len() {
        let f = feat(i);
        for r in 0..k { b[r] += minutes[i] * f[r] * pass1[i]; for c in 0..k { a[r * k + c] += minutes[i] * f[r] * f[c]; } }
    }
    for r in 0..k { a[r * k + r] += 1e-6; }
    cholesky_solve(&mut a, &mut b, k)?;
    Ok((0..pass1.len()).map(|i| {
        let p: f64 = feat(i).iter().zip(&b).map(|(x, c)| x * c).sum();
        // per-36 rates from a few garbage-time minutes are extreme: bound the prior
        let p = p.clamp(-5.0, 7.0);
        if minutes[i] < 50.0 { p.min(0.0) } else { p }
    }).collect())
}

/// Per-player box totals used by the prior: pts reb ast stl blk tov fga fta fg3m.
const NSTAT: usize = 9;

/// Two-pass fit: ridge toward 0, then toward the box prior. (ratings, prior, home court)
pub(crate) fn fit_with_prior(obs: &[Obs], n: usize, minutes: &[f64], stats: &[[f64; NSTAT]]) -> Result<(Vec<f64>, Vec<f64>, f64), String> {
    let (a, b) = normal(obs, n);
    let (pass1, _) = solve(&a, &b, n, LAMBDA, None)?;
    let prior = box_prior(&pass1, minutes, stats)?;
    let (beta, hca) = solve(&a, &b, n, LAMBDA_PRIOR, Some(&prior))?;
    Ok((beta, prior, hca))
}

/// Fit the ratings as of a date (games strictly before it) and store them.
pub(crate) async fn fit(db: &tokio_postgres::Client, asof: NaiveDate) -> Result<Value, String> {
    let from = asof - chrono::Duration::days(365 * YEARS);
    let rows = db.query("SELECT g.espn_id, g.game_date, g.home, g.home_score - g.away_score, b.team, b.player_id, b.min, \
          COALESCE(b.pts,0), COALESCE(b.oreb,0)+COALESCE(b.dreb,0), COALESCE(b.ast,0), COALESCE(b.stl,0), COALESCE(b.blk,0), COALESCE(b.tov,0), \
          COALESCE(b.fga,0), COALESCE(b.fta,0), COALESCE(b.fg3m,0) \
        FROM nba_games_espn g JOIN nba_box_espn b USING (espn_id) \
        WHERE g.completed AND g.season_type IN (2,3) AND g.game_date >= $1 AND g.game_date < $2 AND NOT b.dnp AND COALESCE(b.min,0) > 0 \
        ORDER BY g.espn_id", &[&from, &asof]).await.map_err(|e| dberr(&e))?;
    let mut idx: HashMap<String, usize> = HashMap::new();
    let mut games: Vec<(NaiveDate, String, f64, Vec<(String, usize, f64)>)> = Vec::new(); // date, home, margin, (team, player, min)
    let (mut minutes, mut stats): (Vec<f64>, Vec<[f64; NSTAT]>) = (Vec::new(), Vec::new()); // recency-weighted totals per player
    let mut cur = String::new();
    for r in &rows {
        let gid: String = r.get(0);
        if gid != cur {
            cur = gid;
            games.push((r.get(1), r.get(2), r.get::<_, Option<i32>>(3).unwrap_or(0) as f64, Vec::new()));
        }
        let pid: String = r.get(5);
        let n = idx.len();
        let i = *idx.entry(pid).or_insert(n);
        if i == minutes.len() { minutes.push(0.0); stats.push([0.0; NSTAT]); }
        let w = 0.5f64.powf((asof - r.get::<_, NaiveDate>(1)).num_days() as f64 / 365.0);
        let mn = r.get::<_, Option<f64>>(6).unwrap_or(0.0);
        minutes[i] += w * mn;
        for k in 0..NSTAT { stats[i][k] += w * r.get::<_, i32>(7 + k) as f64; }
        games.last_mut().unwrap().3.push((r.get(4), i, mn));
    }
    let obs: Vec<Obs> = games.iter().filter_map(|(d, home, y, ps)| {
        let tot = |t: bool| ps.iter().filter(|p| (p.0 == *home) == t).map(|p| p.2).sum::<f64>();
        let (th, ta) = (tot(true), tot(false));
        if th <= 0.0 || ta <= 0.0 { return None; }
        let x = ps.iter().map(|(t, i, m)| (*i, if t == home { 5.0 * m / th } else { -5.0 * m / ta })).collect();
        Some(Obs { y: *y, w: 0.5f64.powf((asof - *d).num_days() as f64 / 365.0), x })
    }).collect();
    if obs.len() < 300 { return Err(format!("only {} games before {asof}", obs.len())); }
    let n = idx.len();
    let (beta, _prior, hca) = web::block(move || fit_with_prior(&obs, n, &minutes, &stats)).await.map_err(|e| e.to_string())??;
    let ratings: serde_json::Map<String, Value> = idx.iter().map(|(p, &i)| (p.clone(), json!((beta[i] * 1000.0).round() / 1000.0))).collect();
    let v = json!({ "asof": asof, "method": METHOD, "lambda": LAMBDA, "lambda_prior": LAMBDA_PRIOR, "years": YEARS, "games": games.len(), "players": n,
        "home_court": (hca * 1000.0).round() / 1000.0, "ratings": ratings });
    db.execute("INSERT INTO sim_model_params (key, params, source) VALUES ($1, $2, $3) ON CONFLICT (key) DO UPDATE SET params = EXCLUDED.params, source = EXCLUDED.source, fitted_at = now()",
        &[&key(asof), &Json(&v), &format!("adjusted plus-minus with a box-score prior, {} games before {asof}", games.len())]).await.map_err(|e| dberr(&e))?;
    Ok(json!({ "asof": asof, "games": games.len(), "players": n, "home_court": v["home_court"] }))
}

/// The ratings in force on a date (the latest fit on or before it); empty when none.
pub(crate) async fn load(db: &tokio_postgres::Client, date: NaiveDate) -> Result<(HashMap<String, f64>, Option<NaiveDate>), String> {
    let Some(r) = db.query_opt("SELECT params FROM sim_model_params WHERE key LIKE 'nba:impact:%' AND key <= $1 ORDER BY key DESC LIMIT 1", &[&key(date)]).await.map_err(|e| dberr(&e))? else {
        return Ok((HashMap::new(), None));
    };
    let v: Json<Value> = r.get(0);
    let asof = v.0["asof"].as_str().and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok());
    Ok((v.0["ratings"].as_object().map(|o| o.iter().filter_map(|(k, x)| x.as_f64().map(|f| (k.clone(), f))).collect()).unwrap_or_default(), asof))
}

/// First of each month from `from` to `to`, in season (October to June).
fn months(from: NaiveDate, to: NaiveDate) -> Vec<NaiveDate> {
    let mut d = NaiveDate::from_ymd_opt(from.year(), from.month(), 1).unwrap_or(from);
    let mut out = Vec::new();
    while d <= to {
        if !(7..=9).contains(&d.month()) { out.push(d); }
        d = if d.month() == 12 { NaiveDate::from_ymd_opt(d.year() + 1, 1, 1) } else { NaiveDate::from_ymd_opt(d.year(), d.month() + 1, 1) }.unwrap_or(to + chrono::Duration::days(1));
    }
    out
}

/// Fit every missing month in the range; returns what was fitted.
pub(crate) async fn ensure(db: &tokio_postgres::Client, from: NaiveDate, to: NaiveDate) -> Result<Vec<Value>, String> {
    let have: std::collections::HashSet<String> = db.query("SELECT key FROM sim_model_params WHERE key LIKE 'nba:impact:%' AND params->>'method' = $1", &[&METHOD]).await.map_err(|e| dberr(&e))?
        .iter().map(|r| r.get(0)).collect();
    let mut out = Vec::new();
    for m in months(from, to) {
        if have.contains(&key(m)) { continue; }
        out.push(fit(db, m).await.unwrap_or_else(|e| json!({ "asof": m, "error": e })));
    }
    Ok(out)
}

#[derive(Deserialize)]
pub struct FitQ { from: Option<String>, to: Option<String> }

pub async fn fit_http(state: web::Data<State>, q: web::Query<FitQ>) -> impl Responder {
    let today = (chrono::Utc::now() - chrono::Duration::hours(5)).date_naive();
    let p = |s: &Option<String>| s.as_deref().and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok());
    let (from, to) = (p(&q.from).unwrap_or(today), p(&q.to).unwrap_or(today));
    match ensure(&state.db, from, to).await {
        Ok(v) => HttpResponse::Ok().json(json!({ "fitted": v })),
        Err(e) => HttpResponse::InternalServerError().json(json!({ "error": e })),
    }
}

#[derive(Deserialize)]
pub struct ShowQ { date: Option<String>, team: Option<String> }

pub async fn show(state: web::Data<State>, q: web::Query<ShowQ>) -> impl Responder {
    let today = (chrono::Utc::now() - chrono::Duration::hours(5)).date_naive();
    let date = q.date.as_deref().and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok()).unwrap_or(today);
    let (ratings, asof) = match load(&state.db, date).await { Ok(v) => v, Err(e) => return HttpResponse::InternalServerError().json(json!({ "error": e })) };
    // name + latest team for display
    let ids: Vec<String> = ratings.keys().cloned().collect();
    let who: HashMap<String, (String, String)> = state.db.query("SELECT DISTINCT ON (b.player_id) b.player_id, b.name, b.team FROM nba_box_espn b JOIN nba_games_espn g USING (espn_id) \
        WHERE b.player_id = ANY($1) AND g.game_date < $2 ORDER BY b.player_id, g.game_date DESC", &[&ids, &date]).await
        .map(|rows| rows.iter().map(|r| (r.get(0), (r.get(1), r.get(2)))).collect()).unwrap_or_default();
    let mut list: Vec<Value> = ratings.iter().filter_map(|(p, r)| {
        let (name, team) = who.get(p)?;
        if q.team.as_deref().map(|t| t != team).unwrap_or(false) { return None; }
        Some(json!({ "player_id": p, "name": name, "team": team, "rating": r }))
    }).collect();
    list.sort_by(|a, b| b["rating"].as_f64().unwrap_or(0.0).total_cmp(&a["rating"].as_f64().unwrap_or(0.0)));
    HttpResponse::Ok().json(json!({ "date": date, "asof": asof, "lambda": LAMBDA, "unit": "points of margin per full game on the floor", "players": list }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ridge_recovers_a_star() {
        // player 0 (+6) plays every home game; 1..4 are average; each game the
        // home side is 0 + 1..4 vs 5..9 (all zero), margin = 3 home court + 6 * share
        let mut obs = Vec::new();
        for g in 0..400 {
            let star = g % 2 == 0;
            let mut x: Vec<(usize, f64)> = (1..5).map(|i| (i, 1.0)).collect();
            if star { x.push((0, 1.0)); } else { x.push((10, 1.0)); }
            for j in 5..10 { x.push((j, -1.0)); }
            obs.push(Obs { y: 3.0 + if star { 6.0 } else { 0.0 }, w: 1.0, x });
        }
        let (b, hca) = ridge(&obs, 11, 1.0).unwrap();
        assert!((b[0] - b[10] - 6.0).abs() < 0.2, "star minus replacement {}", b[0] - b[10]);
        // the replacement games: home court + the regulars + the replacement = 3
        assert!((hca + b[1..5].iter().sum::<f64>() + b[10] - b[5..10].iter().sum::<f64>() - 3.0).abs() < 0.3);
    }

    #[test]
    fn month_starts_skip_the_offseason() {
        let m = months(NaiveDate::from_ymd_opt(2025, 5, 10).unwrap(), NaiveDate::from_ymd_opt(2025, 11, 2).unwrap());
        assert_eq!(m.iter().map(|d| d.month()).collect::<Vec<_>>(), vec![5, 6, 10, 11]);
    }
}
