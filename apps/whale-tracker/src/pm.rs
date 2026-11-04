//! Polymarket research: the Top Traders leaderboard, wallet profiles and the
//! Discover feeds (whale moves, new-wallet spotlight, momentum movers).
//!
//! Everything comes from Polymarket's public APIs; no key is needed:
//!   data-api v2  /leaderboard   rank, P&L, volume (in shares) by category and window
//!                               (`user=` gives one wallet's standing in a category)
//!                /positions (status OPEN | REDEEMABLE | REDEEMABLE_LOST | CLOSED)
//!                /activity /trades /value /user-volume (USD) /user-pnl
//!   gamma        /public-profile   name, avatar, join date
//!
//! Polymarket retired Data API v1 (and lb-api) on 2026-11-02: every v1 path answers
//! HTTP 410. v2 wraps payloads in `data`, pages by cursor only (no offset), names
//! fields in snake_case and reports leaderboard volume in shares, not USD. `v2_rows`
//! and `v2_data` hide the envelope and paging; `legacy` maps rows back to the v1
//! field names this service reads. USD volume comes from /user-volume; v1's
//! markets-traded count (/traded) has no v2 equivalent and is no longer filled.
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
pub(crate) fn num(v: &Value, k: &str) -> Option<f64> {
    match v.get(k)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}
