//! Odds Screen / +EV Finder board for every game market, from the recorded
//! multi-book lines (`market_lines`: ESPN DraftKings, SportsGameOdds books, The
//! Odds API) — full-game moneyline, spread, total and team totals, and the same
//! five for the first segment (1st half; NHL 1st period; MLB first 5 innings).
//!
//!   GET /market/{sport}/screen?market=spread_1h
//!
//! One row per (game, side, line): every book's price at that line, the best
//! price, the no-vig consensus of the books quoting it ("fair"), the best price's
//! edge over fair, and — when the sport's champion engine has frozen the game —
//! the model's probability (pushes excluded) and EV per unit at the best price.

use std::collections::{BTreeMap, HashMap};

use actix_web::{web, HttpResponse, Responder};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_postgres::types::Json;

use crate::picks::{decimal, implied, novig, pick_label, price};
use crate::segments;
use crate::State;

fn dberr(e: &tokio_postgres::Error) -> String {
    e.as_db_error().map(|d| d.message().to_string()).unwrap_or_else(|| e.to_string())
}

fn american_from_prob(p: f64) -> i64 {
    if p >= 0.5 { (-100.0 * p / (1.0 - p)).round() as i64 } else { (100.0 * (1.0 - p) / p).round() as i64 }
}

fn r4(x: f64) -> f64 { (x * 10000.0).round() / 10000.0 }

#[derive(Deserialize)]
pub struct ScreenQ { market: Option<String> }

/// One book's two-way quote: (line, price, partner price, observed).
type Quote = (Option<f64>, i32, i32, DateTime<Utc>);

pub async fn screen(state: web::Data<State>, path: web::Path<String>, q: web::Query<ScreenQ>) -> impl Responder {
    let sport = path.into_inner();
    let market = q.market.clone().unwrap_or_else(|| "moneyline".into());
    if crate::market::league(&sport).is_none() { return HttpResponse::BadRequest().json(json!({ "error": "unknown sport" })); }
    if !segments::is_game_market(&market) { return HttpResponse::BadRequest().json(json!({ "error": format!("market must be one of {:?}", segments::GAME_MARKETS) })); }
    match build(&state, &sport, &market).await {
        Ok(v) => HttpResponse::Ok().json(v),
        Err(e) => HttpResponse::InternalServerError().json(json!({ "error": e })),
    }
}

