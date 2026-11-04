//! Smart Signals: tail-able trades from Polymarket wallets that have proven
//! themselves in the category of the trade, scored 0-100.
//!
//! A cycle (every PM_SIGNALS_INTERVAL_SECS, default 600; or POST /pm/signals/run):
//!  1. Sharp set: the top 50 all-time and top 50 this-month P&L wallets in each
//!     of Polymarket's 10 categories (month entries must also be up all-time in
//!     that category). Refreshed every 6 h into `pm_standings`.
//!  2. Poll each sharp's latest 100 trades into `pm_sharp_trades`, and note its
//!     normal (median) trade size and pace (bot check).
//!  3. Group the last 48 h of buys per (wallet, outcome token) into a position:
//!     VWAP entry, dollars, shares, sells since.
//!  4. For each: event category (from Polymarket event tags), the wallet's
//!     all-time standing in THAT category and overall (looked up if unknown),
//!     the live order book (best ask, dollars within 2c), then score.
//!  5. Upsert `pm_signals`; anything no longer tail-able moves to history with
//!     the reason, and resolved markets get their result so the feed keeps a
//!     track record.
//!
//! GET  /pm/signals?mode=precision|flow&category=&min_score=&limit=
//! GET  /pm/signals/consensus?category=&min_wallets=
//! GET  /pm/signals/history?category=&limit=
//! GET  /pm/signals/record            hit rate and ROI of past signals, by mode
//! GET  /pm/signals/chart?asset=&range=1d|1w|1m
//! GET  /pm/signals/status
//! POST /pm/signals/run?wait=true

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

use actix_web::{web, HttpResponse};
use chrono::{DateTime, Utc};
use futures::stream::{self, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::pm::{category_param, get_json, num, standing, text, urlencode};
use crate::{bad_request, internal, State};

pub const SIGNAL_CATS: [&str; 10] = [
    "POLITICS", "SPORTS", "CRYPTO", "CULTURE", "MENTIONS", "WEATHER", "ECONOMICS", "TECH", "FINANCE", "ESPORTS",
];
const BOARD_N: usize = 50;
const STANDINGS_TTL_H: i64 = 6;
const PROBE_TTL_H: i64 = 24;
const LOOKBACK_H: i64 = 48;
const MAX_CANDIDATES: i64 = 600;
const MIN_POSITION_USD: f64 = 100.0;
const POLL_PER_CYCLE: i64 = 400;

pub(crate) fn clob_url() -> &'static str {
    static U: OnceLock<String> = OnceLock::new();
    U.get_or_init(|| {
        std::env::var("POLYMARKET_CLOB_URL")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "https://clob.polymarket.com".into())
            .trim_end_matches('/')
            .to_string()
    })
}

// ---------------- pure parts ----------------

/// Map Polymarket event tags to a leaderboard category, most specific first
/// (esports events are also tagged Sports; Fed markets also tagged Trump).
pub fn category_from_tags(tags: &[String]) -> &'static str {
    let has = |xs: &[&str]| tags.iter().any(|t| xs.iter().any(|x| t.eq_ignore_ascii_case(x)));
    if has(&["Esports"]) {
        "ESPORTS"
    } else if has(&["Mentions"]) {
        "MENTIONS"
    } else if has(&["Weather", "Temperature", "Climate"]) {
        "WEATHER"
    } else if has(&["Crypto", "Crypto Prices", "Bitcoin", "Ethereum", "Solana"]) {
        "CRYPTO"
    } else if has(&["Sports"]) {
        "SPORTS"
    } else if has(&["Economy", "Economics", "Fed", "Fed Rates", "Inflation", "Macro"]) {
        "ECONOMICS"
    } else if has(&["Finance", "Business", "Stocks", "Earnings", "IPOs", "Commodities"]) {
        "FINANCE"
    } else if has(&["Tech", "AI", "Science", "Big Tech"]) {
        "TECH"
    } else if has(&["Politics", "Geopolitics", "Elections", "US Election", "World", "Trump"]) {
        "POLITICS"
    } else if has(&["Culture", "Pop Culture", "Movies", "Music", "Awards", "Entertainment", "Celebrities"]) {
        "CULTURE"
    } else {
        "OVERALL"
    }
}

fn clamp01(x: f64) -> f64 {
    if x.is_nan() { 0.0 } else { x.clamp(0.0, 1.0) }
}
/// 1.0 for rank 1, 0 at rank 5,000 and beyond (log scale).
fn rank_score(rank: Option<i32>) -> f64 {
    match rank {
        Some(r) if r >= 1 => clamp01(1.0 - (r as f64).ln() / 5000f64.ln()),
        _ => 0.0,
    }
}

#[derive(Debug, Clone, Default)]
pub struct Inputs {
    pub cat_rank: Option<i32>,
    pub cat_pnl: Option<f64>,
    pub cat_vol: Option<f64>,
    pub overall_rank: Option<i32>,
    pub overall_pnl: Option<f64>,
    pub entry: f64,
    pub current: Option<f64>,
    pub rel_size: f64,
    pub usdc: f64,
    pub depth_2c: f64,
    pub age_h: f64,
}

#[derive(Debug, Clone)]
pub struct Scored {
    pub score: i32,
    pub wallet_score: i32,
    pub proven: bool,
    pub arber: bool,
    pub slippage: Option<f64>,
    pub parts: Value,
}

