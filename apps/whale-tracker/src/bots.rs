//! Copy-trading bots: named strategies that each follow one Polymarket wallet.
//!
//! A background loop polls every followed wallet's public activity. Each new
//! trade becomes an event (and a dashboard alert): either a simulated copy,
//! sized by the bot's buy/sell mode and filled at the live CLOB ask/bid, or a
//! skip with the reason (filter, cap, slippage...). PAPER ONLY: no order is
//! ever routed and no wallet or key is touched.
//!
//! A new bot starts from the leader's latest trade, so it copies only what
//! happens after it is created, never their history.
//!
//! GET    /bots                      every bot with its summary (spent, record, P&L)
//! POST   /bots {strategy, wallet, config}
//! GET    /bots/{id}                 bot + positions (open/redeemable/closed) + events
//! PATCH  /bots/{id} {strategy?, status?, config?}
//! DELETE /bots/{id}
//! GET    /bots/alerts?unseen=true&after_id=&limit=
//! POST   /bots/alerts/seen {bot_id?}   acknowledge (all, or one bot's)
//! POST   /bots/run                  run one poll cycle now

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use actix_web::{web, HttpResponse};
use chrono::{DateTime, TimeZone, Utc};
use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::pm::{get_json, is_address, num, text};
use crate::signals::{ask_side, book, clob_url, event_category};
use crate::{bad_request, internal, State};

/// Polymarket's minimum marketable order is about $1.
const MIN_ORDER_USD: f64 = 1.0;
const DUST_SHARES: f64 = 1e-6;
const MARK_TTL: Duration = Duration::from_secs(30);

// ---------------- config ----------------

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct BotConfig {
    /// percentage | range | fixed | portfolio
    pub buy_mode: String,
    /// % of the leader's trade (percentage, range).
    pub buy_pct: f64,
    /// $ per copy (fixed).
    pub buy_fixed: f64,
    /// $ floor / ceiling per copy (range).
    pub range_min: Option<f64>,
    pub range_max: Option<f64>,
    /// Paper bankroll: portfolio mode spends the same share of it as the
    /// leader's trade is of their open-position value.
    pub bankroll: f64,
    /// same | fixed | custom | manual
    pub sell_mode: String,
    /// % of our shares sold when the leader sells (custom).
    pub sell_pct: f64,
    /// $ sold when the leader sells (fixed).
    pub sell_fixed: f64,
    /// all | include | exclude, over Polymarket categories (SPORTS, CRYPTO, ...)
    pub category_mode: String,
    pub categories: Vec<String>,
    /// Total $ this bot may spend on buys.
    pub budget: Option<f64>,
    pub max_per_trade: Option<f64>,
    pub max_per_market: Option<f64>,
    /// Entry price window, as probability (0.05 = 5c).
    pub min_price: Option<f64>,
    pub max_price: Option<f64>,
    /// Max adverse move of our fill vs the leader's price, in price units.
    pub max_slippage: Option<f64>,
    /// Only copy leader trades of at least this many dollars.
    pub min_trigger: Option<f64>,
    pub min_volume: Option<f64>,
    pub min_liquidity: Option<f64>,
    /// Skip markets resolving further out than this.
    pub max_resolution_days: Option<f64>,
    /// Auto-sell a position when its P&L % crosses these.
    pub stop_loss_pct: Option<f64>,
    pub take_profit_pct: Option<f64>,
    /// Bot stops (status = expired) after this.
    pub expires_at: Option<DateTime<Utc>>,
}

impl Default for BotConfig {
    fn default() -> Self {
        BotConfig {
            buy_mode: "percentage".into(),
            buy_pct: 10.0,
            buy_fixed: 10.0,
            range_min: None,
            range_max: None,
            bankroll: 1000.0,
            sell_mode: "same".into(),
            sell_pct: 100.0,
            sell_fixed: 10.0,
            category_mode: "all".into(),
            categories: vec![],
            budget: None,
            max_per_trade: None,
            max_per_market: None,
            min_price: None,
            max_price: None,
            max_slippage: None,
            min_trigger: None,
            min_volume: None,
            min_liquidity: None,
            max_resolution_days: None,
            stop_loss_pct: None,
            take_profit_pct: None,
            expires_at: None,
        }
    }
}

pub fn validate(c: &BotConfig) -> Result<(), String> {
    let pos = |v: Option<f64>, name: &str| match v {
        Some(x) if !(x > 0.0) => Err(format!("{name} must be > 0")),
        _ => Ok(()),
    };
    match c.buy_mode.as_str() {
        "percentage" | "range" | "portfolio" if !(c.buy_pct > 0.0) => return Err("buy_pct must be > 0".into()),
        "percentage" | "range" | "portfolio" => {}
        "fixed" if !(c.buy_fixed >= MIN_ORDER_USD) => return Err(format!("buy_fixed must be at least ${MIN_ORDER_USD}")),
        "fixed" => {}
        m => return Err(format!("buy_mode must be percentage|range|fixed|portfolio, got {m:?}")),
    }
    if c.buy_mode == "portfolio" && !(c.bankroll > 0.0) {
        return Err("bankroll must be > 0 for portfolio mode".into());
    }
    if let (Some(lo), Some(hi)) = (c.range_min, c.range_max) {
        if lo > hi {
            return Err("range_min must be <= range_max".into());
        }
    }
    match c.sell_mode.as_str() {
        "same" | "manual" => {}
        "custom" if c.sell_pct > 0.0 && c.sell_pct <= 100.0 => {}
        "custom" => return Err("sell_pct must be in (0, 100]".into()),
        "fixed" if c.sell_fixed > 0.0 => {}
        "fixed" => return Err("sell_fixed must be > 0".into()),
        m => return Err(format!("sell_mode must be same|fixed|custom|manual, got {m:?}")),
    }
    if !matches!(c.category_mode.as_str(), "all" | "include" | "exclude") {
        return Err("category_mode must be all|include|exclude".into());
    }
    if c.category_mode != "all" && c.categories.is_empty() {
        return Err("pick at least one category to include/exclude".into());
    }
    for (v, n) in [
        (c.budget, "budget"), (c.max_per_trade, "max_per_trade"), (c.max_per_market, "max_per_market"),
        (c.min_trigger, "min_trigger"), (c.min_volume, "min_volume"), (c.min_liquidity, "min_liquidity"),
        (c.max_resolution_days, "max_resolution_days"), (c.stop_loss_pct, "stop_loss_pct"),
        (c.take_profit_pct, "take_profit_pct"), (c.max_slippage, "max_slippage"),
    ] {
        pos(v, n)?;
    }
    for (v, n) in [(c.min_price, "min_price"), (c.max_price, "max_price")] {
        if let Some(p) = v {
            if !(p > 0.0 && p < 1.0) {
                return Err(format!("{n} must be between 0 and 1 (probability)"));
            }
        }
    }
    Ok(())
}

// ---------------- pure decisions ----------------

pub fn category_passes(c: &BotConfig, category: &str) -> bool {
    let listed = c.categories.iter().any(|x| x.eq_ignore_ascii_case(category));
    match c.category_mode.as_str() {
        "include" => listed,
        "exclude" => !listed,
        _ => true,
    }
}

/// What we know when a leader BUY arrives.
pub struct BuyCtx {
    pub trader_usdc: f64,
    pub trader_price: f64,
    /// Our simulated fill (best ask); None when the book is unavailable.
    pub fill_price: Option<f64>,
    /// Leader's open-position value (portfolio mode).
    pub trader_portfolio: Option<f64>,
    pub spent_total: f64,
    pub spent_market: f64,
}

