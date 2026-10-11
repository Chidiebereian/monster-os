//! whale-tracker
//! Ingests large wallets' real trades from Polymarket's public Data API into
//! ClickHouse `monster.wallet_trades` — the same table copy-trading's
//! /autofollow reads — and records each wallet in Postgres `trader_profiles`.
//!
//! POST /ingest  {wallets:["0x.."], limit?, min_cash?, category?}
//!      Pull each wallet's recent Polymarket trades (optionally only fills
//!      >= min_cash USD), dedupe against what's already stored, insert the
//!      new ones, and upsert the trader profile. Returns per-wallet counts.
//! GET  /trades?wallet=&limit=   Read back stored trades (what copy-trading sees).
//!
//! Config: DATABASE_URL, CLICKHOUSE_URL/USER/PASSWORD, POLYMARKET_DATA_URL
//! (default https://data-api.polymarket.com). Kalshi is not covered: it does
//! not expose per-wallet trades publicly.

use std::collections::HashMap;
use std::sync::Arc;

use actix_web::{web, App, HttpServer, HttpResponse, Responder};
use chrono::{DateTime, Utc};
use common::healthz;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_postgres::{Client, NoTls};

struct State {
    db: Arc<Client>,
    http: reqwest::Client,
    pm_url: String,
    lb_url: String,
    gamma_url: String,
    ch_url: String,
    ch_user: Option<String>,
    ch_key: Option<String>,
}

/// Compact market category classifier for the per-category performance breakdown.
fn classify(slug: &str) -> &'static str {
    let s = slug.to_lowercase();
    let any = |kws: &[&str]| kws.iter().any(|k| s.contains(k));
    if any(&[
        "nba", "nfl", "nhl", "mlb", "ufc", "mma", "soccer", "premier-league",
        "champions-league", "tennis", "golf", "nascar", "formula-1", "world-series",
        "super-bowl", "stanley-cup", "guardians", "yankees", "lakers",
    ]) {
        "sports"
    } else if any(&["bitcoin", "btc", "ethereum", "crypto", "solana"]) {
        "crypto"
    } else if any(&[
        "election", "president", "popular-vote", "senate", "congress", "trump",
        "harris", "kamala", "republican", "democrat",
    ]) {
        "politics"
    } else {
        "other"
    }
}

/// Realized P&L and cost basis for one resolved (market, outcome) position,
/// given the outcome's final price (1.0 if it won, 0.0 if it lost).
/// Pure: `trades` are (side, price, size). A win is pnl > 0.
fn realized_pnl(trades: &[(String, f64, f64)], final_price: f64) -> (f64, f64) {
    let (mut shares, mut cash, mut cost) = (0.0, 0.0, 0.0);
    for (side, price, size) in trades {
        if side == "buy" {
            shares += size;
            cash -= size * price;
            cost += size * price;
        } else {
            shares -= size;
            cash += size * price;
        }
    }
    (cash + shares * final_price, cost)
}

/// A trade as returned by Polymarket's Data API `/trades`.
#[derive(Deserialize)]
struct PmTrade {
    #[serde(rename = "proxyWallet")]
    proxy_wallet: String,
    side: String, // BUY | SELL
    #[serde(default)]
    slug: String,
    #[serde(rename = "conditionId", default)]
    condition_id: String,
    price: f64,
    size: f64,
    timestamp: i64, // epoch seconds
    #[serde(default)]
    outcome: String,
    #[serde(rename = "outcomeIndex", default)]
    outcome_index: i64,
    #[serde(default)]
    name: String,
    #[serde(default)]
    pseudonym: String,
}

impl PmTrade {
    /// Prefer the human-readable slug; fall back to the on-chain condition id.
    fn market_id(&self) -> &str {
        if !self.slug.is_empty() {
            &self.slug
        } else {
            &self.condition_id
        }
    }
    fn ts_string(&self) -> String {
        DateTime::<Utc>::from_timestamp(self.timestamp, 0)
            .unwrap_or_else(Utc::now)
            .format("%Y-%m-%d %H:%M:%S%.3f")
            .to_string()
    }
}

