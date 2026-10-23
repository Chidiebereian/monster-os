//! Polymarket research: the Top Traders leaderboard, wallet profiles and the
//! Discover feeds (whale moves, new-wallet spotlight, momentum movers).
//!
//! Everything comes from Polymarket's public APIs; no key is needed:
//!   data-api  /v1/leaderboard   rank, P&L, volume by category and window
//!                               (`user=` gives one wallet's rank in any category)
//!             /positions /closed-positions /activity /value /traded /trades
//!             /v2/user-pnl      P&L time series (realized_pnl matches the
//!                               leaderboard's all-time P&L to the cent)
//!   gamma     /public-profile   name, avatar, join date
//!
//! Per-wallet stats that need many calls (win rate from closed positions,
//! per-category P&L) are cached in Postgres `pm_wallet_stats` and refreshed in
//! the background, one wallet at a time, so the board itself stays one call.
//!
//! GET /pm/leaderboard?category=&window=day|week|month|all&order=pnl|vol&limit=&offset=&q=
//! GET /pm/wallet/{addr}                       profile + stats (+ categories)
//! GET /pm/wallet/{addr}/positions?status=active|resolved|closed&sort=&dir=&limit=&offset=&q=
//! GET /pm/wallet/{addr}/activity?type=&limit=&offset=
//! GET /pm/wallet/{addr}/pnl?range=1d|1w|1m|1y|ytd|all
//! GET /pm/discover/whales?min_trade=&min_value=&limit=
//! GET /pm/discover/insiders?min_trade=&max_age_days=&limit=
//! GET /pm/discover/movers?category=&window=&limit=

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use actix_web::{web, HttpResponse};
use chrono::{DateTime, Datelike, TimeZone, Utc};
use futures::stream::{self, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{bad_request, internal, State};

/// Polymarket's leaderboard categories, in the order its own menu lists them.
pub const CATEGORIES: [&str; 11] = [
    "OVERALL", "POLITICS", "SPORTS", "CRYPTO", "CULTURE", "MENTIONS", "WEATHER",
    "ECONOMICS", "TECH", "FINANCE", "ESPORTS",
];
/// Closed positions read per wallet for the win/loss record (50 per call).
const CLOSED_CAP: usize = 1000;
/// Active positions counted per wallet (500 per call).
const ACTIVE_CAP: usize = 5000;
const LITE_TTL_MIN: i64 = 60;
const FULL_TTL_MIN: i64 = 360;

// ---------- small pure helpers ----------

pub fn window_param(w: &str) -> Option<&'static str> {
    match w.to_lowercase().as_str() {
        "day" | "1d" => Some("DAY"),
        "week" | "1w" | "7d" => Some("WEEK"),
        "month" | "1m" | "30d" => Some("MONTH"),
        "all" | "" => Some("ALL"),
        _ => None,
    }
}

pub fn category_param(c: &str) -> Option<&'static str> {
    let up = c.to_uppercase();
    let up = if up.is_empty() || up == "ALL" { "OVERALL".to_string() } else { up };
    CATEGORIES.iter().copied().find(|k| *k == up)
}

pub fn is_address(s: &str) -> bool {
    s.len() == 42 && s.starts_with("0x") && s[2..].chars().all(|c| c.is_ascii_hexdigit())
}

/// A number that may arrive as a JSON number or a numeric string.
fn num(v: &Value, k: &str) -> Option<f64> {
    match v.get(k)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}
fn text(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(|x| x.as_str()).filter(|s| !s.is_empty()).map(str::to_string)
}

/// Win/loss record over a wallet's closed positions. A position is a win when
/// its realized P&L is above zero, a loss when below; break-evens are neither.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Record {
    pub wins: i32,
    pub losses: i32,
    pub win_amount: f64,
    pub loss_amount: f64,
    pub biggest_win: f64,
    pub biggest_loss: f64,
}
impl Record {
    pub fn add(&mut self, realized: f64) {
        if realized > 0.0 {
            self.wins += 1;
            self.win_amount += realized;
            self.biggest_win = self.biggest_win.max(realized);
        } else if realized < 0.0 {
            self.losses += 1;
            self.loss_amount += realized;
            self.biggest_loss = self.biggest_loss.min(realized);
        }
    }
    #[allow(dead_code)]
    pub fn win_rate(&self) -> Option<f64> {
        let n = self.wins + self.losses;
        (n > 0).then(|| self.wins as f64 / n as f64)
    }
}

/// Cut a P&L series to a range. `1y` keeps the last 365 days; `ytd` keeps
/// points from 1 January (UTC) of the latest point's year.
pub fn slice_range(points: &[(i64, f64)], range: &str) -> Vec<(i64, f64)> {
    let Some(&(last, _)) = points.last() else { return vec![] };
    let from = match range {
        "1y" => last - 365 * 86_400,
        "ytd" => {
            let y = Utc.timestamp_opt(last, 0).single().map(|d| d.year()).unwrap_or(1970);
            Utc.with_ymd_and_hms(y, 1, 1, 0, 0, 0).single().map(|d| d.timestamp()).unwrap_or(0)
        }
        _ => i64::MIN,
    };
    points.iter().copied().filter(|(t, _)| *t >= from).collect()
}

