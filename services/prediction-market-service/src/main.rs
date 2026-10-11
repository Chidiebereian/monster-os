//! prediction-market-service
//! Kalshi/Polymarket market + order book ingestion.
//!
//! GET /quotes/kalshi/{ticker}      e.g. KXMLBGAME-26OCT081700CLECWS-CWS
//! GET /quotes/polymarket/{slug}    e.g. xi-jinping-out-before-2027
//!
//! GET /markets/kalshi?series_ticker=KXMLBGAME   every open market in a series
//! GET /markets/polymarket?league=mlb            open game-winner markets
//!
//! All return common::MarketQuote (YES side, dollars per $1 contract, with
//! the venue's taker fee) from the venues' public, unauthenticated
//! market-data APIs. Base URLs can be overridden with KALSHI_API_URL and
//! POLYMARKET_API_URL.

use std::time::Duration;

use actix_web::{web, App, HttpServer, HttpResponse, Responder};
use chrono::{DateTime, Utc};
use common::{healthz, FeeSchedule, MarketQuote};
use serde_json::{json, Value};

/// Kalshi's published taker fee: 0.07 × P × (1 − P) per contract.
const KALSHI_FEE: FeeSchedule = FeeSchedule { rate: 0.07, exponent: 1.0 };

/// Polymarket games older than this are leftovers, not live markets.
const POLYMARKET_STALE_HOURS: i64 = 12;

struct Venues {
    http: reqwest::Client,
    kalshi: String,
    polymarket: String,
    clob: String,
}

/// One resting order-book level, normalized to a YES book in dollars/shares.
#[derive(serde::Serialize)]
struct Level {
    price: f64,
    size: f64,
}

#[derive(serde::Serialize)]
struct OrderBook {
    bids: Vec<Level>,
    asks: Vec<Level>,
}

/// A JSON number or numeric string as f64 (no price filtering, unlike `price`).
fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn sort_trunc(mut bids: Vec<Level>, mut asks: Vec<Level>) -> OrderBook {
    bids.sort_by(|a, b| b.price.total_cmp(&a.price)); // best (highest) bid first
    asks.sort_by(|a, b| a.price.total_cmp(&b.price)); // best (lowest) ask first
    bids.truncate(20);
    asks.truncate(20);
    OrderBook { bids, asks }
}

/// Polymarket CLOB `/book`: bids/asks are already a YES book in dollars.
fn normalize_polymarket_book(body: &Value) -> OrderBook {
    let levels = |arr: &Value| {
        arr.as_array()
            .into_iter()
            .flatten()
            .filter_map(|o| Some(Level { price: num(&o["price"])?, size: num(&o["size"])? }))
            .collect::<Vec<_>>()
    };
    sort_trunc(levels(&body["bids"]), levels(&body["asks"]))
}

/// Kalshi `/orderbook`: `yes`/`no` resting bids. YES bids map straight over;
/// YES asks are the complement of the NO bids (sell YES at 1 - no_price).
/// Handles the newer `orderbook_fp` (yes_dollars/no_dollars, dollar strings)
/// and the legacy `orderbook` (yes/no, integer cents).
fn normalize_kalshi_book(body: &Value) -> OrderBook {
    let pairs = |arr: &Value, complement: bool, scale: f64| {
        arr.as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| {
                let a = e.as_array()?;
                let p = num(a.first()?)? / scale;
                Some(Level { price: if complement { 1.0 - p } else { p }, size: num(a.get(1)?)? })
            })
            .collect::<Vec<_>>()
    };
    if let Some(ob) = body.get("orderbook_fp") {
        sort_trunc(pairs(&ob["yes_dollars"], false, 1.0), pairs(&ob["no_dollars"], true, 1.0))
    } else {
        let ob = &body["orderbook"];
        sort_trunc(pairs(&ob["yes"], false, 100.0), pairs(&ob["no"], true, 100.0))
    }
}

enum FetchError {
    NotFound,
    Upstream(String),
}

/// Market ids are interpolated into upstream URLs, so only allow the
/// characters real tickers and slugs use.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 200
        && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Accepts a JSON number or numeric string; prices of 0 (no bid) or
