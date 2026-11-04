//! Game-winner discovery as `common::VenueMarket`, one entry per team, with each venue's
//! rules text and link, for arbitrage-engine's pm_arb (games).
//!
//! GET /games/kalshi?series=KXNBAGAME,KXNHLGAME   open game events of those series
//! GET /games/polymarket?league=nba               open moneyline markets of a league
//!
//! Kalshi lists one market per team; the game date is in the event ticker
//! ("KXNBAGAME-26NOV04ATLWAS" = Nov 4, 2026). Polymarket lists one two-team market whose
//! first outcome's book is the second's complement: the second team is `complement`.

use actix_web::{web, HttpResponse, Responder};
use chrono::{Duration, NaiveDate, Utc};
use common::VenueMarket;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::futures::kalshi_event_markets;
use crate::{num, polymarket_fee, polymarket_outcomes, polymarket_time, valid_cursor, FetchError, Venues};

const KALSHI_MAX_PAGES: usize = 5;
const POLYMARKET_MAX_PAGES: usize = 5;
/// A game that started this long ago is over or in play; neither is matched.
const STARTED_HOURS: i64 = 3;

/// "KXNBAGAME-26NOV04ATLWAS" -> 2026-11-04 (the date Kalshi scheduled the game, US time).
pub fn kalshi_game_date(event_id: &str) -> Option<NaiveDate> {
    let seg = event_id.split('-').nth(1)?;
    let (yy, mon, dd) = (seg.get(0..2)?, seg.get(2..5)?, seg.get(5..7)?);
    let m = ["JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC"].iter().position(|x| *x == mon)? as u32 + 1;
    NaiveDate::from_ymd_opt(2000 + yy.parse::<i32>().ok()?, m, dd.parse().ok()?)
}

fn price(v: Option<f64>) -> Option<f64> {
    v.filter(|p| *p > 0.0 && *p < 1.0)
}

/// One Polymarket moneyline market as two team entries.
pub fn polymarket_game(m: &Value) -> Vec<VenueMarket> {
    let (Some(slug), Some(outcomes)) = (m["slug"].as_str(), polymarket_outcomes(m)) else { return vec![] };
    let [first, second] = outcomes.as_slice() else { return vec![] };
    if first.eq_ignore_ascii_case("yes") || m["closed"].as_bool() == Some(true) || m["acceptingOrders"].as_bool() == Some(false) {
        return vec![];
    }
    let start = polymarket_time(&m["gameStartTime"]);
    // US Eastern date: games start after noon ET, so UTC-5 gives the right day all year
    let game_date = start.map(|t| (t - Duration::hours(5)).date_naive());
    let event = m["events"][0]["slug"].as_str().unwrap_or(slug);
    let (bid, ask) = (price(num(&m["bestBid"])), price(num(&m["bestAsk"])));
    let entry = |outcome: &str, yes_bid: Option<f64>, yes_ask: Option<f64>, complement: bool| VenueMarket {
        venue: "polymarket".into(),
        market_id: slug.to_string(),
        event_id: slug.to_string(),
        event_title: m["question"].as_str().unwrap_or("").to_string(),
        outcome: outcome.to_string(),
        code: None,
        season: None,
        yes_bid,
        yes_ask,
        resolves_at: start,
        rules: m["description"].as_str().unwrap_or("").to_string(),
        url: format!("https://polymarket.com/event/{event}"),
        volume: num(&m["volumeNum"]),
        taker_fee: polymarket_fee(m),
        game_date,
        complement,
    };
    vec![entry(first, bid, ask, false), entry(second, ask.map(|a| 1.0 - a), bid.map(|b| 1.0 - b), true)]
}

#[derive(Deserialize)]
pub struct SeriesQ {
    series: String,
}

pub async fn kalshi(venues: web::Data<Venues>, q: web::Query<SeriesQ>) -> impl Responder {
    let series: Vec<&str> = q.series.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    if series.is_empty() || series.len() > 8 || !series.iter().all(|s| s.len() <= 40 && s.chars().all(|c| c.is_ascii_alphanumeric())) {
        return HttpResponse::BadRequest().json(json!({ "error": "series: 1 to 8 comma-separated Kalshi game series" }));
    }
    let today = (Utc::now() - Duration::hours(5)).date_naive();
    let (mut out, mut errors) = (Vec::new(), Vec::new());
    for (i, s) in series.iter().enumerate() {
        let mut cursor = String::new();
        for page in 0..KALSHI_MAX_PAGES {
            if i + page > 0 {
                actix_web::rt::time::sleep(std::time::Duration::from_millis(250)).await;
            }
            let url = format!("{}/events?series_ticker={s}&status=open&with_nested_markets=true&limit=100&cursor={cursor}", venues.kalshi);
            let body = match venues.get_json(&url).await {
                Ok(b) => b,
                Err(FetchError::NotFound) => break,
                Err(FetchError::Upstream(e)) => { errors.push(e); break }
            };
            for ev in body["events"].as_array().into_iter().flatten() {
                let Some(day) = ev["event_ticker"].as_str().and_then(kalshi_game_date) else { continue };
                if day < today {
                    continue;
                }
                out.extend(kalshi_event_markets(ev, s).into_iter().map(|mut m| { m.game_date = Some(day); m }));
            }
            match body["cursor"].as_str() {
                Some(c) if !c.is_empty() && valid_cursor(c) => cursor = c.to_string(),
                _ => break,
            }
        }
    }
    HttpResponse::Ok().json(json!({ "markets": out, "errors": errors }))
}