/// The 0-100 signal score. Weights:
///   category proof 30 · overall record 10 · slippage 20 · relative size 15 ·
///   position value 5 · liquidity 10 · recency 5 · entry band 5.
pub fn score(i: &Inputs) -> Scored {
    let cat_pnl = i.cat_pnl.unwrap_or(0.0);
    let roi = match i.cat_vol { Some(v) if v > 0.0 => cat_pnl / v, _ => 0.0 };
    let proven = cat_pnl > 0.0 && i.cat_rank.is_some();
    let proof = if proven { 0.6 * rank_score(i.cat_rank) + 0.4 * clamp01(roi / 0.08) } else { 0.0 };
    let overall = if i.overall_pnl.unwrap_or(0.0) > 0.0 { 0.5 + 0.5 * rank_score(i.overall_rank) } else { 0.0 };
    let slippage = i.current.map(|c| c - i.entry);
    // Paying more than the trader did costs fully; a big drop below their entry
    // also counts (half weight): the market has moved against them, usually on news.
    let slip = match slippage {
        None => 0.0,
        Some(s) => {
            let cost = if s > 0.0 { s } else { -s / 2.0 };
            if cost <= 0.005 { 1.0 } else { clamp01(1.0 - (cost - 0.005) / 0.055) }
        }
    };
    let size = if i.rel_size >= 1.0 {
        clamp01(0.3 + 0.7 * i.rel_size.ln() / 10f64.ln())
    } else {
        0.3 * i.rel_size.max(0.0)
    };
    let value = clamp01((i.usdc / 100.0).ln() / 100f64.ln());
    let liq = if i.depth_2c <= 200.0 { 0.0 } else { clamp01((i.depth_2c / 200.0).ln() / 25f64.ln()) };
    let recency = clamp01(1.0 - i.age_h / LOOKBACK_H as f64);
    let band = if (0.08..=0.92).contains(&i.entry) { 1.0 } else { 0.3 };
    let pts = [
        ("category_proof", proof, 30.0),
        ("overall_record", overall, 10.0),
        ("slippage", slip, 20.0),
        ("relative_size", size, 15.0),
        ("position_value", value, 5.0),
        ("liquidity", liq, 10.0),
        ("recency", recency, 5.0),
        ("entry_band", band, 5.0),
    ];
    let total: f64 = pts.iter().map(|(_, s, w)| s * w).sum();
    let parts = Value::Object(pts.iter().map(|(k, s, w)| (k.to_string(), json!((s * w * 10.0).round() / 10.0))).collect());
    Scored {
        score: total.round() as i32,
        wallet_score: (100.0 * (0.75 * proof + 0.25 * overall)).round() as i32,
        proven,
        // Huge volume at a tiny return on it: market making / arbitrage, not a view.
        arber: i.cat_vol.unwrap_or(0.0) > 5e6 && roi < 0.005,
        slippage,
        parts,
    }
}

/// Median of a slice (None when empty).
pub fn median(xs: &mut [f64]) -> Option<f64> {
    if xs.is_empty() {
        return None;
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = xs.len();
    Some(if n % 2 == 1 { xs[n / 2] } else { (xs[n / 2 - 1] + xs[n / 2]) / 2.0 })
}

/// Best ask and dollars offered within 2c of it, from a CLOB book. Polymarket
/// returns asks best-last, but sort anyway.
pub fn ask_side(book: &Value) -> (Option<f64>, f64) {
    let mut asks: Vec<(f64, f64)> = book["asks"]
        .as_array()
        .map(|a| a.iter().filter_map(|l| Some((num(l, "price")?, num(l, "size")?))).collect())
        .unwrap_or_default();
    asks.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let Some(&(best, _)) = asks.first() else { return (None, 0.0) };
    let depth = asks.iter().filter(|(p, _)| *p <= best + 0.02 + 1e-9).map(|(p, s)| p * s).sum();
    (Some(best), depth)
}

/// Modes. Precision: proven in the category, no bot/arber flag, clean entry
/// (-5c..+3c from the trader's entry), real depth, score >= 70. Flow: score >=
/// 50, -10c..+8c.
pub fn modes(s: &Scored, depth_2c: f64, bot: bool) -> (bool, bool) {
    let slip = s.slippage.unwrap_or(f64::INFINITY);
    let precision = s.proven && !s.arber && !bot && (-0.05..=0.03).contains(&slip) && depth_2c >= 500.0 && s.score >= 70;
    let flow = (-0.10..=0.08).contains(&slip) && s.score >= 50;
    (precision, flow)
}

// ---------------- cycle ----------------

fn status_cell() -> &'static Mutex<Value> {
    static S: OnceLock<Mutex<Value>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(json!({"state": "never run"})))
}
fn running() -> &'static Mutex<bool> {
    static R: OnceLock<Mutex<bool>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(false))
}

async fn refresh_boards(state: &State) -> Result<usize, String> {
    let fresh: Option<DateTime<Utc>> = state
        .db
        .query_one("SELECT max(refreshed_at) FROM pm_standings WHERE source = 'board'", &[])
        .await
        .map_err(|e| e.to_string())?
        .get(0);
    if fresh.map(|t| Utc::now() - t < chrono::Duration::hours(STANDINGS_TTL_H)).unwrap_or(false) {
        return Ok(0);
    }
    let mut n = 0;
    for cat in SIGNAL_CATS {
        let all = get_json(state, &format!(
            "{}/v1/leaderboard?category={cat}&timePeriod=ALL&orderBy=PNL&limit={BOARD_N}", state.pm_url)).await?;
        let month = get_json(state, &format!(
            "{}/v1/leaderboard?category={cat}&timePeriod=MONTH&orderBy=PNL&limit={BOARD_N}", state.pm_url)).await?;
        let mut rows: HashMap<String, (Option<i32>, f64, f64, Option<f64>, Option<String>, Option<String>)> = HashMap::new();
        for r in all.as_array().into_iter().flatten() {
            if let Some(w) = text(r, "proxyWallet") {
                rows.insert(w, (num(r, "rank").map(|x| x as i32), num(r, "pnl").unwrap_or(0.0), num(r, "vol").unwrap_or(0.0),
                    None, text(r, "userName"), text(r, "profileImage")));
            }
        }
        // Month leaders not already in the all-time top: need their all-time standing.
        let month_rows: Vec<Value> = month.as_array().cloned().unwrap_or_default();
        let month_pnl: HashMap<String, f64> = month_rows.iter()
            .filter_map(|r| Some((text(r, "proxyWallet")?, num(r, "pnl")?))).collect();
        let missing: Vec<(String, Option<String>, Option<String>)> = month_rows.iter()
            .filter_map(|r| {
                let w = text(r, "proxyWallet")?;
                (!rows.contains_key(&w)).then(|| (w, text(r, "userName"), text(r, "profileImage")))
            })
            .collect();
        let looked: Vec<_> = stream::iter(missing)
            .map(|(w, name, img)| async move { let s = standing(state, &w, cat, "ALL").await; (w, name, img, s) })
            .buffer_unordered(8)
            .collect()
            .await;
        for (w, name, img, s) in looked {
            if let Some((rank, pnl, vol)) = s {
                if pnl > 0.0 {
                    rows.insert(w, (Some(rank), pnl, vol, None, name, img));
                }
            }
        }
        for (w, (rank, pnl, vol, _, name, img)) in &rows {
            let mp = month_pnl.get(w).copied();
            state.db.execute(
                "INSERT INTO pm_standings (wallet, category, rank, pnl, vol, month_pnl, source, refreshed_at)
                 VALUES ($1,$2,$3,$4,$5,$6,'board',now())
                 ON CONFLICT (wallet, category) DO UPDATE SET rank=EXCLUDED.rank, pnl=EXCLUDED.pnl, vol=EXCLUDED.vol,
                     month_pnl=EXCLUDED.month_pnl, source='board', refreshed_at=now()",
                &[w, &cat, rank, pnl, vol, &mp],
            ).await.map_err(|e| e.to_string())?;
            state.db.execute(
                "INSERT INTO pm_sharp_wallets (wallet, name, profile_image) VALUES ($1,$2,$3)
                 ON CONFLICT (wallet) DO UPDATE SET name=COALESCE(EXCLUDED.name, pm_sharp_wallets.name),
                     profile_image=COALESCE(EXCLUDED.profile_image, pm_sharp_wallets.profile_image)",
                &[w, name, img],
            ).await.map_err(|e| e.to_string())?;
            n += 1;
        }
    }
    // Drop board rows that fell off every list this refresh (they become probes).
    state.db.execute(
        "UPDATE pm_standings SET source='probe' WHERE source='board' AND refreshed_at < now() - interval '1 minute'", &[],
    ).await.map_err(|e| e.to_string())?;
    Ok(n)
}

