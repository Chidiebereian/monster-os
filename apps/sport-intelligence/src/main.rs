//! sport-intelligence — Smart Signals (sports)
//! Scores recent trades by tracked sharp wallets into 0-100 signals, the way
//! the Smart Signals dashboard does, focused on sports markets.
//!
//! A signal's score blends: wallet quality (realized profit + activity), how
//! much of the wallet's history is in this category (category proof), the
//! trade's size vs the wallet's norm (conviction), entry-vs-current slippage,
//! and recency. Components are returned so each card explains itself.
//!
//! GET /signals?mode=precision|flow&category=sports&min_score=&limit=&max_age_hours=
//!   Reads real trades from ClickHouse `wallet_trades`, wallet performance from
//!   Postgres `trader_performance`, and current prices from prediction-market-
//!   service for slippage. A trade whose market is resolved/unavailable is
//!   "stale" (not tailable) and dropped from Precision.
//!
//! Config: DATABASE_URL, CLICKHOUSE_URL/USER/PASSWORD, PREDICTION_MARKET_URL.
//! Not yet: Consensus signals, Kalshi venue compare, orderbook depth, lifecycle.

use std::collections::HashMap;
use std::sync::Arc;

use actix_web::{web, App, HttpServer, HttpResponse, Responder};
use chrono::{DateTime, Utc};
use common::healthz;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_postgres::{Client, NoTls};

// ---- Category classification -------------------------------------------

const SPORTS_KW: &[&str] = &[
    "nba", "nfl", "nhl", "mlb", "ufc", "mma", "boxing", "soccer", "football",
    "premier-league", "champions-league", "la-liga", "serie-a", "bundesliga",
    "ligue-1", "world-cup", "super-bowl", "world-series", "stanley-cup",
    "nascar", "formula-1", "grand-prix", "pga", "golf", "tennis", "atp", "wta",
    "wimbledon", "cricket", "playoffs", "al-central", "al-east", "al-west",
    "nl-central", "nl-east", "nl-west", "guardians", "yankees", "lakers",
];
const CRYPTO_KW: &[&str] = &["bitcoin", "btc", "ethereum", "crypto", "solana"];
const POLITICS_KW: &[&str] =
    &["election", "president", "senate", "congress", "trump", "biden", "popular-vote", "uranium"];

/// Best-effort category from a market slug/title. A heuristic, not ground truth.
fn classify(slug: &str) -> &'static str {
    let s = slug.to_lowercase();
    let any = |kws: &[&str]| kws.iter().any(|k| s.contains(k));
    if any(SPORTS_KW) {
        "sports"
    } else if any(CRYPTO_KW) {
        "crypto"
    } else if any(POLITICS_KW) {
        "politics"
    } else {
        "other"
    }
}

// ---- Scoring engine (pure) ---------------------------------------------

struct SignalInputs {
    wallet_pnl: f64,
    wallet_trades: i64,
    /// Fraction (0..1) of the wallet's trades in this trade's category.
    category_share: f64,
    /// Reconstructed win-rate (0..1) in this category, when proven; takes
    /// precedence over mere activity share as the category-proof factor.
    category_winrate: Option<f64>,
    entry_price: f64,
    current_price: Option<f64>,
    trade_size: f64,
    wallet_avg_size: f64,
    age_hours: f64,
}

struct Scored {
    score: u32,
    wallet_score: u32,
    relative_size: f64,
    slippage: Option<f64>,
    tailable: bool,
    components: Value,
}

// Weights (sum to 100).
const W_WALLET: f64 = 35.0;
const W_CATEGORY: f64 = 20.0;
const W_SIZE: f64 = 15.0;
const W_SLIP: f64 = 20.0;
const W_RECENCY: f64 = 10.0;
const SLIP_CAP: f64 = 0.10; // 10c of slippage zeroes the entry score
const TAILABLE_SLIP: f64 = 0.08; // beyond this, a signal is no longer clean

/// Wallet quality 0..1: mostly realized profit (log-scaled to $10M), with a
/// smaller activity term so a big one-off doesn't outrank a proven grinder.
fn wallet_quality(pnl: f64, trades: i64) -> f64 {
    let profit = if pnl > 1.0 { (pnl.log10() / 7.0).clamp(0.0, 1.0) } else { 0.0 };
    let activity = (trades as f64 / 500.0).clamp(0.0, 1.0);
    0.8 * profit + 0.2 * activity
}

fn score_signal(i: &SignalInputs) -> Scored {
    let wq = wallet_quality(i.wallet_pnl, i.wallet_trades);
    // Category proof: a proven win-rate in this category beats mere activity share.
    let category_proof =
        i.category_winrate.map(|w| w.clamp(0.0, 1.0)).unwrap_or_else(|| i.category_share.clamp(0.0, 1.0));
    let relative_size = if i.wallet_avg_size > 0.0 { i.trade_size / i.wallet_avg_size } else { 1.0 };
    let size_factor = (relative_size / 3.0).clamp(0.0, 1.0); // 3x+ normal = full marks

    let slippage = i.current_price.map(|c| c - i.entry_price);
    let slip_factor = match slippage {
        Some(s) => 1.0 - (s.abs() / SLIP_CAP).clamp(0.0, 1.0),
        None => 0.0, // no current price => stale, no entry credit
    };
    let recency = 1.0 - (i.age_hours / 168.0).clamp(0.0, 1.0); // decays over a week

    let score = wq * W_WALLET
        + category_proof * W_CATEGORY
        + size_factor * W_SIZE
        + slip_factor * W_SLIP
        + recency * W_RECENCY;

    let tailable = match (i.current_price, slippage) {
        (Some(c), Some(s)) => c > 0.0 && c < 1.0 && s.abs() <= TAILABLE_SLIP,
        _ => false,
    };

    Scored {
        score: score.round() as u32,
        wallet_score: (wq * 100.0).round() as u32,
        relative_size,
        slippage,
        tailable,
        components: json!({
            "wallet": (wq * W_WALLET).round(),
            "category": (category_proof * W_CATEGORY).round(),
            "size": (size_factor * W_SIZE).round(),
            "slippage": (slip_factor * W_SLIP).round(),
            "recency": (recency * W_RECENCY).round(),
        }),
    }
}