fn internal(msg: impl std::fmt::Display) -> HttpResponse {
    eprintln!("whale-tracker: {msg}");
    HttpResponse::InternalServerError().json(json!({ "error": msg.to_string() }))
}

fn bad_request(msg: impl Into<String>) -> HttpResponse {
    HttpResponse::BadRequest().json(json!({ "error": msg.into() }))
}

impl State {
    /// POST a statement/body to ClickHouse with auth headers; return the text body.
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

    /// Latest stored trade time for a wallet, as epoch seconds (0 if none), so
    /// we only ingest strictly newer fills and never double-store history.
    async fn last_ts(&self, wallet: &str) -> Result<i64, String> {
        let sql = format!(
            "SELECT toInt64(max(ts)) FROM monster.wallet_trades \
             WHERE wallet = '{}' FORMAT TabSeparated",
            wallet.replace('\'', "")
        );
        let text = self.ch(sql).await?;
        Ok(text.trim().parse::<i64>().unwrap_or(0))
    }

    async fn fetch_pm(
        &self,
        wallet: &str,
        limit: u32,
        min_cash: Option<f64>,
        start: i64,
    ) -> Result<Vec<PmTrade>, String> {
        let url = format!("{}/trades", self.pm_url);
        let mut q: Vec<(&str, String)> =
            vec![("user", wallet.to_string()), ("limit", limit.to_string())];
        if start > 0 {
            q.push(("start", start.to_string()));
        }
        if let Some(c) = min_cash {
            q.push(("filterType", "CASH".into()));
            q.push(("filterAmount", c.to_string()));
        }
        let resp = self
            .http
            .get(&url)
            .query(&q)
            .send()
            .await
            .map_err(|e| format!("Polymarket request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Polymarket returned {status}: {}", body.trim()));
        }
        resp.json::<Vec<PmTrade>>().await.map_err(|e| format!("Polymarket body unreadable: {e}"))
    }

    async fn insert_trades(&self, trades: &[PmTrade]) -> Result<(), String> {
        if trades.is_empty() {
            return Ok(());
        }
        let mut body = String::from(
            "INSERT INTO monster.wallet_trades \
             (ts, venue, wallet, market_id, side, price, size, outcome, outcome_index) FORMAT JSONEachRow\n",
        );
        for t in trades {
            let row = json!({
                "ts": t.ts_string(),
                "venue": "polymarket",
                "wallet": t.proxy_wallet,
                "market_id": t.market_id(),
                "side": t.side.to_lowercase(),
                "price": t.price,
                "size": t.size,
                "outcome": t.outcome,
                "outcome_index": t.outcome_index,
            });
            body.push_str(&row.to_string());
            body.push('\n');
        }
        self.ch(body).await.map(|_| ())
    }

    /// Count of this wallet's trades we've stored (for the performance snapshot).
    async fn stored_trade_count(&self, wallet: &str) -> Result<i32, String> {
        let sql = format!(
            "SELECT toInt32(count()) FROM monster.wallet_trades WHERE wallet = '{}' FORMAT TabSeparated",
            wallet.replace('\'', "")
        );
        Ok(self.ch(sql).await?.trim().parse::<i32>().unwrap_or(0))
    }

    /// Top traders by realized profit from Polymarket's public leaderboard API.
    /// One leaderboard page (`metric` is "profit" or "volume").
    async fn fetch_leaderboard(&self, metric: &str, window: &str, limit: u32) -> Result<Vec<LbEntry>, String> {
        let url = format!("{}/{metric}", self.lb_url);
        let resp = self
            .http
            .get(&url)
            .query(&[("window", window), ("limit", &limit.to_string())])
            .send()
            .await
            .map_err(|e| format!("leaderboard request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("leaderboard returned {status}: {}", body.trim()));
        }
        resp.json::<Vec<LbEntry>>().await.map_err(|e| format!("leaderboard body unreadable: {e}"))
    }

    /// Upsert today's performance snapshot (PK is wallet+as_of). `pnl`/`volume`/
    /// `trades` are real; win_rate/edge/risk stay NULL until reconstructed. A
    /// None value leaves the existing column untouched (profit and volume lists
    /// are merged across two calls).
    async fn upsert_performance(
        &self,
        wallet: &str,
        window: &str,
        pnl: Option<f64>,
        trades: i32,
        volume: Option<f64>,
    ) -> Result<(), String> {
        self.db
            .execute(
                "INSERT INTO trader_performance (wallet, as_of, time_window, pnl, trades, volume) \
                 VALUES ($1, CURRENT_DATE, $2, $3, $4, $5) \
                 ON CONFLICT (wallet, as_of, time_window) DO UPDATE SET \
                   pnl = COALESCE(EXCLUDED.pnl, trader_performance.pnl), \
                   trades = EXCLUDED.trades, \
                   volume = COALESCE(EXCLUDED.volume, trader_performance.volume)",
                &[&wallet, &window, &pnl, &trades, &volume],
            )
            .await
            .map(|_| ())
            .map_err(|e| format!("Postgres performance upsert failed: {e}"))
    }

    /// Final outcome prices for a resolved Polymarket market (e.g. [1.0, 0.0]),
    /// or None if the market is still open / not found. Read from Gamma.
    async fn resolution(&self, slug: &str) -> Option<Vec<f64>> {
        // Gamma's default list returns only active markets; resolved ones (what
        // we need for P&L) require closed=true.
        let url = format!("{}/markets?slug={slug}&closed=true", self.gamma_url);
        let resp = self.http.get(&url).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let body: Value = resp.json().await.ok()?;
        let m = body.as_array()?.first()?;
        if !m["closed"].as_bool().unwrap_or(false) {
            return None;
        }
        let prices: Vec<String> = serde_json::from_str(m["outcomePrices"].as_str()?).ok()?;
        Some(prices.iter().filter_map(|s| s.parse().ok()).collect())
    }

    /// Upsert today's per-category reconstructed performance for a wallet.
    async fn upsert_category_performance(
        &self,
        wallet: &str,
        category: &str,
        win_rate: f64,
        roi: f64,
        resolved: i32,
        wins: i32,
    ) -> Result<(), String> {
        self.db
            .execute(
                "INSERT INTO trader_category_performance (wallet, category, as_of, win_rate, roi, resolved, wins) \
                 VALUES ($1, $2, CURRENT_DATE, $3, $4, $5, $6) \
                 ON CONFLICT (wallet, category, as_of) DO UPDATE SET \
                   win_rate = EXCLUDED.win_rate, roi = EXCLUDED.roi, \
                   resolved = EXCLUDED.resolved, wins = EXCLUDED.wins",
                &[&wallet, &category, &win_rate, &roi, &resolved, &wins],
            )
            .await
            .map(|_| ())
            .map_err(|e| format!("Postgres category perf upsert failed: {e}"))
    }

    /// Attach reconstructed win_rate/roi to the wallet's most recent performance
    /// snapshot (so it sits beside pnl/trades), or start a row if none exists.
    async fn update_winrate(&self, wallet: &str, win_rate: f64, roi: f64) -> Result<(), String> {
        // Reconstructed win-rate/roi are overall, so they live on the 'all'-window rows.
        let updated = self
            .db
            .execute(
                "UPDATE trader_performance SET win_rate = $2, edge = $3 \
                 WHERE wallet = $1 AND time_window = 'all' \
                   AND as_of = (SELECT max(as_of) FROM trader_performance WHERE wallet = $1 AND time_window = 'all')",
                &[&wallet, &win_rate, &roi],
            )
            .await
            .map_err(|e| format!("Postgres win_rate update failed: {e}"))?;
        if updated == 0 {
            self.db
                .execute(
                    "INSERT INTO trader_performance (wallet, as_of, time_window, win_rate, edge) \
                     VALUES ($1, CURRENT_DATE, 'all', $2, $3)",
                    &[&wallet, &win_rate, &roi],
                )
                .await
                .map_err(|e| format!("Postgres win_rate insert failed: {e}"))?;
        }
        Ok(())
    }

    async fn upsert_profile(
        &self,
        wallet: &str,
        label: Option<&str>,
        category: Option<&str>,
    ) -> Result<(), String> {
        self.db
            .execute(
                "INSERT INTO trader_profiles (wallet, venue, label, category) \
                 VALUES ($1, 'polymarket', $2, $3) \
                 ON CONFLICT (wallet) DO UPDATE SET \
                   label = COALESCE(trader_profiles.label, EXCLUDED.label), \
                   category = COALESCE(EXCLUDED.category, trader_profiles.category)",
                &[&wallet, &label, &category],
            )
            .await
            .map(|_| ())
            .map_err(|e| format!("Postgres upsert failed: {e}"))
    }
}

