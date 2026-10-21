//! Team-points engine for the learning loop (registry keys `nba_team`,
//! `nhl_team`, `nfl_team`).
//!
//! The log-linear Poisson team model of the Projections page (attack, defence,
//! home edge; Negative-Binomial points, dispersion from residuals), but refit
//! for every game from the same game table the sport's simulator uses, on games
//! strictly before the game date with a per-game recency weight. The page's
//! ratings are fit once per season from `sports_games`; this engine is
//! walk-forward by construction, so its backtests and live record are honest.
//!
//! It forecasts the sport simulator's game markets with the same lines and
//! baselines, so the two engines are scored on identical questions. It has no
//! players, lineups or pace: props are the simulators' job.

use std::collections::HashMap;

use chrono::{DateTime, NaiveDate, Utc};
use serde_json::{json, Value};

use crate::learning::{tuning, Forecast, GameEval, Outcomes, Prediction};
use crate::mlb_sim::Rng;
use crate::team_model::{fit, Obs};
use crate::State;

pub fn engine(sport: &str) -> &'static str {
    match sport { "nba" => "nba-team-points-v1", "nhl" => "nhl-team-points-v1", "nfl" => "nfl-team-points-v1", _ => "unknown" }
}

tuning!(Tuning {
    halflife_days: 120.0, 15.0, 1500.0, "recency half-life of past games, days";
    ridge: 2.0, 0.05, 50.0, "ridge prior pulling team ratings toward the league";
    lookback_days: 730.0, 120.0, 2200.0, "games older than this are ignored";
});

/// Sport-specific defaults (NFL teams play 17 games a year, so they need a longer memory).
fn defaults(sport: &str) -> Tuning {
    match sport {
        "nfl" => Tuning { halflife_days: 365.0, ridge: 2.0, lookback_days: 1100.0 },
        "nhl" => Tuning { halflife_days: 150.0, ridge: 2.0, lookback_days: 730.0 },
        _ => Tuning::default(),
    }
}

pub fn tuning_for(sport: &str, cfg: &Value) -> Tuning {
    let d = defaults(sport);
    let g = |k: &str, v: f64| cfg.get(k).and_then(|x| x.as_f64()).unwrap_or(v);
    Tuning { halflife_days: g("halflife_days", d.halflife_days), ridge: g("ridge", d.ridge), lookback_days: g("lookback_days", d.lookback_days) }
}

pub fn tunables(sport: &str) -> Value {
    let mut v = Tuning::default().describe();
    let d = defaults(sport);
    for (k, x) in [("halflife_days", d.halflife_days), ("ridge", d.ridge), ("lookback_days", d.lookback_days)] { v[k]["default"] = json!(x); }
    v
}

fn dberr(e: &tokio_postgres::Error) -> String {
    match e.as_db_error() { Some(d) => format!("{}: {}", d.code().code(), d.message()), None => e.to_string() }
}

fn erf(x: f64) -> f64 {
    let t = 1.0 / (1.0 + 0.3275911 * x.abs());
    let y = 1.0 - (((((1.061405429 * t - 1.453152027) * t) + 1.421413741) * t - 0.284496736) * t + 0.254829592) * t * (-x * x).exp();
    if x >= 0.0 { y } else { -y }
}

struct Target { date: NaiveDate, start: Option<DateTime<Utc>>, home: String, away: String, spread: Option<f64>, total: Option<f64> }
struct Past { date: NaiveDate, home: String, away: String, hs: f64, as_: f64 }

async fn target(db: &tokio_postgres::Client, sport: &str, id: &str) -> Result<Target, String> {
    let r = match sport {
        "nba" => db.query_opt("SELECT game_date, start_time, home, away, spread, over_under FROM nba_games_espn WHERE espn_id = $1", &[&id]).await,
        "nhl" => {
            let gid: i64 = id.parse().map_err(|_| "NHL game id must be numeric")?;
            db.query_opt("SELECT game_date, start_time, home, away, NULL::float8, NULL::float8 FROM nhl_games_api WHERE game_id = $1", &[&gid]).await
        }
        "nfl" => db.query_opt("SELECT game_date, NULL::timestamptz, home_team, away_team, spread_line, total_line FROM sports_games WHERE sport = 'nfl' AND game_id = $1", &[&id]).await,
        _ => return Err(format!("no team engine for {sport}")),
    }.map_err(|e| dberr(&e))?.ok_or_else(|| format!("unknown {sport} game {id}"))?;
    Ok(Target { date: r.get(0), start: r.get(1), home: r.get(2), away: r.get(3), spread: r.get(4), total: r.get(5) })
}