async fn poll_wallet(state: &State, wallet: &str) -> Result<usize, String> {
    let url = format!("{}/trades?user={wallet}&limit=100&takerOnly=false", state.pm_url);
    let trades = get_json(state, &url).await?.as_array().cloned().unwrap_or_default();
    let cutoff = Utc::now().timestamp() - 72 * 3600;
    // Normal bet = median dollars put into one market (outcome token) across
    // these trades. The API returns fills, and one order often fills in many
    // small pieces, so a median fill would make every position look huge.
    let mut per_asset: HashMap<String, f64> = HashMap::new();
    let (mut tmin, mut tmax) = (i64::MAX, i64::MIN);
    let mut inserted = 0;
    for t in &trades {
        let (Some(price), Some(size), Some(ts)) = (num(t, "price"), num(t, "size"), num(t, "timestamp")) else { continue };
        let ts = ts as i64;
        if text(t, "side").map(|x| x.eq_ignore_ascii_case("BUY")).unwrap_or(false) {
            *per_asset.entry(text(t, "asset").unwrap_or_default()).or_default() += price * size;
        }
        tmin = tmin.min(ts);
        tmax = tmax.max(ts);
        if ts < cutoff {
            continue;
        }
        let at = DateTime::<Utc>::from_timestamp(ts, 0);
        let n = state.db.execute(
            "INSERT INTO pm_sharp_trades (tx_hash, asset, wallet, side, price, size, ts, condition_id, outcome, title, slug, event_slug, icon)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) ON CONFLICT DO NOTHING",
            &[&text(t, "transactionHash").unwrap_or_default(), &text(t, "asset").unwrap_or_default(), &wallet,
              &text(t, "side").unwrap_or_default().to_uppercase(), &price, &size, &at,
              &text(t, "conditionId"), &text(t, "outcome"), &text(t, "title"), &text(t, "slug"), &text(t, "eventSlug"), &text(t, "icon")],
        ).await.map_err(|e| e.to_string())?;
        inserted += n as usize;
    }
    let mut sizes: Vec<f64> = per_asset.into_values().collect();
    let med = median(&mut sizes);
    let span = if tmax > tmin { Some((tmax - tmin) as f64 / 60.0) } else { None };
    // 100 fills inside 30 minutes at a small median ticket: an automated strategy.
    let bot = trades.len() >= 100 && span.map(|s| s < 30.0).unwrap_or(false) && med.unwrap_or(0.0) < 100.0;
    let seen = trades.len() as i32;
    state.db.execute(
        "INSERT INTO pm_sharp_wallets (wallet, median_trade, trades_seen, span_minutes, bot, polled_at)
         VALUES ($1,$2,$3,$4,$5,now())
         ON CONFLICT (wallet) DO UPDATE SET median_trade=EXCLUDED.median_trade, trades_seen=EXCLUDED.trades_seen,
             span_minutes=EXCLUDED.span_minutes, bot=EXCLUDED.bot, polled_at=now()",
        &[&wallet, &med, &seen, &span, &bot],
    ).await.map_err(|e| e.to_string())?;
    Ok(inserted)
}

pub(crate) async fn event_category(state: &State, slug: &str, cache: &Mutex<HashMap<String, String>>) -> String {
    if let Some(c) = cache.lock().unwrap().get(slug) {
        return c.clone();
    }
    if let Ok(row) = state.db.query_opt("SELECT category FROM pm_event_cats WHERE event_slug=$1", &[&slug]).await {
        if let Some(r) = row {
            let c: String = r.get(0);
            cache.lock().unwrap().insert(slug.to_string(), c.clone());
            return c;
        }
    }
    let url = format!("{}/events?slug={}", state.gamma_url, urlencode(slug));
    let tags: Vec<String> = get_json(state, &url).await.ok()
        .and_then(|v| v.as_array().and_then(|a| a.first().cloned()))
        .map(|e| e["tags"].as_array().map(|t| t.iter().filter_map(|x| text(x, "label")).collect()).unwrap_or_default())
        .unwrap_or_default();
    let cat = category_from_tags(&tags).to_string();
    let _ = state.db.execute(
        "INSERT INTO pm_event_cats (event_slug, category, tags) VALUES ($1,$2,$3) ON CONFLICT (event_slug) DO NOTHING",
        &[&slug, &cat, &json!(tags)],
    ).await;
    cache.lock().unwrap().insert(slug.to_string(), cat.clone());
    cat
}