/// Consensus strength 0-100 for a market where several sharps are on one side:
/// mostly the average quality of those wallets, plus breadth (how many) and a
/// small bonus when the market is still live (so it's actionable).
fn consensus_score(n_wallets: u32, avg_wallet_quality: f64, has_current: bool) -> u32 {
    let breadth = (n_wallets as f64 / 5.0).clamp(0.0, 1.0); // 5+ sharps = full breadth
    let score = avg_wallet_quality.clamp(0.0, 1.0) * 50.0
        + breadth * 40.0
        + if has_current { 10.0 } else { 0.0 };
    score.round() as u32
}

/// Cost to ENTER `side` from a YES book: YES costs the ask, NO costs 1 - bid
/// (same convention as arbitrage-engine). None if that side has no offer.
fn entry_cost(side: &str, yes_bid: Option<f64>, yes_ask: Option<f64>) -> Option<f64> {
    match side {
        "yes" => yes_ask,
        "no" => yes_bid.map(|b| 1.0 - b),
        _ => None,
    }
}

/// Bid-ask spread as a liquidity proxy (tighter is more liquid).
fn spread(yes_bid: Option<f64>, yes_ask: Option<f64>) -> Option<f64> {
    match (yes_bid, yes_ask) {
        (Some(b), Some(a)) => Some(a - b),
        _ => None,
    }
}

// ---- Polymarket -> Kalshi market matcher --------------------------------
// Both venues embed team abbreviations + date in their identifiers
// (nba-okc-atl-2026-10-12  <->  KXNBAGAME-26OCT12OKCATL-OKC), so we match on
// those rather than needing a nickname<->city alias table.

const MONTHS: [&str; 12] =
    ["JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC"];

fn kalshi_series_for(league: &str) -> Option<&'static str> {
    match league {
        "nba" => Some("KXNBAGAME"),
        "nfl" => Some("KXNFLGAME"),
        "mlb" => Some("KXMLBGAME"),
        "nhl" => Some("KXNHLGAME"),
        _ => None,
    }
}

/// Kalshi's date code, e.g. (2026, 10, 12) -> "26OCT12".
fn kalshi_date_code(y: i32, m: u32, d: u32) -> String {
    format!("{:02}{}{:02}", y.rem_euclid(100), MONTHS[(m - 1) as usize], d)
}

/// Pull (league, team abbreviations, date) out of a Polymarket game slug.
/// Handles trailing junk like "-spread-home-3pt5".
fn parse_poly_slug(slug: &str) -> Option<(String, Vec<String>, (i32, u32, u32))> {
    let parts: Vec<&str> = slug.split('-').collect();
    if parts.len() < 4 {
        return None;
    }
    for i in 1..parts.len().saturating_sub(2) {
        let (Ok(y), Ok(m), Ok(d)) =
            (parts[i].parse::<i32>(), parts[i + 1].parse::<u32>(), parts[i + 2].parse::<u32>())
        else {
            continue;
        };
        if parts[i].len() == 4 && (1..=12).contains(&m) && (1..=31).contains(&d) {
            let teams: Vec<String> = parts[1..i]
                .iter()
                .filter(|t| (2..=4).contains(&t.len()) && t.chars().all(|c| c.is_ascii_alphabetic()))
                .map(|t| t.to_string())
                .collect();
            if teams.len() >= 2 {
                return Some((parts[0].to_string(), teams, (y, m, d)));
            }
        }
    }
    None
}

/// Among Kalshi tickers, the one for this game (contains the date code and all
/// team codes) and, preferably, the side of the first team (the Polymarket YES).
fn match_kalshi_ticker(candidates: &[String], teams: &[String], date_code: &str) -> Option<String> {
    let up: Vec<String> = teams.iter().map(|t| t.to_uppercase()).collect();
    let is_game = |tk: &&String| {
        let u = tk.to_uppercase();
        u.contains(date_code) && up.iter().all(|t| u.contains(t))
    };
    let team0_suffix = format!("-{}", up[0]);
    candidates
        .iter()
        .filter(is_game)
        .find(|tk| tk.to_uppercase().ends_with(&team0_suffix))
        .or_else(|| candidates.iter().find(is_game))
        .cloned()
}

// ---- Service ------------------------------------------------------------

struct State {
    db: Arc<Client>,
    http: reqwest::Client,
    ch_url: String,
    ch_user: Option<String>,
    ch_key: Option<String>,
    pm_url: String,
}

impl State {
    async fn ch(&self, body: String) -> Result<String, String> {
        let mut req = self.http.post(&self.ch_url).body(body);
        if let Some(u) = &self.ch_user {
            req = req.header("X-ClickHouse-User", u);
            if let Some(k) = &self.ch_key {
                req = req.header("X-ClickHouse-Key", k);
            }
        }
        let resp = req.send().await.map_err(|e| format!("ClickHouse request failed: {e}"))?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| format!("ClickHouse body unreadable: {e}"))?;
        if !status.is_success() {
            return Err(format!("ClickHouse returned {status}: {}", text.trim()));
        }
        Ok(text)
    }

    /// Current Polymarket price (yes_ask, else last) for a slug, or None if the
    /// market is resolved/unknown.
    async fn current_price(&self, slug: &str) -> Option<f64> {
        let url = format!("{}/quotes/polymarket/{slug}", self.pm_url);
        let resp = self.http.get(&url).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let v: Value = resp.json().await.ok()?;
        v["yes_ask"].as_f64().or_else(|| v["last"].as_f64()).filter(|p| *p > 0.0 && *p < 1.0)
    }

    /// Full quote (yes_bid/yes_ask/last) for a venue+id via prediction-market-service.
    async fn quote(&self, venue: &str, id: &str) -> Option<VenueQuote> {
        let url = format!("{}/quotes/{venue}/{id}", self.pm_url);
        let resp = self.http.get(&url).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.json::<VenueQuote>().await.ok()
    }

    /// Normalized order book for a market, via prediction-market-service.
    async fn book(&self, venue: &str, id: &str) -> Option<Book> {
        let url = format!("{}/orderbook/{venue}/{id}", self.pm_url);
        let resp = self.http.get(&url).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.json::<Book>().await.ok()
    }

    /// All Kalshi market tickers in a series, via prediction-market-service.
    async fn kalshi_tickers(&self, series: &str) -> Result<Vec<String>, String> {
        let url = format!("{}/markets/kalshi?series_ticker={series}", self.pm_url);
        let resp = self.http.get(&url).send().await.map_err(|e| format!("kalshi list failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("kalshi list returned {}", resp.status()));
        }
        #[derive(Deserialize)]
        struct M {
            market_id: String,
        }
        let ms: Vec<M> = resp.json().await.map_err(|e| format!("kalshi list unreadable: {e}"))?;
        Ok(ms.into_iter().map(|m| m.market_id).collect())
    }

    /// Resolve the Kalshi ticker matching a Polymarket slug (abbreviations + date).
    async fn match_to_kalshi(&self, slug: &str) -> Result<Option<String>, String> {
        let Some((league, teams, (y, m, d))) = parse_poly_slug(slug) else {
            return Ok(None);
        };
        let Some(series) = kalshi_series_for(&league) else {
            return Ok(None);
        };
        let tickers = self.kalshi_tickers(series).await?;
        Ok(match_kalshi_ticker(&tickers, &teams, &kalshi_date_code(y, m, d)))
    }
}