async fn past(db: &tokio_postgres::Client, sport: &str, before: NaiveDate, lookback: i32) -> Result<Vec<Past>, String> {
    let rows = match sport {
        "nba" => db.query("SELECT game_date, home, away, home_score, away_score FROM nba_games_espn \
                           WHERE completed AND season_type IN (2,3) AND home_score IS NOT NULL AND game_date < $1 AND game_date >= $1 - $2::int", &[&before, &lookback]).await,
        "nhl" => db.query("SELECT game_date, home, away, home_score, away_score FROM nhl_games_api \
                           WHERE completed AND game_type IN (2,3) AND home_score IS NOT NULL AND game_date < $1 AND game_date >= $1 - $2::int", &[&before, &lookback]).await,
        _ => db.query("SELECT game_date, home_team, away_team, home_score, away_score FROM sports_games \
                       WHERE sport = 'nfl' AND home_score IS NOT NULL AND away_score IS NOT NULL AND game_date < $1 AND game_date >= $1 - $2::int", &[&before, &lookback]).await,
    }.map_err(|e| dberr(&e))?;
    Ok(rows.iter().map(|r| Past { date: r.get(0), home: r.get(1), away: r.get(2), hs: r.get::<_, i32>(3) as f64, as_: r.get::<_, i32>(4) as f64 }).collect())
}

/// Expected points for (home, away) and the dispersion, from games before the target date.
struct Means { mu_home: f64, mu_away: f64, dispersion: f64, games: usize, teams: usize }

fn fit_means(games: &[Past], t: &Target, tune: &Tuning) -> Result<Means, String> {
    let mut idx: HashMap<&str, usize> = HashMap::new();
    for g in games { for team in [g.home.as_str(), g.away.as_str()] { let n = idx.len(); idx.entry(team).or_insert(n); } }
    let (Some(&h), Some(&a)) = (idx.get(t.home.as_str()), idx.get(t.away.as_str())) else {
        return Err(format!("no games before {} for {} or {}", t.date, t.home, t.away));
    };
    let mut obs = Vec::with_capacity(games.len() * 2);
    for g in games {
        let age = (t.date - g.date).num_days() as f64;
        let w = 0.5f64.powf(age / tune.halflife_days);
        let (gh, ga) = (idx[g.home.as_str()], idx[g.away.as_str()]);
        obs.push(Obs { y: g.hs, team: gh, opp: ga, home: 1.0, w });
        obs.push(Obs { y: g.as_, team: ga, opp: gh, home: 0.0, w });
    }
    let f = fit(&obs, idx.len(), 40, tune.ridge);
    Ok(Means {
        mu_home: (f.intercept + f.off[h] - f.def[a] + f.home_adv).clamp(-2.0, 6.0).exp(),
        mu_away: (f.intercept + f.off[a] - f.def[h]).clamp(-2.0, 6.0).exp(),
        dispersion: f.dispersion, games: games.len(), teams: idx.len(),
    })
}

fn poisson(rng: &mut Rng, lambda: f64) -> f64 {
    if lambda <= 0.0 { return 0.0; }
    if lambda > 30.0 { return (lambda + lambda.sqrt() * rng.normal()).round().max(0.0); }
    let l = (-lambda).exp();
    let (mut k, mut p) = (0.0f64, 1.0f64);
    loop { k += 1.0; p *= rng.f64().max(1e-300); if p <= l { return k - 1.0; } }
}

fn negbin(rng: &mut Rng, mean: f64, r: f64) -> f64 {
    if r >= 1e5 { return poisson(rng, mean); }
    let lambda = rng.gamma(r) * mean / r;
    poisson(rng, lambda)
}

/// Simulated (home margin, total) pairs. Ties follow the sport: NBA/NHL games
/// always have a winner (overtime / shootout adds to the winner, chosen by the
/// scoring means); an NFL tie goes to overtime, won by a field goal 90% of the time.
fn simulate(sport: &str, m: &Means, n: usize, seed: u64) -> Vec<(f64, f64)> {
    let mut rng = Rng::new(seed);
    let p_home = m.mu_home / (m.mu_home + m.mu_away);
    (0..n).map(|_| {
        let (mut h, mut a) = (negbin(&mut rng, m.mu_home, m.dispersion), negbin(&mut rng, m.mu_away, m.dispersion));
        if (h - a).abs() < 0.5 {
            let home = rng.f64() < p_home;
            let bump = match sport { "nfl" => if rng.f64() < 0.9 { 3.0 } else { 0.0 }, _ => 1.0 };
            if home { h += bump } else { a += bump }
        }
        (h - a, h + a)
    }).collect()
}