/// (rank, pnl, vol) for a wallet in a category: stored, or looked up and stored.
async fn get_standing(state: &State, wallet: &str, cat: &str) -> Option<(Option<i32>, f64, f64)> {
    if let Ok(Some(r)) = state.db.query_opt(
        "SELECT rank, pnl, vol FROM pm_standings WHERE wallet=$1 AND category=$2
           AND (source='board' OR refreshed_at > now() - make_interval(hours => $3::int))",
        &[&wallet, &cat, &(PROBE_TTL_H as i32)],
    ).await {
        return Some((r.get(0), r.get::<_, Option<f64>>(1).unwrap_or(0.0), r.get::<_, Option<f64>>(2).unwrap_or(0.0)));
    }
    let s = standing(state, wallet, cat, "ALL").await;
    let (rank, pnl, vol) = match s { Some((r, p, v)) => (Some(r), p, v), None => (None, 0.0, 0.0) };
    let _ = state.db.execute(
        "INSERT INTO pm_standings (wallet, category, rank, pnl, vol, source, refreshed_at) VALUES ($1,$2,$3,$4,$5,'probe',now())
         ON CONFLICT (wallet, category) DO UPDATE SET rank=EXCLUDED.rank, pnl=EXCLUDED.pnl, vol=EXCLUDED.vol, refreshed_at=now()
         WHERE pm_standings.source <> 'board'",
        &[&wallet, &cat, &rank, &pnl, &vol],
    ).await;
    Some((rank, pnl, vol))
}

pub(crate) async fn book(state: &State, asset: &str) -> Option<Value> {
    let resp = state.http.get(format!("{}/book?token_id={asset}", clob_url())).send().await.ok()?;
    if !resp.status().is_success() {
        return None; // 404 "No orderbook exists": market closed / resolved
    }
    resp.json().await.ok()
}

