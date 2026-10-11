//! odds-service
//! Sportsbook moneyline (h2h) odds from The Odds API (the-odds-api.com),
//! with API-key rotation and response caching to stretch quota.
//!
//! GET /odds/{sport}?regions=us   e.g. /odds/baseball_mlb
//!     every event with each book's American odds, the best price per
//!     outcome across books, and a no-vig consensus probability.
//! GET /keys                      masked per-key state (never the key itself)
//!
//! Config (environment or a .env file in the working dir or any parent):
//!   ODDS_API_KEYS=key1,key2,...   rotated round-robin (ODDS_API_KEY also accepted)
//!   ODDS_API_URL                  default https://api.the-odds-api.com/v4
//!   ODDS_CACHE_SECS               default 60
//!
//! Key handling: 401 disables a key (invalid), a quota message parks it for
//! an hour, 429 parks it for a minute; the request is retried on the next key.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use actix_web::{http::StatusCode, web, App, HttpServer, HttpResponse, Responder};
use common::{american_to_decimal, healthz};
use serde::{Deserialize, Serialize};
use serde_json::json;

const RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(60);
const QUOTA_COOLDOWN: Duration = Duration::from_secs(3600);

// ---------- key rotation ----------

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum KeyState {
    Active,
    Invalid,
    QuotaExhausted,
    RateLimited,
}

struct ApiKey {
    value: String,
    state: KeyState,
    retry_at: Option<Instant>,
    remaining: Option<i64>,
    used: Option<i64>,
}

enum KeyOutcome {
    Ok { remaining: Option<i64>, used: Option<i64> },
    Invalid,
    QuotaExhausted,
    RateLimited,
}

struct KeyPool {
    keys: Vec<ApiKey>,
    next: usize,
}

impl KeyPool {
    fn new(values: Vec<String>) -> Self {
        let keys = values
            .into_iter()
            .map(|value| ApiKey { value, state: KeyState::Active, retry_at: None, remaining: None, used: None })
            .collect();
        KeyPool { keys, next: 0 }
    }

    /// Next usable key, round-robin. Parked keys come back once their cooldown passes.
    fn checkout(&mut self, now: Instant) -> Option<(usize, String)> {
        let n = self.keys.len();
        for i in 0..n {
            let idx = (self.next + i) % n;
            let k = &mut self.keys[idx];
            let usable = match k.state {
                KeyState::Active => true,
                KeyState::Invalid => false,
                KeyState::QuotaExhausted | KeyState::RateLimited => {
                    k.retry_at.map_or(true, |t| now >= t)
                }
            };
            if usable {
                self.next = idx + 1;
                return Some((idx, k.value.clone()));
            }
        }
        None
    }

    fn report(&mut self, idx: usize, outcome: KeyOutcome, now: Instant) {
        let k = &mut self.keys[idx];
        match outcome {
            KeyOutcome::Ok { remaining, used } => {
                k.remaining = remaining.or(k.remaining);
                k.used = used.or(k.used);
                // Last call succeeded but spent the final request: park it.
                if remaining.is_some_and(|r| r <= 0) {
                    (k.state, k.retry_at) = (KeyState::QuotaExhausted, Some(now + QUOTA_COOLDOWN));
                } else {
                    (k.state, k.retry_at) = (KeyState::Active, None);
                }
            }
            KeyOutcome::Invalid => (k.state, k.retry_at) = (KeyState::Invalid, None),
            KeyOutcome::QuotaExhausted => {
                (k.state, k.retry_at) = (KeyState::QuotaExhausted, Some(now + QUOTA_COOLDOWN))
            }
            KeyOutcome::RateLimited => {
                (k.state, k.retry_at) = (KeyState::RateLimited, Some(now + RATE_LIMIT_COOLDOWN))
            }
        }
    }
}

/// Only the last 4 characters, so logs and /keys never leak a usable key.
fn mask(key: &str) -> String {
    let tail: String = key.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    format!("…{tail}")
}

fn parse_keys(raw: &str) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for k in raw.split(',').map(str::trim).filter(|k| !k.is_empty()) {
        if !keys.iter().any(|existing| existing == k) {
            keys.push(k.to_string());
        }
    }
    keys
}

// ---------- The Odds API payloads ----------

#[derive(Deserialize)]
struct ApiEvent {
    id: String,
    commence_time: String,
    home_team: String,
    away_team: String,
    #[serde(default)]
    bookmakers: Vec<ApiBook>,
}