/// Dollars to copy for a leader BUY, or the reason to skip it.
pub fn decide_buy(c: &BotConfig, x: &BuyCtx) -> Result<f64, String> {
    if let Some(m) = c.min_trigger {
        if x.trader_usdc < m {
            return Err(format!("leader trade ${:.2} below min trigger ${m:.2}", x.trader_usdc));
        }
    }
    let price = x.fill_price.unwrap_or(x.trader_price);
    if let Some(lo) = c.min_price {
        if price < lo {
            return Err(format!("price {:.1}c below min entry {:.1}c", price * 100.0, lo * 100.0));
        }
    }
    if let Some(hi) = c.max_price {
        if price > hi {
            return Err(format!("price {:.1}c above max entry {:.1}c", price * 100.0, hi * 100.0));
        }
    }
    if let (Some(max), Some(fill)) = (c.max_slippage, x.fill_price) {
        let slip = fill - x.trader_price;
        if slip > max + 1e-9 {
            return Err(format!("slippage {:.1}c over max {:.1}c (ask {:.1}c vs leader {:.1}c)",
                slip * 100.0, max * 100.0, fill * 100.0, x.trader_price * 100.0));
        }
    }
    let mut usd = match c.buy_mode.as_str() {
        "fixed" => c.buy_fixed,
        "range" => {
            let v = x.trader_usdc * c.buy_pct / 100.0;
            v.max(c.range_min.unwrap_or(0.0)).min(c.range_max.unwrap_or(f64::INFINITY))
        }
        "portfolio" => match x.trader_portfolio.filter(|v| *v > 0.0) {
            Some(pv) => (x.trader_usdc / pv).min(1.0) * c.bankroll,
            None => return Err("leader's portfolio value unavailable".into()),
        },
        _ => x.trader_usdc * c.buy_pct / 100.0,
    };
    let mut capped = Vec::new();
    if let Some(m) = c.max_per_trade {
        if usd > m {
            usd = m;
            capped.push("max per trade");
        }
    }
    if let Some(m) = c.max_per_market {
        let room = m - x.spent_market;
        if usd > room {
            usd = room.max(0.0);
            capped.push("max per market");
        }
    }
    if let Some(b) = c.budget {
        let room = b - x.spent_total;
        if usd > room {
            usd = room.max(0.0);
            capped.push("budget");
        }
    }
    if usd < MIN_ORDER_USD {
        return Err(if capped.is_empty() {
            format!("copy size ${usd:.2} under the ${MIN_ORDER_USD:.0} minimum order")
        } else {
            format!("{} exhausted", capped.join(" + "))
        });
    }
    Ok(usd)
}

/// Shares to sell when the leader sells, or the reason not to.
/// `trader_held_before` = leader shares we saw them accumulate since the bot
/// started (0 when the position predates the bot: then mirror a full exit).
pub fn decide_sell(c: &BotConfig, our_shares: f64, trader_sold: f64, trader_held_before: f64, price: f64) -> Result<f64, String> {
    if our_shares <= DUST_SHARES {
        return Err("no copied position to sell".into());
    }
    let shares = match c.sell_mode.as_str() {
        "manual" => return Err("manual sell mode: alert only".into()),
        "fixed" => {
            if !(price > 0.0) {
                return Err("no price to size a fixed sell".into());
            }
            c.sell_fixed / price
        }
        "custom" => our_shares * c.sell_pct / 100.0,
        _ => {
            let frac = if trader_held_before > DUST_SHARES { (trader_sold / trader_held_before).min(1.0) } else { 1.0 };
            our_shares * frac
        }
    };
    Ok(shares.min(our_shares))
}

// ---------------- positions ----------------

/// One stored event, as needed to rebuild positions.
#[derive(Clone)]
pub struct Ev {
    pub ts: DateTime<Utc>,
    pub asset: String,
    pub condition_id: Option<String>,
    pub side: String,
    pub action: String,
    pub title: Option<String>,
    pub slug: Option<String>,
    pub event_slug: Option<String>,
    pub icon: Option<String>,
    pub outcome: Option<String>,
    pub category: Option<String>,
    pub trader_shares: f64,
    pub copy_shares: f64,
    pub copy_usdc: f64,
}

#[derive(Serialize, Clone, Default)]
pub struct Pos {
    pub asset: String,
    pub condition_id: Option<String>,
    pub title: Option<String>,
    pub slug: Option<String>,
    pub event_slug: Option<String>,
    pub icon: Option<String>,
    pub outcome: Option<String>,
    pub category: Option<String>,
    pub shares: f64,
    /// Cost basis of the shares still held.
    pub cost: f64,
    pub bought_usdc: f64,
    pub sold_usdc: f64,
    pub realized: f64,
    pub buys: u32,
    pub sells: u32,
    pub opened_at: Option<DateTime<Utc>>,
    pub last_at: Option<DateTime<Utc>>,
    // filled in by marking
    pub status: String,
    pub price: Option<f64>,
    pub value: Option<f64>,
    pub pnl: Option<f64>,
    pub pnl_pct: Option<f64>,
}

impl Pos {
    pub fn avg_price(&self) -> Option<f64> {
        (self.shares > DUST_SHARES).then(|| self.cost / self.shares)
    }
}

/// Average-cost positions from the copied fills, in time order.
pub fn build_positions(events: &[Ev]) -> Vec<Pos> {
    let mut order: Vec<String> = Vec::new();
    let mut map: HashMap<String, Pos> = HashMap::new();
    for e in events.iter().filter(|e| e.action == "copied") {
        let p = map.entry(e.asset.clone()).or_insert_with(|| {
            order.push(e.asset.clone());
            Pos {
                asset: e.asset.clone(),
                condition_id: e.condition_id.clone(),
                title: e.title.clone(),
                slug: e.slug.clone(),
                event_slug: e.event_slug.clone(),
                icon: e.icon.clone(),
                outcome: e.outcome.clone(),
                category: e.category.clone(),
                opened_at: Some(e.ts),
                ..Default::default()
            }
        });
        if e.side == "BUY" {
            p.shares += e.copy_shares;
            p.cost += e.copy_usdc;
            p.bought_usdc += e.copy_usdc;
            p.buys += 1;
        } else {
            let sold = e.copy_shares.min(p.shares);
            let basis = if p.shares > DUST_SHARES { p.cost * sold / p.shares } else { 0.0 };
            p.realized += e.copy_usdc - basis;
            p.cost -= basis;
            p.shares -= sold;
            p.sold_usdc += e.copy_usdc;
            p.sells += 1;
        }
        p.last_at = Some(e.ts);
    }
    order.into_iter().filter_map(|a| map.remove(&a)).collect()
}

/// Price state for one outcome token.
#[derive(Clone, Copy)]
pub enum Mark {
    Live(f64),
    Settled(f64),
    Unknown,
}