/// One full cycle. Returns a summary.
pub async fn run_cycle(state: &State) -> Result<Value, String> {
    let started = Utc::now();
    let boards = refresh_boards(state).await?;

    // Rotate: each cycle polls the POLL_PER_CYCLE sharps polled longest ago,
    // one request at a time at ~6/s. Polymarket's Cloudflare bans an IP for a
    // while (error 1015) after bursts, so on a hard 429 the cycle stops polling
    // and scores what it has.
    let wallets: Vec<String> = state.db.query(
        "SELECT s.wallet FROM pm_standings s LEFT JOIN pm_sharp_wallets w ON w.wallet = s.wallet
          WHERE s.source='board' AND s.pnl > 0
          GROUP BY s.wallet ORDER BY max(w.polled_at) NULLS FIRST, min(s.rank) LIMIT $1",
        &[&POLL_PER_CYCLE],
    ).await.map_err(|e| e.to_string())?.iter().map(|r| r.get(0)).collect();
    let mut polled: Vec<Result<usize, String>> = Vec::new();
    for w in &wallets {
        let r = poll_wallet(state, w).await;
        let throttled = matches!(&r, Err(e) if e.contains("429") || e.contains("throttled"));
        polled.push(r);
        if throttled {
            break;
        }
        actix_web::rt::time::sleep(std::time::Duration::from_millis(160)).await;
    }
    let new_trades: usize = polled.iter().filter_map(|r| r.as_ref().ok()).sum();
    let poll_errors = polled.iter().filter(|r| r.is_err()).count();
    for e in polled.iter().filter_map(|r| r.as_ref().err()).take(3) {
        crate::log_err(format!("signals poll: {e}"));
    }
    state.db.execute("DELETE FROM pm_sharp_trades WHERE ts < now() - interval '7 days'", &[]).await.map_err(|e| e.to_string())?;

    let cands = state.db.query(
        "SELECT t.wallet, t.asset, max(t.condition_id), max(t.outcome), max(t.title), max(t.slug), max(t.event_slug), max(t.icon),
                sum(t.price*t.size) FILTER (WHERE t.side='BUY') AS usdc,
                sum(t.size) FILTER (WHERE t.side='BUY') AS shares,
                coalesce(sum(t.size) FILTER (WHERE t.side='SELL'), 0) AS sold,
                min(t.ts) FILTER (WHERE t.side='BUY'), max(t.ts) FILTER (WHERE t.side='BUY'),
                max(w.median_trade), bool_or(coalesce(w.bot, false))
           FROM pm_sharp_trades t LEFT JOIN pm_sharp_wallets w ON w.wallet = t.wallet
          WHERE t.ts > now() - make_interval(hours => $1::int)
          GROUP BY t.wallet, t.asset
         HAVING sum(t.price*t.size) FILTER (WHERE t.side='BUY') >= $2
          -- live signals are always re-checked; new positions fill the rest by size
          ORDER BY EXISTS (SELECT 1 FROM pm_signals p WHERE p.wallet = t.wallet AND p.asset = t.asset
                            AND p.status = 'active') DESC, usdc DESC
          LIMIT $3",
        &[&(LOOKBACK_H as i32), &MIN_POSITION_USD, &MAX_CANDIDATES],
    ).await.map_err(|e| e.to_string())?;

    let cat_cache = Mutex::new(HashMap::new());
    let books: Mutex<HashMap<String, Option<Value>>> = Mutex::new(HashMap::new());
    let assets: HashSet<String> = cands.iter().map(|r| r.get::<_, String>(1)).collect();
    let fetched: Vec<(String, Option<Value>)> = stream::iter(assets)
        .map(|a| async move { let b = book(state, &a).await; (a, b) })
        .buffer_unordered(10)
        .collect()
        .await;
    books.lock().unwrap().extend(fetched);

    let (mut active, mut retired) = (0, 0);
    for r in &cands {
        let wallet: String = r.get(0);
        let asset: String = r.get(1);
        let condition: Option<String> = r.get(2);
        let outcome: Option<String> = r.get(3);
        let title: Option<String> = r.get(4);
        let slug: Option<String> = r.get(5);
        let event_slug: Option<String> = r.get(6);
        let icon: Option<String> = r.get(7);
        let usdc: f64 = r.get::<_, Option<f64>>(8).unwrap_or(0.0);
        let shares: f64 = r.get::<_, Option<f64>>(9).unwrap_or(0.0);
        let sold: f64 = r.get(10);
        let first: Option<DateTime<Utc>> = r.get(11);
        let last: Option<DateTime<Utc>> = r.get(12);
        let med: Option<f64> = r.get(13);
        let bot: bool = r.get::<_, Option<bool>>(14).unwrap_or(false);
        if shares <= 0.0 {
            continue;
        }
        let entry = usdc / shares;
        let category = match event_slug.as_deref() {
            Some(s) => event_category(state, s, &cat_cache).await,
            None => "OVERALL".to_string(),
        };
        let cat_st = get_standing(state, &wallet, &category).await;
        let all_st = get_standing(state, &wallet, "OVERALL").await;
        let bk = books.lock().unwrap().get(&asset).cloned().flatten();
        let (best_ask, depth) = bk.as_ref().map(ask_side).unwrap_or((None, 0.0));
        let age_h = last.map(|l| (Utc::now() - l).num_minutes() as f64 / 60.0).unwrap_or(99.0);
        let inp = Inputs {
            cat_rank: cat_st.and_then(|s| s.0), cat_pnl: cat_st.map(|s| s.1), cat_vol: cat_st.map(|s| s.2),
            overall_rank: all_st.and_then(|s| s.0), overall_pnl: all_st.map(|s| s.1),
            entry, current: best_ask, rel_size: med.filter(|m| *m > 0.0).map(|m| usdc / m).unwrap_or(1.0),
            usdc, depth_2c: depth, age_h,
        };
        let s = score(&inp);
        let (precision, flow) = modes(&s, depth, bot);
        let mut flags = Vec::new();
        if !s.proven { flags.push("unproven_in_category"); }
        if s.arber { flags.push("arber_or_maker"); }
        if bot { flags.push("bot_pace"); }
        // Why it is (or is no longer) tail-able.
        let close_reason = if bk.is_none() {
            Some("market closed or resolved")
        } else if sold >= 0.8 * shares {
            Some("trader exited")
        } else if best_ask.map(|a| a >= 0.98 || a <= 0.02).unwrap_or(false) {
            Some("no room left in price")
        } else if s.slippage.map(|x| x > 0.10).unwrap_or(false) {
            Some("price ran away from entry")
        } else if s.slippage.map(|x| x < -0.15).unwrap_or(false) {
            Some("price collapsed below entry")
        } else if !flow {
            Some("below flow threshold")
        } else {
            None
        };
        let flags_v = json!(flags);
        if let Some(reason) = close_reason {
            // Only retire rows that were active; never create a history row for a
            // position that was never a signal.
            let n = state.db.execute(
                "UPDATE pm_signals SET status='history', close_reason=$3, closed_at=now(), current_price=$4
                  WHERE wallet=$1 AND asset=$2 AND status='active'",
                &[&wallet, &asset, &reason, &best_ask],
            ).await.map_err(|e| e.to_string())?;
            retired += n;
            continue;
        }
        state.db.execute(
            "INSERT INTO pm_signals (wallet, asset, condition_id, category, title, slug, event_slug, icon, outcome,
                 entry_price, entry_usdc, entry_shares, first_trade_at, last_trade_at, current_price, price_at_signal,
                 slippage, depth_2c, rel_size, wallet_score, score, best_score, precision_ok, flow_ok, parts, flags,
                 status, first_seen, last_seen)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$15,$16,$17,$18,$19,$20,$20,$21,$22,$23,$24,'active',now(),now())
             ON CONFLICT (wallet, asset) DO UPDATE SET
                 category=EXCLUDED.category, entry_price=EXCLUDED.entry_price, entry_usdc=EXCLUDED.entry_usdc,
                 entry_shares=EXCLUDED.entry_shares, last_trade_at=EXCLUDED.last_trade_at,
                 current_price=EXCLUDED.current_price, slippage=EXCLUDED.slippage, depth_2c=EXCLUDED.depth_2c,
                 rel_size=EXCLUDED.rel_size, wallet_score=EXCLUDED.wallet_score, score=EXCLUDED.score,
                 best_score=GREATEST(pm_signals.best_score, EXCLUDED.score),
                 precision_ok=EXCLUDED.precision_ok, flow_ok=EXCLUDED.flow_ok, parts=EXCLUDED.parts, flags=EXCLUDED.flags,
                 last_seen=now(),
                 -- a re-entry after history starts a fresh signal
                 price_at_signal = CASE WHEN pm_signals.status='history' THEN EXCLUDED.price_at_signal ELSE pm_signals.price_at_signal END,
                 first_seen = CASE WHEN pm_signals.status='history' THEN now() ELSE pm_signals.first_seen END,
                 result = CASE WHEN pm_signals.status='history' THEN NULL ELSE pm_signals.result END,
                 status='active', close_reason=NULL, closed_at=NULL",
            &[&wallet, &asset, &condition, &category, &title, &slug, &event_slug, &icon, &outcome,
              &entry, &usdc, &shares, &first, &last, &best_ask, &s.slippage, &depth, &inp.rel_size,
              &s.wallet_score, &s.score, &precision, &flow, &s.parts, &flags_v],
        ).await.map_err(|e| e.to_string())?;
        active += 1;
    }
    // Active signals whose position fell out of the 48 h window are stale.
    retired += state.db.execute(
        "UPDATE pm_signals SET status='history', close_reason='stale: no new buys in 48 h', closed_at=now()
          WHERE status='active' AND last_seen < $1", &[&started],
    ).await.map_err(|e| e.to_string())?;
    let resolved = resolve(state).await.unwrap_or(0);

    Ok(json!({
        "started": started, "seconds": (Utc::now() - started).num_seconds(),
        "board_rows": boards, "wallets": wallets.len(), "polled": polled.len(), "new_trades": new_trades, "poll_errors": poll_errors,
        "candidates": cands.len(), "active": active, "retired": retired, "resolved": resolved,
    }))
}