/// One row of Polymarket's profit leaderboard.
#[derive(Deserialize)]
struct LbEntry {
    #[serde(rename = "proxyWallet")]
    proxy_wallet: String,
    amount: f64, // realized profit in USD
    #[serde(default)]
    name: String,
    #[serde(default)]
    pseudonym: String,
}

#[derive(Deserialize)]
struct RefreshRequest {
    /// Leaderboard window: all | 1d | 1w | 1m (default all).
    window: Option<String>,
    /// How many top traders to pull (default 20).
    limit: Option<u32>,
    /// Also ingest each trader's recent trades into wallet_trades (default true).
    ingest_trades: Option<bool>,
    /// Per-trader trade fetch cap when ingesting (default 100).
    trade_limit: Option<u32>,
    category: Option<String>,
}

// Polymarket's leaderboard windows (lb-api rejects anything else).
const LB_WINDOWS: [&str; 4] = ["all", "1d", "7d", "30d"];

/// Pull the top traders by profit, snapshot their performance, and (optionally)
/// ingest their trades — populating exactly what copy-trading's /leaderboard
/// and /autofollow read.
async fn refresh_leaderboard(state: web::Data<State>, body: web::Json<RefreshRequest>) -> impl Responder {
    let window = body.window.clone().unwrap_or_else(|| "all".into());
    if !LB_WINDOWS.contains(&window.as_str()) {
        return bad_request(format!("window must be one of {LB_WINDOWS:?}"));
    }
    let limit = body.limit.unwrap_or(20).clamp(1, 100);
    let ingest_trades = body.ingest_trades.unwrap_or(true);
    let trade_limit = body.trade_limit.unwrap_or(100).clamp(1, 10000);

    // Merge the profit and volume leaderboards: each wallet keeps whichever
    // metrics it ranks for (so Top Traders can sort by either).
    let profit = match state.fetch_leaderboard("profit", &window, limit).await {
        Ok(e) => e,
        Err(e) => return internal(e),
    };
    let volume = match state.fetch_leaderboard("volume", &window, limit).await {
        Ok(e) => e,
        Err(e) => return internal(e),
    };
    // wallet -> (pnl, volume, label), insertion order from profit then volume.
    let mut merged: Vec<(String, Option<f64>, Option<f64>, Option<String>)> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let label_of = |e: &LbEntry| {
        if !e.name.is_empty() {
            Some(e.name.clone())
        } else if !e.pseudonym.is_empty() {
            Some(e.pseudonym.clone())
        } else {
            None
        }
    };
    for e in &profit {
        index.insert(e.proxy_wallet.clone(), merged.len());
        merged.push((e.proxy_wallet.clone(), Some(e.amount), None, label_of(e)));
    }
    for e in &volume {
        if let Some(&i) = index.get(&e.proxy_wallet) {
            merged[i].2 = Some(e.amount);
        } else {
            index.insert(e.proxy_wallet.clone(), merged.len());
            merged.push((e.proxy_wallet.clone(), None, Some(e.amount), label_of(e)));
        }
    }

    let mut results = Vec::new();
    for (wallet, pnl, volume, label) in &merged {
        if let Err(err) = state.upsert_profile(wallet, label.as_deref(), body.category.as_deref()).await {
            return internal(err);
        }
        let mut ingested = 0usize;
        if ingest_trades {
            let last = match state.last_ts(wallet).await {
                Ok(t) => t,
                Err(err) => return internal(err),
            };
            match state.fetch_pm(wallet, trade_limit, None, last).await {
                Ok(fetched) => {
                    let fresh: Vec<PmTrade> = fetched.into_iter().filter(|t| t.timestamp > last).collect();
                    ingested = fresh.len();
                    if let Err(err) = state.insert_trades(&fresh).await {
                        return internal(err);
                    }
                }
                Err(err) => return internal(err),
            }
        }
        let trades = match state.stored_trade_count(wallet).await {
            Ok(n) => n,
            Err(err) => return internal(err),
        };
        if let Err(err) = state.upsert_performance(wallet, &window, *pnl, trades, *volume).await {
            return internal(err);
        }
        results.push(json!({
            "wallet": wallet,
            "label": label,
            "pnl": pnl,
            "volume": volume,
            "trades": trades,
            "ingested": ingested,
        }));
    }

    HttpResponse::Ok().json(json!({
        "window": window,
        "traders": results.len(),
        "results": results,
    }))
}