#[derive(Deserialize)]
struct VenueQuote {
    yes_bid: Option<f64>,
    yes_ask: Option<f64>,
}

#[derive(Deserialize)]
struct BookLevel {
    price: f64,
    size: f64,
}

#[derive(Deserialize)]
struct Book {
    #[serde(default)]
    asks: Vec<BookLevel>,
}

/// Total ask-side size resting within `band` of the best ask — i.e. how many
/// shares you can buy near the quoted price (executability). Asks arrive
/// best-first from prediction-market-service.
fn ask_depth_within(asks: &[BookLevel], band: f64) -> Option<f64> {
    let best = asks.first()?.price;
    Some(asks.iter().filter(|l| l.price <= best + band).map(|l| l.size).sum())
}

/// Size resting at the best ask level (top-of-book depth).
fn top_ask_size(asks: &[BookLevel]) -> Option<f64> {
    asks.first().map(|l| l.size)
}

/// An entry is executable if it clears the optional liquidity/top-size gates
/// (monster.bet's min_liquidity / min_top_size). No gate => always true.
fn passes_liquidity(depth: Option<f64>, top: Option<f64>, min_depth: Option<f64>, min_top: Option<f64>) -> bool {
    let depth_ok = min_depth.map_or(true, |m| depth.is_some_and(|d| d >= m));
    let top_ok = min_top.map_or(true, |m| top.is_some_and(|t| t >= m));
    depth_ok && top_ok
}

struct WalletStat {
    pnl: f64,
    trades: i64,
    avg_size: f64,
    category_share: f64,
}

#[derive(Deserialize)]
struct Candidate {
    ts: String,
    wallet: String,
    market_id: String,
    side: String,
    price: f64,
    size: f64,
}

#[derive(Deserialize)]
struct SignalQuery {
    mode: Option<String>,
    category: Option<String>,
    min_score: Option<u32>,
    limit: Option<i64>,
    max_age_hours: Option<i64>,
    /// Enrich each returned card with a Polymarket-vs-Kalshi best-venue compare.
    include_venue: Option<bool>,
}

/// ClickHouse serializes UInt64/Int64 as quoted strings in JSON formats, so a
/// count field may arrive as either a number or a string.
fn json_u64(v: &Value) -> u64 {
    v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())).unwrap_or(0)
}

fn bad_request(msg: impl Into<String>) -> HttpResponse {
    HttpResponse::BadRequest().json(json!({ "error": msg.into() }))
}

fn internal(msg: impl std::fmt::Display) -> HttpResponse {
    eprintln!("sport-intelligence: {msg}");
    HttpResponse::InternalServerError().json(json!({ "error": msg.to_string() }))
}

/// Hex-only wallet, so it is safe to interpolate into the ClickHouse IN list.
fn valid_wallet(w: &str) -> bool {
    w.starts_with("0x") && w.len() == 42 && w[2..].bytes().all(|b| b.is_ascii_hexdigit())
}

/// Score current in-category signals from tracked wallets, highest first.
/// Shared by `/signals` (display) and `/signals/snapshot` (persistence).
async fn compute_signal_cards(
    state: &State,
    mode: &str,
    category: &str,
    min_score: u32,
    max_age: i64,
) -> Result<Vec<Value>, String> {
    // 1) Tracked wallets + latest performance snapshot.
    let (wallets, perf) = load_tracked(&state.db).await?;
    if wallets.is_empty() {
        return Ok(vec![]);
    }
    let in_list = wallets.iter().map(|w| format!("'{w}'")).collect::<Vec<_>>().join(",");

    // 2) Per-wallet avg size + category share (one aggregate, keywords in SQL).
    let kw_array = SPORTS_KW.iter().map(|k| format!("'{k}'")).collect::<Vec<_>>().join(",");
    let stats_sql = format!(
        "SELECT wallet, avg(size) AS avg_size, \
                countIf(multiSearchAny(lower(market_id), [{kw_array}])) AS in_cat, count() AS total \
         FROM monster.wallet_trades WHERE wallet IN ({in_list}) GROUP BY wallet FORMAT JSONEachRow"
    );
    let stats_text = state.ch(stats_sql).await?;
    let mut stats: HashMap<String, WalletStat> = HashMap::new();
    for line in stats_text.lines().filter(|l| !l.trim().is_empty()) {
        let v: Value = serde_json::from_str(line).map_err(|e| format!("bad stats row: {e}"))?;
        let wallet = v["wallet"].as_str().unwrap_or_default().to_string();
        let total = json_u64(&v["total"]).max(1) as f64;
        let in_cat = json_u64(&v["in_cat"]) as f64;
        let (pnl, trades) = perf.get(&wallet).copied().unwrap_or((0.0, 0));
        stats.insert(
            wallet,
            WalletStat { pnl, trades, avg_size: v["avg_size"].as_f64().unwrap_or(0.0), category_share: in_cat / total },
        );
    }

    // 2b) Reconstructed per-category win-rate (proven categories only).
    let cat_rows = state
        .db
        .query(
            "SELECT DISTINCT ON (wallet, category) wallet, category, win_rate \
             FROM trader_category_performance \
             WHERE wallet = ANY($1) AND resolved >= 3 AND win_rate IS NOT NULL \
             ORDER BY wallet, category, as_of DESC",
            &[&wallets],
        )
        .await
        .map_err(|e| e.to_string())?;
    let mut cat_winrate: HashMap<(String, String), f64> = HashMap::new();
    for r in &cat_rows {
        cat_winrate.insert((r.get("wallet"), r.get("category")), r.get("win_rate"));
    }

    // 3) Candidate trades in-category within the age window.
    let cand_sql = format!(
        "SELECT ts, wallet, market_id, side, price, size FROM monster.wallet_trades \
         WHERE wallet IN ({in_list}) \
           AND multiSearchAny(lower(market_id), [{kw_array}]) \
           AND ts > now() - INTERVAL {max_age} HOUR \
         ORDER BY ts DESC LIMIT 500 FORMAT JSONEachRow"
    );
    let cand_text = state.ch(cand_sql).await?;
    let candidates: Vec<Candidate> = cand_text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();

    // 4) Score each, fetching current price per unique market for slippage.
    let mut price_cache: HashMap<String, Option<f64>> = HashMap::new();
    let mut out = Vec::new();
    let now = Utc::now();
    for c in &candidates {
        if category != "any" && classify(&c.market_id) != category {
            continue;
        }
        let Some(ws) = stats.get(&c.wallet) else { continue };
        let current = match price_cache.get(&c.market_id) {
            Some(p) => *p,
            None => {
                let p = state.current_price(&c.market_id).await;
                price_cache.insert(c.market_id.clone(), p);
                p
            }
        };
        let age_hours = DateTime::parse_from_str(&format!("{} +0000", c.ts), "%Y-%m-%d %H:%M:%S%.f %z")
            .map(|t| (now - t.with_timezone(&Utc)).num_minutes() as f64 / 60.0)
            .unwrap_or(f64::MAX);
        let cat = classify(&c.market_id);
        let category_winrate = cat_winrate.get(&(c.wallet.clone(), cat.to_string())).copied();
        let s = score_signal(&SignalInputs {
            wallet_pnl: ws.pnl,
            wallet_trades: ws.trades,
            category_share: ws.category_share,
            category_winrate,
            entry_price: c.price,
            current_price: current,
            trade_size: c.size,
            wallet_avg_size: ws.avg_size,
            age_hours,
        });
        if s.score < min_score {
            continue;
        }
        if mode == "precision" && !s.tailable {
            continue; // Precision only surfaces still-clean, tailable entries
        }
        out.push(json!({
            "score": s.score,
            "wallet": c.wallet,
            "wallet_score": s.wallet_score,
            "category": cat,
            "category_win_rate": category_winrate,
            "market_id": c.market_id,
            "side": c.side,
            "entry_price": c.price,
            "current_price": current,
            "slippage": s.slippage,
            "trade_size": c.size,
            "position_value": c.size * c.price,
            "relative_bet_size": (s.relative_size * 100.0).round() / 100.0,
            "tailable": s.tailable,
            "ts": c.ts,
            "components": s.components,
        }));
    }
    out.sort_by(|a, b| b["score"].as_u64().cmp(&a["score"].as_u64()));
    Ok(out)
}