#[derive(Deserialize)]
pub struct LeagueQ {
    league: String,
}

pub async fn polymarket(venues: web::Data<Venues>, q: web::Query<LeagueQ>) -> impl Responder {
    let league = q.league.to_lowercase();
    if !matches!(league.as_str(), "nba" | "nfl" | "nhl" | "mlb") {
        return HttpResponse::BadRequest().json(json!({ "error": "league: nba, nfl, nhl or mlb" }));
    }
    let sports = match venues.get_json(&format!("{}/sports", venues.polymarket)).await {
        Ok(v) => v,
        Err(FetchError::Upstream(e)) => return HttpResponse::BadGateway().json(json!({ "error": e })),
        Err(FetchError::NotFound) => return HttpResponse::BadGateway().json(json!({ "error": "no Polymarket sports list" })),
    };
    let Some(tag) = sports.as_array().into_iter().flatten().find(|s| s["sport"].as_str() == Some(league.as_str())).and_then(|s| s["primaryTagId"].as_i64()) else {
        return HttpResponse::Ok().json(json!({ "markets": [], "errors": [format!("Polymarket lists no {league} tag")] }));
    };
    let started = Utc::now() - Duration::hours(STARTED_HOURS);
    let mut out = Vec::new();
    for page in 0..POLYMARKET_MAX_PAGES {
        let url = format!("{}/markets?closed=false&sports_market_types=moneyline&tag_id={tag}&limit=100&offset={}", venues.polymarket, page * 100);
        let ms = match venues.get_json(&url).await {
            Ok(v) => v.as_array().cloned().unwrap_or_default(),
            Err(FetchError::NotFound) => vec![],
            Err(FetchError::Upstream(e)) => return HttpResponse::BadGateway().json(json!({ "error": e })),
        };
        out.extend(ms.iter().flat_map(polymarket_game).filter(|m| m.resolves_at.map(|t| t > started).unwrap_or(false)));
        if ms.len() < 100 {
            break;
        }
    }
    HttpResponse::Ok().json(json!({ "markets": out, "errors": [] }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kalshi_dates_from_tickers() {
        assert_eq!(kalshi_game_date("KXNBAGAME-26NOV04ATLWAS"), NaiveDate::from_ymd_opt(2026, 11, 4));
        assert_eq!(kalshi_game_date("KXNFLGAME-27JAN10BUFKC"), NaiveDate::from_ymd_opt(2027, 1, 10));
        assert_eq!(kalshi_game_date("KXSB-27"), None);
    }

    #[test]
    fn a_polymarket_game_is_two_complementary_teams() {
        let m = json!({ "slug": "nba-phi-det-2026-11-04", "question": "76ers vs. Pistons", "outcomes": "[\"76ers\", \"Pistons\"]",
            "bestBid": 0.44, "bestAsk": 0.45, "gameStartTime": "2026-11-05 00:00:00+00", "feesEnabled": true,
            "feeSchedule": { "rate": 0.05, "exponent": 1 }, "events": [{ "slug": "nba-phi-det-2026-11-04" }] });
        let g = polymarket_game(&m);
        assert_eq!(g.len(), 2);
        assert_eq!((g[0].outcome.as_str(), g[0].yes_bid, g[0].yes_ask, g[0].complement), ("76ers", Some(0.44), Some(0.45), false));
        assert_eq!(g[1].outcome, "Pistons");
        assert!(g[1].complement && (g[1].yes_bid.unwrap() - 0.55).abs() < 1e-9 && (g[1].yes_ask.unwrap() - 0.56).abs() < 1e-9);
        // 7 PM ET on Nov 4 is 00:00 UTC on Nov 5
        assert_eq!(g[0].game_date, NaiveDate::from_ymd_opt(2026, 11, 4));
        assert!(polymarket_game(&json!({ "slug": "x", "outcomes": "[\"Yes\", \"No\"]" })).is_empty());
    }
}