#[derive(Deserialize)]
struct IngestRequest {
    wallets: Vec<String>,
    limit: Option<u32>,
    /// Only ingest fills worth at least this many USD (Polymarket CASH filter).
    min_cash: Option<f64>,
    /// Category tag written to the trader profile (e.g. "politics").
    category: Option<String>,
}

async fn ingest(state: web::Data<State>, body: web::Json<IngestRequest>) -> impl Responder {
    if body.wallets.is_empty() {
        return bad_request("wallets must be a non-empty list");
    }
    let limit = body.limit.unwrap_or(100).clamp(1, 10000);
    let mut results = Vec::new();
    let mut total_inserted = 0usize;

    for wallet in &body.wallets {
        if !(wallet.starts_with("0x") && wallet.len() == 42) {
            return bad_request(format!("invalid wallet address: {wallet:?}"));
        }
        let last = match state.last_ts(wallet).await {
            Ok(t) => t,
            Err(e) => return internal(e),
        };
        let fetched = match state.fetch_pm(wallet, limit, body.min_cash, last).await {
            Ok(t) => t,
            Err(e) => return internal(e),
        };
        // Keep only strictly-newer fills (the API `start` is inclusive-ish).
        let fresh: Vec<PmTrade> = fetched.into_iter().filter(|t| t.timestamp > last).collect();
        let label = fresh
            .iter()
            .find_map(|t| {
                let n = if !t.name.is_empty() { &t.name } else { &t.pseudonym };
                (!n.is_empty()).then(|| n.clone())
            });
        if let Err(e) = state.upsert_profile(wallet, label.as_deref(), body.category.as_deref()).await {
            return internal(e);
        }
        if let Err(e) = state.insert_trades(&fresh).await {
            return internal(e);
        }
        let newest = fresh.iter().map(|t| t.timestamp).max();
        total_inserted += fresh.len();
        results.push(json!({
            "wallet": wallet,
            "inserted": fresh.len(),
            "newest_ts": newest,
            "label": label,
        }));
    }

    HttpResponse::Ok().json(json!({
        "venue": "polymarket",
        "wallets": body.wallets.len(),
        "inserted": total_inserted,
        "results": results,
    }))
}