#[derive(Deserialize)]
struct ApiBook {
    key: String,
    title: String,
    last_update: String,
    #[serde(default)]
    markets: Vec<ApiMarket>,
}

#[derive(Deserialize)]
struct ApiMarket {
    key: String,
    outcomes: Vec<ApiOutcome>,
}

#[derive(Deserialize)]
struct ApiOutcome {
    name: String,
    price: f64,
}

// ---------- normalized output ----------

#[derive(Serialize, Clone)]
struct Price {
    name: String,
    price: i64,
}

#[derive(Serialize, Clone)]
struct BookOdds {
    key: String,
    title: String,
    last_update: String,
    outcomes: Vec<Price>,
}

#[derive(Serialize, Clone)]
struct BestPrice {
    name: String,
    price: i64,
    bookmaker: String,
}

#[derive(Serialize, Clone)]
struct Consensus {
    name: String,
    probability: f64,
}

#[derive(Serialize, Clone)]
struct EventOdds {
    id: String,
    commence_time: String,
    home_team: String,
    away_team: String,
    bookmakers: Vec<BookOdds>,
    best: Vec<BestPrice>,
    consensus: Vec<Consensus>,
}

/// Per book, implied probabilities normalized to sum to 1 (removing the vig),
/// then averaged across books for each outcome.
fn consensus(books: &[BookOdds]) -> Vec<Consensus> {
    let mut sums: Vec<(String, f64, usize)> = Vec::new();
    for book in books {
        let implied: Option<Vec<f64>> =
            book.outcomes.iter().map(|o| american_to_decimal(o.price).map(|d| 1.0 / d)).collect();
        let Some(implied) = implied else { continue };
        let total: f64 = implied.iter().sum();
        if total <= 0.0 {
            continue;
        }
        for (o, q) in book.outcomes.iter().zip(implied) {
            match sums.iter_mut().find(|(n, _, _)| *n == o.name) {
                Some(entry) => (entry.1, entry.2) = (entry.1 + q / total, entry.2 + 1),
                None => sums.push((o.name.clone(), q / total, 1)),
            }
        }
    }
    sums.into_iter()
        .map(|(name, sum, count)| Consensus { name, probability: sum / count as f64 })
        .collect()
}

/// Highest American price (best payout) for each outcome across books.
fn best_prices(books: &[BookOdds]) -> Vec<BestPrice> {
    let mut best: Vec<BestPrice> = Vec::new();
    for book in books {
        for o in &book.outcomes {
            match best.iter_mut().find(|b| b.name == o.name) {
                Some(b) if o.price > b.price => {
                    (b.price, b.bookmaker) = (o.price, book.key.clone());
                }
                Some(_) => {}
                None => best.push(BestPrice {
                    name: o.name.clone(),
                    price: o.price,
                    bookmaker: book.key.clone(),
                }),
            }
        }
    }
    best
}

/// Fair American odds from a no-vig probability (favorites negative).
fn american_from_prob(p: f64) -> i64 {
    if p >= 0.5 {
        (-100.0 * p / (1.0 - p)).round() as i64
    } else {
        (100.0 * (1.0 - p) / p).round() as i64
    }
}

/// Line-shopping board: one row per outcome with the best price and the book
/// that offers it, each book's price, the no-vig fair probability/odds, and the
/// `edge` of the best available price vs that fair line (positive = +EV number
/// still on the board). Sorted best-value first.
fn screen_rows(events: &[EventOdds]) -> Vec<serde_json::Value> {
    let mut rows = Vec::new();
    for e in events {
        for bp in &e.best {
            let fair = e.consensus.iter().find(|c| c.name == bp.name).map(|c| c.probability);
            let best_implied = american_to_decimal(bp.price).map(|d| 1.0 / d);
            let edge = match (fair, best_implied) {
                (Some(f), Some(bi)) => Some(f - bi),
                _ => None,
            };
            let mut books = serde_json::Map::new();
            for b in &e.bookmakers {
                if let Some(o) = b.outcomes.iter().find(|o| o.name == bp.name) {
                    books.insert(b.key.clone(), json!(o.price));
                }
            }
            rows.push(json!({
                "game": format!("{} @ {}", e.away_team, e.home_team),
                "commence_time": e.commence_time,
                "outcome": bp.name,
                "best_price": bp.price,
                "best_book": bp.bookmaker,
                "books": serde_json::Value::Object(books),
                "fair_probability": fair,
                "fair_odds": fair.map(american_from_prob),
                "best_implied_probability": best_implied,
                "edge": edge,
            }));
        }
    }
    // Best value (largest edge of the best available price vs fair) first.
    rows.sort_by(|a, b| {
        b["edge"].as_f64().unwrap_or(f64::MIN).total_cmp(&a["edge"].as_f64().unwrap_or(f64::MIN))
    });
    rows
}