/// >= 1 (no ask) mean there is no resting order.
fn price(v: &Value) -> Option<f64> {
    let p = match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }?;
    (p > 0.0 && p < 1.0).then_some(p)
}

fn opt_str(v: &Value) -> Option<String> {
    v.as_str().filter(|s| !s.is_empty()).map(String::from)
}

/// One Kalshi market object (from /markets/{ticker} or the /markets list).
fn kalshi_market_quote(m: &Value) -> Option<MarketQuote> {
    Some(MarketQuote {
        venue: "kalshi".into(),
        market_id: m["ticker"].as_str()?.into(),
        title: m["title"].as_str().unwrap_or_default().into(),
        yes_bid: price(&m["yes_bid_dollars"]),
        yes_ask: price(&m["yes_ask_dollars"]),
        last: price(&m["last_price_dollars"]),
        as_of: Utc::now(),
        event_id: opt_str(&m["event_ticker"]),
        outcome: opt_str(&m["yes_sub_title"]),
        event_time: m["occurrence_datetime"].as_str().and_then(|s| s.parse().ok()),
        taker_fee: Some(KALSHI_FEE),
    })
}

fn kalshi_quote(body: &Value) -> Option<MarketQuote> {
    kalshi_market_quote(body.get("market")?)
}

/// Gamma's `feeSchedule`, or free when `feesEnabled` is false. `None` when
/// the market doesn't say, so callers don't mistake unknown for free.
fn polymarket_fee(m: &Value) -> Option<FeeSchedule> {
    match m["feesEnabled"].as_bool()? {
        false => Some(FeeSchedule::FREE),
        true => Some(FeeSchedule {
            rate: m["feeSchedule"]["rate"].as_f64()?,
            exponent: m["feeSchedule"]["exponent"].as_f64().unwrap_or(1.0),
        }),
    }
}

/// Gamma times look like "2026-10-09 00:00:00+00"; also accepts RFC 3339.
fn polymarket_time(v: &Value) -> Option<DateTime<Utc>> {
    let s = v.as_str()?;
    if let Ok(t) = s.parse() {
        return Some(t);
    }
    let mut s = s.replacen(' ', "T", 1);
    if s.len() >= 3 && s.as_bytes()[s.len() - 3] == b'+' {
        s.push_str(":00");
    }
    s.parse().ok()
}

/// Gamma's `outcomes` is a JSON array encoded as a string.
fn polymarket_outcomes(m: &Value) -> Option<Vec<String>> {
    serde_json::from_str(m["outcomes"].as_str()?).ok()
}

/// Gamma returns an array of markets matching the slug; YES is the first outcome.
fn polymarket_quote(slug: &str, body: &Value) -> Option<MarketQuote> {
    let m = body.as_array()?.first()?;
    Some(MarketQuote {
        venue: "polymarket".into(),
        market_id: slug.into(),
        title: m["question"].as_str().unwrap_or_default().into(),
        yes_bid: price(&m["bestBid"]),
        yes_ask: price(&m["bestAsk"]),
        last: price(&m["lastTradePrice"]),
        as_of: Utc::now(),
        event_id: None,
        outcome: polymarket_outcomes(m).and_then(|o| o.into_iter().next()),
        event_time: polymarket_time(&m["gameStartTime"]),
        taker_fee: polymarket_fee(m),
    })
}

/// A two-team game market ("Cleveland Guardians" vs "Chicago White Sox") as
/// one quote per team. Gamma's best bid/ask are for the first outcome; the
/// second outcome's book is the complement (its ask = 1 − first bid).
fn polymarket_game_quotes(m: &Value) -> Vec<MarketQuote> {
    let (Some(slug), Some(outcomes)) = (m["slug"].as_str(), polymarket_outcomes(m)) else {
        return Vec::new();
    };
    let [first, second] = outcomes.as_slice() else { return Vec::new() };
    if first.eq_ignore_ascii_case("yes") {
        return Vec::new();
    }
    let (bid, ask, last) = (price(&m["bestBid"]), price(&m["bestAsk"]), price(&m["lastTradePrice"]));
    let complement = |p: Option<f64>| p.map(|p| 1.0 - p);
    let quote = |outcome: &str, yes_bid, yes_ask, last| MarketQuote {
        venue: "polymarket".into(),
        market_id: slug.into(),
        title: m["question"].as_str().unwrap_or_default().into(),
        yes_bid,
        yes_ask,
        last,
        as_of: Utc::now(),
        event_id: Some(slug.into()),
        outcome: Some(outcome.into()),
        event_time: polymarket_time(&m["gameStartTime"]),
        taker_fee: polymarket_fee(m),
    };
    vec![
        quote(first, bid, ask, last),
        quote(second, complement(ask), complement(bid), complement(last)),
    ]
}

