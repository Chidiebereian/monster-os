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

pub(crate) fn league(sport: &str) -> Option<&'static str> {
    match sport { "nfl" => Some("NFL"), "nhl" => Some("NHL"), "nba" => Some("NBA"), "mlb" => Some("MLB"), _ => None }
}

/// our stat key -> SportsGameOdds statID (per sport); None = no mapped market.
/// statIDs verified against live SGO events (2026-10).
pub(crate) fn sgo_stat(sport: &str, key: &str) -> Option<&'static str> {
    match (sport, key) {
        // NBA
        ("nba", "points") => Some("points"),
        ("nba", "rebounds") => Some("rebounds"),
        ("nba", "assists") => Some("assists"),
        ("nba", "threes") => Some("threePointersMade"),
        ("nba", "steals") => Some("steals"),
        ("nba", "blocks") => Some("blocks"),
        ("nba", "turnovers") => Some("turnovers"),
        // combos: SGO joins statIDs with '+' (not yet seen on the amateur tier)
        ("nba", "pra") => Some("points+rebounds+assists"),
        ("nba", "pr") => Some("points+rebounds"),
        ("nba", "pa") => Some("points+assists"),
        ("nba", "ra") => Some("rebounds+assists"),
        // NFL
        ("nfl", "pass_yds") => Some("passing_yards"),
        ("nfl", "pass_td") => Some("passing_touchdowns"),
        ("nfl", "pass_att") => Some("passing_attempts"),
        ("nfl", "completions") => Some("passing_completions"),
        ("nfl", "interceptions") => Some("passing_interceptions"),
        ("nfl", "rush_yds") => Some("rushing_yards"),
        ("nfl", "carries") => Some("rushing_attempts"),
        ("nfl", "rush_td") => Some("rushing_touchdowns"),
        ("nfl", "rec_yds") => Some("receiving_yards"),
        ("nfl", "receptions") => Some("receiving_receptions"),
        ("nfl", "targets") => Some("receiving_targets"),
        ("nfl", "rec_td") => Some("receiving_touchdowns"),
        // NHL (shots = shots on goal; saves/GA are goalie markets)
        ("nhl", "shots") => Some("shots_onGoal"),
        ("nhl", "assists") => Some("assists"),
        ("nhl", "points") => Some("points"),
        ("nhl", "hits") => Some("hits"),
        ("nhl", "toi") => Some("minutesPlayed"),
        ("nhl", "saves") => Some("goalie_saves"),
        ("nhl", "goals_against") => Some("goalie_goalsAgainst"),
        // MLB: SGO's batting_/pitching_ naming. NOT yet verified against live
        // events (amateur tier was rate-limited, 2026-10-19); a wrong ID just
        // yields no line, never a fabricated one.
        ("mlb", "hits") => Some("batting_hits"),
        ("mlb", "total_bases") => Some("batting_totalBases"),
        ("mlb", "home_runs") => Some("batting_homeRuns"),
        ("mlb", "runs") => Some("batting_runs"),
        ("mlb", "rbi") => Some("batting_RBI"),
        ("mlb", "hrr") => Some("batting_hits+runs+rbi"),
        ("mlb", "singles") => Some("batting_singles"),
        ("mlb", "doubles") => Some("batting_doubles"),
        ("mlb", "walks") => Some("batting_basesOnBalls"),
        ("mlb", "batter_k") => Some("batting_strikeouts"),
        ("mlb", "stolen_bases") => Some("batting_stolenBases"),
        ("mlb", "pitcher_k") => Some("pitching_strikeouts"),
        ("mlb", "outs") => Some("pitching_outs"),
        ("mlb", "hits_allowed") => Some("pitching_hits"),
        ("mlb", "earned_runs") => Some("pitching_earnedRuns"),
        ("mlb", "walks_allowed") => Some("pitching_basesOnBalls"),
        _ => None,
    }
}

const BOOK_PRIORITY: &[&str] = &["pinnacle", "draftkings", "fanduel", "betmgm", "caesars", "espnbet", "betonline", "bovada"];

