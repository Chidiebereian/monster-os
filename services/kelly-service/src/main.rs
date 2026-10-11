//! kelly-service
//! Staking and bet-math calculators over American odds.
//!
//! GET /kelly?probability=0.55&odds=120[&fraction=0.5]
//!     Kelly stake as a fraction of bankroll (0 if there is no edge).
//! GET /ev?probability=0.55&odds=120&stake=100
//!     Expected value and ROI of a single bet.
//! GET /hedge?odds_a=150&stake_a=100&odds_b=-120
//!     Stake on the other side to lock in an equal return either way.
//! GET /parlay?odds=-110,-110,120
//!     Combined decimal/American odds and implied probability of a parlay.

use actix_web::{web, App, HttpServer, HttpResponse, Responder};
use common::{american_to_decimal, decimal_to_american, healthz};
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
struct KellyQuery {
    probability: f64,
    odds: i64,
    fraction: Option<f64>,
}

#[derive(Deserialize)]
struct EvQuery {
    probability: f64,
    odds: i64,
    stake: f64,
}

#[derive(Deserialize)]
struct HedgeQuery {
    odds_a: i64,
    stake_a: f64,
    odds_b: i64,
}

#[derive(Deserialize)]
struct ParlayQuery {
    odds: String,
}

fn bad_request(msg: &str) -> HttpResponse {
    HttpResponse::BadRequest().json(json!({ "error": msg }))
}

/// Kelly fraction of bankroll for a bet at decimal odds `d` with win prob `p`.
/// f* = p - (1 - p) / (d - 1); negative (no edge) clamps to 0.
async fn kelly(q: web::Query<KellyQuery>) -> impl Responder {
    let p = q.probability;
    if !(p > 0.0 && p < 1.0) {
        return bad_request("probability must be in (0, 1)");
    }
    let Some(decimal) = american_to_decimal(q.odds) else {
        return bad_request("American odds must be <= -100 or >= 100");
    };
    let fraction = q.fraction.unwrap_or(1.0);
    if !(fraction > 0.0 && fraction <= 1.0) {
        return bad_request("fraction must be in (0, 1]");
    }
    let b = decimal - 1.0; // net odds received on a win
    let edge = p * b - (1.0 - p); // expected profit per unit staked
    let full_kelly = (edge / b).max(0.0);
    HttpResponse::Ok().json(json!({
        "probability": p,
        "odds": q.odds,
        "fraction": fraction,
        "edge_per_unit": edge,
        "full_kelly": full_kelly,
        "stake_fraction": full_kelly * fraction,
    }))
}

/// Expected value and ROI of staking `stake` at American `odds` with win prob `p`.
async fn ev(q: web::Query<EvQuery>) -> impl Responder {
    let p = q.probability;
    if !(p > 0.0 && p < 1.0) {
        return bad_request("probability must be in (0, 1)");
    }
    if q.stake <= 0.0 {
        return bad_request("stake must be positive");
    }
    let Some(decimal) = american_to_decimal(q.odds) else {
        return bad_request("American odds must be <= -100 or >= 100");
    };
    let profit_if_win = q.stake * (decimal - 1.0);
    let ev = p * profit_if_win - (1.0 - p) * q.stake;
    HttpResponse::Ok().json(json!({
        "probability": p,
        "odds": q.odds,
        "stake": q.stake,
        "profit_if_win": profit_if_win,
        "loss_if_lose": q.stake,
        "ev": ev,
        "roi": ev / q.stake,
    }))
}