#[derive(Deserialize)]
struct TradesQuery {
    wallet: String,
    limit: Option<u32>,
}

async fn trades(state: web::Data<State>, q: web::Query<TradesQuery>) -> impl Responder {
    let limit = q.limit.unwrap_or(50).clamp(1, 1000);
    let sql = format!(
        "SELECT ts, venue, wallet, market_id, side, price, size \
         FROM monster.wallet_trades WHERE wallet = '{}' \
         ORDER BY ts DESC LIMIT {limit} FORMAT JSONEachRow",
        q.wallet.replace('\'', "")
    );
    match state.ch(sql).await {
        Ok(text) => {
            let rows: Vec<serde_json::Value> = text
                .lines()
                .filter(|l| !l.trim().is_empty())
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect();
            HttpResponse::Ok().json(rows)
        }
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
struct ComputePerfRequest {
    wallets: Vec<String>,
}

#[derive(Deserialize)]
struct StoredTrade {
    market_id: String,
    outcome_index: i64,
    side: String,
    price: f64,
    size: f64,
}

/// Reconstruct realized win-rate and ROI for each wallet from its stored trades
/// plus each market's resolution, and write them into trader_performance.
/// Accurate only where we hold the wallet's complete trade set for a market.
async fn compute_performance(state: web::Data<State>, body: web::Json<ComputePerfRequest>) -> impl Responder {
    if body.wallets.is_empty() {
        return bad_request("wallets must be a non-empty list");
    }
    let mut results = Vec::new();
    for wallet in &body.wallets {
        if !(wallet.starts_with("0x") && wallet.len() == 42) {
            return bad_request(format!("invalid wallet: {wallet:?}"));
        }
        // All of this wallet's trades that carry an outcome index.
        let sql = format!(
            "SELECT market_id, outcome_index, side, price, size FROM monster.wallet_trades \
             WHERE wallet = '{}' AND outcome_index >= 0 ORDER BY ts FORMAT JSONEachRow",
            wallet.replace('\'', "")
        );
        let text = match state.ch(sql).await {
            Ok(t) => t,
            Err(e) => return internal(e),
        };
        // Group trades by (market, outcome).
        let mut groups: HashMap<(String, i64), Vec<(String, f64, f64)>> = HashMap::new();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            if let Ok(t) = serde_json::from_str::<StoredTrade>(line) {
                groups.entry((t.market_id, t.outcome_index)).or_default().push((t.side, t.price, t.size));
            }
        }

        let mut resolution_cache: HashMap<String, Option<Vec<f64>>> = HashMap::new();
        let (mut resolved, mut wins, mut sum_pnl, mut sum_cost, mut unresolved) = (0u32, 0u32, 0.0, 0.0, 0u32);
        // category -> (resolved, wins, sum_pnl, sum_cost)
        let mut by_cat: HashMap<&'static str, (u32, u32, f64, f64)> = HashMap::new();
        for ((market, idx), trades) in &groups {
            let prices = match resolution_cache.get(market) {
                Some(p) => p.clone(),
                None => {
                    let p = state.resolution(market).await;
                    resolution_cache.insert(market.clone(), p.clone());
                    p
                }
            };
            let Some(prices) = prices else {
                unresolved += 1;
                continue;
            };
            let Some(&final_price) = prices.get(*idx as usize) else {
                unresolved += 1;
                continue;
            };
            let (pnl, cost) = realized_pnl(trades, final_price);
            resolved += 1;
            if pnl > 0.0 {
                wins += 1;
            }
            sum_pnl += pnl;
            sum_cost += cost;
            let c = by_cat.entry(classify(market)).or_insert((0, 0, 0.0, 0.0));
            c.0 += 1;
            if pnl > 0.0 {
                c.1 += 1;
            }
            c.2 += pnl;
            c.3 += cost;
        }

        let win_rate = if resolved > 0 { Some(wins as f64 / resolved as f64) } else { None };
        let roi = if sum_cost > 0.0 { Some(sum_pnl / sum_cost) } else { None };
        if let (Some(wr), Some(r)) = (win_rate, roi) {
            if let Err(e) = state.update_winrate(wallet, wr, r).await {
                return internal(e);
            }
        }
        // Persist and report the per-category breakdown.
        let mut categories = Vec::new();
        for (cat, (res, w, pnl, cost)) in &by_cat {
            let cwr = *w as f64 / *res as f64;
            let croi = if *cost > 0.0 { pnl / cost } else { 0.0 };
            if let Err(e) =
                state.upsert_category_performance(wallet, cat, cwr, croi, *res as i32, *w as i32).await
            {
                return internal(e);
            }
            categories.push(json!({
                "category": cat, "resolved": res, "wins": w,
                "win_rate": cwr, "roi": croi,
            }));
        }
        results.push(json!({
            "wallet": wallet,
            "resolved_positions": resolved,
            "unresolved_positions": unresolved,
            "wins": wins,
            "win_rate": win_rate,
            "roi": roi,
            "realized_pnl": sum_pnl,
            "by_category": categories,
        }));
    }
    HttpResponse::Ok().json(json!({ "results": results }))
}

#[derive(Deserialize)]
struct ExposureQuery {
    wallet: String,
    limit: Option<u32>,
}

/// A trader's current net market exposure, reconstructed from stored trades:
/// net shares and cost basis per (market, outcome). Largest positions first.
async fn exposure(state: web::Data<State>, q: web::Query<ExposureQuery>) -> impl Responder {
    if !(q.wallet.starts_with("0x") && q.wallet.len() == 42) {
        return bad_request("invalid wallet");
    }
    let limit = q.limit.unwrap_or(25).clamp(1, 200);
    let sql = format!(
        "SELECT market_id, outcome, \
                sum(if(side='buy', size, -size)) AS net_shares, \
                sum(if(side='buy', size*price, -size*price)) AS net_cost \
         FROM monster.wallet_trades WHERE wallet = '{}' \
         GROUP BY market_id, outcome HAVING abs(net_shares) > 0.01 \
         ORDER BY net_cost DESC LIMIT {limit} FORMAT JSONEachRow",
        q.wallet.replace('\'', "")
    );
    match state.ch(sql).await {
        Ok(text) => {
            let rows: Vec<Value> = text
                .lines()
                .filter(|l| !l.trim().is_empty())
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .map(|v| {
                    let shares = v["net_shares"].as_f64().unwrap_or(0.0);
                    let cost = v["net_cost"].as_f64().unwrap_or(0.0);
                    json!({
                        "market_id": v["market_id"],
                        "outcome": v["outcome"],
                        "net_shares": shares,
                        "cost_basis": cost,
                        "avg_entry": if shares.abs() > 0.01 { Some(cost / shares) } else { None },
                    })
                })
                .collect();
            HttpResponse::Ok().json(rows)
        }
        Err(e) => internal(e),
    }
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8116);
    let url = std::env::var("DATABASE_URL")
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "DATABASE_URL must be set"))?;
    let (client, connection) = tokio_postgres::connect(&url, NoTls)
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    actix_web::rt::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("whale-tracker: postgres connection closed: {e}");
        }
    });

    let env_opt = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let state = web::Data::new(State {
        db: Arc::new(client),
        http: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?,
        pm_url: env_opt("POLYMARKET_DATA_URL")
            .unwrap_or_else(|| "https://data-api.polymarket.com".into())
            .trim_end_matches('/')
            .to_string(),
        lb_url: env_opt("POLYMARKET_LB_URL")
            .unwrap_or_else(|| "https://lb-api.polymarket.com".into())
            .trim_end_matches('/')
            .to_string(),
        gamma_url: env_opt("POLYMARKET_GAMMA_URL")
            .unwrap_or_else(|| "https://gamma-api.polymarket.com".into())
            .trim_end_matches('/')
            .to_string(),
        ch_url: env_opt("CLICKHOUSE_URL").unwrap_or_else(|| "http://localhost:8123/".into()),
        ch_user: env_opt("CLICKHOUSE_USER"),
        ch_key: env_opt("CLICKHOUSE_PASSWORD"),
    });

    println!("whale-tracker listening on 0.0.0.0:{port} (polymarket {}, clickhouse {})", state.pm_url, state.ch_url);
    HttpServer::new(move || {
        App::new()
            .app_data(state.clone())
            .route("/healthz", web::get().to(healthz))
            .route("/ingest", web::post().to(ingest))
            .route("/refresh-leaderboard", web::post().to(refresh_leaderboard))
            .route("/compute-performance", web::post().to(compute_performance))
            .route("/exposure", web::get().to(exposure))
            .route("/trades", web::get().to(trades))
    })
    .bind(("0.0.0.0", port))?
    .run()
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(slug: &str, cond: &str, side: &str, ts: i64) -> PmTrade {
        PmTrade {
            proxy_wallet: "0xabc".into(),
            side: side.into(),
            slug: slug.into(),
            condition_id: cond.into(),
            price: 0.5,
            size: 100.0,
            timestamp: ts,
            outcome: String::new(),
            outcome_index: 0,
            name: String::new(),
            pseudonym: String::new(),
        }
    }

    #[test]
    fn realized_pnl_winner_and_loser() {
        // Bought 100 shares at 0.40; outcome WON (final 1.0).
        let buys = vec![("buy".into(), 0.40, 100.0)];
        let (pnl, cost) = realized_pnl(&buys, 1.0);
        assert!((pnl - 60.0).abs() < 1e-9 && (cost - 40.0).abs() < 1e-9); // 100*(1-0.4)
        // Same position, outcome LOST (final 0.0): lose the stake.
        let (pnl, _) = realized_pnl(&buys, 0.0);
        assert!((pnl + 40.0).abs() < 1e-9);
    }

    #[test]
    fn classify_buckets_markets() {
        assert_eq!(classify("will-donald-trump-win-the-2024-us-presidential-election"), "politics");
        assert_eq!(classify("nba-okc-atl-2026-10-12"), "sports");
        assert_eq!(classify("will-bitcoin-dip-to-55000"), "crypto");
        assert_eq!(classify("something-else"), "other");
    }

    #[test]
    fn realized_pnl_accounts_for_sells() {
        // Buy 100 @ 0.40, sell 100 @ 0.60 before resolution: locked +20, no exposure.
        let trades = vec![("buy".into(), 0.40, 100.0), ("sell".into(), 0.60, 100.0)];
        let (pnl, cost) = realized_pnl(&trades, 1.0);
        assert!((pnl - 20.0).abs() < 1e-9 && (cost - 40.0).abs() < 1e-9);
    }

    #[test]
    fn market_id_prefers_slug() {
        assert_eq!(t("will-x-win", "0xcond", "BUY", 1).market_id(), "will-x-win");
        assert_eq!(t("", "0xcond", "BUY", 1).market_id(), "0xcond");
    }

    #[test]
    fn ts_string_is_clickhouse_datetime() {
        // 2021-01-01 00:00:00 UTC
        let s = t("m", "c", "BUY", 1_609_459_200).ts_string();
        assert_eq!(s, "2021-01-01 00:00:00.000");
    }

    #[test]
    fn insert_body_has_lowercase_side_and_mapped_market() {
        // Exercise the row JSON the ClickHouse insert would send.
        let tr = t("slug-a", "0xc", "SELL", 1_609_459_200);
        let row = json!({
            "ts": tr.ts_string(), "venue": "polymarket", "wallet": tr.proxy_wallet,
            "market_id": tr.market_id(), "side": tr.side.to_lowercase(),
            "price": tr.price, "size": tr.size,
        });
        assert_eq!(row["side"], "sell");
        assert_eq!(row["market_id"], "slug-a");
        assert_eq!(row["venue"], "polymarket");
    }
}