pub fn apply_mark(p: &mut Pos, m: Mark) {
    if p.shares <= DUST_SHARES {
        p.status = "closed".into();
        p.shares = 0.0;
        p.cost = 0.0;
        p.value = Some(0.0);
        p.pnl = Some(p.realized);
        p.pnl_pct = (p.bought_usdc > 0.0).then(|| p.realized / p.bought_usdc * 100.0);
        return;
    }
    let (status, price) = match m {
        Mark::Live(px) => ("open", Some(px)),
        Mark::Settled(px) => ("redeemable", Some(px)),
        Mark::Unknown => ("open", None),
    };
    p.status = status.into();
    p.price = price;
    p.value = price.map(|px| px * p.shares);
    p.pnl = p.value.map(|v| v - p.cost + p.realized);
    p.pnl_pct = match (p.pnl, p.bought_usdc > 0.0) {
        (Some(x), true) => Some(x / p.bought_usdc * 100.0),
        _ => None,
    };
}

#[derive(Serialize, Default)]
pub struct Summary {
    pub positions_open: usize,
    pub positions_redeemable: usize,
    pub positions_closed: usize,
    pub buys: u32,
    pub sells: u32,
    pub spent: f64,
    pub open_cost: f64,
    pub open_value: f64,
    pub realized_pnl: f64,
    pub unrealized_pnl: f64,
    pub total_pnl: f64,
    pub roi: Option<f64>,
    pub wins: u32,
    pub losses: u32,
    pub win_rate: Option<f64>,
}

/// Totals over marked positions. A position is graded once it is closed or
/// its market has settled: a win if it made money.
pub fn summarize(ps: &[Pos]) -> Summary {
    let mut s = Summary::default();
    for p in ps {
        s.buys += p.buys;
        s.sells += p.sells;
        s.spent += p.bought_usdc;
        match p.status.as_str() {
            "closed" => s.positions_closed += 1,
            "redeemable" => s.positions_redeemable += 1,
            _ => s.positions_open += 1,
        }
        if p.status == "closed" {
            s.realized_pnl += p.realized;
        } else {
            s.realized_pnl += p.realized;
            s.open_cost += p.cost;
            if let Some(v) = p.value {
                s.open_value += v;
                s.unrealized_pnl += v - p.cost;
            }
        }
        if p.status != "open" {
            match p.pnl {
                Some(x) if x > 0.005 => s.wins += 1,
                Some(x) if x < -0.005 => s.losses += 1,
                _ => {}
            }
        }
    }
    s.total_pnl = s.realized_pnl + s.unrealized_pnl;
    s.roi = (s.spent > 0.0).then(|| s.total_pnl / s.spent * 100.0);
    let graded = s.wins + s.losses;
    s.win_rate = (graded > 0).then(|| s.wins as f64 / graded as f64);
    s
}

/// Leader trades merged by (tx, asset, side): one order can fill in pieces.
#[derive(Clone, Debug, PartialEq)]
pub struct LeaderTrade {
    pub ts: i64,
    pub tx: String,
    pub asset: String,
    pub condition_id: Option<String>,
    pub side: String,
    pub price: f64,
    pub shares: f64,
    pub usdc: f64,
    pub title: Option<String>,
    pub slug: Option<String>,
    pub event_slug: Option<String>,
    pub icon: Option<String>,
    pub outcome: Option<String>,
}

pub fn merge_trades(rows: &[Value], after_ts: i64) -> Vec<LeaderTrade> {
    let mut out: Vec<LeaderTrade> = Vec::new();
    for a in rows {
        if text(a, "type").as_deref() != Some("TRADE") {
            continue;
        }
        let (Some(ts), Some(tx), Some(asset), Some(side)) =
            (num(a, "timestamp"), text(a, "transactionHash"), text(a, "asset"), text(a, "side"))
        else {
            continue;
        };
        let ts = ts as i64;
        if ts < after_ts {
            continue;
        }
        let side = side.to_uppercase();
        let shares = num(a, "size").unwrap_or(0.0);
        let usdc = num(a, "usdcSize").unwrap_or_else(|| shares * num(a, "price").unwrap_or(0.0));
        if let Some(t) = out.iter_mut().find(|t| t.tx == tx && t.asset == asset && t.side == side) {
            t.shares += shares;
            t.usdc += usdc;
            t.price = if t.shares > 0.0 { t.usdc / t.shares } else { t.price };
            continue;
        }
        out.push(LeaderTrade {
            ts,
            tx,
            asset,
            condition_id: text(a, "conditionId"),
            side,
            price: num(a, "price").unwrap_or(0.0),
            shares,
            usdc,
            title: text(a, "title"),
            slug: text(a, "slug"),
            event_slug: text(a, "eventSlug"),
            icon: text(a, "icon"),
            outcome: text(a, "outcome"),
        });
    }
    out.sort_by_key(|t| t.ts);
    out
}

// ---------------- I/O helpers ----------------

fn mark_cache() -> &'static Mutex<HashMap<String, (Instant, Mark)>> {
    static C: OnceLock<Mutex<HashMap<String, (Instant, Mark)>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Live midpoint, or the settlement price once the market has resolved.
async fn mark(state: &State, asset: &str) -> Mark {
    if let Some((at, m)) = mark_cache().lock().unwrap().get(asset).copied() {
        if matches!(m, Mark::Settled(_)) || at.elapsed() < MARK_TTL {
            return m;
        }
    }
    let mut m = Mark::Unknown;
    if let Ok(resp) = state.http.get(format!("{}/midpoint?token_id={asset}", clob_url())).send().await {
        if resp.status().is_success() {
            if let Ok(v) = resp.json::<Value>().await {
                if let Some(px) = num(&v, "mid") {
                    m = Mark::Live(px);
                }
            }
        }
    }
    if matches!(m, Mark::Unknown) {
        // No book: the market has likely closed. Read the settlement.
        let url = format!("{}/markets?clob_token_ids={asset}&closed=true", state.gamma_url);
        if let Ok(v) = get_json(state, &url).await {
            if let Some(mk) = v.as_array().and_then(|a| a.first()) {
                let parse = |k: &str| -> Vec<String> {
                    mk[k].as_str().and_then(|s| serde_json::from_str::<Vec<String>>(s).ok()).unwrap_or_default()
                };
                let (ids, prices) = (parse("clobTokenIds"), parse("outcomePrices"));
                if let Some(px) = ids.iter().position(|x| x == asset).and_then(|i| prices.get(i)).and_then(|p| p.parse::<f64>().ok()) {
                    m = if mk["closed"].as_bool().unwrap_or(false) { Mark::Settled(px) } else { Mark::Live(px) };
                }
            }
        }
    }
    if !matches!(m, Mark::Unknown) {
        mark_cache().lock().unwrap().insert(asset.to_string(), (Instant::now(), m));
    }
    m
}

async fn marked_positions(state: &State, events: &[Ev]) -> Vec<Pos> {
    let ps = build_positions(events);
    stream::iter(ps)
        .map(|mut p| async move {
            let m = if p.shares > DUST_SHARES { mark(state, &p.asset).await } else { Mark::Unknown };
            apply_mark(&mut p, m);
            p
        })
        .buffered(8)
        .collect()
        .await
}

const EV_COLS: &str = "id, bot_id, trade_ts, tx, asset, condition_id, side, title, slug, event_slug, icon, outcome, \
    category, trader_price, trader_shares, trader_usdc, action, copy_price, copy_shares, copy_usdc, reason, seen";