/// Hedge an existing bet (stake_a at odds_a) by staking on the other side at
/// odds_b so the total return is equal whichever side wins.
async fn hedge(q: web::Query<HedgeQuery>) -> impl Responder {
    if q.stake_a <= 0.0 {
        return bad_request("stake_a must be positive");
    }
    let (Some(da), Some(db)) = (american_to_decimal(q.odds_a), american_to_decimal(q.odds_b)) else {
        return bad_request("both American odds must be <= -100 or >= 100");
    };
    let return_a = q.stake_a * da; // total return (incl. stake) if side A wins
    let hedge_stake = return_a / db; // makes side B's return equal to side A's
    let total_outlay = q.stake_a + hedge_stake;
    let guaranteed_return = return_a; // == hedge_stake * db
    HttpResponse::Ok().json(json!({
        "odds_a": q.odds_a,
        "stake_a": q.stake_a,
        "odds_b": q.odds_b,
        "hedge_stake": hedge_stake,
        "total_outlay": total_outlay,
        "guaranteed_return": guaranteed_return,
        "guaranteed_profit": guaranteed_return - total_outlay,
    }))
}

/// Combine several legs into one parlay: multiply the decimal odds.
async fn parlay(q: web::Query<ParlayQuery>) -> impl Responder {
    let mut legs = Vec::new();
    for tok in q.odds.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        match tok.parse::<i64>().ok().and_then(american_to_decimal) {
            Some(d) => legs.push((tok.parse::<i64>().unwrap(), d)),
            None => return bad_request(&format!("invalid American odds leg: {tok:?}")),
        }
    }
    if legs.len() < 2 {
        return bad_request("a parlay needs at least 2 legs, e.g. odds=-110,-110");
    }
    let combined_decimal: f64 = legs.iter().map(|(_, d)| d).product();
    HttpResponse::Ok().json(json!({
        "legs": legs.iter().map(|(o, _)| o).collect::<Vec<_>>(),
        "combined_decimal": combined_decimal,
        "combined_odds": decimal_to_american(combined_decimal),
        "combined_probability": 1.0 / combined_decimal,
    }))
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8111);
    println!("kelly-service listening on 0.0.0.0:{}", port);
    HttpServer::new(|| {
        App::new()
            .route("/healthz", web::get().to(healthz))
            .route("/kelly", web::get().to(kelly))
            .route("/ev", web::get().to(ev))
            .route("/hedge", web::get().to(hedge))
            .route("/parlay", web::get().to(parlay))
    })
    .bind(("0.0.0.0", port))?
    .run()
    .await
}

#[cfg(test)]
mod tests {
    use common::{american_to_decimal, decimal_to_american};

    fn full_kelly(p: f64, odds: i64) -> f64 {
        let b = american_to_decimal(odds).unwrap() - 1.0;
        ((p * b - (1.0 - p)) / b).max(0.0)
    }

    #[test]
    fn kelly_zero_without_edge() {
        // Even money, 50% win: no edge, so no stake.
        assert_eq!(full_kelly(0.5, 100), 0.0);
        // Negative edge also clamps to 0.
        assert_eq!(full_kelly(0.4, 100), 0.0);
    }

    #[test]
    fn kelly_matches_formula() {
        // p=0.55 at +100 (b=1): f* = 2p - 1 = 0.10.
        assert!((full_kelly(0.55, 100) - 0.10).abs() < 1e-9);
    }

    #[test]
    fn ev_is_positive_when_price_beats_probability() {
        let decimal = american_to_decimal(120).unwrap();
        let (p, stake) = (0.55, 100.0);
        let ev = p * stake * (decimal - 1.0) - (1.0 - p) * stake;
        assert!(ev > 0.0, "ev={ev}");
    }

    #[test]
    fn hedge_equalizes_both_outcomes() {
        let (da, db) = (american_to_decimal(150).unwrap(), american_to_decimal(-120).unwrap());
        let stake_a = 100.0;
        let hedge_stake = stake_a * da / db;
        // Return is the same whichever side wins.
        assert!((stake_a * da - hedge_stake * db).abs() < 1e-9);
    }

    #[test]
    fn parlay_multiplies_decimals() {
        // Two -110 legs: 1.9091^2 ≈ 3.645 decimal ≈ +264 American.
        let d = american_to_decimal(-110).unwrap().powi(2);
        assert!((d - 3.6446).abs() < 0.001, "{d}");
        assert_eq!(decimal_to_american(d), Some(264));
    }
}