fn normalize(events: Vec<ApiEvent>) -> Vec<EventOdds> {
    events
        .into_iter()
        .map(|e| {
            let bookmakers: Vec<BookOdds> = e
                .bookmakers
                .into_iter()
                .filter_map(|b| {
                    let h2h = b.markets.into_iter().find(|m| m.key == "h2h")?;
                    Some(BookOdds {
                        key: b.key,
                        title: b.title,
                        last_update: b.last_update,
                        outcomes: h2h
                            .outcomes
                            .into_iter()
                            .map(|o| Price { name: o.name, price: o.price.round() as i64 })
                            .collect(),
                    })
                })
                .collect();
            EventOdds {
                best: best_prices(&bookmakers),
                consensus: consensus(&bookmakers),
                id: e.id,
                commence_time: e.commence_time,
                home_team: e.home_team,
                away_team: e.away_team,
                bookmakers,
            }
        })
        .collect()
}

// ---------- service ----------

struct OddsApi {
    http: reqwest::Client,
    base_url: String,
    pool: Mutex<KeyPool>,
    cache: Mutex<HashMap<(String, String), (Instant, Vec<EventOdds>)>>,
    cache_ttl: Duration,
}

#[derive(Deserialize)]
struct OddsQuery {
    #[serde(default = "default_regions")]
    regions: String,
}

fn default_regions() -> String {
    "us".into()
}

fn valid_param(s: &str, allow_comma: bool) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || (allow_comma && c == ','))
}

fn error(status: StatusCode, msg: impl std::fmt::Display) -> HttpResponse {
    HttpResponse::build(status).json(json!({ "error": msg.to_string() }))
}

fn header_i64(resp: &reqwest::Response, name: &str) -> Option<i64> {
    // The API reports these as e.g. "487" or "487.0".
    let v = resp.headers().get(name)?.to_str().ok()?;
    v.parse::<f64>().ok().map(|f| f as i64)
}

impl OddsApi {
    async fn fetch(&self, sport: &str, regions: &str) -> Result<Vec<EventOdds>, HttpResponse> {
        let attempts = self.pool.lock().unwrap().keys.len();
        for _ in 0..attempts {
            let Some((idx, key)) = self.pool.lock().unwrap().checkout(Instant::now()) else {
                break;
            };
            let url = format!("{}/sports/{sport}/odds", self.base_url);
            let resp = self
                .http
                .get(&url)
                .query(&[("apiKey", key.as_str()), ("regions", regions), ("markets", "h2h"), ("oddsFormat", "american")])
                .send()
                .await
                .map_err(|e| {
                    // reqwest errors include the URL; strip the query so the key isn't logged.
                    eprintln!("odds-service: request to {url} failed: {}", e.without_url());
                    error(StatusCode::BAD_GATEWAY, "odds provider unreachable")
                })?;
            let status = resp.status();
            let (remaining, used) =
                (header_i64(&resp, "x-requests-remaining"), header_i64(&resp, "x-requests-used"));

            if status.is_success() {
                let events: Vec<ApiEvent> = resp.json().await.map_err(|e| {
                    error(StatusCode::BAD_GATEWAY, format!("unexpected odds payload: {}", e.without_url()))
                })?;
                self.pool.lock().unwrap().report(idx, KeyOutcome::Ok { remaining, used }, Instant::now());
                return Ok(normalize(events));
            }

            let body = resp.text().await.unwrap_or_default();
            let outcome = match status.as_u16() {
                401 if body.to_lowercase().contains("quota") => KeyOutcome::QuotaExhausted,
                401 => KeyOutcome::Invalid,
                429 => KeyOutcome::RateLimited,
                // Bad sport/region etc. is the caller's problem, not the key's.
                400..=499 => {
                    return Err(error(
                        StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_REQUEST),
                        format!("odds provider rejected request: {}", body.chars().take(300).collect::<String>()),
                    ))
                }
                _ => return Err(error(StatusCode::BAD_GATEWAY, format!("odds provider returned {status}"))),
            };
            eprintln!("odds-service: key {} got {status}; rotating", mask(&key));
            self.pool.lock().unwrap().report(idx, outcome, Instant::now());
        }
        Err(error(
            StatusCode::SERVICE_UNAVAILABLE,
            "no usable odds API key (all invalid, out of quota, or rate limited); see GET /keys",
        ))
    }
}