/// Fill in won/lost for signals whose market has resolved.
async fn resolve(state: &State) -> Result<usize, String> {
    let rows = state.db.query(
        "SELECT DISTINCT asset FROM pm_signals WHERE result IS NULL
            AND (status='history' OR last_seen < now() - interval '1 hour') LIMIT 100", &[],
    ).await.map_err(|e| e.to_string())?;
    let mut n = 0;
    for r in rows {
        let asset: String = r.get(0);
        let url = format!("{}/markets?clob_token_ids={asset}&closed=true", state.gamma_url);
        let Ok(v) = get_json(state, &url).await else { continue };
        let Some(m) = v.as_array().and_then(|a| a.first()) else { continue };
        if !m["closed"].as_bool().unwrap_or(false) {
            continue;
        }
        let parse = |k: &str| -> Vec<String> {
            m[k].as_str().and_then(|s| serde_json::from_str::<Vec<String>>(s).ok()).unwrap_or_default()
        };
        let (ids, prices) = (parse("clobTokenIds"), parse("outcomePrices"));
        let Some(i) = ids.iter().position(|x| *x == asset) else { continue };
        let Some(p) = prices.get(i).and_then(|p| p.parse::<f64>().ok()) else { continue };
        let result = if p >= 0.99 { "won" } else if p <= 0.01 { "lost" } else { "void" };
        n += state.db.execute(
            "UPDATE pm_signals SET result=$2, final_price=$3,
                 status='history', close_reason=COALESCE(close_reason, 'resolved'), closed_at=COALESCE(closed_at, now())
              WHERE asset=$1 AND result IS NULL",
            &[&asset, &result, &p],
        ).await.map_err(|e| e.to_string())? as usize;
    }
    Ok(n)
}