async fn signals(state: web::Data<State>, q: web::Query<SignalQuery>) -> impl Responder {
    let mode = q.mode.clone().unwrap_or_else(|| "precision".into());
    // Precision: fewer, cleaner, higher bar. Flow: broader.
    let default_min = match mode.as_str() {
        "precision" => 70,
        "flow" => 50,
        other => return bad_request(format!("mode must be precision|flow, got {other:?}")),
    };
    let min_score = q.min_score.unwrap_or(default_min);
    let category = q.category.clone().unwrap_or_else(|| "sports".into());
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let max_age = q.max_age_hours.unwrap_or(720).clamp(1, 24 * 365); // default 30 days

    let mut out = match compute_signal_cards(&state, &mode, &category, min_score, max_age).await {
        Ok(o) => o,
        Err(e) => return internal(e),
    };
    out.truncate(limit as usize);

    // Optionally enrich the final cards with the cleaner venue (Kalshi auto-matched).
    // Done after truncation so at most `limit` cross-venue lookups happen.
    if q.include_venue.unwrap_or(false) {
        for card in out.iter_mut() {
            let Some(slug) = card["market_id"].as_str().map(String::from) else { continue };
            let poly_cost = card["current_price"].as_f64(); // yes_ask = buy-YES entry
            let kalshi_ticker = state.match_to_kalshi(&slug).await.ok().flatten();
            let kalshi_cost = match &kalshi_ticker {
                Some(tk) => state.quote("kalshi", tk).await.and_then(|v| entry_cost("yes", v.yes_bid, v.yes_ask)),
                None => None,
            };
            let best = match (poly_cost, kalshi_cost) {
                (Some(p), Some(k)) if p <= k => "polymarket",
                (Some(_), Some(_)) => "kalshi",
                (Some(_), None) => "polymarket",
                (None, Some(_)) => "kalshi",
                (None, None) => "unknown",
            };
            let saving = match (poly_cost, kalshi_cost) {
                (Some(p), Some(k)) => Some(((p - k).abs() * 1000.0).round() / 1000.0),
                _ => None,
            };
            if let Some(obj) = card.as_object_mut() {
                obj.insert(
                    "venue".into(),
                    json!({
                        "polymarket_cost": poly_cost,
                        "kalshi_ticker": kalshi_ticker,
                        "kalshi_cost": kalshi_cost,
                        "best_venue": best,
                        "entry_saving": saving,
                    }),
                );
            }
        }
    }

    HttpResponse::Ok().json(json!({
        "mode": mode,
        "category": category,
        "min_score": min_score,
        "count": out.len(),
        "signals": out,
    }))
}

/// Latest performance snapshot per tracked wallet: (valid wallets, wallet -> (pnl, trades)).
async fn load_tracked(db: &Client) -> Result<(Vec<String>, HashMap<String, (f64, i64)>), String> {
    let rows = db
        .query(
            "SELECT DISTINCT ON (wallet) wallet, pnl, trades \
             FROM trader_performance ORDER BY wallet, as_of DESC",
            &[],
        )
        .await
        .map_err(|e| e.to_string())?;
    let mut perf = HashMap::new();
    let mut wallets = Vec::new();
    for r in &rows {
        let w: String = r.get("wallet");
        if !valid_wallet(&w) {
            continue;
        }
        let pnl: Option<f64> = r.get("pnl");
        let trades: Option<i32> = r.get("trades");
        perf.insert(w.clone(), (pnl.unwrap_or(0.0), trades.unwrap_or(0) as i64));
        wallets.push(w);
    }
    Ok((wallets, perf))
}

#[derive(Deserialize)]
struct ConsensusRow {
    market_id: String,
    side: String,
    n_wallets: Value,
    wallets: Vec<String>,
    avg_entry: f64,
    total_size: f64,
}