impl Venues {
    async fn get_json(&self, url: &str) -> Result<Value, FetchError> {
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| FetchError::Upstream(format!("request to {url} failed: {e}")))?;
        match resp.status() {
            reqwest::StatusCode::NOT_FOUND => Err(FetchError::NotFound),
            s if !s.is_success() => Err(FetchError::Upstream(format!("{url} returned {s}"))),
            _ => resp
                .json()
                .await
                .map_err(|e| FetchError::Upstream(format!("{url} returned bad JSON: {e}"))),
        }
    }

    async fn quote(&self, venue: &str, id: &str) -> Result<MarketQuote, FetchError> {
        match venue {
            "kalshi" => {
                let body = self.get_json(&format!("{}/markets/{id}", self.kalshi)).await?;
                kalshi_quote(&body).ok_or(FetchError::NotFound)
            }
            "polymarket" => {
                let body = self.get_json(&format!("{}/markets?slug={id}", self.polymarket)).await?;
                polymarket_quote(id, &body).ok_or(FetchError::NotFound)
            }
            _ => Err(FetchError::NotFound),
        }
    }

    /// Normalized YES order book for a market. Polymarket needs its CLOB token
    /// id, read from the Gamma market's `clobTokenIds`.
    async fn book(&self, venue: &str, id: &str) -> Result<OrderBook, FetchError> {
        match venue {
            "kalshi" => {
                let body = self.get_json(&format!("{}/markets/{id}/orderbook", self.kalshi)).await?;
                Ok(normalize_kalshi_book(&body))
            }
            "polymarket" => {
                let gamma = self.get_json(&format!("{}/markets?slug={id}", self.polymarket)).await?;
                let token = gamma
                    .as_array()
                    .and_then(|a| a.first())
                    .and_then(|m| m["clobTokenIds"].as_str())
                    .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
                    .and_then(|v| v.into_iter().next())
                    .filter(|t| t.chars().all(|c| c.is_ascii_digit()))
                    .ok_or(FetchError::NotFound)?;
                let body = self.get_json(&format!("{}/book?token_id={token}", self.clob)).await?;
                Ok(normalize_polymarket_book(&body))
            }
            _ => Err(FetchError::NotFound),
        }
    }

    /// Every open market in a Kalshi series, following the pagination cursor.
    async fn kalshi_series(&self, series: &str) -> Result<Vec<MarketQuote>, FetchError> {
        let mut quotes = Vec::new();
        let mut cursor = String::new();
        for _ in 0..MAX_KALSHI_PAGES {
            let url = format!(
                "{}/markets?series_ticker={series}&status=open&limit=200&cursor={cursor}",
                self.kalshi
            );
            let body = self.get_json(&url).await?;
            quotes.extend(
                body["markets"].as_array().into_iter().flatten().filter_map(kalshi_market_quote),
            );
            match body["cursor"].as_str() {
                // Cursors are opaque base64; keep them URL-safe before reuse.
                Some(c) if !c.is_empty() && valid_cursor(c) => cursor = c.to_string(),
                _ => return Ok(quotes),
            }
        }
        Ok(quotes)
    }
}