fn ev_from_row(r: &tokio_postgres::Row) -> Ev {
    Ev {
        ts: r.get("trade_ts"),
        asset: r.get("asset"),
        condition_id: r.get("condition_id"),
        side: r.get("side"),
        action: r.get("action"),
        title: r.get("title"),
        slug: r.get("slug"),
        event_slug: r.get("event_slug"),
        icon: r.get("icon"),
        outcome: r.get("outcome"),
        category: r.get("category"),
        trader_shares: r.get::<_, Option<f64>>("trader_shares").unwrap_or(0.0),
        copy_shares: r.get::<_, Option<f64>>("copy_shares").unwrap_or(0.0),
        copy_usdc: r.get::<_, Option<f64>>("copy_usdc").unwrap_or(0.0),
    }
}

fn event_json(r: &tokio_postgres::Row) -> Value {
    json!({
        "id": r.get::<_, i64>("id"),
        "bot_id": r.get::<_, i64>("bot_id"),
        "ts": r.get::<_, DateTime<Utc>>("trade_ts"),
        "tx": r.get::<_, String>("tx"),
        "asset": r.get::<_, String>("asset"),
        "side": r.get::<_, String>("side"),
        "title": r.get::<_, Option<String>>("title"),
        "slug": r.get::<_, Option<String>>("slug"),
        "event_slug": r.get::<_, Option<String>>("event_slug"),
        "icon": r.get::<_, Option<String>>("icon"),
        "outcome": r.get::<_, Option<String>>("outcome"),
        "category": r.get::<_, Option<String>>("category"),
        "trader_price": r.get::<_, Option<f64>>("trader_price"),
        "trader_shares": r.get::<_, Option<f64>>("trader_shares"),
        "trader_usdc": r.get::<_, Option<f64>>("trader_usdc"),
        "action": r.get::<_, String>("action"),
        "copy_price": r.get::<_, Option<f64>>("copy_price"),
        "copy_shares": r.get::<_, Option<f64>>("copy_shares"),
        "copy_usdc": r.get::<_, Option<f64>>("copy_usdc"),
        "reason": r.get::<_, Option<String>>("reason"),
        "seen": r.get::<_, bool>("seen"),
    })
}

async fn load_events(state: &State, bot_id: i64) -> Result<Vec<tokio_postgres::Row>, String> {
    state.db.query(&format!("SELECT {EV_COLS} FROM copy_bot_events WHERE bot_id=$1 ORDER BY trade_ts, id"), &[&bot_id])
        .await.map_err(|e| e.to_string())
}

const BOT_COLS: &str = "id, strategy, wallet, trader_name, trader_image, status, config, cursor_ts, created_at, \
    updated_at, last_checked_at, last_trade_at, last_error";

struct Bot {
    id: i64,
    strategy: String,
    wallet: String,
    config: BotConfig,
    cursor_ts: i64,
}

fn bot_from_row(r: &tokio_postgres::Row) -> Bot {
    Bot {
        id: r.get("id"),
        strategy: r.get("strategy"),
        wallet: r.get("wallet"),
        config: serde_json::from_value(r.get::<_, Value>("config")).unwrap_or_default(),
        cursor_ts: r.get("cursor_ts"),
    }
}

fn bot_json(r: &tokio_postgres::Row) -> Value {
    json!({
        "id": r.get::<_, i64>("id"),
        "strategy": r.get::<_, String>("strategy"),
        "wallet": r.get::<_, String>("wallet"),
        "trader_name": r.get::<_, Option<String>>("trader_name"),
        "trader_image": r.get::<_, Option<String>>("trader_image"),
        "status": r.get::<_, String>("status"),
        "config": serde_json::from_value::<BotConfig>(r.get::<_, Value>("config")).unwrap_or_default(),
        "created_at": r.get::<_, DateTime<Utc>>("created_at"),
        "updated_at": r.get::<_, DateTime<Utc>>("updated_at"),
        "last_checked_at": r.get::<_, Option<DateTime<Utc>>>("last_checked_at"),
        "last_trade_at": r.get::<_, Option<DateTime<Utc>>>("last_trade_at"),
        "last_error": r.get::<_, Option<String>>("last_error"),
    })
}

/// Market facts needed by the volume/liquidity/resolution filters.
async fn market_facts(state: &State, asset: &str) -> Option<(f64, f64, Option<DateTime<Utc>>)> {
    let url = format!("{}/markets?clob_token_ids={asset}", state.gamma_url);
    let v = get_json(state, &url).await.ok()?;
    let m = v.as_array()?.first()?.clone();
    let end = text(&m, "endDate").and_then(|s| DateTime::parse_from_rfc3339(&s).ok()).map(|d| d.with_timezone(&Utc));
    Some((num(&m, "volumeNum").or_else(|| num(&m, "volume")).unwrap_or(0.0),
          num(&m, "liquidityNum").or_else(|| num(&m, "liquidity")).unwrap_or(0.0), end))
}

fn best_bid(book: &Value) -> Option<f64> {
    book["bids"].as_array()?.iter().filter_map(|l| num(l, "price")).fold(None, |m, p| Some(m.map_or(p, |x: f64| x.max(p))))
}

// ---------------- the poll cycle ----------------

/// Per-asset running state for one bot while processing new trades.
#[derive(Default, Clone, Copy)]
struct Hold {
    our_shares: f64,
    trader_shares: f64,
}

struct Outcome {
    action: &'static str,
    price: Option<f64>,
    shares: Option<f64>,
    usdc: Option<f64>,
    reason: Option<String>,
}