#[derive(Deserialize)]
struct ConsensusQuery {
    category: Option<String>,
    min_wallets: Option<u32>,
    limit: Option<i64>,
    max_age_hours: Option<i64>,
}

/// Markets where several tracked sharp wallets are positioned on the same side.
async fn consensus(state: web::Data<State>, q: web::Query<ConsensusQuery>) -> impl Responder {
    let category = q.category.clone().unwrap_or_else(|| "sports".into());
    let min_wallets = q.min_wallets.unwrap_or(2).clamp(2, 50);
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let max_age = q.max_age_hours.unwrap_or(720).clamp(1, 24 * 365);

    let (wallets, perf) = match load_tracked(&state.db).await {
        Ok(t) => t,
        Err(e) => return internal(e),
    };
    if wallets.is_empty() {
        return HttpResponse::Ok().json(json!({ "category": category, "consensus": [] }));
    }
    let in_list = wallets.iter().map(|w| format!("'{w}'")).collect::<Vec<_>>().join(",");
    let kw_array = SPORTS_KW.iter().map(|k| format!("'{k}'")).collect::<Vec<_>>().join(",");

    // Group same-side positions per market; a sharp counts once per side.
    let sql = format!(
        "SELECT market_id, side, uniqExact(wallet) AS n_wallets, \
                groupUniqArray(wallet) AS wallets, avg(price) AS avg_entry, sum(size) AS total_size \
         FROM monster.wallet_trades \
         WHERE wallet IN ({in_list}) \
           AND multiSearchAny(lower(market_id), [{kw_array}]) \
           AND ts > now() - INTERVAL {max_age} HOUR \
         GROUP BY market_id, side \
         HAVING n_wallets >= {min_wallets} \
         ORDER BY n_wallets DESC LIMIT 200 FORMAT JSONEachRow"
    );
    let text = match state.ch(sql).await {
        Ok(t) => t,
        Err(e) => return internal(e),
    };
    let rows: Vec<ConsensusRow> =
        text.lines().filter(|l| !l.trim().is_empty()).filter_map(|l| serde_json::from_str(l).ok()).collect();

    let mut price_cache: HashMap<String, Option<f64>> = HashMap::new();
    let mut out = Vec::new();
    for r in &rows {
        if category != "any" && classify(&r.market_id) != category {
            continue;
        }
        let qualities: Vec<f64> = r
            .wallets
            .iter()
            .filter_map(|w| perf.get(w))
            .map(|(pnl, trades)| wallet_quality(*pnl, *trades))
            .collect();
        let avg_q = if qualities.is_empty() {
            0.0
        } else {
            qualities.iter().sum::<f64>() / qualities.len() as f64
        };
        let current = match price_cache.get(&r.market_id) {
            Some(p) => *p,
            None => {
                let p = state.current_price(&r.market_id).await;
                price_cache.insert(r.market_id.clone(), p);
                p
            }
        };
        let n = json_u64(&r.n_wallets) as u32;
        out.push(json!({
            "score": consensus_score(n, avg_q, current.is_some()),
            "market_id": r.market_id,
            "side": r.side,
            "category": classify(&r.market_id),
            "num_wallets": n,
            "wallets": r.wallets,
            "avg_wallet_score": (avg_q * 100.0).round() as u32,
            "avg_entry": (r.avg_entry * 1000.0).round() / 1000.0,
            "current_price": current,
            "total_size": r.total_size,
            "tailable": current.map(|c| c > 0.0 && c < 1.0).unwrap_or(false),
        }));
    }
    out.sort_by(|a, b| b["score"].as_u64().cmp(&a["score"].as_u64()));
    out.truncate(limit as usize);

    HttpResponse::Ok().json(json!({
        "category": category,
        "min_wallets": min_wallets,
        "count": out.len(),
        "consensus": out,
    }))
}

#[derive(Deserialize)]
struct MatchQuery {
    polymarket: String,
}

/// Resolve the Kalshi ticker equivalent to a Polymarket slug.
async fn match_market(state: web::Data<State>, q: web::Query<MatchQuery>) -> impl Responder {
    let parsed = parse_poly_slug(&q.polymarket);
    let kalshi = match state.match_to_kalshi(&q.polymarket).await {
        Ok(k) => k,
        Err(e) => return internal(e),
    };
    let (league, teams, date_code) = match &parsed {
        Some((l, t, (y, m, d))) => (Some(l.clone()), t.clone(), Some(kalshi_date_code(*y, *m, *d))),
        None => (None, vec![], None),
    };
    HttpResponse::Ok().json(json!({
        "polymarket": q.polymarket,
        "league": league,
        "teams": teams,
        "date_code": date_code,
        "kalshi_series": league.as_deref().and_then(kalshi_series_for),
        "kalshi_ticker": kalshi,
        "matched": kalshi.is_some(),
    }))
}

#[derive(Deserialize)]
struct BestVenueQuery {
    /// Polymarket slug; the Kalshi ticker is matched automatically unless given.
    polymarket: String,
    kalshi: Option<String>,
    side: Option<String>,
    /// Optional executability gates (monster.bet's min_liquidity / min_top_size):
    /// shares available within 2c of the ask, and at the best ask level.
    min_depth: Option<f64>,
    min_top: Option<f64>,
}

fn round2(x: Option<f64>) -> Option<f64> {
    x.map(|d| (d * 100.0).round() / 100.0)
}

fn venue_json(
    q: &Option<VenueQuote>,
    side: &str,
    ask_depth_2c: Option<f64>,
    top_size: Option<f64>,
    executable: bool,
) -> Value {
    match q {
        Some(v) => json!({
            "available": true,
            "entry_cost": entry_cost(side, v.yes_bid, v.yes_ask),
            "yes_bid": v.yes_bid,
            "yes_ask": v.yes_ask,
            "spread": spread(v.yes_bid, v.yes_ask),
            "ask_depth_2c": round2(ask_depth_2c),
            "top_size": round2(top_size),
            "executable": executable,
        }),
        None => json!({ "available": false }),
    }
}