impl Venues {
    /// Open game-winner markets for a Polymarket league ("mlb", "nba", ...),
    /// found via the league's primary tag, skipping games long since started.
    async fn polymarket_league(&self, league: &str) -> Result<Vec<MarketQuote>, FetchError> {
        let sports = self.get_json(&format!("{}/sports", self.polymarket)).await?;
        let tag = sports
            .as_array()
            .into_iter()
            .flatten()
            .find(|s| s["sport"].as_str() == Some(league))
            .and_then(|s| s["primaryTagId"].as_i64())
            .ok_or(FetchError::NotFound)?;

        let cutoff = Utc::now() - chrono::Duration::hours(POLYMARKET_STALE_HOURS);
        let mut quotes = Vec::new();
        for page in 0..MAX_POLYMARKET_PAGES {
            let url = format!(
                "{}/markets?closed=false&sports_market_types=moneyline&tag_id={tag}&limit={PAGE}&offset={}",
                self.polymarket,
                page * PAGE
            );
            let body = self.get_json(&url).await?;
            let markets = body.as_array().cloned().unwrap_or_default();
            quotes.extend(
                markets
                    .iter()
                    .flat_map(polymarket_game_quotes)
                    .filter(|q| q.event_time.is_some_and(|t| t >= cutoff)),
            );
            if markets.len() < PAGE {
                break;
            }
        }
        Ok(quotes)
    }
}

const PAGE: usize = 100;
const MAX_POLYMARKET_PAGES: usize = 10;
const MAX_KALSHI_PAGES: usize = 10;

#[derive(serde::Deserialize)]
struct LeagueQuery {
    league: String,
}

async fn polymarket_markets(venues: web::Data<Venues>, q: web::Query<LeagueQuery>) -> impl Responder {
    if !valid_id(&q.league) {
        return HttpResponse::BadRequest().json(json!({ "error": "invalid league" }));
    }
    match venues.polymarket_league(&q.league.to_lowercase()).await {
        Ok(quotes) => HttpResponse::Ok().json(quotes),
        Err(FetchError::NotFound) => HttpResponse::NotFound()
            .json(json!({ "error": format!("no polymarket league {:?}", q.league) })),
        Err(FetchError::Upstream(msg)) => {
            eprintln!("prediction-market-service: {msg}");
            HttpResponse::BadGateway().json(json!({ "error": "polymarket unavailable" }))
        }
    }
}

fn valid_cursor(c: &str) -> bool {
    c.len() <= 1000 && c.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '='))
}

#[derive(serde::Deserialize)]
struct SeriesQuery {
    series_ticker: String,
}

async fn kalshi_markets(venues: web::Data<Venues>, q: web::Query<SeriesQuery>) -> impl Responder {
    if !valid_id(&q.series_ticker) {
        return HttpResponse::BadRequest().json(json!({ "error": "invalid series_ticker" }));
    }
    match venues.kalshi_series(&q.series_ticker).await {
        Ok(quotes) => HttpResponse::Ok().json(quotes),
        Err(FetchError::NotFound) => HttpResponse::NotFound()
            .json(json!({ "error": format!("no kalshi series {:?}", q.series_ticker) })),
        Err(FetchError::Upstream(msg)) => {
            eprintln!("prediction-market-service: {msg}");
            HttpResponse::BadGateway().json(json!({ "error": "kalshi unavailable" }))
        }
    }
}

async fn quote(venues: web::Data<Venues>, path: web::Path<(String, String)>) -> impl Responder {
    let (venue, id) = path.into_inner();
    if !matches!(venue.as_str(), "kalshi" | "polymarket") {
        return HttpResponse::NotFound().json(json!({ "error": "venue must be kalshi or polymarket" }));
    }
    if !valid_id(&id) {
        return HttpResponse::BadRequest().json(json!({ "error": "invalid market id" }));
    }
    match venues.quote(&venue, &id).await {
        Ok(q) => HttpResponse::Ok().json(q),
        Err(FetchError::NotFound) => HttpResponse::NotFound()
            .json(json!({ "error": format!("no {venue} market {id:?}") })),
        Err(FetchError::Upstream(msg)) => {
            eprintln!("prediction-market-service: {msg}");
            HttpResponse::BadGateway().json(json!({ "error": format!("{venue} unavailable") }))
        }
    }
}