/// Background loop started from main.
pub fn spawn_loop(state: web::Data<State>) {
    let secs: u64 = std::env::var("PM_SIGNALS_INTERVAL_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(600);
    if secs == 0 {
        return;
    }
    actix_web::rt::spawn(async move {
        loop {
            trigger(&state).await;
            actix_web::rt::time::sleep(std::time::Duration::from_secs(secs)).await;
        }
    });
}

async fn trigger(state: &State) -> Value {
    {
        let mut r = running().lock().unwrap();
        if *r {
            return json!({"state": "already running"});
        }
        *r = true;
    }
    *status_cell().lock().unwrap() = json!({"state": "running", "since": Utc::now()});
    let out = match run_cycle(state).await {
        Ok(v) => json!({"state": "ok", "last": v}),
        Err(e) => {
            crate::log_err(format!("signals cycle: {e}"));
            json!({"state": "error", "error": e, "at": Utc::now()})
        }
    };
    *status_cell().lock().unwrap() = out.clone();
    *running().lock().unwrap() = false;
    out
}

// ---------------- HTTP ----------------

#[derive(Deserialize)]
pub struct RunQ {
    #[serde(default)]
    wait: bool,
}

pub async fn run_http(state: web::Data<State>, q: web::Query<RunQ>) -> HttpResponse {
    if q.wait {
        return HttpResponse::Ok().json(trigger(&state).await);
    }
    let s = state.clone();
    actix_web::rt::spawn(async move { trigger(&s).await; });
    HttpResponse::Accepted().json(json!({"state": "started"}))
}

pub async fn status(_state: web::Data<State>) -> HttpResponse {
    HttpResponse::Ok().json(status_cell().lock().unwrap().clone())
}

const SIG_COLS: &str = "s.id, s.wallet, s.asset, s.category, s.title, s.slug, s.event_slug, s.icon, s.outcome,
    s.entry_price, s.entry_usdc, s.entry_shares, s.first_trade_at, s.last_trade_at, s.current_price, s.price_at_signal,
    s.slippage, s.depth_2c, s.rel_size, s.wallet_score, s.score, s.best_score, s.precision_ok, s.flow_ok, s.parts,
    s.flags, s.status, s.close_reason, s.first_seen, s.last_seen, s.closed_at, s.result, s.final_price,
    w.name, w.profile_image, w.median_trade, cs.rank, cs.pnl, cs.vol, os.rank, os.pnl";
const SIG_FROM: &str = "FROM pm_signals s
    LEFT JOIN pm_sharp_wallets w ON w.wallet = s.wallet
    LEFT JOIN pm_standings cs ON cs.wallet = s.wallet AND cs.category = s.category
    LEFT JOIN pm_standings os ON os.wallet = s.wallet AND os.category = 'OVERALL'";

fn sig_json(r: &tokio_postgres::Row) -> Value {
    let cat_pnl: Option<f64> = r.get(37);
    let cat_vol: Option<f64> = r.get(38);
    json!({
        "id": r.get::<_, i64>(0), "wallet": r.get::<_, String>(1), "asset": r.get::<_, String>(2),
        "category": r.get::<_, Option<String>>(3), "title": r.get::<_, Option<String>>(4),
        "slug": r.get::<_, Option<String>>(5), "event_slug": r.get::<_, Option<String>>(6),
        "icon": r.get::<_, Option<String>>(7), "outcome": r.get::<_, Option<String>>(8),
        "entry_price": r.get::<_, Option<f64>>(9), "entry_usdc": r.get::<_, Option<f64>>(10),
        "entry_shares": r.get::<_, Option<f64>>(11),
        "first_trade_at": r.get::<_, Option<DateTime<Utc>>>(12), "last_trade_at": r.get::<_, Option<DateTime<Utc>>>(13),
        "current_price": r.get::<_, Option<f64>>(14), "price_at_signal": r.get::<_, Option<f64>>(15),
        "slippage": r.get::<_, Option<f64>>(16), "depth_2c": r.get::<_, Option<f64>>(17),
        "rel_size": r.get::<_, Option<f64>>(18), "wallet_score": r.get::<_, Option<i32>>(19),
        "score": r.get::<_, Option<i32>>(20), "best_score": r.get::<_, Option<i32>>(21),
        "precision": r.get::<_, Option<bool>>(22), "flow": r.get::<_, Option<bool>>(23),
        "parts": r.get::<_, Option<Value>>(24), "flags": r.get::<_, Option<Value>>(25),
        "status": r.get::<_, String>(26), "close_reason": r.get::<_, Option<String>>(27),
        "first_seen": r.get::<_, DateTime<Utc>>(28), "last_seen": r.get::<_, DateTime<Utc>>(29),
        "closed_at": r.get::<_, Option<DateTime<Utc>>>(30), "result": r.get::<_, Option<String>>(31),
        "final_price": r.get::<_, Option<f64>>(32),
        "name": r.get::<_, Option<String>>(33), "profile_image": r.get::<_, Option<String>>(34),
        "normal_bet": r.get::<_, Option<f64>>(35),
        "cat_rank": r.get::<_, Option<i32>>(36), "cat_pnl": cat_pnl, "cat_vol": cat_vol,
        "cat_roi": match (cat_pnl, cat_vol) { (Some(p), Some(v)) if v > 0.0 => Some(p / v), _ => None },
        "overall_rank": r.get::<_, Option<i32>>(39), "overall_pnl": r.get::<_, Option<f64>>(40),
    })
}

#[derive(Deserialize)]
pub struct SigQ {
    mode: Option<String>,
    category: Option<String>,
    min_score: Option<i32>,
    limit: Option<i64>,
    wallet: Option<String>,
}

fn cat_filter(c: &Option<String>) -> Result<Option<String>, HttpResponse> {
    match c.as_deref().filter(|c| !c.is_empty() && !c.eq_ignore_ascii_case("all") && !c.eq_ignore_ascii_case("overall")) {
        None => Ok(None),
        Some(c) => category_param(c).map(|x| Some(x.to_string())).ok_or_else(|| bad_request("unknown category")),
    }
}

pub async fn list(state: web::Data<State>, q: web::Query<SigQ>) -> HttpResponse {
    let cat = match cat_filter(&q.category) { Ok(c) => c, Err(r) => return r };
    let mode_col = match q.mode.as_deref().unwrap_or("precision") {
        "precision" => "s.precision_ok",
        "flow" => "s.flow_ok",
        _ => return bad_request("mode must be precision or flow"),
    };
    let sql = format!(
        "SELECT {SIG_COLS} {SIG_FROM} WHERE s.status='active' AND {mode_col}
           AND ($1::text IS NULL OR s.category=$1) AND s.score >= $2 AND ($4::text IS NULL OR s.wallet=$4)
         ORDER BY s.score DESC, s.last_trade_at DESC LIMIT $3"
    );
    match state.db.query(&sql, &[&cat, &q.min_score.unwrap_or(0), &q.limit.unwrap_or(60), &q.wallet]).await {
        Ok(rows) => HttpResponse::Ok().json(json!({
            "rows": rows.iter().map(sig_json).collect::<Vec<_>>(),
            "status": status_cell().lock().unwrap().clone(),
        })),
        Err(e) => internal(e),
    }
}

pub async fn history(state: web::Data<State>, q: web::Query<SigQ>) -> HttpResponse {
    let cat = match cat_filter(&q.category) { Ok(c) => c, Err(r) => return r };
    let sql = format!(
        "SELECT {SIG_COLS} {SIG_FROM} WHERE s.status='history' AND ($1::text IS NULL OR s.category=$1)
         ORDER BY s.closed_at DESC NULLS LAST LIMIT $2"
    );
    match state.db.query(&sql, &[&cat, &q.limit.unwrap_or(100)]).await {
        Ok(rows) => HttpResponse::Ok().json(json!({"rows": rows.iter().map(sig_json).collect::<Vec<_>>()})),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
pub struct ConsQ {
    category: Option<String>,
    min_wallets: Option<i64>,
    limit: Option<i64>,
}

/// Markets where several tracked sharps hold the same outcome right now.
pub async fn consensus(state: web::Data<State>, q: web::Query<ConsQ>) -> HttpResponse {
    let cat = match cat_filter(&q.category) { Ok(c) => c, Err(r) => return r };
    let rows = state.db.query(
        "SELECT s.asset, max(s.title), max(s.slug), max(s.event_slug), max(s.icon), max(s.outcome), max(s.category),
                count(DISTINCT s.wallet) AS n, sum(s.entry_usdc) AS usdc,
                sum(s.entry_price*s.entry_shares)/nullif(sum(s.entry_shares),0) AS avg_entry,
                max(s.current_price), avg(s.wallet_score)::float8, max(s.score), max(s.depth_2c),
                json_agg(json_build_object('wallet', s.wallet, 'name', w.name, 'entry', s.entry_price,
                         'usdc', s.entry_usdc, 'wallet_score', s.wallet_score) ORDER BY s.entry_usdc DESC),
                (SELECT count(DISTINCT o.wallet) FROM pm_signals o
                  WHERE o.status='active' AND o.flow_ok AND o.condition_id = max(s.condition_id) AND o.asset <> s.asset)
           FROM pm_signals s LEFT JOIN pm_sharp_wallets w ON w.wallet = s.wallet
          WHERE s.status='active' AND s.flow_ok AND ($1::text IS NULL OR s.category=$1)
          GROUP BY s.asset HAVING count(DISTINCT s.wallet) >= $2
          ORDER BY count(DISTINCT s.wallet) DESC, avg(s.wallet_score) DESC LIMIT $3",
        &[&cat, &q.min_wallets.unwrap_or(2), &q.limit.unwrap_or(40)],
    ).await;
    match rows {
        Ok(rows) => {
            let out: Vec<Value> = rows.iter().map(|r| {
                let n: i64 = r.get(7);
                let avg_ws: f64 = r.get::<_, Option<f64>>(11).unwrap_or(0.0);
                // 60% average wallet quality, 40% net breadth (5+ more sharps on
                // this side than the other = full). Sharps on the other side count against it.
                let opposed: i64 = r.get(15);
                let net = (n - opposed) as f64;
                let cscore = (0.6 * avg_ws + 40.0 * ((net - 1.0) / 4.0).clamp(0.0, 1.0)).round();
                json!({
                    "asset": r.get::<_, String>(0), "title": r.get::<_, Option<String>>(1),
                    "slug": r.get::<_, Option<String>>(2), "event_slug": r.get::<_, Option<String>>(3),
                    "icon": r.get::<_, Option<String>>(4), "outcome": r.get::<_, Option<String>>(5),
                    "category": r.get::<_, Option<String>>(6), "wallets": n, "usdc": r.get::<_, Option<f64>>(8),
                    "avg_entry": r.get::<_, Option<f64>>(9), "current_price": r.get::<_, Option<f64>>(10),
                    "avg_wallet_score": avg_ws, "top_signal_score": r.get::<_, Option<i32>>(12),
                    "depth_2c": r.get::<_, Option<f64>>(13), "score": cscore,
                    "members": r.get::<_, Option<Value>>(14),
                    "opposed": r.get::<_, i64>(15),
                })
            }).collect();
            HttpResponse::Ok().json(json!({"rows": out}))
        }
        Err(e) => internal(e),
    }
}

/// How past signals did, tailing at the best ask when each was first seen
/// ($1 per signal): resolved count, hit rate, ROI. Precision vs Flow.
pub async fn record(state: web::Data<State>) -> HttpResponse {
    let rows = state.db.query(
        "SELECT m.mode, count(*) AS n,
                count(*) FILTER (WHERE s.result='won') AS won,
                sum(CASE WHEN s.result='won' THEN 1.0/s.price_at_signal - 1 ELSE -1 END) AS units
           FROM pm_signals s
           CROSS JOIN LATERAL (VALUES ('flow', s.flow_ok OR s.precision_ok), ('precision', s.precision_ok)) m(mode, ok)
          WHERE m.ok AND s.result IN ('won','lost') AND s.price_at_signal > 0
          GROUP BY m.mode", &[],
    ).await;
    let pending: i64 = state.db.query_one("SELECT count(*) FROM pm_signals WHERE result IS NULL", &[]).await
        .map(|r| r.get(0)).unwrap_or(0);
    match rows {
        Ok(rows) => {
            let modes: Vec<Value> = rows.iter().map(|r| {
                let n: i64 = r.get(1);
                let won: i64 = r.get(2);
                let units: f64 = r.get::<_, Option<f64>>(3).unwrap_or(0.0);
                json!({"mode": r.get::<_, String>(0), "resolved": n, "won": won,
                       "hit_rate": if n > 0 { Some(won as f64 / n as f64) } else { None },
                       "roi": if n > 0 { Some(units / n as f64) } else { None }})
            }).collect();
            HttpResponse::Ok().json(json!({"modes": modes, "unresolved": pending}))
        }
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
pub struct ChartQ {
    asset: String,
    range: Option<String>,
}

pub async fn chart(state: web::Data<State>, q: web::Query<ChartQ>) -> HttpResponse {
    if q.asset.is_empty() || !q.asset.chars().all(|c| c.is_ascii_digit()) {
        return bad_request("asset must be a Polymarket token id");
    }
    let (interval, fidelity) = match q.range.as_deref().unwrap_or("1w") {
        "1d" => ("1d", 5),
        "1w" => ("1w", 60),
        "1m" => ("1m", 240),
        _ => return bad_request("range must be 1d, 1w or 1m"),
    };
    let url = format!("{}/prices-history?market={}&interval={interval}&fidelity={fidelity}", clob_url(), q.asset);
    match get_json(&state, &url).await {
        Ok(v) => {
            let pts: Vec<Value> = v["history"].as_array().map(|a| a.iter()
                .filter_map(|p| Some(json!({"t": num(p, "t")?, "p": num(p, "p")?}))).collect()).unwrap_or_default();
            HttpResponse::Ok().json(json!({"asset": q.asset, "points": pts}))
        }
        Err(e) => internal(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(xs: &[&str]) -> Vec<String> { xs.iter().map(|s| s.to_string()).collect() }

    #[test]
    fn tags_map_to_most_specific_category() {
        assert_eq!(category_from_tags(&tags(&["Esports", "league of legends", "Games", "Sports"])), "ESPORTS");
        assert_eq!(category_from_tags(&tags(&["Sports", "NBA", "Games"])), "SPORTS");
        assert_eq!(category_from_tags(&tags(&["Fed", "fomc", "Trump", "Economy"])), "ECONOMICS");
        assert_eq!(category_from_tags(&tags(&["Ethereum", "Crypto Prices"])), "CRYPTO");
        assert_eq!(category_from_tags(&tags(&["Iran", "Geopolitics"])), "POLITICS");
        assert_eq!(category_from_tags(&tags(&["something"])), "OVERALL");
    }

    fn base() -> Inputs {
        Inputs {
            cat_rank: Some(10), cat_pnl: Some(500_000.0), cat_vol: Some(5_000_000.0),
            overall_rank: Some(50), overall_pnl: Some(800_000.0),
            entry: 0.40, current: Some(0.40), rel_size: 3.0, usdc: 5_000.0, depth_2c: 10_000.0, age_h: 1.0,
        }
    }

    #[test]
    fn proven_wallet_clean_entry_scores_high_and_passes_precision() {
        let s = score(&base());
        assert!(s.proven && !s.arber);
        assert!(s.score >= 80, "{}", s.score);
        assert_eq!(modes(&s, 10_000.0, false), (true, true));
    }

    #[test]
    fn unproven_category_loses_proof_and_precision() {
        let s = score(&Inputs { cat_pnl: Some(-20_000.0), ..base() });
        assert!(!s.proven);
        assert_eq!(s.parts["category_proof"], json!(0.0));
        assert!(!modes(&s, 10_000.0, false).0);
    }

    #[test]
    fn slippage_and_depth_gate_precision() {
        let s = score(&Inputs { current: Some(0.45), ..base() }); // +5c
        assert!(!modes(&s, 10_000.0, false).0);
        let s = score(&base());
        assert!(!modes(&s, 300.0, false).0); // thin book
        assert!(!modes(&s, 10_000.0, true).0); // bot pace
    }

    #[test]
    fn big_drop_below_entry_is_not_a_clean_entry() {
        let s = score(&Inputs { current: Some(0.27), entry: 0.43, ..base() }); // -16c
        assert!(s.parts["slippage"].as_f64().unwrap() < 5.0);
        assert!(!modes(&s, 10_000.0, false).0);
        let s = score(&Inputs { current: Some(0.39), ..base() }); // -1c: still clean
        assert!(s.parts["slippage"].as_f64().unwrap() >= 19.0);
    }

    #[test]
    fn arber_flag_on_huge_volume_tiny_return() {
        let s = score(&Inputs { cat_pnl: Some(10_000.0), cat_vol: Some(50_000_000.0), ..base() });
        assert!(s.arber);
        assert!(!modes(&s, 10_000.0, false).0);
    }

    #[test]
    fn ask_side_finds_best_ask_and_2c_depth() {
        let b = json!({"asks": [{"price":"0.44","size":"1000"},{"price":"0.43","size":"100"},{"price":"0.42","size":"50"}]});
        let (best, depth) = ask_side(&b);
        assert_eq!(best, Some(0.42));
        assert!((depth - (0.42 * 50.0 + 0.43 * 100.0 + 0.44 * 1000.0)).abs() < 1e-6);
        assert_eq!(ask_side(&json!({"asks": []})), (None, 0.0));
    }

    #[test]
    fn median_works() {
        assert_eq!(median(&mut [3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&mut [4.0, 1.0, 2.0, 3.0]), Some(2.5));
        assert_eq!(median(&mut []), None);
    }
}