/// Compare the same market on Polymarket and Kalshi and point to the cleaner
/// entry (lower cost for the side), with each book's spread as a liquidity cue.
/// The two ids must be supplied by the caller — cross-venue market matching is
/// a separate problem and isn't attempted here.
async fn best_venue(state: web::Data<State>, q: web::Query<BestVenueQuery>) -> impl Responder {
    let side = q.side.clone().unwrap_or_else(|| "yes".into());
    if side != "yes" && side != "no" {
        return bad_request("side must be yes|no");
    }
    // Use the caller's Kalshi ticker, else auto-match from the Polymarket slug.
    let kalshi_id = match &q.kalshi {
        Some(k) => Some(k.clone()),
        None => match state.match_to_kalshi(&q.polymarket).await {
            Ok(k) => k,
            Err(e) => return internal(e),
        },
    };
    // Ask-side depth within 2c + size at best ask = how executable the entry is.
    let poly_book = state.book("polymarket", &q.polymarket).await;
    let poly_depth = poly_book.as_ref().and_then(|b| ask_depth_within(&b.asks, 0.02));
    let poly_top = poly_book.as_ref().and_then(|b| top_ask_size(&b.asks));
    let poly_exec = passes_liquidity(poly_depth, poly_top, q.min_depth, q.min_top);

    let Some(kalshi_id) = kalshi_id else {
        return HttpResponse::Ok().json(json!({
            "side": side,
            "polymarket": venue_json(&state.quote("polymarket", &q.polymarket).await, &side, poly_depth, poly_top, poly_exec),
            "kalshi": { "available": false, "reason": "no matching Kalshi market found" },
            "best_venue": "polymarket",
            "matched_kalshi": null,
        }));
    };
    let (poly, kalshi) = (state.quote("polymarket", &q.polymarket).await, state.quote("kalshi", &kalshi_id).await);
    let kalshi_book = state.book("kalshi", &kalshi_id).await;
    let kalshi_depth = kalshi_book.as_ref().and_then(|b| ask_depth_within(&b.asks, 0.02));
    let kalshi_top = kalshi_book.as_ref().and_then(|b| top_ask_size(&b.asks));
    let kalshi_exec = passes_liquidity(kalshi_depth, kalshi_top, q.min_depth, q.min_top);

    let poly_cost = poly.as_ref().and_then(|v| entry_cost(&side, v.yes_bid, v.yes_ask));
    let kalshi_cost = kalshi.as_ref().and_then(|v| entry_cost(&side, v.yes_bid, v.yes_ask));

    // Only venues that clear the liquidity gates are candidates for "best". If a
    // gate is set and neither clears it, fall back to price alone and flag it.
    let gated = q.min_depth.is_some() || q.min_top.is_some();
    let poly_ok = poly_cost.filter(|_| !gated || poly_exec);
    let kalshi_ok = kalshi_cost.filter(|_| !gated || kalshi_exec);
    let (best, saving) = match (poly_ok, kalshi_ok) {
        (Some(p), Some(k)) if p <= k => (Some("polymarket"), Some(k - p)),
        (Some(p), Some(k)) => (Some("kalshi"), Some(p - k)),
        (Some(_), None) => (Some("polymarket"), None),
        (None, Some(_)) => (Some("kalshi"), None),
        // Neither clears the gate: pick the cheaper anyway, caller sees executable=false.
        (None, None) => match (poly_cost, kalshi_cost) {
            (Some(p), Some(k)) if p <= k => (Some("polymarket"), Some(k - p)),
            (Some(p), Some(k)) => (Some("kalshi"), Some(p - k)),
            (Some(_), None) => (Some("polymarket"), None),
            (None, Some(_)) => (Some("kalshi"), None),
            (None, None) => (None, None),
        },
    };

    HttpResponse::Ok().json(json!({
        "side": side,
        "matched_kalshi": kalshi_id,
        "polymarket": venue_json(&poly, &side, poly_depth, poly_top, poly_exec),
        "kalshi": venue_json(&kalshi, &side, kalshi_depth, kalshi_top, kalshi_exec),
        "best_venue": best,
        "best_venue_executable": best.map(|b| if b == "polymarket" { poly_exec } else { kalshi_exec }),
        "entry_saving": saving.map(|s| (s * 1000.0).round() / 1000.0),
    }))
}

// ---- Signal lifecycle: persist active signals, retire stale ones --------

#[derive(Deserialize)]
struct SnapshotQuery {
    mode: Option<String>,
    category: Option<String>,
    min_score: Option<u32>,
    max_age_hours: Option<i64>,
}