// ---------- HTTP plumbing ----------

async fn get_json(state: &State, url: &str) -> Result<Value, String> {
    let resp = state.http.get(url).send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    let body: Value = resp.json().await.map_err(|e| format!("{url}: {e}"))?;
    if !status.is_success() {
        return Err(format!("{url}: HTTP {status}: {body}"));
    }
    Ok(body)
}

fn lb_url(state: &State, category: &str, window: &str, order: &str, limit: usize, offset: usize) -> String {
    format!(
        "{}/v1/leaderboard?category={category}&timePeriod={window}&orderBy={order}&limit={limit}&offset={offset}",
        state.pm_url
    )
}

/// One wallet's standing on one leaderboard (rank, pnl, vol), if it has one.
async fn standing(state: &State, wallet: &str, category: &str, window: &str) -> Option<(i32, f64, f64)> {
    let url = format!(
        "{}/v1/leaderboard?category={category}&timePeriod={window}&orderBy=PNL&user={wallet}",
        state.pm_url
    );
    let v = get_json(state, &url).await.ok()?;
    let r = v.as_array()?.first()?.clone();
    Some((num(&r, "rank")? as i32, num(&r, "pnl").unwrap_or(0.0), num(&r, "vol").unwrap_or(0.0)))
}

// ---------- wallet stats cache ----------

const STATS_COLS: &str = "wallet, name, pseudonym, profile_image, x_username, verified, joined_at, \
    positions_value, markets_traded, all_pnl, all_vol, all_rank, lite_at, active_positions, \
    closed_counted, closed_capped, wins, losses, win_amount, loss_amount, biggest_win, \
    biggest_loss, categories, full_at";

fn stats_json(r: &tokio_postgres::Row) -> Value {
    let wins: Option<i32> = r.get("wins");
    let losses: Option<i32> = r.get("losses");
    let win_rate = match (wins, losses) {
        (Some(w), Some(l)) if w + l > 0 => Some(w as f64 / (w + l) as f64),
        _ => None,
    };
    let joined: Option<DateTime<Utc>> = r.get("joined_at");
    json!({
        "wallet": r.get::<_, String>("wallet"),
        "name": r.get::<_, Option<String>>("name"),
        "pseudonym": r.get::<_, Option<String>>("pseudonym"),
        "profile_image": r.get::<_, Option<String>>("profile_image"),
        "x_username": r.get::<_, Option<String>>("x_username"),
        "verified": r.get::<_, Option<bool>>("verified"),
        "joined_at": joined,
        "age_days": joined.map(|j| (Utc::now() - j).num_days()),
        "positions_value": r.get::<_, Option<f64>>("positions_value"),
        "markets_traded": r.get::<_, Option<i32>>("markets_traded"),
        "all_pnl": r.get::<_, Option<f64>>("all_pnl"),
        "all_vol": r.get::<_, Option<f64>>("all_vol"),
        "all_rank": r.get::<_, Option<i32>>("all_rank"),
        "active_positions": r.get::<_, Option<i32>>("active_positions"),
        "closed_counted": r.get::<_, Option<i32>>("closed_counted"),
        "closed_capped": r.get::<_, Option<bool>>("closed_capped"),
        "wins": wins,
        "losses": losses,
        "win_rate": win_rate,
        "win_amount": r.get::<_, Option<f64>>("win_amount"),
        "loss_amount": r.get::<_, Option<f64>>("loss_amount"),
        "biggest_win": r.get::<_, Option<f64>>("biggest_win"),
        "biggest_loss": r.get::<_, Option<f64>>("biggest_loss"),
        "categories": r.get::<_, Option<Value>>("categories"),
        "lite_at": r.get::<_, Option<DateTime<Utc>>>("lite_at"),
        "full_at": r.get::<_, Option<DateTime<Utc>>>("full_at"),
    })
}

async fn load_stats(state: &State, wallets: &[String]) -> Result<Vec<Value>, String> {
    let rows = state
        .db
        .query(&format!("SELECT {STATS_COLS} FROM pm_wallet_stats WHERE wallet = ANY($1)"), &[&wallets])
        .await
        .map_err(|e| e.to_string())?;
    Ok(rows.iter().map(stats_json).collect())
}