/// P(x > line) among non-push outcomes.
fn over(xs: &[(f64, f64)], f: impl Fn(&(f64, f64)) -> f64, line: f64) -> f64 {
    let (mut o, mut u) = (0.0f64, 0.0f64);
    for x in xs { let v = f(x); if v > line + 1e-9 { o += 1.0 } else if v < line - 1e-9 { u += 1.0 } }
    o / (o + u).max(1e-9)
}

/// The sport simulator's game markets, same lines and baselines.
fn forecasts(sport: &str, t: &Target, sims: &[(f64, f64)]) -> Vec<Forecast> {
    let margin = |x: &(f64, f64)| x.0;
    let total = |x: &(f64, f64)| x.1;
    let n = sims.len().max(1) as f64;
    match sport {
        "nba" => {
            let mkt = t.spread.map(|s| 0.5 * (1.0 + erf(-s / (12.0 * std::f64::consts::SQRT_2))));
            let mut out = vec![Forecast::game("home_win", "market spread (normal, sd 12)", "home_margin", 0.0, over(sims, margin, 0.0), mkt.unwrap_or(0.58))];
            if let Some(s) = t.spread { out.push(Forecast::game("home_cover", "0.5 at the line", "home_margin", -s, over(sims, margin, -s), 0.5).pushes()); }
            if let Some(l) = t.total { out.push(Forecast::game("total_over", "0.5 at the line", "total", l, over(sims, total, l), 0.5).pushes()); }
            out
        }
        "nhl" => vec![
            Forecast::game("home_win", "league home-win rate 0.535", "home_margin", 0.0, over(sims, margin, 0.0), 0.535),
            Forecast::game("total_over_5_5", "league rate 0.53", "total", 5.5, over(sims, total, 5.5), 0.53),
        ],
        _ => {
            let ties = sims.iter().filter(|x| x.0.abs() < 0.5).count() as f64;
            let wins = sims.iter().filter(|x| x.0 > 0.5).count() as f64;
            let market_p = t.spread.map(|s| 0.5 * (1.0 + erf(s / (13.45 * std::f64::consts::SQRT_2))));
            let mut out = vec![Forecast::game("home_win", "closing spread (normal, sd 13.45)", "home_margin", 0.0, (wins + 0.5 * ties) / n, market_p.unwrap_or(0.55)).pushes()];
            if let Some(s) = t.spread { out.push(Forecast::game("home_cover", "0.5 (closing spread is the line)", "home_margin", s, over(sims, margin, s), 0.5).pushes()); }
            if let Some(l) = t.total { out.push(Forecast::game("total_over", "0.5 (closing total is the line)", "total", l, over(sims, total, l), 0.5).pushes()); }
            out
        }
    }
}

async fn run(state: &State, sport: &str, id: &str, cfg: &Value, n: usize, seed: u64) -> Result<(Target, Means, Vec<(f64, f64)>), String> {
    let tune = tuning_for(sport, cfg);
    let t = target(&state.db, sport, id).await?;
    let games = past(&state.db, sport, t.date, tune.lookback_days as i32).await?;
    if games.len() < 100 { return Err(format!("only {} completed games before {}", games.len(), t.date)); }
    let (sport_s, tune2) = (sport.to_string(), tune.clone());
    let (t, m, sims) = actix_web::web::block(move || {
        let m = fit_means(&games, &t, &tune2)?;
        let sims = simulate(&sport_s, &m, n, seed);
        Ok::<_, String>((t, m, sims))
    }).await.map_err(|e| e.to_string())??;
    Ok((t, m, sims))
}

/// Integer distribution of simulated values: {"pmf_offset": min, "pmf": [P(min), P(min+1), ...]}.
fn int_dist(xs: impl Iterator<Item = f64>) -> Value {
    let v: Vec<i64> = xs.map(|x| x.round() as i64).collect();
    let (Some(&lo), Some(&hi)) = (v.iter().min(), v.iter().max()) else { return Value::Null };
    let mut c = vec![0u64; (hi - lo + 1) as usize];
    for x in &v { c[(x - lo) as usize] += 1; }
    let n = v.len() as f64;
    json!({ "pmf_offset": lo, "pmf": c.iter().map(|k| (*k as f64 / n * 1e5).round() / 1e5).collect::<Vec<_>>() })
}

