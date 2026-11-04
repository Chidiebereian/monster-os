//! Futures discovery: every outcome of open multi-outcome events (champion, conference,
//! division winner), as `common::VenueMarket`, with each venue's rules text and link.
//! arbitrage-engine matches these across venues (pm_arb.rs).
//!
//! GET /futures/kalshi?series=KXSB,KXNBA        events of those Kalshi series
//! GET /futures/polymarket?tag=nfl[&contains=champion]   Polymarket events tagged with a league
//!                                                (contains: only events whose title has it)
//!
//! Prices: a missing bid (0) or ask (1.00 or more) means no resting order, so `None`.

use actix_web::{web, HttpResponse, Responder};
use chrono::{DateTime, Utc};
use common::VenueMarket;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{num, polymarket_fee, polymarket_time, FetchError, Venues, KALSHI_FEE};

/// Polymarket events read per tag (100 per page).
const POLYMARKET_MAX_EVENTS: usize = 500;
/// Kalshi series per request.
const KALSHI_MAX_SERIES: usize = 40;
/// Pause between Kalshi series calls.
const KALSHI_SPACING_MS: u64 = 250;

fn bid(v: Option<f64>) -> Option<f64> {
    v.filter(|p| *p > 0.0 && *p < 1.0)
}
fn ask(v: Option<f64>) -> Option<f64> {
    v.filter(|p| *p > 0.0 && *p < 1.0)
}

/// "KXSB-27" -> 2027 (Kalshi names a season by the year it ends).
pub fn kalshi_season(event_ticker: &str) -> Option<i32> {
    let tail = event_ticker.rsplit('-').next()?;
    (tail.len() == 2 && tail.chars().all(|c| c.is_ascii_digit())).then(|| 2000 + tail.parse::<i32>().unwrap_or(0))
}

/// The first 4-digit year from 2000 to 2099 in a title.
pub fn title_year(title: &str) -> Option<i32> {
    title
        .split(|c: char| !c.is_ascii_digit())
        .filter(|t| t.len() == 4)
        .filter_map(|t| t.parse::<i32>().ok())
        .find(|y| (2000..2100).contains(y))
}

fn opt_time(v: &Value) -> Option<DateTime<Utc>> {
    v.as_str().and_then(|s| s.parse().ok())
}

/// Every open market of one Kalshi event (`with_nested_markets=true`).
pub fn kalshi_event_markets(ev: &Value, series: &str) -> Vec<VenueMarket> {
    let Some(event_id) = ev["event_ticker"].as_str() else { return vec![] };
    let season = kalshi_season(event_id);
    let title = ev["title"].as_str().unwrap_or("").to_string();
    ev["markets"].as_array().into_iter().flatten().filter_map(|m| {
        let status = m["status"].as_str().unwrap_or("active");
        if !matches!(status, "active" | "open") {
            return None;
        }
        let ticker = m["ticker"].as_str()?;
        let rules = [m["rules_primary"].as_str(), m["rules_secondary"].as_str()]
            .into_iter().flatten().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n\n");
        Some(VenueMarket {
            venue: "kalshi".into(),
            market_id: ticker.to_string(),
            event_id: event_id.to_string(),
            event_title: title.clone(),
            outcome: m["yes_sub_title"].as_str().unwrap_or("").to_string(),
            code: ticker.strip_prefix(&format!("{event_id}-")).map(str::to_string),
            season,
            yes_bid: bid(num(&m["yes_bid_dollars"])),
            yes_ask: ask(num(&m["yes_ask_dollars"])),
            resolves_at: opt_time(&m["expected_expiration_time"]).or_else(|| opt_time(&m["close_time"])),
            rules,
            url: format!("https://kalshi.com/markets/{}", series.to_lowercase()),
            volume: num(&m["volume_fp"]).or_else(|| num(&m["volume"])),
            taker_fee: Some(KALSHI_FEE),
            game_date: None,
            complement: false,
        })
    }).collect()
}

/// Every open outcome of one Polymarket multi-outcome event (markets with a
/// `groupItemTitle`, i.e. one market per team).
pub fn polymarket_event_markets(ev: &Value) -> Vec<VenueMarket> {
    let (Some(slug), Some(title)) = (ev["slug"].as_str(), ev["title"].as_str()) else { return vec![] };
    let season = title_year(title);
    ev["markets"].as_array().into_iter().flatten().filter_map(|m| {
        if m["closed"].as_bool() == Some(true) || m["active"].as_bool() == Some(false) || m["acceptingOrders"].as_bool() == Some(false) {
            return None;
        }
        let outcome = m["groupItemTitle"].as_str().filter(|s| !s.is_empty())?;
        Some(VenueMarket {
            venue: "polymarket".into(),
            market_id: m["slug"].as_str()?.to_string(),
            event_id: slug.to_string(),
            event_title: title.trim().to_string(),
            outcome: outcome.to_string(),
            code: None,
            season,
            yes_bid: bid(num(&m["bestBid"])),
            yes_ask: ask(num(&m["bestAsk"])),
            resolves_at: polymarket_time(&m["endDate"]).or_else(|| polymarket_time(&ev["endDate"])),
            rules: m["description"].as_str().or(ev["description"].as_str()).unwrap_or("").to_string(),
            url: format!("https://polymarket.com/event/{slug}"),
            volume: num(&m["volumeNum"]),
            taker_fee: polymarket_fee(m),
            game_date: None,
            complement: false,
        })
    }).collect()
}