pub(crate) fn text(v: &Value, k: &str) -> Option<String> {
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

/// GET a JSON body. Polymarket's Cloudflare throttle answers 429 (or a 5xx)
/// with a non-JSON page, so check the status first and back off and retry
/// those up to 3 times (1 s, 2 s, 4 s, or the Retry-After it sends).
pub(crate) async fn get_json(state: &State, url: &str) -> Result<Value, String> {
    let mut wait = 1u64;
    for attempt in 0..4 {
        let resp = state.http.get(url).send().await.map_err(|e| format!("{url}: {e}"))?;
        let status = resp.status();
        if (status.as_u16() == 429 || status.is_server_error()) && attempt < 3 {
            let after = resp.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
            actix_web::rt::time::sleep(std::time::Duration::from_secs(after.unwrap_or(wait).min(30))).await;
            wait *= 2;
            continue;
        }
        let text = resp.text().await.map_err(|e| format!("{url}: {e}"))?;
        if !status.is_success() {
            return Err(format!("{url}: HTTP {status}: {}", text.chars().take(200).collect::<String>()));
        }
        return serde_json::from_str(&text).map_err(|e| format!("{url}: {e}: {}", text.chars().take(120).collect::<String>()));
    }
    Err(format!("{url}: still throttled after retries"))
}

/// Largest page v2 serves.
const V2_PAGE: usize = 1000;

fn camel(k: &str) -> String {
    let mut out = String::with_capacity(k.len());
    let mut up = false;
    for c in k.chars() {
        if c == '_' { up = true; } else if up { out.extend(c.to_uppercase()); up = false; } else { out.push(c); }
    }
    out
}

/// A Data API v2 row with the v1 field names added next to its own: every snake_case
/// key also under its camelCase name, plus the fields v1 named differently.
pub(crate) fn legacy(row: &Value) -> Value {
    let Some(obj) = row.as_object() else { return row.clone() };
    let mut out = obj.clone();
    for (k, v) in obj {
        if k.contains('_') {
            out.entry(camel(k)).or_insert_with(|| v.clone());
        }
    }
    for (v2, v1) in [
        ("token_id", "asset"), ("user_id", "proxyWallet"), ("verified", "verifiedBadge"),
        ("current_price", "curPrice"), ("current_size", "size"), ("total_size", "totalBought"),
        ("entry_cost_usdc", "initialValue"), ("unrealized_pnl", "cashPnl"), ("last_event_at", "timestamp"),
    ] {
        if let Some(v) = obj.get(v2) {
            out.entry(v1.to_string()).or_insert_with(|| v.clone());
        }
    }
    Value::Object(out)
}

/// Rows `skip..skip + take` of a v2 list endpoint (`path` without `/v2/`), walking its
/// cursor: v2 has no offset. `query` is resent with every page (cursors bind to it).
pub(crate) async fn v2_rows(state: &State, path: &str, query: &str, skip: usize, take: usize) -> Result<Vec<Value>, String> {
    let want = skip + take;
    let mut out: Vec<Value> = Vec::new();
    let mut cursor: Option<String> = None;
    while out.len() < want {
        let url = match &cursor {
            Some(c) => format!("{}/v2/{path}?{query}&cursor={}", state.pm_url, urlencode(c)),
            None => format!("{}/v2/{path}?{query}&limit={}", state.pm_url, want.clamp(1, V2_PAGE)),
        };
        let v = get_json(state, &url).await?;
        let page = v["data"].as_array().cloned().unwrap_or_default();
        let got = page.len();
        out.extend(page);
        cursor = v["pagination"]["next_cursor"].as_str().map(str::to_string);
        if cursor.is_none() || got == 0 {
            break;
        }
    }
    Ok(out.into_iter().skip(skip).take(take).map(|r| legacy(&r)).collect())
}

/// The `data` object of a v2 endpoint that returns one record (Null on a documented miss).
pub(crate) async fn v2_data(state: &State, path: &str, query: &str) -> Result<Value, String> {
    let v = get_json(state, &format!("{}/v2/{path}?{query}", state.pm_url)).await?;
    Ok(legacy(&v["data"]))
}

/// v2 leaderboard spellings of our (v1-style) category and window names.
fn board_args(category: &str, window: &str) -> String {
    format!("category={}&time_period={}", category.to_lowercase(), window.to_lowercase())
}

/// One wallet's all-time standing on a category's P&L board: (rank, pnl, volume in
/// shares), if it has one.
pub(crate) async fn standing(state: &State, wallet: &str, category: &str, window: &str) -> Option<(i32, f64, f64)> {
    let r = v2_data(state, "leaderboard", &format!("{}&sort_by=PNL&user={wallet}", board_args(category, window))).await.ok()?;
    Some((num(&r, "rank_pnl")? as i32, num(&r, "pnl").unwrap_or(0.0), num(&r, "volume").unwrap_or(0.0)))
}

/// A wallet's traded volume in USD since `start` (epoch seconds; 0 = all time).
pub(crate) async fn usd_volume(state: &State, wallet: &str, start: i64) -> Option<f64> {
    let q = if start > 0 { format!("user={wallet}&start={start}") } else { format!("user={wallet}") };
    num(&v2_data(state, "user-volume", &q).await.ok()?, "volume_usdc")
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

/// Profile, positions value, all-time USD volume and all-time standing: 4 calls.
async fn refresh_lite(state: &State, wallet: &str) -> Result<(), String> {
    let prof_url = format!("{}/public-profile?address={wallet}", state.gamma_url);
    let user = format!("user={wallet}");
    let (prof, value, vol, all) = futures::join!(
        get_json(state, &prof_url),
        v2_data(state, "value", &user),
        usd_volume(state, wallet, 0),
        standing(state, wallet, "OVERALL", "ALL"),
    );
    let prof = prof.unwrap_or(Value::Null);
    let joined: Option<DateTime<Utc>> = text(&prof, "createdAt").and_then(|s| s.parse().ok());
    let value = value.ok().and_then(|r| num(&r, "value"));
    // v1's /traded (markets traded) has no v2 equivalent
    let traded: Option<i32> = None;
    let (rank, pnl) = match all {
        Some((r, p, _shares)) => (Some(r), Some(p)),
        None => (None, None),
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
                 markets_traded = COALESCE(EXCLUDED.markets_traded, pm_wallet_stats.markets_traded),
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
    let closed = v2_rows(state, "positions", &format!("user={wallet}&status=CLOSED&sort_by=TIMESTAMP&sort_direction=DESC"), 0, CLOSED_CAP).await?;
    for p in &closed {
        rec.add(num(p, "realizedPnl").unwrap_or(0.0));
    }
    let mut counted = closed.len();
    let capped = counted >= CLOSED_CAP;

    // Lost bets are rarely redeemed (redeeming pays nothing), so they never reach
    // CLOSED. Counting only closed positions would skip most losses and inflate the
    // win rate (one top wallet: 89% closed-only vs 43% real). So resolved-but-
    // unredeemed positions join the record at their cash P&L: winners sit in OPEN
    // with `redeemable` set, losers under REDEEMABLE_LOST.
    let open = v2_rows(state, "positions", &format!("user={wallet}&status=OPEN"), 0, ACTIVE_CAP).await?;
    let lost = v2_rows(state, "positions", &format!("user={wallet}&status=REDEEMABLE_LOST"), 0, ACTIVE_CAP).await?;
    let mut seen = HashSet::new();
    let mut active = 0usize;
    for p in open.iter().chain(lost.iter()) {
        if !seen.insert(text(p, "asset").unwrap_or_default()) {
            continue;
        }
        if p.get("redeemable").and_then(|b| b.as_bool()).unwrap_or(false) || text(p, "status").as_deref() == Some("REDEEMABLE") {
            rec.add(num(p, "cashPnl").unwrap_or(0.0));
            counted += 1;
        } else {
            active += 1;
        }
    }

    let cats: Vec<Value> = stream::iter(CATEGORIES.iter().skip(1).copied())
        .map(|c| async move {
            standing(state, wallet, c, "ALL")
                .await
                .map(|(rank, pnl, shares)| json!({"category": c, "rank": rank, "pnl": pnl, "vol": null, "vol_shares": shares}))
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
                crate::log_err(format!("pm refresh {w}: {e}"));
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
    let args = format!("{}&sort_by={order}", board_args(category, window));
    if let Some(q) = q.filter(|q| !q.is_empty()) {
        if is_address(q) {
            // one wallet's standing: a single record carrying both ranks
            let mut r = v2_data(state, "leaderboard", &format!("{args}&user={}", q.to_lowercase())).await?;
            if r.is_null() {
                return Ok(vec![]);
            }
            r["rank"] = r[if order == "VOLUME" { "rank_volume" } else { "rank_pnl" }].clone();
            return Ok(vec![r]);
        }
        // v2 has no name search: match names within the board's first page
        let q = q.to_lowercase();
        let rows = v2_rows(state, "leaderboard", &args, 0, V2_PAGE).await?;
        return Ok(rows.into_iter().filter(|r| text(r, "userName").map(|n| n.to_lowercase().contains(&q)).unwrap_or(false)).take(limit).collect());
    }
    v2_rows(state, "leaderboard", &args, offset, limit).await
}

pub(crate) fn urlencode(s: &str) -> String {
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
        "vol" | "volume" => "VOLUME",
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
                    &[&category, &window, &order, &w, &(rank as i32), &num(r, "pnl"), &None::<f64>],
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
                // v2 reports board volume in shares only
                "vol": null,
                "vol_shares": num(r, "volume"),
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
    let status = q.status.as_deref().unwrap_or("active");
    let closed = status == "closed";
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    let offset = q.offset.unwrap_or(0);
    let dir = if q.dir.as_deref() == Some("asc") { "ASC" } else { "DESC" };
    // v2 sorts: CURRENT_VALUE, PRICE, TOKENS, UNREALIZED_PNL, REALIZED_PNL, TOTAL_PNL, TIMESTAMP
    let sort = match (closed, q.sort.as_deref().unwrap_or("")) {
        (false, "pnl" | "pnl_pct") => "UNREALIZED_PNL",
        (false, "price") => "PRICE",
        (false, _) => "CURRENT_VALUE",
        (true, "pnl") => "REALIZED_PNL",
        (true, _) => "TIMESTAMP",
    };
    let mut args = format!("user={w}&sort_by={sort}&sort_direction={dir}");
    if let Some(t) = q.q.as_deref().filter(|t| !t.is_empty()) {
        args.push_str(&format!("&title={}", urlencode(t)));
    }
    // "active" = still trading; "resolved" = settled but not yet redeemed (mostly lost
    // bets, which pay nothing to redeem): v2 keeps unredeemed winners inside OPEN and
    // losers under REDEEMABLE_LOST.
    let fetched = match status {
        "closed" => v2_rows(&state, "positions", &format!("{args}&status=CLOSED"), offset, limit).await,
        "resolved" => {
            let (won_q, lost_q) = (format!("{args}&status=REDEEMABLE"), format!("{args}&status=REDEEMABLE_LOST"));
            let (won, lost) = futures::join!(
                v2_rows(&state, "positions", &won_q, 0, offset + limit),
                v2_rows(&state, "positions", &lost_q, 0, offset + limit),
            );
            match (won, lost) {
                (Ok(mut a), Ok(b)) => {
                    a.extend(b);
                    a.sort_by(|x, y| num(y, "currentValue").partial_cmp(&num(x, "currentValue")).unwrap_or(std::cmp::Ordering::Equal));
                    Ok(a.into_iter().skip(offset).take(limit).collect())
                }
                (Err(e), _) | (_, Err(e)) => Err(e),
            }
        }
        _ => v2_rows(&state, "positions", &format!("{args}&status=OPEN"), 0, (offset + limit + 200).min(5000)).await
            .map(|rows| rows.into_iter().filter(|p| p["redeemable"].as_bool() != Some(true)).skip(offset).take(limit).collect()),
    };
    let rows: Vec<Value> = match fetched {
        Ok(r) => r,
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
    HttpResponse::Ok().json(json!({"wallet": w, "status": status, "rows": out}))
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
    let mut args = format!("user={w}");
    if let Some(t) = q.kind.as_deref().filter(|t| !t.is_empty()) {
        args.push_str(&format!("&type={}", urlencode(&t.to_uppercase())));
    }
    let rows = match v2_rows(&state, "activity", &args, q.offset.unwrap_or(0), limit).await {
        Ok(r) => r,
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
    let trades = v2_rows(state, "trades", &format!("taker_only=true&filter_type=CASH&filter_amount={min_trade}"), 0, 500).await?;
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
                crate::log_err(format!("pm lite {w}: {e}"));
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
                "vol": null,
                "vol_shares": num(&r, "volume"),
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
    fn v2_rows_keep_their_fields_and_gain_the_v1_names() {
        let trade = json!({"proxy_wallet": "0xa", "condition_id": "0xc", "token_id": "123", "transaction_hash": "0xt",
                           "usdc_size": 33.6, "outcome_index": 1, "event_slug": "e", "size": 40.0, "timestamp": 5});
        let t = legacy(&trade);
        assert_eq!(t["proxyWallet"], "0xa");
        assert_eq!(t["conditionId"], "0xc");
        assert_eq!(t["asset"], "123");
        assert_eq!(t["transactionHash"], "0xt");
        assert_eq!(t["usdcSize"], 33.6);
        assert_eq!(t["outcomeIndex"], 1);
        assert_eq!(t["eventSlug"], "e");
        assert_eq!((t["size"].as_f64(), t["timestamp"].as_i64()), (Some(40.0), Some(5)));
        assert_eq!(t["proxy_wallet"], "0xa", "v2 names stay");
        // positions: current size and price under v1 names; a trade's own size is never replaced
        let pos = legacy(&json!({"current_size": 24296.5, "current_price": 0.725, "current_value": 17615.0,
                                 "unrealized_pnl": -1927.9, "avg_price": 0.8043, "total_size": 24296.5,
                                 "entry_cost_usdc": 19542.9, "realized_pnl": 0.0, "last_event_at": 9, "end_date": "2026-11-05"}));
        assert_eq!((num(&pos, "size"), num(&pos, "curPrice"), num(&pos, "currentValue")), (Some(24296.5), Some(0.725), Some(17615.0)));
        assert_eq!((num(&pos, "cashPnl"), num(&pos, "avgPrice"), num(&pos, "initialValue")), (Some(-1927.9), Some(0.8043), Some(19542.9)));
        assert_eq!((num(&pos, "totalBought"), num(&pos, "timestamp")), (Some(24296.5), Some(9.0)));
        assert_eq!(text(&pos, "endDate").as_deref(), Some("2026-11-05"));
        let lb = legacy(&json!({"rank": 1, "user_id": "0xw", "user_name": "Theo4", "profile_image": "i", "x_username": "x", "verified": true}));
        assert_eq!((text(&lb, "proxyWallet"), text(&lb, "userName")), (Some("0xw".into()), Some("Theo4".into())));
        assert_eq!((text(&lb, "profileImage"), text(&lb, "xUsername")), (Some("i".into()), Some("x".into())));
        assert_eq!(lb["verifiedBadge"], true);
        assert_eq!(legacy(&Value::Null), Value::Null);
        assert_eq!(board_args("OVERALL", "ALL"), "category=overall&time_period=all");
    }

    #[test]
    fn address_check() {
        assert!(is_address("0xf0318c32136c2db7fec88b84869aee6a1106c80c"));
        assert!(!is_address("BreakTheBank"));
        assert_eq!(urlencode("a b&c"), "a%20b%26c");
    }
}
