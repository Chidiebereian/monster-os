//! copy-trading
//! Trader leaderboards and managed copy-sizing with risk controls.
//! Backing tables in infrastructure/postgres/migrations/0001_init.sql.
//!
//! This is a research/advisory layer: it recommends and records copy sizing,
//! it does NOT route real orders or touch any wallet or exchange.
//!
//! GET  /leaderboard?venue=&metric=pnl|edge|win_rate|trades&limit=20
//!      Top traders by their latest performance snapshot.
//! GET  /traders/{wallet}
//!      One trader's profile and full performance history.
//! POST /copy/size   {bankroll, max_position_fraction, slippage_tolerance,
//!                    reference_price, current_price, and either copy_ratio
//!                    or (trader_size + trader_bankroll), optional budget}
//!      Recommended stake after position/budget caps and a slippage gate.
//! POST /copy        {user_id, wallet, market_id, size}  record a copied position
//! GET  /copy?user_id=&open=true                         list a user's copies
//! POST /copy/{id}/close                                 mark one closed

use std::collections::HashMap;
use std::sync::Arc;

use actix_web::{web, App, HttpServer, HttpResponse, Responder};
use chrono::{DateTime, NaiveDate, Utc};
use common::healthz;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio_postgres::{error::SqlState, Client, NoTls, Row};

type Db = web::Data<Arc<Client>>;

/// ClickHouse HTTP client (time-series store holding `wallet_trades`).
struct Ch {
    http: reqwest::Client,
    url: String,
    user: Option<String>,
    key: Option<String>,
}
type ChData = web::Data<Ch>;

// ---- Risk-sizing engine (pure, no I/O) ----------------------------------

#[derive(Deserialize)]
struct SizeRequest {
    /// Follower's total bankroll.
    bankroll: f64,
    /// Remaining copy budget; defaults to the full bankroll.
    budget: Option<f64>,
    /// Hard cap on any single position as a fraction of bankroll, e.g. 0.05.
    max_position_fraction: f64,
    /// Max acceptable fractional price slippage vs the trader's entry, e.g. 0.02.
    slippage_tolerance: f64,
    /// Price the trader entered at (contract price in (0, 1)).
    reference_price: f64,
    /// Price the follower would pay now.
    current_price: f64,
    /// Fixed fraction of bankroll to stake...
    copy_ratio: Option<f64>,
    /// ...or mirror the trader proportionally: trader_size * bankroll / trader_bankroll.
    trader_size: Option<f64>,
    trader_bankroll: Option<f64>,
}

#[derive(Serialize)]
struct SizeResult {
    accepted: bool,
    recommended_size: f64,
    raw_size: f64,
    slippage: f64,
    caps_applied: Vec<String>,
    reason: Option<String>,
}

/// Proportional stake before caps, from whichever sizing mode was supplied.
fn raw_stake(req: &SizeRequest) -> Result<f64, String> {
    match (req.copy_ratio, req.trader_size, req.trader_bankroll) {
        (Some(r), _, _) if r > 0.0 => Ok(req.bankroll * r),
        (None, Some(ts), Some(tb)) if ts > 0.0 && tb > 0.0 => Ok(ts * req.bankroll / tb),
        _ => Err("provide copy_ratio > 0, or both trader_size and trader_bankroll > 0".into()),
    }
}