async fn orderbook(venues: web::Data<Venues>, path: web::Path<(String, String)>) -> impl Responder {
    let (venue, id) = path.into_inner();
    if !matches!(venue.as_str(), "kalshi" | "polymarket") {
        return HttpResponse::NotFound().json(json!({ "error": "venue must be kalshi or polymarket" }));
    }
    if !valid_id(&id) {
        return HttpResponse::BadRequest().json(json!({ "error": "invalid market id" }));
    }
    match venues.book(&venue, &id).await {
        Ok(ob) => HttpResponse::Ok().json(json!({
            "venue": venue,
            "market_id": id,
            "bids": ob.bids,
            "asks": ob.asks,
        })),
        Err(FetchError::NotFound) => HttpResponse::NotFound()
            .json(json!({ "error": format!("no {venue} order book for {id:?}") })),
        Err(FetchError::Upstream(msg)) => {
            eprintln!("prediction-market-service: {msg}");
            HttpResponse::BadGateway().json(json!({ "error": format!("{venue} unavailable") }))
        }
    }
}

fn env_url(var: &str, default: &str) -> String {
    std::env::var(var).unwrap_or_else(|_| default.into()).trim_end_matches('/').to_string()
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8105);
    let venues = web::Data::new(Venues {
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent("monster-os/prediction-market-service")
            .build()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?,
        kalshi: env_url("KALSHI_API_URL", "https://api.elections.kalshi.com/trade-api/v2"),
        polymarket: env_url("POLYMARKET_API_URL", "https://gamma-api.polymarket.com"),
        clob: env_url("POLYMARKET_CLOB_URL", "https://clob.polymarket.com"),
    });
    println!("prediction-market-service listening on 0.0.0.0:{}", port);
    HttpServer::new(move || {
        App::new()
            .app_data(venues.clone())
            .route("/healthz", web::get().to(healthz))
            .route("/quotes/{venue}/{id}", web::get().to(quote))
            .route("/orderbook/{venue}/{id}", web::get().to(orderbook))
            .route("/markets/kalshi", web::get().to(kalshi_markets))
            .route("/markets/polymarket", web::get().to(polymarket_markets))
    })
    .bind(("0.0.0.0", port))?
    .run()
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_kalshi_market() {
        let body = json!({ "market": {
            "ticker": "KX-CWS", "event_ticker": "KX", "yes_sub_title": "Chicago WS",
            "occurrence_datetime": "2026-10-09T00:00:00Z",
            "title": "Chicago WS wins", "yes_bid_dollars": "0.4800",
            "yes_ask_dollars": "0.4900", "last_price_dollars": "0.0000"
        }});
        let q = kalshi_quote(&body).unwrap();
        assert_eq!((q.yes_bid, q.yes_ask, q.last), (Some(0.48), Some(0.49), None));
        assert_eq!((q.market_id.as_str(), q.title.as_str()), ("KX-CWS", "Chicago WS wins"));
        assert_eq!(q.outcome.as_deref(), Some("Chicago WS"));
        assert_eq!(q.event_time.unwrap().to_rfc3339(), "2026-10-09T00:00:00+00:00");
        assert!(kalshi_quote(&json!({})).is_none());
    }

    #[test]
    fn parses_polymarket_market() {
        let body = json!([{ "question": "Q?", "bestBid": 0.026, "bestAsk": 0.027, "lastTradePrice": 0.027 }]);
        let q = polymarket_quote("q", &body).unwrap();
        assert_eq!((q.yes_bid, q.yes_ask), (Some(0.026), Some(0.027)));
        assert!(polymarket_quote("q", &json!([])).is_none());
    }

    #[test]
    fn normalizes_polymarket_book() {
        // Prices/sizes come as strings; bids sort high->low, asks low->high.
        let body = json!({
            "bids": [{ "price": "0.36", "size": "100" }, { "price": "0.38", "size": "50" }],
            "asks": [{ "price": "0.41", "size": "20" }, { "price": "0.40", "size": "80" }]
        });
        let ob = normalize_polymarket_book(&body);
        assert_eq!((ob.bids[0].price, ob.bids[0].size), (0.38, 50.0));
        assert_eq!((ob.asks[0].price, ob.asks[0].size), (0.40, 80.0));
    }

    #[test]
    fn normalizes_kalshi_book_with_complement() {
        // yes bids in cents map straight; no bids become yes asks at 1 - price.
        let body = json!({ "orderbook": {
            "yes": [[41, 300], [40, 500]],
            "no":  [[58, 200], [57, 400]]
        }});
        let ob = normalize_kalshi_book(&body);
        assert_eq!((ob.bids[0].price, ob.bids[0].size), (0.41, 300.0)); // best yes bid
        // best yes ask = 1 - best(no)=1-0.58=0.42, from the 58c/200 no bid.
        assert!((ob.asks[0].price - 0.42).abs() < 1e-9 && ob.asks[0].size == 200.0);
    }

    #[test]
    fn normalizes_kalshi_fp_book_in_dollars() {
        // Newer API: orderbook_fp with dollar strings (no cents scaling).
        let body = json!({ "orderbook_fp": {
            "yes_dollars": [["0.38", "793.43"], ["0.39", "1509.09"]],
            "no_dollars":  [["0.58", "397.67"], ["0.57", "986.31"]]
        }});
        let ob = normalize_kalshi_book(&body);
        assert_eq!((ob.bids[0].price, ob.bids[0].size), (0.39, 1509.09)); // best yes bid
        assert!((ob.asks[0].price - 0.42).abs() < 1e-9); // 1 - 0.58
    }

    #[test]
    fn splits_polymarket_game_into_team_quotes() {
        let m = json!({
            "slug": "mlb-cle-cws-2026-10-08",
            "question": "Cleveland Guardians vs. Chicago White Sox",
            "outcomes": "[\"Cleveland Guardians\", \"Chicago White Sox\"]",
            "bestBid": 0.51, "bestAsk": 0.52, "lastTradePrice": 0.515,
            "gameStartTime": "2026-10-09 00:00:00+00",
            "feesEnabled": true, "feeSchedule": { "rate": 0.05, "exponent": 1 }
        });
        let q = polymarket_game_quotes(&m);
        assert_eq!(q.len(), 2);
        assert_eq!(q[0].outcome.as_deref(), Some("Cleveland Guardians"));
        assert_eq!((q[0].yes_bid, q[0].yes_ask), (Some(0.51), Some(0.52)));
        assert_eq!(q[1].outcome.as_deref(), Some("Chicago White Sox"));
        let (b, a) = (q[1].yes_bid.unwrap(), q[1].yes_ask.unwrap());
        assert!((b - 0.48).abs() < 1e-12 && (a - 0.49).abs() < 1e-12);
        assert_eq!(q[1].event_id.as_deref(), Some("mlb-cle-cws-2026-10-08"));
        assert_eq!(q[0].event_time.unwrap().to_rfc3339(), "2026-10-09T00:00:00+00:00");
        assert_eq!(q[0].taker_fee, Some(FeeSchedule { rate: 0.05, exponent: 1.0 }));
        assert!((q[0].taker_fee.unwrap().per_contract(0.5) - 0.0125).abs() < 1e-12);
    }

    #[test]
    fn skips_yes_no_and_multi_outcome_markets() {
        let yes_no = json!({ "slug": "s", "outcomes": "[\"Yes\", \"No\"]" });
        let three = json!({ "slug": "s", "outcomes": "[\"A\", \"B\", \"Draw\"]" });
        assert!(polymarket_game_quotes(&yes_no).is_empty());
        assert!(polymarket_game_quotes(&three).is_empty());
    }

    #[test]
    fn polymarket_fee_is_unknown_unless_stated() {
        assert_eq!(polymarket_fee(&json!({})), None);
        assert_eq!(polymarket_fee(&json!({ "feesEnabled": false })), Some(FeeSchedule::FREE));
        assert_eq!(polymarket_fee(&json!({ "feesEnabled": true })), None);
    }

    #[test]
    fn empty_book_sides_are_none() {
        assert_eq!(price(&json!("1.0000")), None);
        assert_eq!(price(&json!(0)), None);
        assert_eq!(price(&json!("abc")), None);
        assert_eq!(price(&json!(0.5)), Some(0.5));
    }

    #[test]
    fn rejects_ids_that_could_escape_the_url() {
        assert!(valid_id("KXMLBGAME-26OCT081700CLECWS-CWS"));
        assert!(!valid_id("a/../b"));
        assert!(!valid_id("x?slug=y"));
        assert!(!valid_id(""));
    }
}