async fn odds(api: web::Data<OddsApi>, sport: web::Path<String>, q: web::Query<OddsQuery>) -> impl Responder {
    if !valid_param(&sport, false) || !valid_param(&q.regions, true) {
        return error(StatusCode::BAD_REQUEST, "sport and regions must be lowercase keys, e.g. baseball_mlb, us");
    }
    let cache_key = (sport.clone(), q.regions.clone());
    if let Some((at, events)) = api.cache.lock().unwrap().get(&cache_key) {
        if at.elapsed() < api.cache_ttl {
            return HttpResponse::Ok().insert_header(("x-cache", "hit")).json(events);
        }
    }
    match api.fetch(&sport, &q.regions).await {
        Ok(events) => {
            api.cache.lock().unwrap().insert(cache_key, (Instant::now(), events.clone()));
            HttpResponse::Ok().insert_header(("x-cache", "miss")).json(events)
        }
        Err(resp) => resp,
    }
}

/// Odds Screen: the line-shopping board for a sport (best number per outcome,
/// each book's price, no-vig fair, and the best price's edge). Same fetch and
/// cache as /odds.
async fn screen(api: web::Data<OddsApi>, sport: web::Path<String>, q: web::Query<OddsQuery>) -> impl Responder {
    if !valid_param(&sport, false) || !valid_param(&q.regions, true) {
        return error(StatusCode::BAD_REQUEST, "sport and regions must be lowercase keys, e.g. baseball_mlb, us");
    }
    let cache_key = (sport.clone(), q.regions.clone());
    if let Some((at, events)) = api.cache.lock().unwrap().get(&cache_key) {
        if at.elapsed() < api.cache_ttl {
            return HttpResponse::Ok().insert_header(("x-cache", "hit")).json(screen_rows(events));
        }
    }
    match api.fetch(&sport, &q.regions).await {
        Ok(events) => {
            let rows = screen_rows(&events);
            api.cache.lock().unwrap().insert(cache_key, (Instant::now(), events));
            HttpResponse::Ok().insert_header(("x-cache", "miss")).json(rows)
        }
        Err(resp) => resp,
    }
}

