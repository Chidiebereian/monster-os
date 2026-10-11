//! arbitrage-engine
//! Compares a model's fair probability against a market price,
//! e.g. {"edge":0.08,"roi":0.05}
//!
//! GET /edges?fair_probability=0.61&odds=-130   (sportsbook, American odds)
//! GET /edges?fair_probability=0.61&price=0.53  (prediction-market contract, $1 payout)
//! GET /edges?fair_probability=0.61&venue=kalshi&market_id=KX...[&side=yes|no]
//!     fetches the live quote from prediction-market-service
//!     (PREDICTION_MARKET_URL) and prices the edge at the ask you would pay:
//!     YES costs yes_ask, NO costs 1 - yes_bid. Without `side`, the better
//!     side is returned.
//!
//! GET /scan?sport=baseball_mlb&series=KXMLBGAME[&min_edge=0.02]
//!     matches every open Kalshi market in the series to the same game in
//!     odds-service (ODDS_SERVICE_URL) and prices it against the sportsbooks'
//!     no-vig consensus, after an estimated Kalshi taker fee. See scan.rs.
//!
//! `edge` is fair probability minus the market's implied probability; `roi`
//! is expected profit per unit staked.

mod scan;

use std::time::Duration;

use actix_web::{http::StatusCode, web, App, HttpServer, HttpResponse, Responder};
use common::{american_to_decimal, healthz, ArbitrageEdge, MarketQuote};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Deserialize)]
struct EdgeQuery {
    fair_probability: f64,
    odds: Option<i64>,
    price: Option<f64>,
    venue: Option<String>,
    market_id: Option<String>,
    side: Option<Side>,
}

#[derive(Deserialize, Serialize, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
enum Side {
    Yes,
    No,
}

#[derive(Serialize)]
struct LiveEdge {
    #[serde(flatten)]
    edge: ArbitrageEdge,
    side: Side,
    price: f64,
    quote: MarketQuote,
}

struct Markets {
    http: reqwest::Client,
    base_url: String,
    odds_url: String,
}

#[derive(Deserialize)]
struct ScanQuery {
    sport: String,
    series: String,
    #[serde(default = "default_min_edge")]
    min_edge: f64,
    /// Optional executability gates (shares within 2c of the touch / at the
    /// touch) — mirrors monster.bet's min_liquidity / min_top_size. When set,
    /// each opportunity's Kalshi order book is checked and non-executable ones
    /// are dropped.
    min_depth: Option<f64>,
    min_top: Option<f64>,
}

fn default_min_edge() -> f64 {
    f64::NEG_INFINITY
}

#[derive(serde::Deserialize)]
struct BookLevel {
    price: f64,
    size: f64,
}

#[derive(serde::Deserialize)]
struct Book {
    #[serde(default)]
    bids: Vec<BookLevel>,
    #[serde(default)]
    asks: Vec<BookLevel>,
}

/// Resting size at the touch and within 2c, for the side being bought: YES
/// buys consume the asks (best-first ascending), NO buys consume the YES bids
/// (best-first descending).
fn side_liquidity(book: &Book, side: Side) -> (Option<f64>, Option<f64>) {
    let (levels, ascending) = match side {
        Side::Yes => (&book.asks, true),
        Side::No => (&book.bids, false),
    };
    let top = levels.first().map(|l| l.size);
    let depth = levels.first().map(|f| {
        let best = f.price;
        levels
            .iter()
            .filter(|l| if ascending { l.price <= best + 0.02 } else { l.price >= best - 0.02 })
            .map(|l| l.size)
            .sum()
    });
    (depth, top)
}

fn passes_liquidity(depth: Option<f64>, top: Option<f64>, min_depth: Option<f64>, min_top: Option<f64>) -> bool {
    let depth_ok = min_depth.map_or(true, |m| depth.is_some_and(|d| d >= m));
    let top_ok = min_top.map_or(true, |m| top.is_some_and(|t| t >= m));
    depth_ok && top_ok
}

/// Edge and ROI for a bet paying `decimal` per unit staked.
fn edge(fair_p: f64, decimal: f64) -> ArbitrageEdge {
    ArbitrageEdge { edge: fair_p - 1.0 / decimal, roi: fair_p * decimal - 1.0 }
}

/// Edge from buying `side` at the current ask. `None` if that side has no offer.
fn side_edge(fair_p: f64, quote: &MarketQuote, side: Side) -> Option<(f64, ArbitrageEdge)> {
    let (p, cost) = match side {
        Side::Yes => (fair_p, quote.yes_ask?),
        Side::No => (1.0 - fair_p, 1.0 - quote.yes_bid?),
    };
    Some((cost, edge(p, 1.0 / cost)))
}

/// The requested side, or whichever available side has the higher edge.
fn best_edge(fair_p: f64, quote: &MarketQuote, side: Option<Side>) -> Option<(Side, f64, ArbitrageEdge)> {
    let sides: &[Side] = match side {
        Some(Side::Yes) => &[Side::Yes],
        Some(Side::No) => &[Side::No],
        None => &[Side::Yes, Side::No],
    };
    sides
        .iter()
        .filter_map(|&s| side_edge(fair_p, quote, s).map(|(c, e)| (s, c, e)))
        .max_by(|a, b| a.2.edge.total_cmp(&b.2.edge))
}

