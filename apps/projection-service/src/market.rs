//! Live market lines from SportsGameOdds (api.sportsgameodds.com) for EV.
//! Game markets use statID `points` (winner-determining) at period `game`:
//!   points-{home|away}-game-ml-{side}, -sp-{side} (spread), points-all-game-ou-{over|under}.
//! Player props: {statID}-{PLAYERID}-game-ou-{over|under}; the players map gives
//! the name. Teams match our abbrev via names.short; players via accent-normalized
//! name. A reference book is chosen per market; odds are de-vigged for implied %.
//! Needs SPORTSGAMEODDS_API_KEY in the environment.

use std::collections::HashMap;

use actix_web::{web, HttpResponse, Responder};
use serde_json::{json, Value};

use crate::State;

fn internal(m: impl std::fmt::Display) -> HttpResponse {
    eprintln!("projection-service(market): {m}");
    HttpResponse::InternalServerError().json(json!({ "error": m.to_string() }))
}
fn bad_request(m: impl Into<String>) -> HttpResponse { HttpResponse::BadRequest().json(json!({ "error": m.into() })) }

fn league(sport: &str) -> Option<&'static str> {
    match sport { "nfl" => Some("NFL"), "nhl" => Some("NHL"), "nba" => Some("NBA"), "mlb" => Some("MLB"), _ => None }
}

/// our stat key -> SportsGameOdds statID (per sport); None = no mapped market.
fn sgo_stat(sport: &str, key: &str) -> Option<&'static str> {
    match (sport, key) {
        ("nba", "points") => Some("points"), ("nba", "rebounds") => Some("rebounds"), ("nba", "assists") => Some("assists"),
        ("nba", "threes") => Some("threePointersMade"), ("nba", "steals") => Some("steals"), ("nba", "blocks") => Some("blocks"),
        ("nba", "turnovers") => Some("turnovers"),
        ("nfl", "pass_yds") => Some("passing_yards"), ("nfl", "rush_yds") => Some("rushing_yards"),
        ("nfl", "rec_yds") => Some("receiving_yards"), ("nfl", "receptions") => Some("receptions"),
        ("nfl", "pass_td") => Some("passing_touchdowns"), ("nfl", "interceptions") => Some("passing_interceptions"),
        ("nhl", "shots") => Some("shots_on_goal"), ("nhl", "goals") => Some("goals"), ("nhl", "assists") => Some("assists"),
        ("nhl", "points") => Some("points"), ("nhl", "saves") => Some("saves"),
        _ => None,
    }
}

const BOOK_PRIORITY: &[&str] = &["pinnacle", "draftkings", "fanduel", "betmgm", "caesars", "espnbet", "betonline", "bovada"];

/// Accent-normalize a name for matching (č->c, ć->c, ö->o, ...), keep ascii alnum lowercased.
fn norm_name(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        let c = match ch {
            'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' | 'ā' => 'a',
            'é' | 'è' | 'ê' | 'ë' | 'ē' | 'ė' => 'e',
            'í' | 'ì' | 'î' | 'ï' | 'ī' => 'i',
            'ó' | 'ò' | 'ô' | 'ö' | 'õ' | 'ø' | 'ō' => 'o',
            'ú' | 'ù' | 'û' | 'ü' | 'ū' => 'u',
            'ñ' => 'n', 'ç' | 'č' | 'ć' => 'c', 'š' | 'ś' => 's', 'ž' | 'ź' | 'ż' => 'z',
            'ð' | 'đ' => 'd', 'ý' | 'ÿ' => 'y', 'ł' => 'l', 'ß' => 's',
            other => other,
        };
        if c.is_ascii_alphanumeric() { out.push(c.to_ascii_lowercase()); }
    }
    out
}

async fn fetch_events(http: &reqwest::Client, key: &str, lg: &str) -> Result<Vec<Value>, String> {
    let url = format!("https://api.sportsgameodds.com/v2/events?leagueID={lg}&oddsAvailable=true&limit=50");
    let resp = http.get(&url).header("x-api-key", key).send().await.map_err(|e| format!("SGO request: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("SGO returned {}", resp.status()));
    }
    let v: Value = resp.json().await.map_err(|e| format!("SGO body: {e}"))?;
    Ok(v["data"].as_array().cloned().unwrap_or_default())
}

fn key_from_env() -> Option<String> {
    std::env::var("SPORTSGAMEODDS_API_KEY").ok().filter(|k| !k.is_empty())
}

/// best book's value for an oddID: returns (odds, spread, overUnder)
fn book_pick<'a>(market: &'a Value) -> Option<&'a Value> {
    let bb = market.get("byBookmaker")?.as_object()?;
    for &b in BOOK_PRIORITY {
        if let Some(v) = bb.get(b) {
            if v.get("available").and_then(|a| a.as_bool()).unwrap_or(true) { return Some(v); }
        }
    }
    bb.values().next()
}
fn odds_of(v: Option<&Value>) -> Option<f64> {
    v?.get("odds")?.as_str().and_then(|s| s.trim_start_matches('+').parse::<f64>().ok())
}
fn num_field(v: Option<&Value>, field: &str) -> Option<f64> {
    v?.get(field)?.as_str().and_then(|s| s.parse::<f64>().ok())
}
fn implied(american: f64) -> f64 {
    if american < 0.0 { -american / (-american + 100.0) } else { 100.0 / (american + 100.0) }
}