async fn keys(api: web::Data<OddsApi>) -> impl Responder {
    let now = Instant::now();
    let pool = api.pool.lock().unwrap();
    let keys: Vec<_> = pool
        .keys
        .iter()
        .map(|k| {
            json!({
                "key": mask(&k.value),
                "state": k.state,
                "retry_in_secs": k.retry_at.map(|t| t.saturating_duration_since(now).as_secs()),
                "requests_remaining": k.remaining,
                "requests_used": k.used,
            })
        })
        .collect();
    HttpResponse::Ok().json(keys)
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    match dotenvy::dotenv() {
        Ok(path) => println!("odds-service: loaded {}", path.display()),
        Err(e) if e.not_found() => println!("odds-service: no .env found; using environment only"),
        Err(e) => eprintln!("odds-service: could not parse .env: {e}"),
    }
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8120);
    let raw = std::env::var("ODDS_API_KEYS").or_else(|_| std::env::var("ODDS_API_KEY")).unwrap_or_default();
    let key_list = parse_keys(&raw);
    if key_list.is_empty() {
        let state = |name: &str| match std::env::var(name) {
            Err(_) => "not set",
            Ok(_) => "set but empty",
        };
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!(
                "set ODDS_API_KEYS=key1,key2,... in the environment or .env \
                 (ODDS_API_KEYS is {}, ODDS_API_KEY is {})",
                state("ODDS_API_KEYS"),
                state("ODDS_API_KEY")
            ),
        ));
    }
    let api = web::Data::new(OddsApi {
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?,
        base_url: std::env::var("ODDS_API_URL")
            .unwrap_or_else(|_| "https://api.the-odds-api.com/v4".into())
            .trim_end_matches('/')
            .to_string(),
        cache_ttl: Duration::from_secs(
            std::env::var("ODDS_CACHE_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(60),
        ),
        cache: Mutex::new(HashMap::new()),
        pool: Mutex::new(KeyPool::new(key_list)),
    });
    println!(
        "odds-service listening on 0.0.0.0:{} ({} API keys loaded)",
        port,
        api.pool.lock().unwrap().keys.len()
    );
    HttpServer::new(move || {
        App::new()
            .app_data(api.clone())
            .route("/healthz", web::get().to(healthz))
            .route("/odds/{sport}", web::get().to(odds))
            .route("/screen/{sport}", web::get().to(screen))
            .route("/keys", web::get().to(keys))
    })
    .bind(("0.0.0.0", port))?
    .run()
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(n: usize) -> KeyPool {
        KeyPool::new((0..n).map(|i| format!("key-number-{i}")).collect())
    }

    #[test]
    fn rotates_round_robin() {
        let mut p = pool(3);
        let now = Instant::now();
        let picks: Vec<usize> = (0..4).map(|_| p.checkout(now).unwrap().0).collect();
        assert_eq!(picks, vec![0, 1, 2, 0]);
    }

    #[test]
    fn skips_invalid_and_parked_keys_until_cooldown() {
        let mut p = pool(3);
        let now = Instant::now();
        p.report(0, KeyOutcome::Invalid, now);
        p.report(1, KeyOutcome::RateLimited, now);
        assert_eq!(p.checkout(now).unwrap().0, 2);
        assert_eq!(p.checkout(now).unwrap().0, 2);
        let later = now + RATE_LIMIT_COOLDOWN;
        assert_eq!(p.checkout(later).unwrap().0, 1);
        p.report(2, KeyOutcome::QuotaExhausted, now);
        p.report(1, KeyOutcome::Invalid, now);
        assert!(p.checkout(later).is_none());
        assert_eq!(p.checkout(now + QUOTA_COOLDOWN).unwrap().0, 2);
    }

    #[test]
    fn zero_remaining_parks_key() {
        let mut p = pool(2);
        let now = Instant::now();
        p.report(0, KeyOutcome::Ok { remaining: Some(0), used: Some(500) }, now);
        assert_eq!(p.keys[0].state, KeyState::QuotaExhausted);
        assert_eq!(p.checkout(now).unwrap().0, 1);
        assert_eq!(p.checkout(now).unwrap().0, 1);
    }

    #[test]
    fn masks_and_dedupes_keys() {
        assert_eq!(mask("abcdef123456"), "…3456");
        assert_eq!(parse_keys(" a, b ,,a,c "), vec!["a", "b", "c"]);
    }

    fn book(key: &str, a: i64, b: i64) -> BookOdds {
        BookOdds {
            key: key.into(),
            title: key.into(),
            last_update: String::new(),
            outcomes: vec![Price { name: "A".into(), price: a }, Price { name: "B".into(), price: b }],
        }
    }

    #[test]
    fn consensus_removes_vig() {
        // -110/-110 is a fair coin once the vig is removed.
        let c = consensus(&[book("x", -110, -110)]);
        assert!((c[0].probability - 0.5).abs() < 1e-12);
        let c = consensus(&[book("x", -150, 130), book("y", -140, 120)]);
        assert!((c[0].probability + c[1].probability - 1.0).abs() < 1e-12);
        assert!(c[0].probability > 0.5);
    }

    #[test]
    fn best_price_is_highest_payout() {
        let b = best_prices(&[book("x", -150, 130), book("y", -140, 120)]);
        assert_eq!((b[0].price, b[0].bookmaker.as_str()), (-140, "y"));
        assert_eq!((b[1].price, b[1].bookmaker.as_str()), (130, "x"));
    }

    #[test]
    fn screen_board_flags_best_and_value() {
        let books = vec![book("x", -150, 130), book("y", -140, 120)];
        let ev = EventOdds {
            id: "g1".into(),
            commence_time: "2026-10-11T00:00:00Z".into(),
            home_team: "Home".into(),
            away_team: "Away".into(),
            best: best_prices(&books),
            consensus: consensus(&books),
            bookmakers: books,
        };
        let rows = screen_rows(&[ev]);
        assert_eq!(rows.len(), 2);
        // Row for outcome A: best price is -140 at book y, both books listed.
        let a = rows.iter().find(|r| r["outcome"] == "A").unwrap();
        assert_eq!(a["best_price"], json!(-140));
        assert_eq!(a["best_book"], json!("y"));
        assert_eq!(a["books"]["x"], json!(-150));
        assert_eq!(a["books"]["y"], json!(-140));
        // edge = fair_prob - best_price_implied; present and finite.
        assert!(a["edge"].as_f64().is_some());
        // Sorted best-value first (descending edge).
        assert!(rows[0]["edge"].as_f64() >= rows[1]["edge"].as_f64());
    }
}