/// Production prediction for the ledger.
pub async fn predict(state: &State, sport: &str, id: &str, cfg: &Value, seed: u64, n: usize) -> Result<Prediction, String> {
    let (t, m, sims) = run(state, sport, id, cfg, n, seed).await?;
    let fc = forecasts(sport, &t, &sims);
    let nn = sims.len().max(1) as f64;
    let mean = |f: fn(&(f64, f64)) -> f64| sims.iter().map(f).sum::<f64>() / nn;
    let payload = json!({
        "engine": engine(sport), "game_id": id, "date": t.date, "away": t.away, "home": t.home, "seed": seed, "n": sims.len(),
        "proj_home": (m.mu_home * 100.0).round() / 100.0, "proj_away": (m.mu_away * 100.0).round() / 100.0, "dispersion": m.dispersion,
        "margin_home": mean(|x| x.0), "total": mean(|x| x.1),
        "dist": { "margin": int_dist(sims.iter().map(|x| x.0)), "total": int_dist(sims.iter().map(|x| x.1)) },
        "market": { "spread": t.spread, "total": t.total },
        "forecasts": fc.iter().map(|f| json!({ "market": f.market, "line": f.line, "p": f.p_model })).collect::<Vec<_>>(),
        "fit": { "games": m.games, "teams": m.teams, "tuning": tuning_for(sport, cfg).describe() },
        "note": "team-points model refit on games strictly before the game date; no players or lineups",
    });
    Ok(Prediction { payload, forecasts: fc, date: t.date, start: t.start, engine: engine(sport), n: sims.len() })
}

/// Walk-forward evaluation of one completed game.
pub async fn evaluate(state: &State, sport: &str, id: &str, cfg: &Value, n: usize, seed: u64, outcomes: Option<Outcomes>) -> Result<Option<GameEval>, String> {
    let Some(o) = outcomes else { return Ok(None) };
    let (t, _m, sims) = match run(state, sport, id, cfg, n, seed).await { Ok(x) => x, Err(_) => return Ok(None) };
    let fc = forecasts(sport, &t, &sims);
    let nn = sims.len().max(1) as f64;
    let sim_total = sims.iter().map(|x| x.1).sum::<f64>() / nn;
    let diag = o.game.get("total").map(|&a| vec![("total", sim_total, a)]).unwrap_or_default();
    Ok(Some(GameEval { forecasts: fc, outcomes: o, diag }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(d: u32, h: &str, a: &str, hs: f64, as_: f64) -> Past {
        Past { date: NaiveDate::from_ymd_opt(2025, 1, 1).unwrap() + chrono::Duration::days(d as i64), home: h.into(), away: a.into(), hs, as_ }
    }

    #[test]
    fn stronger_team_is_favoured_and_markets_are_consistent() {
        // A beats everyone by ~10, B and C are even
        let mut games = Vec::new();
        for d in 0..60 {
            games.push(g(d, "A", "B", 115.0, 105.0));
            games.push(g(d, "C", "A", 104.0, 114.0));
            games.push(g(d, "B", "C", 110.0, 109.0));
        }
        let t = Target { date: NaiveDate::from_ymd_opt(2025, 4, 1).unwrap(), start: None, home: "A".into(), away: "C".into(), spread: Some(-8.5), total: Some(219.5) };
        let m = fit_means(&games, &t, &Tuning::default()).unwrap();
        assert!(m.mu_home > m.mu_away + 5.0, "{} vs {}", m.mu_home, m.mu_away);
        let sims = simulate("nba", &m, 20_000, 7);
        let fc = forecasts("nba", &t, &sims);
        let p = |k: &str| fc.iter().find(|f| f.market == k).unwrap().p_model;
        assert!(p("home_win") > 0.7);
        // covering -8.5 is harder than winning
        assert!(p("home_cover") < p("home_win"));
        assert!(sims.iter().all(|x| x.0.abs() > 0.5), "NBA games always have a winner");
    }

    #[test]
    fn unknown_team_is_an_error() {
        let games = vec![g(0, "A", "B", 3.0, 2.0)];
        let t = Target { date: NaiveDate::from_ymd_opt(2025, 4, 1).unwrap(), start: None, home: "A".into(), away: "Z".into(), spread: None, total: None };
        assert!(fit_means(&games, &t, &Tuning::default()).is_err());
    }
}