// --------------------------------------------------------------------------
// GET /market/{sport}/games  -> current game lines (ml / spread / total)
// --------------------------------------------------------------------------
pub async fn games(state: web::Data<State>, path: web::Path<String>) -> impl Responder {
    let sport = path.into_inner();
    let Some(lg) = league(&sport) else { return bad_request("unknown sport"); };
    let Some(key) = key_from_env() else { return internal("SPORTSGAMEODDS_API_KEY not set"); };
    let events = match fetch_events(&state.http, &key, lg).await { Ok(e) => e, Err(e) => return internal(e) };

    let mut out = Vec::new();
    for ev in &events {
        let home = ev["teams"]["home"]["names"]["short"].as_str().unwrap_or("");
        let away = ev["teams"]["away"]["names"]["short"].as_str().unwrap_or("");
        if home.is_empty() || away.is_empty() { continue; }
        let odds = &ev["odds"];
        let ml_h = odds_of(book_pick(&odds["points-home-game-ml-home"]));
        let ml_a = odds_of(book_pick(&odds["points-away-game-ml-away"]));
        let sp_m = book_pick(&odds["points-home-game-sp-home"]);
        let ou_o = book_pick(&odds["points-all-game-ou-over"]);
        let ou_u = book_pick(&odds["points-all-game-ou-under"]);
        let novig_home = match (ml_h, ml_a) { (Some(h), Some(a)) => { let (ih, ia) = (implied(h), implied(a)); Some(ih / (ih + ia)) } _ => None };
        out.push(json!({
            "home": home, "away": away, "starts_at": ev["status"]["startsAt"],
            "ml_home_odds": ml_h, "ml_away_odds": ml_a, "novig_home_win": novig_home.map(|x| (x*1000.0).round()/1000.0),
            "spread": num_field(sp_m, "spread"), "spread_home_odds": odds_of(sp_m),
            "spread_away_odds": odds_of(book_pick(&odds["points-away-game-sp-away"])),
            "total": num_field(ou_o, "overUnder"), "over_odds": odds_of(ou_o), "under_odds": odds_of(ou_u),
        }));
    }
    HttpResponse::Ok().json(json!({ "sport": sport, "count": out.len(), "games": out }))
}

// --------------------------------------------------------------------------
// GET /market/{sport}/props/{stat}  -> player prop lines matched to our players
// --------------------------------------------------------------------------
pub async fn props(state: web::Data<State>, path: web::Path<(String, String)>) -> impl Responder {
    let (sport, stat) = path.into_inner();
    let Some(lg) = league(&sport) else { return bad_request("unknown sport"); };
    let Some(sgo) = sgo_stat(&sport, &stat) else { return bad_request("stat has no mapped market"); };
    let Some(key) = key_from_env() else { return internal("SPORTSGAMEODDS_API_KEY not set"); };
    let events = match fetch_events(&state.http, &key, lg).await { Ok(e) => e, Err(e) => return internal(e) };

    // our players (normalized name -> player_id) for matching
    let rows = match state.db.query("SELECT player_id, display_name FROM sports_players WHERE sport=$1", &[&sport]).await {
        Ok(r) => r, Err(e) => return internal(e),
    };
    let mut by_name: HashMap<String, String> = HashMap::new();
    for r in &rows { by_name.insert(norm_name(&r.get::<_, String>("display_name")), r.get("player_id")); }

    let over_suffix = format!("-game-ou-over");
    let mut out = Vec::new();
    for ev in &events {
        let players = ev["players"].as_object();
        let odds = ev["odds"].as_object();
        let (Some(players), Some(odds)) = (players, odds) else { continue };
        for (oid, market) in odds {
            // {sgo}-{PLAYERID}-game-ou-over
            if !oid.ends_with(&over_suffix) { continue; }
            let Some(rest) = oid.strip_suffix(&over_suffix) else { continue };
            let Some(pid_sgo) = rest.strip_prefix(&format!("{sgo}-")) else { continue };
            // must be a player market (playerID present in players map), not a team market
            let Some(p) = players.get(pid_sgo) else { continue };
            let name = p["name"].as_str().unwrap_or("");
            let over = book_pick(market);
            let under_oid = format!("{sgo}-{pid_sgo}-game-ou-under");
            let under = odds.get(&under_oid).and_then(book_pick);
            let line = num_field(over, "overUnder");
            if line.is_none() { continue; }
            let matched = by_name.get(&norm_name(name)).cloned();
            out.push(json!({
                "player": name, "player_id": matched,
                "team": p["teamID"].as_str().map(|t| t.rsplit('_').nth(1).unwrap_or(t)),
                "line": line, "over_odds": odds_of(over), "under_odds": odds_of(under),
            }));
        }
    }
    HttpResponse::Ok().json(json!({ "sport": sport, "stat": stat, "sgo_stat": sgo, "count": out.len(), "props": out }))
}