fn error(status: StatusCode, msg: impl std::fmt::Display) -> HttpResponse {
    HttpResponse::build(status).json(json!({ "error": msg.to_string() }))
}

fn bad_request(msg: &str) -> HttpResponse {
    error(StatusCode::BAD_REQUEST, msg)
}

impl Markets {
    /// GET a JSON body from an upstream service. 4xx responses (unknown
    /// venue/market/sport, bad id) pass through; anything else becomes a 502.
    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        service: &str,
    ) -> Result<T, HttpResponse> {
        let resp = self.http.get(url).send().await.map_err(|e| {
            eprintln!("arbitrage-engine: {service} unreachable at {url}: {e}");
            error(StatusCode::BAD_GATEWAY, format!("{service} unreachable"))
        })?;
        let status = resp.status();
        if status.is_success() {
            return resp
                .json()
                .await
                .map_err(|e| error(StatusCode::BAD_GATEWAY, format!("bad {service} payload: {e}")));
        }
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        let msg = body["error"].as_str().map(String::from).unwrap_or(format!("{service} returned {status}"));
        match StatusCode::from_u16(status.as_u16()) {
            Ok(s) if s.is_client_error() => Err(error(s, msg)),
            _ => Err(error(StatusCode::BAD_GATEWAY, msg)),
        }
    }

    async fn quote(&self, venue: &str, market_id: &str) -> Result<MarketQuote, HttpResponse> {
        let url = format!("{}/quotes/{venue}/{market_id}", self.base_url);
        self.get_json(&url, "prediction-market-service").await
    }

    async fn book(&self, venue: &str, market_id: &str) -> Result<Book, HttpResponse> {
        let url = format!("{}/orderbook/{venue}/{market_id}", self.base_url);
        self.get_json(&url, "prediction-market-service").await
    }
}

