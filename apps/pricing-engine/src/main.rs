//! pricing-engine
//! Converts between probabilities and market prices.
//!
//! GET /price?probability=0.61        -> fair American odds, e.g. {"fair_price":-156}
//! GET /implied?odds=-150             -> implied (vig-inclusive) win probability
//! GET /novig?odds_a=-150&odds_b=130  -> two-sided no-vig fair probabilities & odds

use actix_web::{web, App, HttpServer, HttpResponse, Responder};
use common::{healthz, implied_probability, PricingOutput};
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
struct PriceQuery {
    probability: f64,
}

#[derive(Deserialize)]
struct ImpliedQuery {
    odds: i64,
}

#[derive(Deserialize)]
struct NoVigQuery {
    odds_a: i64,
    odds_b: i64,
}

/// Fair (no-vig) American odds for a win probability in (0, 1).
/// Favorites (p >= 0.5) are negative, underdogs positive.
fn american_odds(p: f64) -> i64 {
    if p >= 0.5 {
        (-100.0 * p / (1.0 - p)).round() as i64
    } else {
        (100.0 * (1.0 - p) / p).round() as i64
    }
}

async fn price(q: web::Query<PriceQuery>) -> impl Responder {
    let p = q.probability;
    if !(p > 0.0 && p < 1.0) {
        return HttpResponse::BadRequest().json(json!({ "error": "probability must be in (0, 1)" }));
    }
    HttpResponse::Ok().json(PricingOutput { probability: p, fair_price: american_odds(p) })
}

async fn implied(q: web::Query<ImpliedQuery>) -> impl Responder {
    match implied_probability(q.odds) {
        Some(p) => HttpResponse::Ok().json(json!({ "odds": q.odds, "implied_probability": p })),
        None => HttpResponse::BadRequest()
            .json(json!({ "error": "American odds must be <= -100 or >= 100" })),
    }
}

/// Strip the vig from a two-outcome market: normalize the raw implied
/// probabilities so they sum to 1, then re-price each side.
async fn novig(q: web::Query<NoVigQuery>) -> impl Responder {
    let (Some(a), Some(b)) = (implied_probability(q.odds_a), implied_probability(q.odds_b)) else {
        return HttpResponse::BadRequest()
            .json(json!({ "error": "both American odds must be <= -100 or >= 100" }));
    };
    let overround = a + b; // > 1 for a book with vig
    let fair_a = a / overround;
    let fair_b = b / overround;
    HttpResponse::Ok().json(json!({
        "odds_a": q.odds_a,
        "odds_b": q.odds_b,
        "overround": overround,
        "hold": overround - 1.0,
        "fair_probability_a": fair_a,
        "fair_probability_b": fair_b,
        "fair_odds_a": american_odds(fair_a),
        "fair_odds_b": american_odds(fair_b),
    }))
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8082);
    println!("pricing-engine listening on 0.0.0.0:{}", port);
    HttpServer::new(|| {
        App::new()
            .route("/healthz", web::get().to(healthz))
            .route("/price", web::get().to(price))
            .route("/implied", web::get().to(implied))
            .route("/novig", web::get().to(novig))
    })
    .bind(("0.0.0.0", port))?
    .run()
    .await
}

#[cfg(test)]
mod tests {
    use super::american_odds;
    use common::implied_probability;

    #[test]
    fn favorites_are_negative() {
        assert_eq!(american_odds(0.61), -156);
        assert_eq!(american_odds(0.5), -100);
        assert_eq!(american_odds(0.75), -300);
    }

    #[test]
    fn underdogs_are_positive() {
        assert_eq!(american_odds(0.25), 300);
        assert_eq!(american_odds(0.4), 150);
    }

    #[test]
    fn implied_is_inverse_of_price() {
        // -156 implies ~0.609; re-pricing that lands back on -156.
        let p = implied_probability(-156).unwrap();
        assert!((p - 0.609).abs() < 0.002, "{p}");
        assert_eq!(american_odds(p), -156);
    }

    #[test]
    fn novig_probabilities_sum_to_one() {
        let a = implied_probability(-150).unwrap();
        let b = implied_probability(130).unwrap();
        let overround = a + b;
        let (fair_a, fair_b) = (a / overround, b / overround);
        assert!(overround > 1.0, "a real book holds vig: {overround}");
        assert!((fair_a + fair_b - 1.0).abs() < 1e-9);
    }
}