/// `mirror`: the trades are the leader's existing open positions, bought into
/// once at creation. They don't move the cursor and don't raise alerts.
async fn process_bot(state: &State, bot: &Bot, trades: &[LeaderTrade], cats: &Mutex<HashMap<String, String>>, mirror: bool) -> Result<usize, String> {
    let new: Vec<&LeaderTrade> = trades.iter().filter(|t| t.ts >= bot.cursor_ts).collect();
    if new.is_empty() {
        return Ok(0);
    }
    let rows = load_events(state, bot.id).await?;
    let seen: HashSet<(String, String, String)> = rows.iter()
        .map(|r| (r.get::<_, String>("tx"), r.get::<_, String>("asset"), r.get::<_, String>("side"))).collect();
    let events: Vec<Ev> = rows.iter().map(ev_from_row).collect();
    let mut hold: HashMap<String, Hold> = HashMap::new();
    let mut spent_total = 0.0;
    let mut spent_market: HashMap<String, f64> = HashMap::new();
    for e in &events {
        let h = hold.entry(e.asset.clone()).or_default();
        let sign = if e.side == "BUY" { 1.0 } else { -1.0 };
        h.trader_shares = (h.trader_shares + sign * e.trader_shares).max(0.0);
        if e.action == "copied" {
            h.our_shares = (h.our_shares + sign * e.copy_shares).max(0.0);
            if e.side == "BUY" {
                spent_total += e.copy_usdc;
                *spent_market.entry(e.condition_id.clone().unwrap_or_else(|| e.asset.clone())).or_default() += e.copy_usdc;
            }
        }
    }
    let c = &bot.config;
    let mut portfolio: Option<Option<f64>> = None;
    let mut inserted = 0;
    let mut max_ts = bot.cursor_ts;
    for t in new {
        max_ts = max_ts.max(t.ts);
        if seen.contains(&(t.tx.clone(), t.asset.clone(), t.side.clone())) {
            continue;
        }
        let category = match t.event_slug.as_deref() {
            Some(s) => event_category(state, s, cats).await,
            None => "OVERALL".to_string(),
        };
        let market_key = t.condition_id.clone().unwrap_or_else(|| t.asset.clone());
        let h = hold.get(&t.asset).copied().unwrap_or_default();

        let outcome = if t.side == "BUY" {
            let mut skip: Option<String> = None;
            if !category_passes(c, &category) {
                skip = Some(format!("category {} filtered out", category.to_lowercase()));
            }
            if skip.is_none() && (c.min_volume.is_some() || c.min_liquidity.is_some() || c.max_resolution_days.is_some()) {
                match market_facts(state, &t.asset).await {
                    Some((vol, liq, end)) => {
                        if let Some(m) = c.min_volume.filter(|m| vol < *m) {
                            skip = Some(format!("market volume ${vol:.0} under ${m:.0}"));
                        } else if let Some(m) = c.min_liquidity.filter(|m| liq < *m) {
                            skip = Some(format!("market liquidity ${liq:.0} under ${m:.0}"));
                        } else if let (Some(d), Some(end)) = (c.max_resolution_days, end) {
                            let days = (end - Utc::now()).num_minutes() as f64 / 1440.0;
                            if days > d {
                                skip = Some(format!("resolves in {days:.0}d, over {d:.0}d max"));
                            }
                        }
                    }
                    None => skip = Some("market details unavailable for volume/liquidity/resolution filters".into()),
                }
            }
            match skip {
                Some(reason) => Outcome { action: "skipped", price: None, shares: None, usdc: None, reason: Some(reason) },
                None => {
                    let fill = book(state, &t.asset).await.and_then(|b| ask_side(&b).0);
                    if c.buy_mode == "portfolio" && portfolio.is_none() {
                        let v = get_json(state, &format!("{}/value?user={}", state.pm_url, bot.wallet)).await.ok();
                        portfolio = Some(v.as_ref().and_then(|v| v.as_array()).and_then(|a| a.first()).and_then(|r| num(r, "value")));
                    }
                    let ctx = BuyCtx {
                        trader_usdc: t.usdc,
                        trader_price: t.price,
                        fill_price: fill,
                        trader_portfolio: portfolio.flatten(),
                        spent_total,
                        spent_market: spent_market.get(&market_key).copied().unwrap_or(0.0),
                    };
                    match decide_buy(c, &ctx) {
                        Ok(usd) => {
                            let px = fill.unwrap_or(t.price);
                            let shares = usd / px;
                            spent_total += usd;
                            *spent_market.entry(market_key.clone()).or_default() += usd;
                            hold.entry(t.asset.clone()).or_default().our_shares += shares;
                            let note = match (mirror, fill.is_none()) {
                                (true, true) => Some("mirrored existing position; no live book: filled at leader's price".to_string()),
                                (true, false) => Some("mirrored existing position".to_string()),
                                (false, true) => Some("no live book: filled at leader's price".to_string()),
                                (false, false) => None,
                            };
                            Outcome { action: "copied", price: Some(px), shares: Some(shares), usdc: Some(usd), reason: note }
                        }
                        Err(reason) => Outcome { action: "skipped", price: fill, shares: None, usdc: None, reason: Some(reason) },
                    }
                }
            }
        } else {
            let bid = if h.our_shares > DUST_SHARES { book(state, &t.asset).await.and_then(|b| best_bid(&b)) } else { None };
            let px = bid.unwrap_or(t.price);
            match decide_sell(c, h.our_shares, t.shares, h.trader_shares, px) {
                Ok(sh) => {
                    hold.entry(t.asset.clone()).or_default().our_shares -= sh;
                    Outcome { action: "copied", price: Some(px), shares: Some(sh), usdc: Some(sh * px), reason: None }
                }
                Err(reason) => Outcome { action: "skipped", price: None, shares: None, usdc: None, reason: Some(reason) },
            }
        };
        {
            let e = hold.entry(t.asset.clone()).or_default();
            e.trader_shares = if t.side == "BUY" { e.trader_shares + t.shares } else { (e.trader_shares - t.shares).max(0.0) };
        }
        let ts = Utc.timestamp_opt(t.ts, 0).single().unwrap_or_else(Utc::now);
        inserted += state.db.execute(
            "INSERT INTO copy_bot_events (bot_id, trade_ts, tx, asset, condition_id, side, title, slug, event_slug, icon,
                 outcome, category, trader_price, trader_shares, trader_usdc, action, copy_price, copy_shares, copy_usdc, reason, seen)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21)
             ON CONFLICT (bot_id, tx, asset, side) DO NOTHING",
            &[&bot.id, &ts, &t.tx, &t.asset, &t.condition_id, &t.side, &t.title, &t.slug, &t.event_slug, &t.icon,
              &t.outcome, &category, &t.price, &t.shares, &t.usdc, &outcome.action, &outcome.price, &outcome.shares,
              &outcome.usdc, &outcome.reason, &mirror],
        ).await.map_err(|e| e.to_string())? as usize;
    }
    if mirror {
        return Ok(inserted);
    }
    state.db.execute(
        "UPDATE copy_bots SET cursor_ts=GREATEST(cursor_ts,$2), last_trade_at=to_timestamp($2) WHERE id=$1",
        &[&bot.id, &max_ts],
    ).await.map_err(|e| e.to_string())?;
    Ok(inserted)
}

/// Stop-loss / take-profit: sell a whole position at the bid when its P&L %
/// crosses the bot's thresholds.
async fn check_exits(state: &State, bot: &Bot) -> Result<usize, String> {
    let c = &bot.config;
    if c.stop_loss_pct.is_none() && c.take_profit_pct.is_none() {
        return Ok(0);
    }
    let rows = load_events(state, bot.id).await?;
    let events: Vec<Ev> = rows.iter().map(ev_from_row).collect();
    let mut n = 0;
    for p in build_positions(&events).into_iter().filter(|p| p.shares > DUST_SHARES) {
        let Mark::Live(mid) = mark(state, &p.asset).await else { continue };
        let Some(avg) = p.avg_price() else { continue };
        let move_pct = (mid - avg) / avg * 100.0;
        let reason = if c.stop_loss_pct.is_some_and(|sl| move_pct <= -sl) {
            format!("stop loss: {move_pct:.1}% from entry")
        } else if c.take_profit_pct.is_some_and(|tp| move_pct >= tp) {
            format!("take profit: +{move_pct:.1}% from entry")
        } else {
            continue;
        };
        let px = book(state, &p.asset).await.and_then(|b| best_bid(&b)).unwrap_or(mid);
        let now = Utc::now();
        let tx = format!("exit:{}", now.timestamp_millis());
        n += state.db.execute(
            "INSERT INTO copy_bot_events (bot_id, trade_ts, tx, asset, condition_id, side, title, slug, event_slug, icon,
                 outcome, category, action, copy_price, copy_shares, copy_usdc, reason)
             VALUES ($1,$2,$3,$4,$5,'SELL',$6,$7,$8,$9,$10,$11,'copied',$12,$13,$14,$15)
             ON CONFLICT (bot_id, tx, asset, side) DO NOTHING",
            &[&bot.id, &now, &tx, &p.asset, &p.condition_id, &p.title, &p.slug, &p.event_slug, &p.icon,
              &p.outcome, &p.category, &px, &p.shares, &(p.shares * px), &reason],
        ).await.map_err(|e| e.to_string())? as usize;
    }
    Ok(n)
}

fn running() -> &'static Mutex<bool> {
    static R: OnceLock<Mutex<bool>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(false))
}

pub async fn run_cycle(state: &State) -> Result<Value, String> {
    {
        let mut r = running().lock().unwrap();
        if *r {
            return Ok(json!({"state": "already running"}));
        }
        *r = true;
    }
    let out = cycle(state).await;
    *running().lock().unwrap() = false;
    out
}