/// Profile, positions value, markets traded and all-time standing: 4 calls.
async fn refresh_lite(state: &State, wallet: &str) -> Result<(), String> {
    let prof_url = format!("{}/public-profile?address={wallet}", state.gamma_url);
    let value_url = format!("{}/value?user={wallet}", state.pm_url);
    let traded_url = format!("{}/traded?user={wallet}", state.pm_url);
    let (prof, value, traded, all) = futures::join!(
        get_json(state, &prof_url),
        get_json(state, &value_url),
        get_json(state, &traded_url),
        standing(state, wallet, "OVERALL", "ALL"),
    );
    let prof = prof.unwrap_or(Value::Null);
    let joined: Option<DateTime<Utc>> = text(&prof, "createdAt").and_then(|s| s.parse().ok());
    let value = value.ok().and_then(|v| v.as_array().and_then(|a| a.first().cloned())).and_then(|r| num(&r, "value"));
    let traded = traded.ok().and_then(|v| num(&v, "traded")).map(|n| n as i32);
    let (rank, pnl, vol) = match all {
        Some((r, p, v)) => (Some(r), Some(p), Some(v)),
        None => (None, None, None),
    };
    let name = text(&prof, "name");
    let pseudonym = text(&prof, "pseudonym");
    let image = text(&prof, "profileImage");
    let x = text(&prof, "xUsername");
    let verified = prof.get("verifiedBadge").and_then(|b| b.as_bool());
    state
        .db
        .execute(
            "INSERT INTO pm_wallet_stats (wallet, name, pseudonym, profile_image, x_username, verified, joined_at,
                 positions_value, markets_traded, all_pnl, all_vol, all_rank, lite_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12, now())
             ON CONFLICT (wallet) DO UPDATE SET
                 name = COALESCE(EXCLUDED.name, pm_wallet_stats.name),
                 pseudonym = COALESCE(EXCLUDED.pseudonym, pm_wallet_stats.pseudonym),
                 profile_image = COALESCE(EXCLUDED.profile_image, pm_wallet_stats.profile_image),
                 x_username = COALESCE(EXCLUDED.x_username, pm_wallet_stats.x_username),
                 verified = COALESCE(EXCLUDED.verified, pm_wallet_stats.verified),
                 joined_at = COALESCE(EXCLUDED.joined_at, pm_wallet_stats.joined_at),
                 positions_value = EXCLUDED.positions_value,
                 markets_traded = EXCLUDED.markets_traded,
                 all_pnl = EXCLUDED.all_pnl, all_vol = EXCLUDED.all_vol, all_rank = EXCLUDED.all_rank,
                 lite_at = now()",
            &[&wallet, &name, &pseudonym, &image, &x, &verified, &joined, &value, &traded, &pnl, &vol, &rank],
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Win/loss record (up to CLOSED_CAP most recent closed positions), active
/// position count, and P&L/volume/rank in every category.
async fn refresh_full(state: &State, wallet: &str) -> Result<(), String> {
    refresh_lite(state, wallet).await?;

    let mut rec = Record::default();
    let mut counted = 0usize;
    let mut capped = false;
    while counted < CLOSED_CAP {
        let url = format!(
            "{}/closed-positions?user={wallet}&limit=50&offset={counted}&sortBy=TIMESTAMP&sortDirection=DESC",
            state.pm_url
        );
        let page = get_json(state, &url).await?;
        let page = page.as_array().cloned().unwrap_or_default();
        for p in &page {
            rec.add(num(p, "realizedPnl").unwrap_or(0.0));
        }
        counted += page.len();
        if page.len() < 50 {
            break;
        }
        if counted >= CLOSED_CAP {
            capped = true;
        }
    }

    // Lost bets are rarely redeemed (redeeming pays nothing), so Polymarket
    // keeps them under /positions at 0c with `redeemable` set, never in
    // /closed-positions. Counting only closed positions would skip most losses
    // and inflate the win rate (one top wallet: 89% closed-only vs 43% real).
    // So resolved-but-unredeemed positions join the record at their cash P&L.
    let (mut active, mut scanned) = (0usize, 0usize);
    while scanned < ACTIVE_CAP {
        let url = format!("{}/positions?user={wallet}&limit=500&offset={scanned}&sizeThreshold=0.01", state.pm_url);
        let page = get_json(state, &url).await?.as_array().cloned().unwrap_or_default();
        for p in &page {
            if p.get("redeemable").and_then(|b| b.as_bool()).unwrap_or(false) {
                rec.add(num(p, "cashPnl").unwrap_or(0.0));
                counted += 1;
            } else {
                active += 1;
            }
        }
        scanned += page.len();
        if page.len() < 500 {
            break;
        }
    }

    let cats: Vec<Value> = stream::iter(CATEGORIES.iter().skip(1).copied())
        .map(|c| async move {
            standing(state, wallet, c, "ALL")
                .await
                .map(|(rank, pnl, vol)| json!({"category": c, "rank": rank, "pnl": pnl, "vol": vol}))
        })
        .buffer_unordered(5)
        .filter_map(|x| async move { x })
        .collect()
        .await;
    let mut cats = cats;
    cats.sort_by(|a, b| num(b, "pnl").partial_cmp(&num(a, "pnl")).unwrap_or(std::cmp::Ordering::Equal));

    let (counted, active) = (counted as i32, active as i32);
    let cats = Value::Array(cats);
    state
        .db
        .execute(
            "UPDATE pm_wallet_stats SET active_positions=$2, closed_counted=$3, closed_capped=$4, wins=$5,
                 losses=$6, win_amount=$7, loss_amount=$8, biggest_win=$9, biggest_loss=$10,
                 categories=$11, full_at=now()
             WHERE wallet=$1",
            &[
                &wallet, &active, &counted, &capped, &rec.wins, &rec.losses, &rec.win_amount,
                &rec.loss_amount, &rec.biggest_win, &rec.biggest_loss, &cats,
            ],
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn inflight() -> &'static Mutex<HashSet<String>> {
    static S: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Refresh these wallets in the background, one at a time, skipping any
/// already queued. `full` also reads closed positions and categories.
fn spawn_refresh(state: web::Data<State>, wallets: Vec<String>, full: bool) {
    let wallets: Vec<String> = {
        let mut set = inflight().lock().unwrap();
        wallets.into_iter().filter(|w| set.insert(w.clone())).collect()
    };
    if wallets.is_empty() {
        return;
    }
    actix_web::rt::spawn(async move {
        for w in wallets {
            let r = if full { refresh_full(&state, &w).await } else { refresh_lite(&state, &w).await };
            if let Err(e) = r {
                eprintln!("whale-tracker: pm refresh {w}: {e}");
            }
            inflight().lock().unwrap().remove(&w);
        }
    });
}

/// Wallets among `wallets` whose cached stats are missing or older than the TTL.
async fn stale(state: &State, wallets: &[String], full: bool) -> Vec<String> {
    let (col, ttl) = if full { ("full_at", FULL_TTL_MIN) } else { ("lite_at", LITE_TTL_MIN) };
    let fresh: HashSet<String> = state
        .db
        .query(
            &format!(
                "SELECT wallet FROM pm_wallet_stats WHERE wallet = ANY($1)
                   AND {col} > now() - make_interval(mins => $2::int)"
            ),
            &[&wallets, &(ttl as i32)],
        )
        .await
        .map(|rows| rows.iter().map(|r| r.get::<_, String>(0)).collect())
        .unwrap_or_default();
    wallets.iter().filter(|w| !fresh.contains(*w)).cloned().collect()
}

// ---------- leaderboard ----------

#[derive(Deserialize)]
pub struct LbQ {
    category: Option<String>,
    window: Option<String>,
    order: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
    q: Option<String>,
}

async fn fetch_board(
    state: &State,
    category: &str,
    window: &str,
    order: &str,
    limit: usize,
    offset: usize,
    q: Option<&str>,
) -> Result<Vec<Value>, String> {
    if let Some(q) = q.filter(|q| !q.is_empty()) {
        let key = if is_address(q) { "user" } else { "userName" };
        let url = format!(
            "{}/v1/leaderboard?category={category}&timePeriod={window}&orderBy={order}&{key}={}",
            state.pm_url,
            urlencode(q)
        );
        return Ok(get_json(state, &url).await?.as_array().cloned().unwrap_or_default());
    }
    let mut out = Vec::new();
    let mut off = offset;
    while out.len() < limit {
        let n = (limit - out.len()).min(50);
        let page = get_json(state, &lb_url(state, category, window, order, n, off)).await?;
        let page = page.as_array().cloned().unwrap_or_default();
        let got = page.len();
        out.extend(page);
        off += got;
        if got < n {
            break;
        }
    }
    Ok(out)
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

pub async fn leaderboard(state: web::Data<State>, q: web::Query<LbQ>) -> HttpResponse {
    let Some(category) = category_param(q.category.as_deref().unwrap_or("")) else {
        return bad_request(format!("category must be one of {}", CATEGORIES.join(", ")));
    };
    let Some(window) = window_param(q.window.as_deref().unwrap_or("all")) else {
        return bad_request("window must be day, week, month or all");
    };
    let order = match q.order.as_deref().unwrap_or("pnl").to_lowercase().as_str() {
        "pnl" => "PNL",
        "vol" | "volume" => "VOL",
        _ => return bad_request("order must be pnl or vol"),
    };
    let limit = q.limit.unwrap_or(25).clamp(1, 100);
    let offset = q.offset.unwrap_or(0);
    let rows = match fetch_board(&state, category, window, order, limit, offset, q.q.as_deref()).await {
        Ok(r) => r,
        Err(e) => return internal(e),
    };

    let wallets: Vec<String> = rows.iter().filter_map(|r| text(r, "proxyWallet")).collect();
    // Record today's ranks; compare with the most recent earlier day for movement.
    for r in &rows {
        if let (Some(w), Some(rank)) = (text(r, "proxyWallet"), num(r, "rank")) {
            let _ = state
                .db
                .execute(
                    "INSERT INTO pm_rank_history (category, time_window, order_by, as_of, wallet, rank, pnl, vol)
                     VALUES ($1,$2,$3,current_date,$4,$5,$6,$7)
                     ON CONFLICT (category, time_window, order_by, as_of, wallet)
                     DO UPDATE SET rank=EXCLUDED.rank, pnl=EXCLUDED.pnl, vol=EXCLUDED.vol",
                    &[&category, &window, &order, &w, &(rank as i32), &num(r, "pnl"), &num(r, "vol")],
                )
                .await;
        }
    }
    let prev: std::collections::HashMap<String, i32> = state
        .db
        .query(
            "SELECT DISTINCT ON (wallet) wallet, rank FROM pm_rank_history
              WHERE category=$1 AND time_window=$2 AND order_by=$3 AND as_of < current_date
                AND wallet = ANY($4)
              ORDER BY wallet, as_of DESC",
            &[&category, &window, &order, &wallets],
        )
        .await
        .map(|rows| rows.iter().map(|r| (r.get(0), r.get(1))).collect())
        .unwrap_or_default();

    let stats = load_stats(&state, &wallets).await.unwrap_or_default();
    let by_wallet: std::collections::HashMap<String, Value> =
        stats.into_iter().map(|s| (s["wallet"].as_str().unwrap_or("").to_string(), s)).collect();

    let need = stale(&state, &wallets, true).await;
    let refreshing = need.len();
    spawn_refresh(state.clone(), need, true);

    let out: Vec<Value> = rows
        .iter()
        .map(|r| {
            let w = text(r, "proxyWallet").unwrap_or_default();
            let rank = num(r, "rank").map(|x| x as i32);
            let change = match (prev.get(&w), rank) {
                (Some(p), Some(n)) => Some(p - n),
                _ => None,
            };
            json!({
                "rank": rank,
                "rank_change": change,
                "wallet": w,
                "name": text(r, "userName"),
                "profile_image": text(r, "profileImage"),
                "x_username": text(r, "xUsername"),
                "verified": r.get("verifiedBadge").and_then(|b| b.as_bool()).unwrap_or(false),
                "pnl": num(r, "pnl"),
                "vol": num(r, "vol"),
                "stats": by_wallet.get(&w).cloned(),
            })
        })
        .collect();
    HttpResponse::Ok().json(json!({
        "category": category, "window": window, "order": order,
        "rows": out, "refreshing": refreshing,
    }))
}

// ---------- wallet profile ----------

fn wallet_arg(path: &web::Path<String>) -> Result<String, HttpResponse> {
    let w = path.to_lowercase();
    if is_address(&w) { Ok(w) } else { Err(bad_request("wallet must be a 0x address")) }
}

#[derive(Deserialize)]
pub struct WalletQ {
    refresh: Option<bool>,
}

pub async fn wallet(state: web::Data<State>, path: web::Path<String>, q: web::Query<WalletQ>) -> HttpResponse {
    let w = match wallet_arg(&path) {
        Ok(w) => w,
        Err(r) => return r,
    };
    let one = vec![w.clone()];
    // A profile view refreshes stats inline when they're missing or stale, so
    // the page never shows another wallet's numbers or nothing at all.
    if q.refresh.unwrap_or(false) || !stale(&state, &one, true).await.is_empty() {
        let first = inflight().lock().unwrap().insert(w.clone());
        if first {
            let r = refresh_full(&state, &w).await;
            inflight().lock().unwrap().remove(&w);
            if let Err(e) = r {
                return internal(e);
            }
        }
    }
    match load_stats(&state, &one).await {
        Ok(mut s) if !s.is_empty() => HttpResponse::Ok().json(s.remove(0)),
        Ok(_) => HttpResponse::Accepted().json(json!({"wallet": w, "status": "refreshing"})),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
pub struct PosQ {
    status: Option<String>,
    sort: Option<String>,
    dir: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
    q: Option<String>,
}

pub async fn positions(state: web::Data<State>, path: web::Path<String>, q: web::Query<PosQ>) -> HttpResponse {
    let w = match wallet_arg(&path) {
        Ok(w) => w,
        Err(r) => return r,
    };
    let closed = q.status.as_deref() == Some("closed");
    let limit = q.limit.unwrap_or(50).clamp(1, if closed { 50 } else { 500 });
    let offset = q.offset.unwrap_or(0);
    let dir = if q.dir.as_deref() == Some("asc") { "ASC" } else { "DESC" };
    let sort = match (closed, q.sort.as_deref().unwrap_or("")) {
        (false, "pnl") => "CASHPNL",
        (false, "pnl_pct") => "PERCENTPNL",
        (false, "price") => "PRICE",
        (false, "avg") => "AVGPRICE",
        (false, "title") => "TITLE",
        (false, _) => "CURRENT",
        (true, "pnl") => "REALIZEDPNL",
        (true, "title") => "TITLE",
        (true, "price") => "PRICE",
        (true, "avg") => "AVGPRICE",
        (true, _) => "TIMESTAMP",
    };
    let path = if closed { "closed-positions" } else { "positions" };
    let mut url = format!(
        "{}/{path}?user={w}&limit={limit}&offset={offset}&sortBy={sort}&sortDirection={dir}",
        state.pm_url
    );
    if !closed {
        // "active" = still trading; "resolved" = settled but not yet redeemed
        // (mostly lost bets, which pay nothing to redeem).
        let resolved = q.status.as_deref() == Some("resolved");
        url.push_str(&format!("&sizeThreshold=0.01&redeemable={resolved}"));
    }
    if let Some(t) = q.q.as_deref().filter(|t| !t.is_empty()) {
        url.push_str(&format!("&title={}", urlencode(t)));
    }
    let rows = match get_json(&state, &url).await {
        Ok(v) => v.as_array().cloned().unwrap_or_default(),
        Err(e) => return internal(e),
    };
    let out: Vec<Value> = rows
        .iter()
        .map(|p| {
            json!({
                "title": text(p, "title"),
                "slug": text(p, "slug"),
                "event_slug": text(p, "eventSlug"),
                "icon": text(p, "icon"),
                "outcome": text(p, "outcome"),
                "condition_id": text(p, "conditionId"),
                "asset": text(p, "asset"),
                "size": num(p, "size"),
                "avg_price": num(p, "avgPrice"),
                "cur_price": num(p, "curPrice"),
                "initial_value": num(p, "initialValue"),
                "current_value": num(p, "currentValue"),
                "cash_pnl": num(p, "cashPnl"),
                "percent_pnl": num(p, "percentPnl"),
                "total_bought": num(p, "totalBought"),
                "realized_pnl": num(p, "realizedPnl"),
                "redeemable": p.get("redeemable").and_then(|b| b.as_bool()),
                "end_date": text(p, "endDate"),
                "closed_at": num(p, "timestamp"),
            })
        })
        .collect();
    HttpResponse::Ok().json(json!({"wallet": w, "status": q.status.as_deref().unwrap_or("active"), "rows": out}))
}

#[derive(Deserialize)]
pub struct ActQ {
    #[serde(rename = "type")]
    kind: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
}

pub async fn activity(state: web::Data<State>, path: web::Path<String>, q: web::Query<ActQ>) -> HttpResponse {
    let w = match wallet_arg(&path) {
        Ok(w) => w,
        Err(r) => return r,
    };
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    let mut url = format!("{}/activity?user={w}&limit={limit}&offset={}", state.pm_url, q.offset.unwrap_or(0));
    if let Some(t) = q.kind.as_deref().filter(|t| !t.is_empty()) {
        url.push_str(&format!("&type={}", t.to_uppercase()));
    }
    let rows = match get_json(&state, &url).await {
        Ok(v) => v.as_array().cloned().unwrap_or_default(),
        Err(e) => return internal(e),
    };
    let out: Vec<Value> = rows
        .iter()
        .map(|a| {
            json!({
                "ts": num(a, "timestamp"),
                "type": text(a, "type"),
                "side": text(a, "side"),
                "title": text(a, "title"),
                "slug": text(a, "slug"),
                "icon": text(a, "icon"),
                "outcome": text(a, "outcome"),
                "price": num(a, "price"),
                "size": num(a, "size"),
                "usdc": num(a, "usdcSize"),
                "tx": text(a, "transactionHash"),
            })
        })
        .collect();
    HttpResponse::Ok().json(json!({"wallet": w, "rows": out}))
}

#[derive(Deserialize)]
pub struct PnlQ {
    range: Option<String>,
}

pub async fn pnl(state: web::Data<State>, path: web::Path<String>, q: web::Query<PnlQ>) -> HttpResponse {
    let w = match wallet_arg(&path) {
        Ok(w) => w,
        Err(r) => return r,
    };
    let range = q.range.as_deref().unwrap_or("all").to_lowercase();
    let (interval, fidelity) = match range.as_str() {
        "1d" => ("1d", "1h"),
        "1w" | "7d" => ("1w", "1h"),
        "1m" | "30d" => ("1m", "1d"),
        "1y" | "ytd" | "all" => ("all", "1d"),
        _ => return bad_request("range must be 1d, 1w, 1m, 1y, ytd or all"),
    };
    let url = format!("{}/v2/user-pnl?user={w}&interval={interval}&fidelity={fidelity}", state.pm_url);
    let v = match get_json(&state, &url).await {
        Ok(v) => v,
        Err(e) => return internal(e),
    };
    let pts: Vec<(i64, f64)> = v["data"]["points"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|p| Some((num(p, "timestamp")? as i64, num(p, "realized_pnl")?)))
                .collect()
        })
        .unwrap_or_default();
    let pts = slice_range(&pts, &range);
    let latest = v["data"]["points"].as_array().and_then(|a| a.last().cloned()).unwrap_or(Value::Null);
    let change = match (pts.first(), pts.last()) {
        (Some(a), Some(b)) => Some(b.1 - a.1),
        _ => None,
    };
    HttpResponse::Ok().json(json!({
        "wallet": w,
        "range": range,
        "points": pts.iter().map(|(t, p)| json!({"t": t, "pnl": p})).collect::<Vec<_>>(),
        "change": change,
        "latest": {
            "realized_pnl": num(&latest, "realized_pnl"),
            "unrealized_pnl": num(&latest, "unrealized_pnl"),
            "volume": num(&latest, "volume"),
            "trade_count": num(&latest, "trade_count"),
            "fees_paid": num(&latest, "fees_paid"),
        },
    }))
}

// ---------- discover ----------

#[derive(Deserialize)]
pub struct WhaleQ {
    min_trade: Option<f64>,
    min_value: Option<f64>,
    max_age_days: Option<i64>,
    limit: Option<usize>,
}

/// Recent large trades, newest first, grouped to each wallet's latest one, with
/// the wallet's cached lite stats. Wallets with missing stats are fetched now
/// (lite only: 4 calls each) so filters on value and age are real.
async fn big_trades(state: &web::Data<State>, min_trade: f64) -> Result<(Vec<Value>, Vec<Value>), String> {
    let url = format!(
        "{}/trades?limit=500&takerOnly=true&filterType=CASH&filterAmount={min_trade}",
        state.pm_url
    );
    let trades = get_json(state, &url).await?.as_array().cloned().unwrap_or_default();
    let mut seen = HashSet::new();
    let latest: Vec<Value> = trades
        .into_iter()
        .filter(|t| text(t, "proxyWallet").map(|w| seen.insert(w)).unwrap_or(false))
        .collect();
    let wallets: Vec<String> = latest.iter().filter_map(|t| text(t, "proxyWallet")).collect();
    let need = stale(state, &wallets, false).await;
    stream::iter(need)
        .map(|w| async move {
            if let Err(e) = refresh_lite(state, &w).await {
                eprintln!("whale-tracker: pm lite {w}: {e}");
            }
        })
        .buffer_unordered(6)
        .collect::<Vec<_>>()
        .await;
    Ok((latest, load_stats(state, &wallets).await?))
}

fn trade_card(t: &Value, s: Option<&Value>) -> Value {
    let price = num(t, "price");
    let size = num(t, "size");
    json!({
        "wallet": text(t, "proxyWallet"),
        "name": text(t, "name"),
        "ts": num(t, "timestamp"),
        "side": text(t, "side"),
        "title": text(t, "title"),
        "slug": text(t, "slug"),
        "event_slug": text(t, "eventSlug"),
        "icon": text(t, "icon"),
        "outcome": text(t, "outcome"),
        "price": price,
        "size": size,
        "usdc": match (price, size) { (Some(p), Some(s)) => Some(p * s), _ => None },
        "stats": s,
    })
}

fn by_wallet(stats: Vec<Value>) -> std::collections::HashMap<String, Value> {
    stats.into_iter().map(|s| (s["wallet"].as_str().unwrap_or("").to_string(), s)).collect()
}

pub async fn whales(state: web::Data<State>, q: web::Query<WhaleQ>) -> HttpResponse {
    let min_trade = q.min_trade.unwrap_or(10_000.0);
    let min_value = q.min_value.unwrap_or(100_000.0);
    let (trades, stats) = match big_trades(&state, min_trade).await {
        Ok(x) => x,
        Err(e) => return internal(e),
    };
    let stats = by_wallet(stats);
    let rows: Vec<Value> = trades
        .iter()
        .filter_map(|t| {
            let s = stats.get(&text(t, "proxyWallet")?)?;
            (s["positions_value"].as_f64().unwrap_or(0.0) >= min_value).then(|| trade_card(t, Some(s)))
        })
        .take(q.limit.unwrap_or(20))
        .collect();
    HttpResponse::Ok().json(json!({"min_trade": min_trade, "min_value": min_value, "rows": rows}))
}

pub async fn insiders(state: web::Data<State>, q: web::Query<WhaleQ>) -> HttpResponse {
    let min_trade = q.min_trade.unwrap_or(10_000.0);
    let max_age = q.max_age_days.unwrap_or(7);
    let (trades, stats) = match big_trades(&state, min_trade).await {
        Ok(x) => x,
        Err(e) => return internal(e),
    };
    let stats = by_wallet(stats);
    let rows: Vec<Value> = trades
        .iter()
        .filter_map(|t| {
            let s = stats.get(&text(t, "proxyWallet")?)?;
            let age = s["age_days"].as_i64()?;
            (age <= max_age).then(|| trade_card(t, Some(s)))
        })
        .take(q.limit.unwrap_or(20))
        .collect();
    HttpResponse::Ok().json(json!({"min_trade": min_trade, "max_age_days": max_age, "rows": rows}))
}

#[derive(Deserialize)]
pub struct MoversQ {
    category: Option<String>,
    window: Option<String>,
    limit: Option<usize>,
    /// Keep only wallets that are up all-time in the category (default true).
    proven: Option<bool>,
}

/// Traders climbing fastest: top of the window's P&L board but ranked far
/// lower all-time in the same category. `climb` = all-time rank − window rank.
/// A hot week often belongs to an all-time loser (one sports wallet: #26 this
/// week, -$2.2M all-time), so by default only wallets up all-time are kept.
pub async fn movers(state: web::Data<State>, q: web::Query<MoversQ>) -> HttpResponse {
    let Some(category) = category_param(q.category.as_deref().unwrap_or("")) else {
        return bad_request("unknown category");
    };
    let Some(window) = window_param(q.window.as_deref().unwrap_or("week")) else {
        return bad_request("window must be day, week or month");
    };
    if window == "ALL" {
        return bad_request("movers compare a recent window with all-time; pick day, week or month");
    }
    let board = match fetch_board(&state, category, window, "PNL", 50, 0, None).await {
        Ok(b) => b,
        Err(e) => return internal(e),
    };
    let st = &state;
    let mut rows: Vec<Value> = stream::iter(board)
        .map(|r| async move {
            let w = text(&r, "proxyWallet")?;
            let now_rank = num(&r, "rank")? as i32;
            let all = standing(st, &w, category, "ALL").await;
            let all_rank = all.map(|a| a.0);
            let all_pnl = all.map(|a| a.1);
            Some(json!({
                "wallet": w,
                "name": text(&r, "userName"),
                "profile_image": text(&r, "profileImage"),
                "rank": now_rank,
                "all_rank": all_rank,
                "climb": all_rank.map(|a| a - now_rank),
                "all_pnl": all_pnl,
                "pnl": num(&r, "pnl"),
                "vol": num(&r, "vol"),
            }))
        })
        .buffer_unordered(8)
        .filter_map(|x| async move { x })
        .collect()
        .await;
    if q.proven.unwrap_or(true) {
        rows.retain(|r| r["all_pnl"].as_f64().unwrap_or(0.0) > 0.0);
    }
    rows.sort_by_key(|r| -r["climb"].as_i64().unwrap_or(i64::MIN / 2));
    rows.truncate(q.limit.unwrap_or(12));
    let wallets: Vec<String> = rows.iter().filter_map(|r| r["wallet"].as_str().map(String::from)).collect();
    let stats = by_wallet(load_stats(&state, &wallets).await.unwrap_or_default());
    for r in rows.iter_mut() {
        let w = r["wallet"].as_str().unwrap_or("").to_string();
        r["stats"] = stats.get(&w).cloned().unwrap_or(Value::Null);
    }
    let need = stale(&state, &wallets, true).await;
    spawn_refresh(state.clone(), need, true);
    HttpResponse::Ok().json(json!({"category": category, "window": window, "rows": rows}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_map_to_polymarket_values() {
        assert_eq!(window_param("week"), Some("WEEK"));
        assert_eq!(window_param("30d"), Some("MONTH"));
        assert_eq!(window_param("1y"), None);
        assert_eq!(category_param(""), Some("OVERALL"));
        assert_eq!(category_param("all"), Some("OVERALL"));
        assert_eq!(category_param("sports"), Some("SPORTS"));
        assert_eq!(category_param("nope"), None);
    }

    #[test]
    fn record_counts_wins_losses_and_ignores_flat() {
        let mut r = Record::default();
        for p in [120.0, -40.0, 0.0, 30.0, -200.0] {
            r.add(p);
        }
        assert_eq!((r.wins, r.losses), (2, 2));
        assert_eq!(r.win_rate(), Some(0.5));
        assert_eq!((r.biggest_win, r.biggest_loss), (120.0, -200.0));
        assert_eq!((r.win_amount, r.loss_amount), (150.0, -240.0));
        assert_eq!(Record::default().win_rate(), None);
    }

    #[test]
    fn ytd_and_1y_slices() {
        let jan1 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap().timestamp();
        let pts = vec![(jan1 - 86_400, 1.0), (jan1, 2.0), (jan1 + 200 * 86_400, 3.0), (jan1 + 400 * 86_400, 4.0)];
        // latest point is in 2027, so YTD starts 2027-01-01
        assert_eq!(slice_range(&pts, "ytd").len(), 1);
        assert_eq!(slice_range(&pts, "1y").len(), 2);
        assert_eq!(slice_range(&pts, "all").len(), 4);
    }

    #[test]
    fn address_check() {
        assert!(is_address("0xf0318c32136c2db7fec88b84869aee6a1106c80c"));
        assert!(!is_address("BreakTheBank"));
        assert_eq!(urlencode("a b&c"), "a%20b%26c");
    }
}