/// Recompute signals and reconcile them with the `signals` table: still-tailable
/// signals are upserted active; previously-active ones that no longer qualify
/// are moved to history. Idempotent. Returns (upserted_active, retired).
async fn run_snapshot(
    state: &State,
    mode: &str,
    category: &str,
    min_score: u32,
    max_age: i64,
) -> Result<(u64, u64), String> {
    let cards = compute_signal_cards(state, mode, category, min_score, max_age).await?;
    // Timestamp before upserts: rows not refreshed past this are retired below.
    let start: DateTime<Utc> =
        state.db.query_one("SELECT now()", &[]).await.map_err(|e| e.to_string())?.get(0);

    let mut upserted = 0u64;
    for c in &cards {
        let (Some(wallet), Some(market_id), Some(side)) =
            (c["wallet"].as_str(), c["market_id"].as_str(), c["side"].as_str())
        else {
            continue;
        };
        let category = c["category"].as_str();
        let entry_price = c["entry_price"].as_f64();
        let current_price = c["current_price"].as_f64();
        let score = c["score"].as_i64().map(|s| s as i32);
        // entry_price/first_seen are set once on insert and preserved on update.
        state
            .db
            .execute(
                "INSERT INTO signals (wallet, market_id, side, category, entry_price, current_price, score) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7) \
                 ON CONFLICT (wallet, market_id, side) DO UPDATE SET \
                   current_price = EXCLUDED.current_price, score = EXCLUDED.score, \
                   category = EXCLUDED.category, status = 'active', last_seen = now(), \
                   close_reason = NULL, closed_at = NULL",
                &[&wallet, &market_id, &side, &category, &entry_price, &current_price, &score],
            )
            .await
            .map_err(|e| e.to_string())?;
        upserted += 1;
    }

    // Any still-active signal we did not just refresh has gone stale/resolved/too far.
    let retired = state
        .db
        .execute(
            "UPDATE signals SET status = 'history', \
               close_reason = 'no longer tail-able (stale, resolved, or too far from entry)', \
               closed_at = now() \
             WHERE status = 'active' AND last_seen < $1",
            &[&start],
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok((upserted, retired))
}

async fn snapshot(state: web::Data<State>, q: web::Query<SnapshotQuery>) -> impl Responder {
    let mode = q.mode.clone().unwrap_or_else(|| "precision".into());
    let min_score = q.min_score.unwrap_or(if mode == "flow" { 50 } else { 70 });
    let category = q.category.clone().unwrap_or_else(|| "sports".into());
    let max_age = q.max_age_hours.unwrap_or(720).clamp(1, 24 * 365);
    match run_snapshot(&state, &mode, &category, min_score, max_age).await {
        Ok((upserted, retired)) => {
            HttpResponse::Ok().json(json!({ "active_upserted": upserted, "retired_to_history": retired }))
        }
        Err(e) => internal(e),
    }
}

fn signal_row(r: &tokio_postgres::Row) -> Value {
    json!({
        "id": r.get::<_, i64>("id"),
        "wallet": r.get::<_, String>("wallet"),
        "market_id": r.get::<_, String>("market_id"),
        "side": r.get::<_, String>("side"),
        "category": r.get::<_, Option<String>>("category"),
        "entry_price": r.get::<_, Option<f64>>("entry_price"),
        "current_price": r.get::<_, Option<f64>>("current_price"),
        "score": r.get::<_, Option<i32>>("score"),
        "status": r.get::<_, String>("status"),
        "close_reason": r.get::<_, Option<String>>("close_reason"),
        "first_seen": r.get::<_, DateTime<Utc>>("first_seen").to_rfc3339(),
        "last_seen": r.get::<_, DateTime<Utc>>("last_seen").to_rfc3339(),
        "closed_at": r.get::<_, Option<DateTime<Utc>>>("closed_at").map(|t| t.to_rfc3339()),
    })
}

#[derive(Deserialize)]
struct ListSignalsQuery {
    limit: Option<i64>,
}

const SIGNAL_COLS: &str = "id, wallet, market_id, side, category, entry_price, \
    current_price, score, status, close_reason, first_seen, last_seen, closed_at";

async fn active_signals(state: web::Data<State>, q: web::Query<ListSignalsQuery>) -> impl Responder {
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    let sql = format!("SELECT {SIGNAL_COLS} FROM signals WHERE status='active' ORDER BY score DESC NULLS LAST LIMIT $1");
    match state.db.query(&sql, &[&limit]).await {
        Ok(rows) => HttpResponse::Ok().json(rows.iter().map(signal_row).collect::<Vec<_>>()),
        Err(e) => internal(e),
    }
}

async fn signal_history(state: web::Data<State>, q: web::Query<ListSignalsQuery>) -> impl Responder {
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    let sql = format!("SELECT {SIGNAL_COLS} FROM signals WHERE status='history' ORDER BY closed_at DESC NULLS LAST LIMIT $1");
    match state.db.query(&sql, &[&limit]).await {
        Ok(rows) => HttpResponse::Ok().json(rows.iter().map(signal_row).collect::<Vec<_>>()),
        Err(e) => internal(e),
    }
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8119);
    let url = std::env::var("DATABASE_URL")
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "DATABASE_URL must be set"))?;
    let (client, connection) = tokio_postgres::connect(&url, NoTls)
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    actix_web::rt::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("sport-intelligence: postgres connection closed: {e}");
        }
    });

    let env_opt = |n: &str| std::env::var(n).ok().filter(|v| !v.is_empty());
    let state = web::Data::new(State {
        db: Arc::new(client),
        http: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?,
        ch_url: env_opt("CLICKHOUSE_URL").unwrap_or_else(|| "http://localhost:8123/".into()),
        ch_user: env_opt("CLICKHOUSE_USER"),
        ch_key: env_opt("CLICKHOUSE_PASSWORD"),
        pm_url: env_opt("PREDICTION_MARKET_URL")
            .unwrap_or_else(|| "http://localhost:8105".into())
            .trim_end_matches('/')
            .to_string(),
    });

    // Opt-in background loop that keeps the active/history feeds current without
    // an external cron. Set SNAPSHOT_INTERVAL_SECS to enable it.
    if let Some(secs) =
        env_opt("SNAPSHOT_INTERVAL_SECS").and_then(|v| v.parse::<u64>().ok()).filter(|n| *n > 0)
    {
        let st = state.clone();
        actix_web::rt::spawn(async move {
            loop {
                actix_web::rt::time::sleep(std::time::Duration::from_secs(secs)).await;
                match run_snapshot(&st, "precision", "sports", 70, 720).await {
                    Ok((u, r)) => println!("sport-intelligence: auto-snapshot upserted {u} active, retired {r}"),
                    Err(e) => eprintln!("sport-intelligence: auto-snapshot error: {e}"),
                }
            }
        });
        println!("sport-intelligence: auto-snapshot every {secs}s");
    }

    println!("sport-intelligence listening on 0.0.0.0:{port}");
    HttpServer::new(move || {
        App::new()
            .app_data(state.clone())
            .route("/healthz", web::get().to(healthz))
            .route("/signals", web::get().to(signals))
            .route("/consensus", web::get().to(consensus))
            .route("/best-venue", web::get().to(best_venue))
            .route("/match", web::get().to(match_market))
            .route("/signals/snapshot", web::post().to(snapshot))
            .route("/signals/active", web::get().to(active_signals))
            .route("/signals/history", web::get().to(signal_history))
    })
    .bind(("0.0.0.0", port))?
    .run()
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_buckets() {
        assert_eq!(classify("ufc-fight-night-fares-ziam-vs-tom-nolan"), "sports");
        assert_eq!(classify("will-cleveland-guardians-win-the-2026-al-central-title"), "sports");
        assert_eq!(classify("will-bitcoin-dip-to-55000-in-june"), "crypto");
        assert_eq!(classify("will-a-republican-win-the-popular-vote"), "politics");
        assert_eq!(classify("something-random"), "other");
    }

    fn base() -> SignalInputs {
        SignalInputs {
            wallet_pnl: 10_000_000.0, // log10=7 -> full profit term
            wallet_trades: 500,
            category_share: 1.0,
            category_winrate: None,
            entry_price: 0.33,
            current_price: Some(0.35),
            trade_size: 3000.0,
            wallet_avg_size: 1000.0, // 3x conviction
            age_hours: 0.0,
        }
    }

    #[test]
    fn category_winrate_overrides_activity_share() {
        // Low activity share but a strong proven win-rate -> category proof is high.
        let mut i = base();
        i.category_share = 0.1;
        i.category_winrate = Some(0.85);
        let strong = score_signal(&i);
        // Same wallet with no proven win-rate falls back to the low share.
        i.category_winrate = None;
        let weak = score_signal(&i);
        assert!(strong.score > weak.score, "{} !> {}", strong.score, weak.score);
        // The category component should reflect ~0.85 * 20 = 17.
        assert_eq!(strong.components["category"], json!(17.0));
    }

    #[test]
    fn strong_signal_scores_high_and_tailable() {
        let s = score_signal(&base());
        assert!(s.score >= 90, "score {}", s.score);
        assert!(s.tailable);
        assert_eq!(s.wallet_score, 100);
        assert!((s.slippage.unwrap() - 0.02).abs() < 1e-9);
    }

    #[test]
    fn stale_without_current_price_is_not_tailable() {
        let mut i = base();
        i.current_price = None;
        let s = score_signal(&i);
        assert!(!s.tailable);
        assert!(s.slippage.is_none());
        // Loses the 20-pt slippage component vs the strong case.
        assert!(s.score < score_signal(&base()).score);
    }

    #[test]
    fn big_slippage_breaks_tailability() {
        let mut i = base();
        i.current_price = Some(0.45); // 12c > 8c tailable threshold
        let s = score_signal(&i);
        assert!(!s.tailable);
    }

    #[test]
    fn weak_wallet_scores_low() {
        let mut i = base();
        i.wallet_pnl = 0.0;
        i.wallet_trades = 0;
        i.category_share = 0.0;
        let s = score_signal(&i);
        assert_eq!(s.wallet_score, 0);
        assert!(s.score < 50, "score {}", s.score);
    }

    #[test]
    fn relative_size_reflects_conviction() {
        let mut i = base();
        i.trade_size = 500.0; // half normal
        let s = score_signal(&i);
        assert!((s.relative_size - 0.5).abs() < 1e-9);
    }

    #[test]
    fn consensus_rises_with_wallets_and_quality() {
        // 5 top-quality sharps on a live market = near-max.
        assert!(consensus_score(5, 1.0, true) >= 95);
        // More wallets beats fewer, all else equal.
        assert!(consensus_score(4, 0.8, true) > consensus_score(2, 0.8, true));
        // Higher wallet quality beats lower.
        assert!(consensus_score(3, 0.9, true) > consensus_score(3, 0.5, true));
        // A stale (not-live) market loses the actionability bonus.
        assert!(consensus_score(3, 0.8, false) < consensus_score(3, 0.8, true));
    }

    #[test]
    fn json_u64_parses_number_or_string() {
        assert_eq!(json_u64(&json!(7)), 7);
        assert_eq!(json_u64(&json!("7")), 7); // ClickHouse UInt64-as-string
        assert_eq!(json_u64(&json!(null)), 0);
    }

    #[test]
    fn entry_cost_by_side() {
        // YES costs the ask; NO costs 1 - bid.
        assert_eq!(entry_cost("yes", Some(0.33), Some(0.35)), Some(0.35));
        assert_eq!(entry_cost("no", Some(0.33), Some(0.35)), Some(1.0 - 0.33));
        assert_eq!(entry_cost("yes", Some(0.33), None), None);
        assert_eq!(entry_cost("no", None, Some(0.35)), None);
    }

    #[test]
    fn spread_needs_both_sides() {
        assert_eq!(spread(Some(0.33), Some(0.35)), Some(0.35 - 0.33));
        assert_eq!(spread(None, Some(0.35)), None);
    }

    #[test]
    fn ask_depth_sums_within_band() {
        let asks = vec![
            BookLevel { price: 0.38, size: 100.0 },
            BookLevel { price: 0.39, size: 50.0 },  // within 2c of 0.38
            BookLevel { price: 0.41, size: 999.0 }, // outside 2c
        ];
        assert_eq!(ask_depth_within(&asks, 0.02), Some(150.0));
        assert_eq!(ask_depth_within(&[], 0.02), None);
        assert_eq!(top_ask_size(&asks), Some(100.0));
        assert_eq!(top_ask_size(&[]), None);
    }

    #[test]
    fn liquidity_gates() {
        // No gate set -> always passes.
        assert!(passes_liquidity(Some(10.0), Some(5.0), None, None));
        // Depth gate: 150 >= 100 ok; 50 < 100 fails.
        assert!(passes_liquidity(Some(150.0), Some(20.0), Some(100.0), None));
        assert!(!passes_liquidity(Some(50.0), Some(20.0), Some(100.0), None));
        // Top-size gate: 20 >= 25 fails.
        assert!(!passes_liquidity(Some(150.0), Some(20.0), None, Some(25.0)));
        // Missing book data fails a set gate (can't confirm executability).
        assert!(!passes_liquidity(None, None, Some(100.0), None));
    }

    #[test]
    fn parse_slug_pulls_teams_and_date() {
        let (l, teams, date) = parse_poly_slug("nba-okc-atl-2026-10-12").unwrap();
        assert_eq!(l, "nba");
        assert_eq!(teams, vec!["okc", "atl"]);
        assert_eq!(date, (2026, 10, 12));
        // Trailing junk after the date is ignored.
        let (_, teams, date) = parse_poly_slug("nfl-nyg-was-2026-10-11-spread-home-3pt5").unwrap();
        assert_eq!(teams, vec!["nyg", "was"]);
        assert_eq!(date, (2026, 10, 11));
        // Non-game slugs don't parse.
        assert!(parse_poly_slug("will-bitcoin-dip-to-55000-in-june").is_none());
    }

    #[test]
    fn kalshi_date_code_format() {
        assert_eq!(kalshi_date_code(2026, 10, 12), "26OCT12");
        assert_eq!(kalshi_date_code(2026, 1, 5), "26JAN05");
    }

    #[test]
    fn matches_kalshi_ticker_by_teams_and_date() {
        let cands = vec![
            "KXNBAGAME-26OCT12OKCATL-OKC".to_string(),
            "KXNBAGAME-26OCT12OKCATL-ATL".to_string(),
            "KXNBAGAME-26OCT15OKCHOU-OKC".to_string(), // wrong date + opponent
        ];
        // First team (okc) = Polymarket YES side.
        assert_eq!(
            match_kalshi_ticker(&cands, &["okc".into(), "atl".into()], "26OCT12").as_deref(),
            Some("KXNBAGAME-26OCT12OKCATL-OKC")
        );
        // No match on a different date.
        assert!(match_kalshi_ticker(&cands, &["okc".into(), "atl".into()], "26OCT20").is_none());
    }
}