fn valid_key(s: &str) -> bool {
    !s.is_empty() && s.len() <= 100 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

async fn scan_markets(markets: web::Data<Markets>, q: web::Query<ScanQuery>) -> impl Responder {
    if !valid_key(&q.sport) || !valid_key(&q.series) {
        return bad_request("sport and series must be keys like baseball_mlb and KXMLBGAME");
    }
    let odds_url = format!("{}/odds/{}", markets.odds_url, q.sport);
    let kalshi_url = format!("{}/markets/kalshi?series_ticker={}", markets.base_url, q.series);
    let (events, quotes) = futures_util::future::join(
        markets.get_json::<Vec<scan::OddsEvent>>(&odds_url, "odds-service"),
        markets.get_json::<Vec<MarketQuote>>(&kalshi_url, "prediction-market-service"),
    )
    .await;
    let (events, quotes) = match (events, quotes) {
        (Ok(e), Ok(q)) => (e, q),
        (Err(resp), _) | (_, Err(resp)) => return resp,
    };
    let (kalshi_markets, sportsbook_games) = (quotes.len(), events.len());
    let (mut opportunities, unmatched) = scan::scan(quotes, &events);
    opportunities.retain(|o| o.net_edge >= q.min_edge);

    // Without gates, return opportunities as-is. With gates, fetch each one's
    // Kalshi order book, drop the non-executable, and annotate the rest.
    let gated = q.min_depth.is_some() || q.min_top.is_some();
    let opp_json: Vec<serde_json::Value> = if !gated {
        opportunities.iter().filter_map(|o| serde_json::to_value(o).ok()).collect()
    } else {
        let mut out = Vec::new();
        for o in &opportunities {
            let (depth, top) = match markets.book("kalshi", &o.market_id).await {
                Ok(b) => side_liquidity(&b, o.side),
                Err(_) => (None, None),
            };
            if !passes_liquidity(depth, top, q.min_depth, q.min_top) {
                continue;
            }
            if let Ok(serde_json::Value::Object(mut m)) = serde_json::to_value(o) {
                m.insert("exec_depth_2c".into(), json!(depth));
                m.insert("top_size".into(), json!(top));
                m.insert("executable".into(), json!(true));
                out.push(serde_json::Value::Object(m));
            }
        }
        out
    };

    HttpResponse::Ok().json(json!({
        "kalshi_markets": kalshi_markets,
        "sportsbook_games": sportsbook_games,
        "opportunities": opp_json,
        "unmatched": unmatched,
    }))
}

async fn edges(markets: web::Data<Markets>, q: web::Query<EdgeQuery>) -> impl Responder {
    let p = q.fair_probability;
    if !(p > 0.0 && p < 1.0) {
        return bad_request("fair_probability must be in (0, 1)");
    }
    let decimal = match (q.odds, q.price, &q.venue, &q.market_id) {
        (Some(o), None, None, None) => match american_to_decimal(o) {
            Some(d) => d,
            None => return bad_request("odds must be <= -100 or >= +100"),
        },
        (None, Some(c), None, None) if c > 0.0 && c < 1.0 => 1.0 / c,
        (None, Some(_), None, None) => return bad_request("price must be in (0, 1)"),
        (None, None, Some(venue), Some(market_id)) => {
            let quote = match markets.quote(venue, market_id).await {
                Ok(q) => q,
                Err(resp) => return resp,
            };
            return match best_edge(p, &quote, q.side) {
                Some((side, price, edge)) => {
                    HttpResponse::Ok().json(LiveEdge { edge, side, price, quote })
                }
                None => error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "no resting offer on the requested side of the book",
                ),
            };
        }
        _ => return bad_request("pass exactly one of odds, price, or venue & market_id"),
    };
    HttpResponse::Ok().json(edge(p, decimal))
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8083);
    let markets = web::Data::new(Markets {
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?,
        base_url: std::env::var("PREDICTION_MARKET_URL")
            .unwrap_or_else(|_| "http://localhost:8105".into())
            .trim_end_matches('/')
            .to_string(),
        odds_url: std::env::var("ODDS_SERVICE_URL")
            .unwrap_or_else(|_| "http://localhost:8120".into())
            .trim_end_matches('/')
            .to_string(),
    });
    println!("arbitrage-engine listening on 0.0.0.0:{}", port);
    HttpServer::new(move || {
        App::new()
            .app_data(markets.clone())
            .route("/healthz", web::get().to(healthz))
            .route("/edges", web::get().to(edges))
            .route("/scan", web::get().to(scan_markets))
    })
    .bind(("0.0.0.0", port))?
    .run()
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn converts_american_odds() {
        assert!(close(american_to_decimal(150).unwrap(), 2.5));
        assert!(close(american_to_decimal(-200).unwrap(), 1.5));
        assert!(close(american_to_decimal(-100).unwrap(), 2.0));
        assert!(american_to_decimal(50).is_none());
    }

    #[test]
    fn side_liquidity_picks_the_right_book_side() {
        let book = Book {
            bids: vec![BookLevel { price: 0.40, size: 300.0 }, BookLevel { price: 0.39, size: 200.0 }],
            asks: vec![BookLevel { price: 0.42, size: 100.0 }, BookLevel { price: 0.43, size: 50.0 }],
        };
        // YES buy -> asks: top 100, depth within 2c of 0.42 = 100 + 50 = 150.
        let (d, t) = side_liquidity(&book, Side::Yes);
        assert_eq!((t, d), (Some(100.0), Some(150.0)));
        // NO buy -> YES bids: top 300, depth within 2c of 0.40 = 300 + 200.
        let (d, t) = side_liquidity(&book, Side::No);
        assert_eq!((t, d), (Some(300.0), Some(500.0)));
    }

    #[test]
    fn liquidity_gate() {
        assert!(passes_liquidity(Some(200.0), Some(100.0), Some(150.0), Some(50.0)));
        assert!(!passes_liquidity(Some(200.0), Some(40.0), None, Some(50.0)));
        assert!(!passes_liquidity(None, None, Some(150.0), None)); // no book -> fails a set gate
        assert!(passes_liquidity(None, None, None, None)); // no gate -> passes
    }

    #[test]
    fn positive_edge_on_underpriced_contract() {
        let e = edge(0.61, 1.0 / 0.53);
        assert!(close(e.edge, 0.08));
        assert!(close(e.roi, 0.61 / 0.53 - 1.0));
    }

    #[test]
    fn fair_price_has_zero_edge() {
        let e = edge(0.5, 2.0);
        assert!(close(e.edge, 0.0) && close(e.roi, 0.0));
    }

    fn quote(bid: Option<f64>, ask: Option<f64>) -> MarketQuote {
        MarketQuote {
            venue: "kalshi".into(),
            market_id: "KX".into(),
            title: "t".into(),
            yes_bid: bid,
            yes_ask: ask,
            last: None,
            as_of: chrono::Utc::now(),
            event_id: None,
            outcome: None,
            event_time: None,
            taker_fee: None,
        }
    }

    #[test]
    fn prices_each_side_at_its_ask() {
        let q = quote(Some(0.48), Some(0.49));
        let (cost, e) = side_edge(0.61, &q, Side::Yes).unwrap();
        assert!(close(cost, 0.49) && close(e.edge, 0.12));
        let (cost, e) = side_edge(0.61, &q, Side::No).unwrap();
        assert!(close(cost, 0.52) && close(e.edge, 0.39 - 0.52));
    }

    #[test]
    fn picks_better_side_unless_one_is_requested() {
        let q = quote(Some(0.70), Some(0.72));
        assert_eq!(best_edge(0.5, &q, None).unwrap().0, Side::No);
        assert_eq!(best_edge(0.5, &q, Some(Side::Yes)).unwrap().0, Side::Yes);
    }

    #[test]
    fn missing_book_side_has_no_edge() {
        let q = quote(None, Some(0.4));
        assert!(side_edge(0.5, &q, Side::No).is_none());
        assert!(best_edge(0.5, &q, Some(Side::No)).is_none());
        assert_eq!(best_edge(0.5, &q, None).unwrap().0, Side::Yes);
    }
}