/// Accent-normalize a name for matching (č->c, ć->c, ö->o, ...), keep ascii alnum lowercased.
pub(crate) fn norm_name(s: &str) -> String {
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

pub(crate) fn key_from_env() -> Option<String> {
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
pub(crate) fn implied(american: f64) -> f64 {
    if american < 0.0 { -american / (-american + 100.0) } else { 100.0 / (american + 100.0) }
}

/// One current player-prop line, matched (by name) to our player where possible.
pub(crate) struct PropLine {
    pub stat: &'static str,
    pub player: String,
    pub player_id: Option<String>,
    pub team: Option<String>,
    pub opponent: Option<String>,
    pub home: Option<bool>,
    pub starts_at: Value,
    pub line: f64,
    pub over_odds: Option<f64>,
    pub under_odds: Option<f64>,
}

/// Current game lines (ml / spread / total) for a sport. `spread` is the HOME
/// spread in bettor convention (negative = home favored).
pub(crate) async fn fetch_games(state: &State, sport: &str) -> Result<Vec<Value>, String> {
    let lg = league(sport).ok_or("unknown sport")?;
    let key = key_from_env().ok_or("SPORTSGAMEODDS_API_KEY not set")?;
    let events = fetch_events(&state.http, &key, lg).await?;
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
    Ok(out)
}

/// Current over/under player-prop lines for the given stat keys (one SGO fetch).
pub(crate) async fn fetch_props(state: &State, sport: &str, stats: &[&'static str]) -> Result<Vec<PropLine>, String> {
    let lg = league(sport).ok_or("unknown sport")?;
    let key = key_from_env().ok_or("SPORTSGAMEODDS_API_KEY not set")?;
    let mapped: Vec<(&'static str, &'static str)> = stats.iter().filter_map(|k| sgo_stat(sport, k).map(|s| (*k, s))).collect();
    if mapped.is_empty() { return Err("no mapped market for these stats".into()); }
    let events = fetch_events(&state.http, &key, lg).await?;

    // our players (normalized name -> player_id) for matching
    let rows = state.db.query("SELECT player_id, display_name FROM sports_players WHERE sport=$1", &[&sport])
        .await.map_err(|e| e.to_string())?;
    let mut by_name: HashMap<String, String> = HashMap::new();
    for r in &rows { by_name.insert(norm_name(&r.get::<_, String>("display_name")), r.get("player_id")); }

    const OVER: &str = "-game-ou-over";
    let mut out = Vec::new();
    for ev in &events {
        let (Some(players), Some(odds)) = (ev["players"].as_object(), ev["odds"].as_object()) else { continue };
        let side = |s: &str| (ev["teams"][s]["teamID"].as_str().unwrap_or(""), ev["teams"][s]["names"]["short"].as_str().map(str::to_string));
        let (home_id, home_short) = side("home");
        let (away_id, away_short) = side("away");
        for (oid, market) in odds {
            // {sgo}-{PLAYERID}-game-ou-over
            let Some(rest) = oid.strip_suffix(OVER) else { continue };
            for &(our, sgo) in &mapped {
                let Some(pid_sgo) = rest.strip_prefix(sgo).and_then(|r| r.strip_prefix('-')) else { continue };
                // must be a player market (playerID present in players map), not a team market
                let Some(p) = players.get(pid_sgo) else { continue };
                let over = book_pick(market);
                let Some(line) = num_field(over, "overUnder") else { continue };
                let under = odds.get(&format!("{sgo}-{pid_sgo}-game-ou-under")).and_then(book_pick);
                let name = p["name"].as_str().unwrap_or("").to_string();
                let tid = p["teamID"].as_str().unwrap_or("");
                let (team, opponent, home) = if !tid.is_empty() && tid == home_id { (home_short.clone(), away_short.clone(), Some(true)) }
                    else if !tid.is_empty() && tid == away_id { (away_short.clone(), home_short.clone(), Some(false)) }
                    else { (None, None, None) };
                out.push(PropLine {
                    stat: our, player_id: by_name.get(&norm_name(&name)).cloned(), player: name,
                    team, opponent, home, starts_at: ev["status"]["startsAt"].clone(),
                    line, over_odds: odds_of(over), under_odds: odds_of(under),
                });
            }
        }
    }
    Ok(out)
}

// --------------------------------------------------------------------------
// GET /market/{sport}/games  -> current game lines (ml / spread / total)
// --------------------------------------------------------------------------
pub async fn games(state: web::Data<State>, path: web::Path<String>) -> impl Responder {
    let sport = path.into_inner();
    if league(&sport).is_none() { return bad_request("unknown sport"); }
    match fetch_games(&state, &sport).await {
        Ok(out) => HttpResponse::Ok().json(json!({ "sport": sport, "count": out.len(), "games": out })),
        Err(e) => internal(e),
    }
}

// --------------------------------------------------------------------------
// GET /market/{sport}/props/{stat}  -> player prop lines matched to our players
// --------------------------------------------------------------------------
pub async fn props(state: web::Data<State>, path: web::Path<(String, String)>) -> impl Responder {
    let (sport, stat) = path.into_inner();
    if league(&sport).is_none() { return bad_request("unknown sport"); }
    let Some(sgo) = sgo_stat(&sport, &stat) else { return bad_request("stat has no mapped market"); };
    let Some(def) = crate::player_backtest::stat_def(&sport, &stat) else { return bad_request("unknown stat"); };
    match fetch_props(&state, &sport, &[def.key]).await {
        Ok(lines) => {
            let out: Vec<Value> = lines.iter().map(|p| json!({
                "player": p.player, "player_id": p.player_id, "team": p.team,
                "line": p.line, "over_odds": p.over_odds, "under_odds": p.under_odds,
            })).collect();
            HttpResponse::Ok().json(json!({ "sport": sport, "stat": stat, "sgo_stat": sgo, "count": out.len(), "props": out }))
        }
        Err(e) => internal(e),
    }
}