/// Apply the slippage gate and the position/budget caps. Assumes inputs are
/// already range-checked by the caller.
fn size_copy(req: &SizeRequest) -> Result<SizeResult, String> {
    let raw = raw_stake(req)?;
    // A buy fill above the reference price is adverse slippage.
    let slippage = (req.current_price - req.reference_price) / req.reference_price;
    if slippage > req.slippage_tolerance {
        return Ok(SizeResult {
            accepted: false,
            recommended_size: 0.0,
            raw_size: raw,
            slippage,
            caps_applied: vec![],
            reason: Some(format!(
                "slippage {:.4} exceeds tolerance {:.4}",
                slippage, req.slippage_tolerance
            )),
        });
    }

    let mut size = raw;
    let mut caps = Vec::new();
    let position_cap = req.bankroll * req.max_position_fraction;
    if size > position_cap {
        size = position_cap;
        caps.push("max_position_fraction".to_string());
    }
    let budget = req.budget.unwrap_or(req.bankroll);
    if size > budget {
        size = budget;
        caps.push("budget".to_string());
    }

    if size <= 0.0 {
        return Ok(SizeResult {
            accepted: false,
            recommended_size: 0.0,
            raw_size: raw,
            slippage,
            caps_applied: caps,
            reason: Some("no budget or position capacity available".into()),
        });
    }
    Ok(SizeResult { accepted: true, recommended_size: size, raw_size: raw, slippage, caps_applied: caps, reason: None })
}

// ---- HTTP helpers -------------------------------------------------------

fn bad_request(msg: impl Into<String>) -> HttpResponse {
    HttpResponse::BadRequest().json(json!({ "error": msg.into() }))
}

fn internal(msg: impl std::fmt::Display) -> HttpResponse {
    eprintln!("copy-trading: {msg}");
    HttpResponse::InternalServerError().finish()
}

fn db_error(e: tokio_postgres::Error) -> HttpResponse {
    match e.code() {
        Some(c) if *c == SqlState::FOREIGN_KEY_VIOLATION => {
            bad_request("unknown user_id or wallet (foreign key)")
        }
        _ => internal(e),
    }
}

// ---- Leaderboard --------------------------------------------------------

#[derive(Deserialize)]
struct LeaderboardQuery {
    venue: Option<String>,
    category: Option<String>,
    metric: Option<String>,
    /// Time window for the ranking: all | 1d | 1w | 1m (default all).
    window: Option<String>,
    limit: Option<i64>,
}

/// Whitelist + qualify the sort column (interpolated into SQL, never raw input).
/// pnl/volume/trades are window-specific; win_rate/edge come from the overall rows.
fn metric_column(metric: &Option<String>) -> Result<&'static str, HttpResponse> {
    match metric.as_deref().unwrap_or("pnl") {
        "pnl" => Ok("perf.pnl"),
        "volume" => Ok("perf.volume"),
        "trades" => Ok("perf.trades"),
        "win_rate" => Ok("wr.win_rate"),
        "edge" => Ok("wr.edge"),
        other => Err(bad_request(format!(
            "metric must be one of pnl|volume|edge|win_rate|trades, got {other:?}"
        ))),
    }
}

fn trader_json(row: &Row) -> serde_json::Value {
    json!({
        "wallet": row.get::<_, String>("wallet"),
        "venue": row.get::<_, String>("venue"),
        "label": row.get::<_, Option<String>>("label"),
        "category": row.get::<_, Option<String>>("category"),
        "window": row.get::<_, String>("time_window"),
        "as_of": row.get::<_, NaiveDate>("as_of").to_string(),
        "win_rate": row.get::<_, Option<f64>>("win_rate"),
        "edge": row.get::<_, Option<f64>>("edge"),
        "pnl": row.get::<_, Option<f64>>("pnl"),
        "volume": row.get::<_, Option<f64>>("volume"),
        "risk": row.get::<_, Option<f64>>("risk"),
        "trades": row.get::<_, Option<i32>>("trades"),
    })
}