async fn build(state: &State, sport: &str, market: &str) -> Result<Value, String> {
    let db = &state.db;
    // the latest quote of each book on each side
    let rows = db.query(
        "SELECT DISTINCT ON (m.book, m.game_id, m.side) m.book, m.game_id, m.side, m.line, m.price, m.observed_at, m.starts_at, g.home_team, g.away_team \
         FROM market_lines m LEFT JOIN sports_games g ON g.sport = m.sport AND g.game_id = m.game_id \
         WHERE m.sport=$1 AND m.kind='game' AND m.market=$2 AND m.starts_at > now() AND m.observed_at > now() - interval '8 hours' \
         ORDER BY m.book, m.game_id, m.side, m.observed_at DESC", &[&sport, &market]).await.map_err(|e| dberr(&e))?;
    let other = |s: &str| match s { "home" => "away", "away" => "home", "over" => "under", _ => "over" };
    let mut sides: HashMap<(String, String, String), (Option<f64>, i32, DateTime<Utc>)> = HashMap::new();
    let mut games: HashMap<String, (String, String, Option<DateTime<Utc>>)> = HashMap::new();
    for r in &rows {
        let (book, game, side): (String, String, String) = (r.get(0), r.get(1), r.get(2));
        games.entry(game.clone()).or_insert((r.get::<_, Option<String>>(7).unwrap_or_default(), r.get::<_, Option<String>>(8).unwrap_or_default(), r.get(6)));
        sides.insert((book, game, side), (r.get(3), r.get(4), r.get(5)));
    }
    // pair each side with its partner at the same book and number
    let base = segments::base_market(market);
    let mut quotes: BTreeMap<(String, String), Vec<(String, Quote)>> = BTreeMap::new(); // (game, side) -> (book, quote)
    for ((book, game, side), (line, price, obs)) in &sides {
        let Some((oline, oprice, oobs)) = sides.get(&(book.clone(), game.clone(), other(side).to_string())) else { continue };
        let same = match base { "moneyline" => true, "spread" => matches!((line, oline), (Some(a), Some(b)) if (a + b).abs() < 1e-9), _ => line == oline };
        if !same || (*obs - *oobs).num_minutes().abs() > 10 { continue; }
        quotes.entry((game.clone(), side.clone())).or_default().push((book.clone(), (*line, *price, *oprice, *obs)));
    }
    // champion engines' frozen forecasts for these games (simulator first, then the team engine)
    let ids: Vec<String> = games.keys().cloned().collect();
    let champs: HashMap<String, String> = db.query("SELECT sport, version FROM model_registry WHERE status='champion' AND sport = ANY($1)",
        &[&vec![sport.to_string(), format!("{sport}_team")]]).await.map_err(|e| dberr(&e))?.iter().map(|r| (r.get(0), r.get(1))).collect();
    let mut ledgers: Vec<(String, bool, HashMap<String, Value>)> = Vec::new();
    for (engine, team) in [(sport.to_string(), false), (format!("{sport}_team"), true)] {
        let Some(v) = champs.get(&engine) else { continue };
        let m: HashMap<String, Value> = db.query("SELECT DISTINCT ON (game_id) game_id, payload FROM prediction_ledger WHERE sport=$1 AND model_version=$2 AND game_id = ANY($3) \
            ORDER BY game_id, (lineup_mode='confirmed') DESC, id DESC", &[&engine, v, &ids]).await.map_err(|e| dberr(&e))?
            .iter().map(|r| { let p: Json<Value> = r.get(1); (r.get(0), p.0) }).collect();
        ledgers.push((engine, team, m));
    }

    let mut out = Vec::new();
    let mut newest: Option<DateTime<Utc>> = None;
    for ((game, side), qs) in &quotes {
        let (home, away, start) = games.get(game).cloned().unwrap_or_default();
        // one row per number quoted
        let mut by_line: BTreeMap<String, Vec<&(String, Quote)>> = BTreeMap::new();
        for x in qs { by_line.entry(format!("{:?}", x.1 .0)).or_default().push(x); }
        let main_books = by_line.values().map(|v| v.len()).max().unwrap_or(0);
        for group in by_line.values() {
            let line = group[0].1 .0;
            let fair = group.iter().map(|(_, (_, a, b, _))| novig(*a, *b)).sum::<f64>() / group.len() as f64;
            let (best_book, best) = group.iter().map(|(b, q)| (b.clone(), q.1)).max_by(|a, b| decimal(a.1).total_cmp(&decimal(b.1))).unwrap();
            let books: serde_json::Map<String, Value> = group.iter().map(|(b, q)| (b.clone(), json!(q.1))).collect();
            for (_, q) in group.iter() { if newest.map(|n| q.3 > n).unwrap_or(true) { newest = Some(q.3); } }
            let best_implied = implied(best);
            // the model, when an engine has frozen this game and can price this market
            let model = ledgers.iter().find_map(|(engine, team, m)| {
                let pr = price(m.get(game)?, *team, market, "game", side, line)?;
                Some((engine.clone(), pr))
            });
            let mut row = json!({
                "game": format!("{away} @ {home}"), "game_id": game, "home": home, "away": away, "commence_time": start,
                "market": market, "market_label": segments::market_label(sport, market), "side": side, "line": line,
                "outcome": pick_label(sport, market, side, line, &home, &away),
                "main_line": group.len() == main_books,
                "best_price": best, "best_book": best_book, "books": Value::Object(books), "book_count": group.len(),
                "fair_probability": r4(fair), "fair_odds": american_from_prob(fair.clamp(1e-4, 1.0 - 1e-4)),
                "best_implied_probability": r4(best_implied), "edge": r4(fair - best_implied),
            });
            if let Some((engine, pr)) = model {
                let p = pr.p();
                row["model_engine"] = json!(engine);
                row["model_probability"] = json!(r4(p));
                row["model_push"] = json!(r4(pr.push));
                row["model_edge"] = json!(r4(p - fair));
                row["model_ev"] = json!(r4(pr.win * (decimal(best) - 1.0) - pr.loss));
                row["projection"] = json!(pr.projection.map(|x| (x * 10.0).round() / 10.0));
            }
            out.push(row);
        }
    }
    out.sort_by(|a, b| b["edge"].as_f64().unwrap_or(f64::MIN).total_cmp(&a["edge"].as_f64().unwrap_or(f64::MIN)));
    Ok(json!({ "sport": sport, "market": market, "market_label": segments::market_label(sport, market), "segment": segments::label(sport),
               "as_of": newest, "count": out.len(), "rows": out,
               "note": "recorded lines (ESPN DraftKings every 20 min, SportsGameOdds books every 4 h, The Odds API every 6 h); fair = no-vig consensus of the books at that number" }))
}