async fn cycle(state: &State) -> Result<Value, String> {
    state.db.execute(
        "UPDATE copy_bots SET status='expired', updated_at=now()
          WHERE status='active' AND (config->>'expires_at') IS NOT NULL AND (config->>'expires_at')::timestamptz < now()",
        &[],
    ).await.map_err(|e| e.to_string())?;
    let rows = state.db.query(&format!("SELECT {BOT_COLS} FROM copy_bots WHERE status='active' ORDER BY id"), &[])
        .await.map_err(|e| e.to_string())?;
    let bots: Vec<Bot> = rows.iter().map(bot_from_row).collect();
    let mut by_wallet: HashMap<String, Vec<&Bot>> = HashMap::new();
    for b in &bots {
        by_wallet.entry(b.wallet.clone()).or_default().push(b);
    }
    let cats = Mutex::new(HashMap::new());
    let mut copied = 0;
    for (wallet, group) in by_wallet {
        let since = group.iter().map(|b| b.cursor_ts).min().unwrap_or(0);
        let url = format!("{}/activity?user={wallet}&type=TRADE&limit=200", state.pm_url);
        let fetched = get_json(state, &url).await.map(|v| merge_trades(v.as_array().map(|a| a.as_slice()).unwrap_or(&[]), since));
        for b in group {
            let res = match &fetched {
                Ok(trades) => process_bot(state, b, trades, &cats, false).await,
                Err(e) => Err(e.clone()),
            };
            let res = match res {
                Ok(n) => check_exits(state, b).await.map(|m| n + m),
                Err(e) => Err(e),
            };
            let err = res.as_ref().err().cloned();
            if let Some(e) = &err {
                crate::log_err(format!("bot {} ({}): {e}", b.id, b.strategy));
            }
            copied += res.unwrap_or(0);
            let _ = state.db.execute(
                "UPDATE copy_bots SET last_checked_at=now(), last_error=$2 WHERE id=$1", &[&b.id, &err],
            ).await;
        }
    }
    Ok(json!({"bots": bots.len(), "events": copied}))
}