async fn leaderboard(db: Db, q: web::Query<LeaderboardQuery>) -> impl Responder {
    let col = match metric_column(&q.metric) {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let window = q.window.clone().unwrap_or_else(|| "all".into());
    let limit = q.limit.unwrap_or(20).clamp(1, 200);
    // Latest snapshot per wallet for the chosen window (pnl/volume/trades), with
    // the wallet's overall win_rate/edge joined in so they show in any window.
    let sql = format!(
        "SELECT p.wallet, p.venue, p.label, p.category, \
                perf.time_window, perf.as_of, perf.pnl, perf.volume, perf.risk, perf.trades, \
                wr.win_rate, wr.edge \
         FROM trader_profiles p \
         JOIN LATERAL ( \
             SELECT * FROM trader_performance tp \
             WHERE tp.wallet = p.wallet AND tp.time_window = $3 ORDER BY as_of DESC LIMIT 1 \
         ) perf ON true \
         LEFT JOIN LATERAL ( \
             SELECT win_rate, edge FROM trader_performance tp \
             WHERE tp.wallet = p.wallet AND tp.time_window = 'all' AND win_rate IS NOT NULL \
             ORDER BY as_of DESC LIMIT 1 \
         ) wr ON true \
         WHERE ($1::text IS NULL OR p.venue = $1) \
           AND ($2::text IS NULL OR p.category = $2) \
         ORDER BY {col} DESC NULLS LAST \
         LIMIT $4"
    );
    match db.query(&sql, &[&q.venue, &q.category, &window, &limit]).await {
        Ok(rows) => HttpResponse::Ok().json(rows.iter().map(trader_json).collect::<Vec<_>>()),
        Err(e) => db_error(e),
    }
}

async fn trader_detail(db: Db, wallet: web::Path<String>) -> impl Responder {
    let profile = match db
        .query_opt(
            "SELECT wallet, venue, label, category, first_seen FROM trader_profiles WHERE wallet = $1",
            &[&wallet.as_str()],
        )
        .await
    {
        Ok(Some(row)) => json!({
            "wallet": row.get::<_, String>("wallet"),
            "venue": row.get::<_, String>("venue"),
            "label": row.get::<_, Option<String>>("label"),
            "category": row.get::<_, Option<String>>("category"),
            "first_seen": row.get::<_, DateTime<Utc>>("first_seen").to_rfc3339(),
        }),
        Ok(None) => return HttpResponse::NotFound().finish(),
        Err(e) => return db_error(e),
    };
    let history = match db
        .query(
            "SELECT win_rate, edge, pnl, risk, trades, as_of FROM trader_performance \
             WHERE wallet = $1 ORDER BY as_of DESC",
            &[&wallet.as_str()],
        )
        .await
    {
        Ok(rows) => rows
            .iter()
            .map(|r| {
                json!({
                    "as_of": r.get::<_, NaiveDate>("as_of").to_string(),
                    "win_rate": r.get::<_, Option<f64>>("win_rate"),
                    "edge": r.get::<_, Option<f64>>("edge"),
                    "pnl": r.get::<_, Option<f64>>("pnl"),
                    "risk": r.get::<_, Option<f64>>("risk"),
                    "trades": r.get::<_, Option<i32>>("trades"),
                })
            })
            .collect::<Vec<_>>(),
        Err(e) => return db_error(e),
    };
    // Latest reconstructed performance per category (strongest first).
    let category_performance = match db
        .query(
            "SELECT DISTINCT ON (category) category, win_rate, roi, wins, resolved \
             FROM trader_category_performance WHERE wallet = $1 \
             ORDER BY category, as_of DESC",
            &[&wallet.as_str()],
        )
        .await
    {
        Ok(rows) => {
            let mut v = rows
                .iter()
                .map(|r| {
                    json!({
                        "category": r.get::<_, String>("category"),
                        "win_rate": r.get::<_, Option<f64>>("win_rate"),
                        "roi": r.get::<_, Option<f64>>("roi"),
                        "wins": r.get::<_, Option<i32>>("wins"),
                        "resolved": r.get::<_, Option<i32>>("resolved"),
                    })
                })
                .collect::<Vec<_>>();
            // Strongest category (by resolved count) first.
            v.sort_by(|a, b| b["resolved"].as_i64().cmp(&a["resolved"].as_i64()));
            v
        }
        Err(e) => return db_error(e),
    };
    HttpResponse::Ok().json(json!({
        "profile": profile,
        "performance": history,
        "category_performance": category_performance,
    }))
}

// ---- Sizing + copied positions -----------------------------------------

async fn size_handler(body: web::Json<SizeRequest>) -> impl Responder {
    let r = &*body;
    if !(r.bankroll > 0.0) {
        return bad_request("bankroll must be positive");
    }
    if !(r.max_position_fraction > 0.0 && r.max_position_fraction <= 1.0) {
        return bad_request("max_position_fraction must be in (0, 1]");
    }
    if r.slippage_tolerance < 0.0 {
        return bad_request("slippage_tolerance must be >= 0");
    }
    if !(r.reference_price > 0.0 && r.reference_price < 1.0 && r.current_price > 0.0 && r.current_price < 1.0) {
        return bad_request("reference_price and current_price must be in (0, 1)");
    }
    if let Some(b) = r.budget {
        if b < 0.0 {
            return bad_request("budget must be >= 0");
        }
    }
    match size_copy(r) {
        Ok(result) => HttpResponse::Ok().json(result),
        Err(msg) => bad_request(msg),
    }
}

#[derive(Deserialize)]
struct NewCopy {
    user_id: i64,
    wallet: String,
    market_id: String,
    size: f64,
}

fn copied_json(row: &Row) -> serde_json::Value {
    json!({
        "id": row.get::<_, i64>("id"),
        "user_id": row.get::<_, i64>("user_id"),
        "wallet": row.get::<_, String>("wallet"),
        "market_id": row.get::<_, String>("market_id"),
        "size": row.get::<_, f64>("size"),
        "opened_at": row.get::<_, DateTime<Utc>>("opened_at").to_rfc3339(),
        "closed_at": row.get::<_, Option<DateTime<Utc>>>("closed_at").map(|t| t.to_rfc3339()),
    })
}

async fn open_copy(db: Db, body: web::Json<NewCopy>) -> impl Responder {
    if !(body.size > 0.0) {
        return bad_request("size must be positive");
    }
    match db
        .query_one(
            "INSERT INTO copied_positions (user_id, wallet, market_id, size) \
             VALUES ($1, $2, $3, $4) \
             RETURNING id, user_id, wallet, market_id, size, opened_at, closed_at",
            &[&body.user_id, &body.wallet, &body.market_id, &body.size],
        )
        .await
    {
        Ok(row) => HttpResponse::Created().json(copied_json(&row)),
        Err(e) => db_error(e),
    }
}

#[derive(Deserialize)]
struct ListCopyQuery {
    user_id: i64,
    open: Option<bool>,
}

async fn list_copies(db: Db, q: web::Query<ListCopyQuery>) -> impl Responder {
    // open=true -> only still-open; open=false -> only closed; omitted -> all.
    let sql = "SELECT id, user_id, wallet, market_id, size, opened_at, closed_at \
               FROM copied_positions \
               WHERE user_id = $1 \
                 AND ($2::bool IS NULL \
                      OR ($2 AND closed_at IS NULL) \
                      OR (NOT $2 AND closed_at IS NOT NULL)) \
               ORDER BY opened_at DESC";
    match db.query(sql, &[&q.user_id, &q.open]).await {
        Ok(rows) => HttpResponse::Ok().json(rows.iter().map(copied_json).collect::<Vec<_>>()),
        Err(e) => db_error(e),
    }
}

async fn close_copy(db: Db, id: web::Path<i64>) -> impl Responder {
    match db
        .query_opt(
            "UPDATE copied_positions SET closed_at = now() \
             WHERE id = $1 AND closed_at IS NULL \
             RETURNING id, user_id, wallet, market_id, size, opened_at, closed_at",
            &[&*id],
        )
        .await
    {
        Ok(Some(row)) => HttpResponse::Ok().json(copied_json(&row)),
        // Either no such id, or it was already closed.
        Ok(None) => HttpResponse::NotFound().json(json!({ "error": "no open position with that id" })),
        Err(e) => db_error(e),
    }
}

// ---- Auto-follow: mirror a trader's recent trades through the risk engine --

/// One of the trader's fills, as read from ClickHouse `monster.wallet_trades`.
#[derive(Deserialize, Clone)]
struct Trade {
    ts: String,
    venue: String,
    market_id: String,
    side: String,
    price: f64,
    size: f64,
}

#[derive(Deserialize)]
struct AutoFollowRequest {
    user_id: i64,
    wallet: String,
    /// Only consider trades strictly after this time (ClickHouse-parseable).
    /// Defaults to the latest copy already recorded for this user+wallet.
    since: Option<String>,
    bankroll: f64,
    /// Total stake budget for this run; defaults to bankroll. Spent down as
    /// positions are accepted, so later trades are capped once it runs out.
    budget: Option<f64>,
    max_position_fraction: f64,
    slippage_tolerance: f64,
    /// Fixed fraction of bankroll per trade...
    copy_ratio: Option<f64>,
    /// ...or mirror proportionally: each trade's size scaled by bankroll/trader_bankroll.
    trader_bankroll: Option<f64>,
    /// Current price per market_id, for the slippage gate; absent => use the
    /// trade's own price (no slippage).
    current_prices: Option<HashMap<String, f64>>,
    /// When false, accepted trades are recorded as copied positions.
    /// Defaults to true (advise only).
    dry_run: Option<bool>,
}

struct TradePlan {
    trade: Trade,
    result: SizeResult,
}

/// Evaluate each trade against the risk engine in time order, spending down a
/// shared budget. Pure: no I/O, so the sizing/budget logic is unit-testable.
fn plan_autofollow(trades: &[Trade], cfg: &AutoFollowRequest) -> Vec<TradePlan> {
    let mut remaining = cfg.budget.unwrap_or(cfg.bankroll);
    let mut plans = Vec::with_capacity(trades.len());
    for t in trades {
        let current_price = cfg
            .current_prices
            .as_ref()
            .and_then(|m| m.get(&t.market_id).copied())
            .unwrap_or(t.price);
        // Guard against prices outside a contract's (0, 1) range.
        if !(t.price > 0.0 && t.price < 1.0 && current_price > 0.0 && current_price < 1.0) {
            plans.push(TradePlan {
                trade: t.clone(),
                result: SizeResult {
                    accepted: false,
                    recommended_size: 0.0,
                    raw_size: 0.0,
                    slippage: 0.0,
                    caps_applied: vec![],
                    reason: Some("trade or current price outside (0, 1)".into()),
                },
            });
            continue;
        }
        let req = SizeRequest {
            bankroll: cfg.bankroll,
            budget: Some(remaining),
            max_position_fraction: cfg.max_position_fraction,
            slippage_tolerance: cfg.slippage_tolerance,
            reference_price: t.price,
            current_price,
            copy_ratio: cfg.copy_ratio,
            trader_size: cfg.trader_bankroll.map(|_| t.size),
            trader_bankroll: cfg.trader_bankroll,
        };
        let result = match size_copy(&req) {
            Ok(r) => r,
            Err(msg) => SizeResult {
                accepted: false,
                recommended_size: 0.0,
                raw_size: 0.0,
                slippage: 0.0,
                caps_applied: vec![],
                reason: Some(msg),
            },
        };
        if result.accepted {
            remaining -= result.recommended_size;
        }
        plans.push(TradePlan { trade: t.clone(), result });
    }
    plans
}

/// Fetch a wallet's trades after `since` from ClickHouse over HTTP, using a
/// parameterized query so the wallet string can't inject SQL.
async fn fetch_trades(ch: &Ch, wallet: &str, since: &str) -> Result<Vec<Trade>, String> {
    // JSONEachRow serializes DateTime64 as a quoted string, so `ts` needs no
    // cast — and casting it to a `ts` alias would shadow the column in WHERE.
    let sql = "SELECT ts, venue, market_id, side, price, size \
               FROM monster.wallet_trades \
               WHERE wallet = {wallet:String} \
                 AND ts > parseDateTime64BestEffort({since:String}) \
               ORDER BY ts \
               FORMAT JSONEachRow";
    let mut request = ch
        .http
        .post(&ch.url)
        .query(&[("param_wallet", wallet), ("param_since", since)])
        .body(sql);
    // Credentials go in headers, not the URL, so they stay out of logs.
    if let Some(user) = &ch.user {
        request = request.header("X-ClickHouse-User", user);
        if let Some(key) = &ch.key {
            request = request.header("X-ClickHouse-Key", key);
        }
    }
    let resp = request
        .send()
        .await
        .map_err(|e| format!("ClickHouse request failed: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| format!("ClickHouse body unreadable: {e}"))?;
    if !status.is_success() {
        return Err(format!("ClickHouse returned {status}: {}", text.trim()));
    }
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<Trade>(l).map_err(|e| format!("bad trade row {l:?}: {e}")))
        .collect()
}

async fn autofollow(db: Db, ch: ChData, body: web::Json<AutoFollowRequest>) -> impl Responder {
    let cfg = &*body;
    if !(cfg.bankroll > 0.0) {
        return bad_request("bankroll must be positive");
    }
    if !(cfg.max_position_fraction > 0.0 && cfg.max_position_fraction <= 1.0) {
        return bad_request("max_position_fraction must be in (0, 1]");
    }
    if cfg.slippage_tolerance < 0.0 {
        return bad_request("slippage_tolerance must be >= 0");
    }
    if cfg.copy_ratio.is_none() && cfg.trader_bankroll.is_none() {
        return bad_request("provide copy_ratio or trader_bankroll for sizing");
    }

    // Default `since` to the latest copy already recorded for this user+wallet,
    // so repeated runs don't double-copy the same history.
    let since = match &cfg.since {
        Some(s) => s.clone(),
        None => {
            let row = db
                .query_opt(
                    "SELECT max(opened_at) AS last FROM copied_positions \
                     WHERE user_id = $1 AND wallet = $2",
                    &[&cfg.user_id, &cfg.wallet],
                )
                .await;
            match row {
                Ok(Some(r)) => r
                    .get::<_, Option<DateTime<Utc>>>("last")
                    .map(|t| t.format("%Y-%m-%d %H:%M:%S%.3f").to_string())
                    .unwrap_or_else(|| "1970-01-01 00:00:00".into()),
                Ok(None) => "1970-01-01 00:00:00".into(),
                Err(e) => return db_error(e),
            }
        }
    };

    let trades = match fetch_trades(&ch, &cfg.wallet, &since).await {
        Ok(t) => t,
        Err(e) => return internal(e),
    };
    let plans = plan_autofollow(&trades, cfg);

    // Record accepted copies unless this is a dry run.
    let dry_run = cfg.dry_run.unwrap_or(true);
    let mut recorded = 0;
    if !dry_run {
        for p in plans.iter().filter(|p| p.result.accepted) {
            match db
                .query_one(
                    "INSERT INTO copied_positions (user_id, wallet, market_id, size) \
                     VALUES ($1, $2, $3, $4) RETURNING id",
                    &[&cfg.user_id, &cfg.wallet, &p.trade.market_id, &p.result.recommended_size],
                )
                .await
            {
                Ok(_) => recorded += 1,
                Err(e) => return db_error(e),
            }
        }
    }

    let accepted = plans.iter().filter(|p| p.result.accepted).count();
    let evaluated = plans
        .iter()
        .map(|p| {
            json!({
                "ts": p.trade.ts,
                "venue": p.trade.venue,
                "market_id": p.trade.market_id,
                "side": p.trade.side,
                "trader_price": p.trade.price,
                "trader_size": p.trade.size,
                "recommended_size": p.result.recommended_size,
                "accepted": p.result.accepted,
                "slippage": p.result.slippage,
                "caps_applied": p.result.caps_applied,
                "reason": p.result.reason,
            })
        })
        .collect::<Vec<_>>();
    HttpResponse::Ok().json(json!({
        "wallet": cfg.wallet,
        "since": since,
        "dry_run": dry_run,
        "trades_found": plans.len(),
        "accepted": accepted,
        "recorded": recorded,
        "evaluated": evaluated,
    }))
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8115);
    let url = std::env::var("DATABASE_URL")
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "DATABASE_URL must be set"))?;
    let (client, connection) = tokio_postgres::connect(&url, NoTls)
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    actix_web::rt::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("copy-trading: postgres connection closed: {e}");
        }
    });
    let db = web::Data::new(Arc::new(client));

    let ch = web::Data::new(Ch {
        http: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?,
        url: std::env::var("CLICKHOUSE_URL")
            .unwrap_or_else(|_| "http://localhost:8123/".into()),
        user: std::env::var("CLICKHOUSE_USER").ok().filter(|v| !v.is_empty()),
        key: std::env::var("CLICKHOUSE_PASSWORD").ok().filter(|v| !v.is_empty()),
    });

    println!("copy-trading listening on 0.0.0.0:{} (clickhouse {})", port, ch.url);
    HttpServer::new(move || {
        App::new()
            .app_data(db.clone())
            .app_data(ch.clone())
            .route("/healthz", web::get().to(healthz))
            .route("/leaderboard", web::get().to(leaderboard))
            .route("/traders/{wallet}", web::get().to(trader_detail))
            .route("/copy/size", web::post().to(size_handler))
            .route("/copy", web::post().to(open_copy))
            .route("/copy", web::get().to(list_copies))
            .route("/copy/{id}/close", web::post().to(close_copy))
            .route("/autofollow", web::post().to(autofollow))
    })
    .bind(("0.0.0.0", port))?
    .run()
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> SizeRequest {
        SizeRequest {
            bankroll: 1000.0,
            budget: None,
            max_position_fraction: 0.05,
            slippage_tolerance: 0.02,
            reference_price: 0.50,
            current_price: 0.50,
            copy_ratio: Some(0.10),
            trader_size: None,
            trader_bankroll: None,
        }
    }

    #[test]
    fn position_cap_limits_stake() {
        // copy_ratio 0.10 of 1000 = 100 raw, capped to 5% = 50.
        let r = size_copy(&req()).unwrap();
        assert!(r.accepted);
        assert_eq!(r.raw_size, 100.0);
        assert_eq!(r.recommended_size, 50.0);
        assert!(r.caps_applied.contains(&"max_position_fraction".to_string()));
    }

    #[test]
    fn budget_caps_below_position_cap() {
        let mut q = req();
        q.budget = Some(30.0);
        let r = size_copy(&q).unwrap();
        assert_eq!(r.recommended_size, 30.0);
        assert!(r.caps_applied.contains(&"budget".to_string()));
    }

    #[test]
    fn slippage_gate_rejects() {
        let mut q = req();
        q.current_price = 0.53; // (0.53-0.50)/0.50 = 6% > 2% tolerance
        let r = size_copy(&q).unwrap();
        assert!(!r.accepted);
        assert_eq!(r.recommended_size, 0.0);
        assert!(r.reason.unwrap().contains("slippage"));
    }

    #[test]
    fn favorable_price_is_not_slippage() {
        let mut q = req();
        q.current_price = 0.45; // cheaper than reference -> negative slippage, fine
        let r = size_copy(&q).unwrap();
        assert!(r.accepted);
        assert!(r.slippage < 0.0);
    }

    #[test]
    fn proportional_mode_mirrors_trader() {
        let mut q = req();
        q.copy_ratio = None;
        q.trader_size = Some(2000.0);
        q.trader_bankroll = Some(100_000.0);
        // 2000 * 1000 / 100000 = 20 raw, under the 50 cap.
        let r = size_copy(&q).unwrap();
        assert_eq!(r.raw_size, 20.0);
        assert_eq!(r.recommended_size, 20.0);
        assert!(r.caps_applied.is_empty());
    }

    #[test]
    fn missing_sizing_mode_errors() {
        let mut q = req();
        q.copy_ratio = None;
        assert!(size_copy(&q).is_err());
    }

    #[test]
    fn zero_budget_rejects() {
        let mut q = req();
        q.budget = Some(0.0);
        let r = size_copy(&q).unwrap();
        assert!(!r.accepted);
        assert!(r.reason.unwrap().contains("no budget"));
    }

    fn trade(market: &str, price: f64, size: f64) -> Trade {
        Trade {
            ts: "2026-10-08 12:00:00.000".into(),
            venue: "polymarket".into(),
            market_id: market.into(),
            side: "buy".into(),
            price,
            size,
        }
    }

    fn follow_cfg() -> AutoFollowRequest {
        AutoFollowRequest {
            user_id: 1,
            wallet: "0xw".into(),
            since: None,
            bankroll: 1000.0,
            budget: Some(120.0),
            max_position_fraction: 0.05, // cap 50 per position
            slippage_tolerance: 0.02,
            copy_ratio: Some(0.10), // 100 raw -> capped to 50
            trader_bankroll: None,
            current_prices: None,
            dry_run: Some(true),
        }
    }

    #[test]
    fn autofollow_spends_budget_across_trades() {
        let trades = vec![trade("m1", 0.50, 10.0), trade("m2", 0.50, 10.0), trade("m3", 0.50, 10.0)];
        let plans = plan_autofollow(&trades, &follow_cfg());
        // Each wants 50 (position cap); budget 120 covers 50, 50, then only 20 left.
        assert_eq!(plans[0].result.recommended_size, 50.0);
        assert_eq!(plans[1].result.recommended_size, 50.0);
        assert_eq!(plans[2].result.recommended_size, 20.0);
        assert!(plans[2].result.caps_applied.contains(&"budget".to_string()));
        assert_eq!(plans.iter().filter(|p| p.result.accepted).count(), 3);
    }

    #[test]
    fn autofollow_applies_slippage_gate_per_trade() {
        let trades = vec![trade("m1", 0.50, 10.0)];
        let mut cfg = follow_cfg();
        cfg.current_prices = Some(HashMap::from([("m1".to_string(), 0.60)])); // 20% worse
        let plans = plan_autofollow(&trades, &cfg);
        assert!(!plans[0].result.accepted);
        assert!(plans[0].result.reason.as_ref().unwrap().contains("slippage"));
    }

    #[test]
    fn autofollow_proportional_uses_trade_size() {
        let trades = vec![trade("m1", 0.50, 3000.0)];
        let mut cfg = follow_cfg();
        cfg.copy_ratio = None;
        cfg.trader_bankroll = Some(100_000.0); // 3000 * 1000/100000 = 30 raw
        let plans = plan_autofollow(&trades, &cfg);
        assert_eq!(plans[0].result.raw_size, 30.0);
        assert_eq!(plans[0].result.recommended_size, 30.0);
    }

    #[test]
    fn autofollow_skips_out_of_range_price() {
        let trades = vec![trade("m1", 1.5, 10.0)];
        let plans = plan_autofollow(&trades, &follow_cfg());
        assert!(!plans[0].result.accepted);
        assert!(plans[0].result.reason.as_ref().unwrap().contains("outside"));
    }
}