fn valid_series(s: &str) -> bool {
    !s.is_empty() && s.len() <= 40 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

#[derive(Deserialize)]
pub struct SeriesQ {
    series: String,
}

pub async fn kalshi(venues: web::Data<Venues>, q: web::Query<SeriesQ>) -> impl Responder {
    let series: Vec<&str> = q.series.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    if series.is_empty() || series.len() > KALSHI_MAX_SERIES || !series.iter().all(|s| valid_series(s)) {
        return HttpResponse::BadRequest().json(json!({ "error": format!("series: 1 to {KALSHI_MAX_SERIES} comma-separated Kalshi series tickers") }));
    }
    let mut out = Vec::new();
    let mut errors = Vec::new();
    for (i, s) in series.iter().enumerate() {
        let url = format!("{}/events?series_ticker={s}&status=open&with_nested_markets=true&limit=200", venues.kalshi);
        // Kalshi rate-limits bursts (429 after ~17 quick calls): space the calls, retry once
        if i > 0 {
            actix_web::rt::time::sleep(std::time::Duration::from_millis(KALSHI_SPACING_MS)).await;
        }
        let mut got = venues.get_json(&url).await;
        if matches!(&got, Err(FetchError::Upstream(e)) if e.contains("429")) {
            actix_web::rt::time::sleep(std::time::Duration::from_secs(2)).await;
            got = venues.get_json(&url).await;
        }
        match got {
            Ok(body) => {
                for ev in body["events"].as_array().into_iter().flatten() {
                    out.extend(kalshi_event_markets(ev, s));
                }
            }
            Err(FetchError::NotFound) => {}
            Err(FetchError::Upstream(e)) => errors.push(e),
        }
    }
    HttpResponse::Ok().json(json!({ "markets": out, "errors": errors }))
}

#[derive(Deserialize)]
pub struct TagQ {
    tag: String,
    contains: Option<String>,
}

pub async fn polymarket(venues: web::Data<Venues>, q: web::Query<TagQ>) -> impl Responder {
    if !valid_series(&q.tag) {
        return HttpResponse::BadRequest().json(json!({ "error": "tag: a Polymarket tag slug such as nfl" }));
    }
    let mut out = Vec::new();
    let mut offset = 0;
    while offset < POLYMARKET_MAX_EVENTS {
        let url = format!("{}/events?tag_slug={}&active=true&closed=false&limit=100&offset={offset}", venues.polymarket, q.tag);
        let page = match venues.get_json(&url).await {
            Ok(v) => v.as_array().cloned().unwrap_or_default(),
            Err(FetchError::NotFound) => vec![],
            Err(FetchError::Upstream(e)) => return HttpResponse::BadGateway().json(json!({ "error": e })),
        };
        let want = q.contains.as_deref().map(str::to_lowercase);
        for ev in &page {
            let title = ev["title"].as_str().unwrap_or("").to_lowercase();
            if want.as_deref().map(|w| title.contains(w)).unwrap_or(true) {
                out.extend(polymarket_event_markets(ev));
            }
        }
        if page.len() < 100 {
            break;
        }
        offset += 100;
    }
    HttpResponse::Ok().json(json!({ "markets": out, "errors": [] }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seasons_from_tickers_and_titles() {
        assert_eq!(kalshi_season("KXNFLAFCCHAMP-27"), Some(2027));
        assert_eq!(kalshi_season("KXNBA"), None);
        assert_eq!(title_year("Pro Football: 2027 AFC Champion"), Some(2027));
        assert_eq!(title_year("Pro Football: AFC East Champion"), None);
    }

    #[test]
    fn reads_a_kalshi_event() {
        let ev = json!({ "event_ticker": "KXNFLAFCCHAMP-27", "title": "AFC Championship Winner", "markets": [
            { "ticker": "KXNFLAFCCHAMP-27-BAL", "yes_sub_title": "Baltimore", "yes_bid_dollars": "0.0000", "yes_ask_dollars": "0.0200",
              "expected_expiration_time": "2027-02-08T15:00:00Z", "rules_primary": "If Baltimore wins ...", "volume_fp": "555719.46", "status": "active" },
            { "ticker": "KXNFLAFCCHAMP-27-BUF", "yes_sub_title": "Buffalo", "status": "finalized" } ] });
        let m = kalshi_event_markets(&ev, "KXNFLAFCCHAMP");
        assert_eq!(m.len(), 1, "finalized markets are left out");
        assert_eq!((m[0].code.as_deref(), m[0].season, m[0].yes_bid, m[0].yes_ask), (Some("BAL"), Some(2027), None, Some(0.02)));
        assert_eq!(m[0].url, "https://kalshi.com/markets/kxnflafcchamp");
        assert_eq!(m[0].taker_fee, Some(KALSHI_FEE));
    }

    #[test]
    fn reads_a_polymarket_event() {
        let ev = json!({ "slug": "pro-football-2027-afc-champion", "title": "Pro Football: 2027 AFC Champion ", "endDate": "2027-04-01T03:59:00Z",
            "markets": [
                { "slug": "will-buffalo-bills-win", "groupItemTitle": "Buffalo Bills", "bestBid": 0.23, "bestAsk": 0.25, "description": "rules",
                  "volumeNum": 44650.5, "feesEnabled": false, "active": true, "closed": false, "acceptingOrders": true },
                { "slug": "will-x-win", "groupItemTitle": "X", "closed": true },
                { "slug": "no-group", "groupItemTitle": "" } ] });
        let m = polymarket_event_markets(&ev);
        assert_eq!(m.len(), 1);
        assert_eq!((m[0].outcome.as_str(), m[0].season, m[0].yes_bid, m[0].yes_ask), ("Buffalo Bills", Some(2027), Some(0.23), Some(0.25)));
        assert_eq!(m[0].event_title, "Pro Football: 2027 AFC Champion");
        assert_eq!(m[0].url, "https://polymarket.com/event/pro-football-2027-afc-champion");
        assert!(m[0].resolves_at.is_some());
    }
}