pub fn spawn_loop(state: web::Data<State>) {
    let secs: u64 = std::env::var("COPY_BOTS_INTERVAL_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(15);
    if secs == 0 {
        return;
    }
    actix_web::rt::spawn(async move {
        loop {
            if let Err(e) = run_cycle(&state).await {
                crate::log_err(format!("copy bots cycle: {e}"));
            }
            actix_web::rt::time::sleep(Duration::from_secs(secs)).await;
        }
    });
}

// ---------------- HTTP ----------------

async fn bot_with_summary(state: &State, r: &tokio_postgres::Row) -> Result<Value, String> {
    let id: i64 = r.get("id");
    let rows = load_events(state, id).await?;
    let events: Vec<Ev> = rows.iter().map(ev_from_row).collect();
    let ps = marked_positions(state, &events).await;
    let mut v = bot_json(r);
    let unseen = rows.iter().filter(|r| !r.get::<_, bool>("seen")).count();
    let skipped = rows.iter().filter(|r| r.get::<_, String>("action") == "skipped").count();
    v["summary"] = json!(summarize(&ps));
    v["summary"]["leader_trades"] = json!(rows.len());
    v["summary"]["skipped"] = json!(skipped);
    v["summary"]["unseen"] = json!(unseen);
    Ok(v)
}

pub async fn list(state: web::Data<State>) -> HttpResponse {
    let rows = match state.db.query(&format!("SELECT {BOT_COLS} FROM copy_bots ORDER BY created_at DESC"), &[]).await {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let st = &state;
    let out: Vec<Result<Value, String>> = stream::iter(rows.iter()).map(|r| bot_with_summary(st, r)).buffered(4).collect().await;
    match out.into_iter().collect::<Result<Vec<_>, _>>() {
        Ok(v) => HttpResponse::Ok().json(json!({"rows": v})),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
pub struct NewBot {
    strategy: String,
    wallet: String,
    #[serde(default)]
    config: BotConfig,
    /// Also buy into the leader's currently open positions at today's price.
    #[serde(default)]
    mirror_existing: bool,
}

/// Most open positions mirrored at creation (largest by value first).
const MIRROR_MAX: usize = 50;

/// The leader's open (unresolved) positions as synthetic BUY trades sized at
/// their current value, so they flow through the normal sizing and filters.
pub fn positions_as_trades(rows: &[Value], bot_id: i64, now: i64) -> Vec<LeaderTrade> {
    let mut out: Vec<LeaderTrade> = rows.iter().filter_map(|p| {
        let asset = text(p, "asset")?;
        let shares = num(p, "size")?;
        let price = num(p, "curPrice")?;
        if p["redeemable"].as_bool() == Some(true) || !(0.01..=0.99).contains(&price) || shares <= 0.0 {
            return None;
        }
        let usdc = num(p, "currentValue").unwrap_or(shares * price);
        (usdc >= 1.0).then(|| LeaderTrade {
            ts: now,
            tx: format!("mirror:{bot_id}"),
            asset,
            condition_id: text(p, "conditionId"),
            side: "BUY".into(),
            price,
            shares,
            usdc,
            title: text(p, "title"),
            slug: text(p, "slug"),
            event_slug: text(p, "eventSlug"),
            icon: text(p, "icon"),
            outcome: text(p, "outcome"),
        })
    }).collect();
    out.sort_by(|a, b| b.usdc.total_cmp(&a.usdc));
    out.truncate(MIRROR_MAX);
    out
}

async fn mirror_positions(state: &State, bot: &Bot) -> Result<usize, String> {
    let v = get_json(state, &format!("{}/positions?user={}&sizeThreshold=1&limit=500", state.pm_url, bot.wallet)).await?;
    let trades = positions_as_trades(v.as_array().map(|a| a.as_slice()).unwrap_or(&[]), bot.id, Utc::now().timestamp().max(bot.cursor_ts));
    process_bot(state, bot, &trades, &Mutex::new(HashMap::new()), true).await
}

pub async fn create(state: web::Data<State>, body: web::Json<NewBot>) -> HttpResponse {
    let wallet = body.wallet.trim().to_lowercase();
    if !is_address(&wallet) {
        return bad_request("wallet must be a 0x address");
    }
    let strategy = body.strategy.trim();
    if strategy.is_empty() || strategy.len() > 80 {
        return bad_request("strategy name is required (max 80 characters)");
    }
    if let Err(e) = validate(&body.config) {
        return bad_request(e);
    }
    // Start at the leader's latest trade so only new trades are copied.
    let latest = get_json(&state, &format!("{}/activity?user={wallet}&type=TRADE&limit=1", state.pm_url)).await;
    let first = latest.as_ref().ok().and_then(|v| v.as_array()).and_then(|a| a.first()).cloned();
    let cursor = first.as_ref().and_then(|a| num(a, "timestamp")).map(|t| t as i64 + 1).unwrap_or_else(|| Utc::now().timestamp());
    let prof = get_json(&state, &format!("{}/public-profile?address={wallet}", state.gamma_url)).await.ok();
    let name = prof.as_ref().and_then(|p| text(p, "name"))
        .or_else(|| first.as_ref().and_then(|a| text(a, "name")))
        .or_else(|| prof.as_ref().and_then(|p| text(p, "pseudonym")));
    let image = prof.as_ref().and_then(|p| text(p, "profileImage"))
        .or_else(|| first.as_ref().and_then(|a| text(a, "profileImage")));
    let cfg = json!(body.config);
    match state.db.query_one(
        &format!("INSERT INTO copy_bots (strategy, wallet, trader_name, trader_image, config, cursor_ts)
                  VALUES ($1,$2,$3,$4,$5,$6) RETURNING {BOT_COLS}"),
        &[&strategy, &wallet, &name, &image, &cfg, &cursor],
    ).await {
        Ok(r) => {
            let mut mirror_err = None;
            if body.mirror_existing {
                if let Err(e) = mirror_positions(&state, &bot_from_row(&r)).await {
                    crate::log_err(format!("mirror positions: {e}"));
                    mirror_err = Some(e);
                }
            }
            match bot_with_summary(&state, &r).await {
                Ok(mut v) => {
                    if let Some(e) = mirror_err {
                        v["mirror_error"] = json!(e);
                    }
                    HttpResponse::Created().json(v)
                }
                Err(e) => internal(e),
            }
        }
        Err(e) => internal(e),
    }
}

pub async fn detail(state: web::Data<State>, path: web::Path<i64>) -> HttpResponse {
    let id = path.into_inner();
    let r = match state.db.query_opt(&format!("SELECT {BOT_COLS} FROM copy_bots WHERE id=$1"), &[&id]).await {
        Ok(Some(r)) => r,
        Ok(None) => return HttpResponse::NotFound().json(json!({"error": "no such bot"})),
        Err(e) => return internal(e),
    };
    let rows = match load_events(&state, id).await {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let events: Vec<Ev> = rows.iter().map(ev_from_row).collect();
    let mut ps = marked_positions(&state, &events).await;
    ps.sort_by(|a, b| b.last_at.cmp(&a.last_at));
    let mut v = bot_json(&r);
    v["summary"] = json!(summarize(&ps));
    v["positions"] = json!(ps);
    v["events"] = json!(rows.iter().rev().take(300).map(event_json).collect::<Vec<_>>());
    HttpResponse::Ok().json(v)
}

#[derive(Deserialize)]
pub struct PatchBot {
    strategy: Option<String>,
    status: Option<String>,
    config: Option<BotConfig>,
}

pub async fn update(state: web::Data<State>, path: web::Path<i64>, body: web::Json<PatchBot>) -> HttpResponse {
    let id = path.into_inner();
    if let Some(s) = &body.status {
        if !matches!(s.as_str(), "active" | "paused") {
            return bad_request("status must be active or paused");
        }
    }
    if let Some(s) = &body.strategy {
        if s.trim().is_empty() || s.len() > 80 {
            return bad_request("strategy name is required (max 80 characters)");
        }
    }
    if let Some(c) = &body.config {
        if let Err(e) = validate(c) {
            return bad_request(e);
        }
    }
    let cfg = body.config.as_ref().map(|c| json!(c));
    let strategy = body.strategy.as_ref().map(|s| s.trim().to_string());
    // Resuming skips what the leader did while paused (no stale catch-up copies).
    let resume = body.status.as_deref() == Some("active");
    match state.db.query_opt(
        &format!("UPDATE copy_bots SET strategy=COALESCE($2, strategy), status=COALESCE($3, status),
                      config=COALESCE($4, config), updated_at=now(),
                      cursor_ts=CASE WHEN $5 AND status<>'active' THEN GREATEST(cursor_ts, extract(epoch from now())::bigint) ELSE cursor_ts END
                  WHERE id=$1 RETURNING {BOT_COLS}"),
        &[&id, &strategy, &body.status, &cfg, &resume],
    ).await {
        Ok(Some(r)) => match bot_with_summary(&state, &r).await {
            Ok(v) => HttpResponse::Ok().json(v),
            Err(e) => internal(e),
        },
        Ok(None) => HttpResponse::NotFound().json(json!({"error": "no such bot"})),
        Err(e) => internal(e),
    }
}

pub async fn remove(state: web::Data<State>, path: web::Path<i64>) -> HttpResponse {
    match state.db.execute("DELETE FROM copy_bots WHERE id=$1", &[&path.into_inner()]).await {
        Ok(0) => HttpResponse::NotFound().json(json!({"error": "no such bot"})),
        Ok(_) => HttpResponse::NoContent().finish(),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
pub struct AlertQ {
    unseen: Option<bool>,
    after_id: Option<i64>,
    limit: Option<i64>,
}

pub async fn alerts(state: web::Data<State>, q: web::Query<AlertQ>) -> HttpResponse {
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    let unseen = q.unseen.unwrap_or(false);
    let after = q.after_id.unwrap_or(0);
    let sql = format!(
        "SELECT {}, b.strategy, b.wallet, b.trader_name, b.trader_image
           FROM copy_bot_events e JOIN copy_bots b ON b.id = e.bot_id
          WHERE e.id > $1 AND (NOT $2 OR NOT e.seen)
          ORDER BY e.id DESC LIMIT $3",
        EV_COLS.split(", ").map(|c| format!("e.{c}")).collect::<Vec<_>>().join(", ")
    );
    let rows = match state.db.query(&sql, &[&after, &unseen, &limit]).await {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let unseen_count: i64 = match state.db.query_one("SELECT count(*) FROM copy_bot_events WHERE NOT seen", &[]).await {
        Ok(r) => r.get(0),
        Err(e) => return internal(e),
    };
    let out: Vec<Value> = rows.iter().map(|r| {
        let mut v = event_json(r);
        v["strategy"] = json!(r.get::<_, String>("strategy"));
        v["wallet"] = json!(r.get::<_, String>("wallet"));
        v["trader_name"] = json!(r.get::<_, Option<String>>("trader_name"));
        v["trader_image"] = json!(r.get::<_, Option<String>>("trader_image"));
        v
    }).collect();
    HttpResponse::Ok().json(json!({"rows": out, "unseen": unseen_count}))
}

#[derive(Deserialize)]
pub struct SeenBody {
    bot_id: Option<i64>,
}

pub async fn mark_seen(state: web::Data<State>, body: Option<web::Json<SeenBody>>) -> HttpResponse {
    let bot = body.and_then(|b| b.bot_id);
    match state.db.execute("UPDATE copy_bot_events SET seen=true WHERE NOT seen AND ($1::bigint IS NULL OR bot_id=$1)", &[&bot]).await {
        Ok(n) => HttpResponse::Ok().json(json!({"marked": n})),
        Err(e) => internal(e),
    }
}

pub async fn run_http(state: web::Data<State>) -> HttpResponse {
    match run_cycle(&state).await {
        Ok(v) => HttpResponse::Ok().json(v),
        Err(e) => internal(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(usdc: f64) -> BuyCtx {
        BuyCtx { trader_usdc: usdc, trader_price: 0.50, fill_price: Some(0.50), trader_portfolio: None, spent_total: 0.0, spent_market: 0.0 }
    }

    #[test]
    fn percentage_mode_scales_leader_trade() {
        let c = BotConfig { buy_pct: 10.0, ..Default::default() };
        assert_eq!(decide_buy(&c, &ctx(500.0)).unwrap(), 50.0);
    }

    #[test]
    fn range_mode_clamps_to_min_and_max() {
        let c = BotConfig { buy_mode: "range".into(), buy_pct: 10.0, range_min: Some(5.0), range_max: Some(20.0), ..Default::default() };
        assert_eq!(decide_buy(&c, &ctx(20.0)).unwrap(), 5.0);
        assert_eq!(decide_buy(&c, &ctx(1000.0)).unwrap(), 20.0);
    }

    #[test]
    fn portfolio_mode_mirrors_allocation() {
        let c = BotConfig { buy_mode: "portfolio".into(), bankroll: 1000.0, ..Default::default() };
        let mut x = ctx(5000.0);
        x.trader_portfolio = Some(500_000.0); // 1% of theirs -> 1% of ours
        assert!((decide_buy(&c, &x).unwrap() - 10.0).abs() < 1e-9);
        x.trader_portfolio = None;
        assert!(decide_buy(&c, &x).is_err());
    }

    #[test]
    fn budget_and_caps_spend_down() {
        let c = BotConfig { buy_mode: "fixed".into(), buy_fixed: 25.0, budget: Some(60.0), max_per_market: Some(30.0), ..Default::default() };
        let mut x = ctx(100.0);
        x.spent_total = 40.0;
        assert_eq!(decide_buy(&c, &x).unwrap(), 20.0); // budget room
        x.spent_total = 0.0;
        x.spent_market = 20.0;
        assert_eq!(decide_buy(&c, &x).unwrap(), 10.0); // market room
        x.spent_market = 30.0;
        assert!(decide_buy(&c, &x).unwrap_err().contains("exhausted"));
    }

    #[test]
    fn filters_reject_with_reasons() {
        let c = BotConfig { min_trigger: Some(100.0), ..Default::default() };
        assert!(decide_buy(&c, &ctx(50.0)).unwrap_err().contains("min trigger"));
        let c = BotConfig { max_price: Some(0.40), ..Default::default() };
        assert!(decide_buy(&c, &ctx(500.0)).unwrap_err().contains("max entry"));
        let c = BotConfig { max_slippage: Some(0.02), ..Default::default() };
        let mut x = ctx(500.0);
        x.fill_price = Some(0.55);
        assert!(decide_buy(&c, &x).unwrap_err().contains("slippage"));
        assert!(decide_buy(&BotConfig::default(), &ctx(5.0)).unwrap_err().contains("minimum"));
    }

    #[test]
    fn categories_include_exclude() {
        let inc = BotConfig { category_mode: "include".into(), categories: vec!["SPORTS".into()], ..Default::default() };
        assert!(category_passes(&inc, "SPORTS"));
        assert!(!category_passes(&inc, "CRYPTO"));
        let exc = BotConfig { category_mode: "exclude".into(), categories: vec!["crypto".into()], ..Default::default() };
        assert!(!category_passes(&exc, "CRYPTO"));
        assert!(category_passes(&exc, "OVERALL"));
    }

    #[test]
    fn sells_follow_the_mode() {
        let same = BotConfig::default();
        assert_eq!(decide_sell(&same, 40.0, 50.0, 100.0, 0.6).unwrap(), 20.0); // leader sold half
        assert_eq!(decide_sell(&same, 40.0, 50.0, 0.0, 0.6).unwrap(), 40.0); // unknown basis: full exit
        let custom = BotConfig { sell_mode: "custom".into(), sell_pct: 25.0, ..Default::default() };
        assert_eq!(decide_sell(&custom, 40.0, 1.0, 100.0, 0.6).unwrap(), 10.0);
        let fixed = BotConfig { sell_mode: "fixed".into(), sell_fixed: 6.0, ..Default::default() };
        assert!((decide_sell(&fixed, 40.0, 1.0, 100.0, 0.6).unwrap() - 10.0).abs() < 1e-9);
        let manual = BotConfig { sell_mode: "manual".into(), ..Default::default() };
        assert!(decide_sell(&manual, 40.0, 1.0, 100.0, 0.6).is_err());
        assert!(decide_sell(&same, 0.0, 1.0, 100.0, 0.6).is_err());
    }

    fn ev(side: &str, shares: f64, usdc: f64) -> Ev {
        Ev {
            ts: Utc::now(), asset: "a".into(), condition_id: None, side: side.into(), action: "copied".into(),
            title: None, slug: None, event_slug: None, icon: None, outcome: None, category: None,
            trader_shares: 0.0, copy_shares: shares, copy_usdc: usdc,
        }
    }

    #[test]
    fn positions_use_average_cost() {
        let ps = build_positions(&[ev("BUY", 100.0, 40.0), ev("BUY", 100.0, 60.0), ev("SELL", 100.0, 70.0)]);
        let p = &ps[0];
        assert!((p.shares - 100.0).abs() < 1e-9);
        assert!((p.cost - 50.0).abs() < 1e-9); // avg 0.50
        assert!((p.realized - 20.0).abs() < 1e-9); // sold 100 @ .70 vs .50 basis
        let mut p = p.clone();
        apply_mark(&mut p, Mark::Settled(1.0));
        assert_eq!(p.status, "redeemable");
        assert!((p.pnl.unwrap() - 70.0).abs() < 1e-9); // 100 - 50 + 20
        let s = summarize(&[p]);
        assert_eq!((s.wins, s.losses), (1, 0));
    }

    #[test]
    fn merge_combines_partial_fills_and_drops_old() {
        let rows = vec![
            json!({"type":"TRADE","timestamp":200,"transactionHash":"0x1","asset":"a","side":"BUY","size":10,"usdcSize":5,"price":0.5}),
            json!({"type":"TRADE","timestamp":200,"transactionHash":"0x1","asset":"a","side":"BUY","size":10,"usdcSize":6,"price":0.6}),
            json!({"type":"TRADE","timestamp":100,"transactionHash":"0x0","asset":"a","side":"BUY","size":1,"usdcSize":1,"price":0.5}),
            json!({"type":"REDEEM","timestamp":300,"transactionHash":"0x2","asset":"a","side":"","size":1}),
        ];
        let t = merge_trades(&rows, 150);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].shares, 20.0);
        assert!((t[0].price - 0.55).abs() < 1e-9);
    }

    #[test]
    fn mirror_keeps_only_open_positions_largest_first() {
        let rows = vec![
            json!({"asset": "a", "size": 100.0, "curPrice": 0.5, "currentValue": 50.0, "redeemable": false}),
            json!({"asset": "b", "size": 1000.0, "curPrice": 0.4, "currentValue": 400.0}),
            json!({"asset": "won", "size": 10.0, "curPrice": 1.0, "currentValue": 10.0, "redeemable": true}),
            json!({"asset": "dead", "size": 10.0, "curPrice": 0.0, "currentValue": 0.0}),
            json!({"asset": "dust", "size": 1.0, "curPrice": 0.5, "currentValue": 0.5}),
        ];
        let t = positions_as_trades(&rows, 7, 100);
        assert_eq!(t.iter().map(|t| t.asset.as_str()).collect::<Vec<_>>(), ["b", "a"]);
        assert_eq!((t[0].tx.as_str(), t[0].side.as_str(), t[0].usdc, t[0].ts), ("mirror:7", "BUY", 400.0, 100));
    }

    #[test]
    fn config_validation() {
        assert!(validate(&BotConfig::default()).is_ok());
        assert!(validate(&BotConfig { buy_mode: "yolo".into(), ..Default::default() }).is_err());
        assert!(validate(&BotConfig { category_mode: "include".into(), ..Default::default() }).is_err());
        assert!(validate(&BotConfig { min_price: Some(5.0), ..Default::default() }).is_err());
    }
}
