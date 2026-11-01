//! Market lines for AI Picks, stored as append-only snapshots in `market_lines`
//! (migration 0018). Three providers:
//!
//! * ESPN (free): DraftKings moneyline / spread / total and two-way player
//!   props, with opening and current prices. Fetched every cycle.
//! * The Odds API (credits, shared with the Odds Screen): every US book's
//!   moneyline / spread / total. Budgeted per day.
//! * SportsGameOdds (entities, 2,500 a month on the amateur tier): every
//!   book's game lines and props, with deep links when the book provides one.
//!   Budgeted per day.
//!
//! Only two-way markets are kept, so every price has a partner at the same book
//! and line to de-vig against. Lines are fetched only for games the champion
//! engines have already frozen, matched to our game ids by team codes and start
//! time, and to our player ids by name within that forecast's two rosters.

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};
use tokio_postgres::types::Json;

use crate::hitrates::canon;
use crate::market::norm_name;
use crate::State;

fn dberr(e: &tokio_postgres::Error) -> String {
    e.as_db_error().map(|d| d.message().to_string()).unwrap_or_else(|| e.to_string())
}

/// One side of a two-way market at one book.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Line {
    pub source: &'static str,
    pub game_id: String,
    pub event_ref: String,
    pub starts_at: Option<DateTime<Utc>>,
    pub kind: &'static str,
    pub market: &'static str,
    pub subject: String,
    pub subject_name: Option<String>,
    pub provider_subject: Option<String>,
    pub side: &'static str,
    pub line: Option<f64>,
    pub price: i32,
    pub book: String,
    pub link: Option<String>,
    pub open_line: Option<f64>,
    pub open_price: Option<i32>,
}

// ---------------------------------------------------------------------------
// Games and rosters to match against
// ---------------------------------------------------------------------------

/// Player-name key: first initial + last name, accent-folded, suffixes dropped.
/// "Leon Draisaitl" and "L. Draisaitl" both give "ldraisaitl".
pub(crate) fn name_key(name: &str) -> String {
    let toks: Vec<String> = name.split(|c: char| c.is_whitespace() || c == '.' || c == ',')
        .map(norm_name).filter(|t| !t.is_empty())
        .filter(|t| !matches!(t.as_str(), "jr" | "sr" | "ii" | "iii" | "iv" | "v")).collect();
    match toks.as_slice() {
        [] => String::new(),
        [one] => one.clone(),
        [first, .., last] => format!("{}{}", &first[..1], last),
    }
}

/// The players of one frozen forecast: id -> (name, team), and name key -> id (None when two share it).
#[derive(Default)]
pub(crate) struct Roster {
    pub by_id: HashMap<String, (String, String)>,
    by_key: HashMap<String, Option<String>>,
}

impl Roster {
    pub(crate) fn from_payload(p: &Value) -> Roster {
        let mut r = Roster::default();
        let mut add = |id: &Value, name: &str, team: &str| {
            let id = match id { Value::String(s) => s.clone(), Value::Number(n) => n.to_string(), _ => return };
            if id.is_empty() || name.is_empty() { return; }
            r.by_id.insert(id.clone(), (name.to_string(), team.to_string()));
            let k = name_key(name);
            match r.by_key.get(&k).cloned() {
                Some(Some(other)) if other != id => { r.by_key.insert(k, None); }
                Some(_) => {}
                None => { r.by_key.insert(k, Some(id)); }
            }
        };
        for side in ["home", "away"] {
            let team = p[side].as_str().unwrap_or("");
            for pl in p["players"][side].as_array().into_iter().flatten() {
                add(&pl["id"], pl["name"].as_str().unwrap_or(""), team);
            }
        }
        for g in p["goalies"].as_array().into_iter().flatten() {
            add(&g["id"], g["name"].as_str().unwrap_or(""), g["team"].as_str().unwrap_or(""));
        }
        r
    }
    /// Our id for a provider's player: the same id (NBA uses ESPN ids), else a unique name match.
    pub(crate) fn resolve(&self, provider_id: Option<&str>, name: &str) -> Option<String> {
        if let Some(id) = provider_id { if self.by_id.contains_key(id) { return Some(id.to_string()); } }
        self.by_key.get(&name_key(name)).cloned().flatten()
    }
}

/// A ledger payload's team code: top-level `home` / `away` (NBA, NHL, NFL, team
/// engines) or `game.home` / `game.away` (the MLB simulator, whose top-level
/// `home` / `away` hold each side's lineup).
pub(crate) fn payload_team(p: &Value, side: &str) -> String {
    p[side].as_str().or_else(|| p["game"][side].as_str()).unwrap_or("").to_string()
}

pub(crate) struct Upcoming {
    pub game_id: String,
    pub start: DateTime<Utc>,
    pub home: String,
    pub away: String,
    pub roster: Roster,
}

/// Games the sport's engines (simulator or team model) have frozen that start in the next 40 hours.
pub(crate) async fn upcoming(db: &tokio_postgres::Client, sport: &str) -> Result<Vec<Upcoming>, String> {
    let engines = vec![sport.to_string(), format!("{sport}_team")];
    let rows = db.query(
        "SELECT * FROM (SELECT DISTINCT ON (l.game_id) l.game_id, COALESCE(l.game_start, k.starts_at) AS start, l.payload FROM prediction_ledger l \
           LEFT JOIN game_kickoffs k ON k.sport = $2 AND k.game_id = l.game_id \
           WHERE l.sport = ANY($1) AND l.game_date >= (now() - interval '1 day')::date \
           ORDER BY l.game_id, (l.sport = $2) DESC, l.id DESC) x \
         WHERE start > now() AND start < now() + interval '40 hours'", &[&engines, &sport]).await.map_err(|e| dberr(&e))?;
    Ok(rows.iter().map(|r| {
        let p: Json<Value> = r.get(2);
        Upcoming { game_id: r.get(0), start: r.get(1), home: payload_team(&p.0, "home"),
            away: payload_team(&p.0, "away"), roster: Roster::from_payload(&p.0) }
    }).collect())
}

fn find_game<'a>(sport: &str, games: &'a [Upcoming], id: Option<&str>, home: &str, away: &str, start: Option<DateTime<Utc>>) -> Option<&'a Upcoming> {
    if let Some(id) = id { if let Some(g) = games.iter().find(|g| g.game_id == id) { return Some(g); } }
    let (h, a) = (canon(sport, home), canon(sport, away));
    games.iter().find(|g| canon(sport, &g.home) == h && canon(sport, &g.away) == a
        && start.map(|s| (s - g.start).num_minutes().abs() <= 6 * 60).unwrap_or(true))
}

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

/// American odds from a number or a string ("+105", "-110", "EVEN").
pub(crate) fn american(v: &Value) -> Option<i32> {
    let x = match v {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => { let s = s.trim(); if s.eq_ignore_ascii_case("even") { 100.0 } else { s.trim_start_matches('+').parse::<f64>().ok()? } }
        _ => return None,
    };
    if !x.is_finite() || x.abs() < 100.0 { return None; }
    Some(x.round() as i32)
}

/// A line from a number or a string ("+1.5", "-3", "PK").
fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => { let s = s.trim(); if s.eq_ignore_ascii_case("pk") { Some(0.0) } else { s.trim_start_matches('+').parse().ok() } }
        _ => None,
    }.filter(|x: &f64| x.is_finite())
}

fn first<T>(xs: impl IntoIterator<Item = Option<T>>) -> Option<T> { xs.into_iter().flatten().next() }

fn book_key(s: &str) -> String { norm_name(s) }

// ---------------------------------------------------------------------------
// ESPN (DraftKings)
// ---------------------------------------------------------------------------

fn espn_league(sport: &str) -> Option<(&'static str, &'static str)> {
    match sport { "nba" => Some(("basketball", "nba")), "nhl" => Some(("hockey", "nhl")), "nfl" => Some(("football", "nfl")), "mlb" => Some(("baseball", "mlb")), _ => None }
}

/// ESPN two-way prop type -> our simulator stat. Only stats the simulators
/// produce AND the box scores settle are mapped.
pub(crate) fn espn_prop_market(sport: &str, type_name: &str) -> Option<&'static str> {
    let t = type_name.trim_end_matches(" (incl. overtime)").trim().to_ascii_lowercase();
    Some(match (sport, t.as_str()) {
        ("nba", "total points") => "pts",
        ("nba", "total rebounds") => "reb",
        ("nba", "total assists") => "ast",
        ("nba", "total 3-point field goals") => "fg3m",
        ("nba", "total steals") => "stl",
        ("nba", "total blocks") => "blk",
        ("nba", "total turnovers") => "tov",
        ("nba", "total points, rebounds, and assists") => "pra",
        ("nba", "total points and rebounds") => "pr",
        ("nba", "total points and assists") => "pa",
        ("nba", "total assists and rebounds" | "total rebounds and assists") => "ra",
        ("nba", "total steals and blocks" | "total blocks and steals") => "stocks",
        ("nba", "total field goals made") => "fgm",
        ("nba", "total field goals attempted") => "fga",
        ("nba", "total 3-point field goals attempted") => "fg3a",
        ("nba", "total free throws made") => "ftm",
        ("nba", "total free throws attempted") => "fta",
        ("nba", "total offensive rebounds") => "oreb",
        ("nba", "total defensive rebounds") => "dreb",
        ("nhl", "total shots on goal") => "sog",
        ("nhl", "total points") => "points",
        ("nhl", "total assists") => "assists",
        ("nhl", "total goals") => "goals",
        ("nhl", "total saves" | "total goalie saves") => "saves",
        ("nhl", "total blocked shots") => "blocks",
        ("nfl", "total passing yards") => "pass_yds",
        ("nfl", "total rushing yards") => "rush_yds",
        ("nfl", "total receiving yards") => "rec_yds",
        ("nfl", "total receptions") => "receptions",
        ("nfl", "total passing touchdowns") => "pass_td",
        ("nfl", "total passing attempts" | "total pass attempts") => "pass_att",
        ("nfl", "total passing completions" | "total pass completions" | "total completions") => "completions",
        ("nfl", "total interceptions thrown" | "total interceptions" | "total passing interceptions") => "interceptions",
        ("nfl", "total rushing attempts" | "total rush attempts" | "total carries") => "carries",
        ("nfl", "total rushing and receiving yards" | "total rushing + receiving yards") => "rush_rec_yds",
        ("nfl", "total passing and rushing yards" | "total passing + rushing yards") => "pass_rush_yds",
        ("nfl", "total kicking points") => "kick_pts",
        ("nfl", "total field goals made") => "fgm",
        ("nfl", "total touchdowns" | "total touchdowns scored") => "tds",
        _ => return None,
    })
}

async fn get_json(http: &reqwest::Client, url: &str) -> Result<Value, String> {
    let r = http.get(url).timeout(std::time::Duration::from_secs(25)).send().await.map_err(|e| format!("{}: {}", url.split('?').next().unwrap_or(url), e.without_url()))?;
    if !r.status().is_success() { return Err(format!("{} returned {}", url.split('?').next().unwrap_or(url), r.status())); }
    r.json().await.map_err(|e| format!("body: {}", e.without_url()))
}

/// Names and headshots for ESPN athlete ids, cached in `espn_athletes`.
async fn espn_athletes(state: &State, sport: &str, refs: &BTreeMap<String, String>) -> Result<HashMap<String, (String, Option<String>)>, String> {
    let ids: Vec<String> = refs.keys().cloned().collect();
    let mut out: HashMap<String, (String, Option<String>)> = state.db.query(
        "SELECT espn_id, name, headshot FROM espn_athletes WHERE sport=$1 AND espn_id = ANY($2)", &[&sport, &ids])
        .await.map_err(|e| dberr(&e))?.iter().map(|r| (r.get(0), (r.get(1), r.get(2)))).collect();
    let missing: Vec<(&String, &String)> = refs.iter().filter(|(id, _)| !out.contains_key(*id)).collect();
    for chunk in missing.chunks(8) {
        let fetched = futures::future::join_all(chunk.iter().map(|(id, url)| async move {
            (id.to_string(), get_json(&state.http, &url.replace("http://", "https://")).await)
        })).await;
        for (id, r) in fetched {
            let Ok(a) = r else { continue };
            let name = first([a["displayName"].as_str(), a["fullName"].as_str()]).unwrap_or("").to_string();
            if name.is_empty() { continue; }
            let head = a["headshot"]["href"].as_str().map(str::to_string);
            let _ = state.db.execute("INSERT INTO espn_athletes (sport, espn_id, name, headshot) VALUES ($1,$2,$3,$4) \
                ON CONFLICT (sport, espn_id) DO UPDATE SET name=EXCLUDED.name, headshot=EXCLUDED.headshot, fetched_at=now()",
                &[&sport, &id, &name, &head]).await;
            out.insert(id, (name, head));
        }
    }
    Ok(out)
}

/// Game lines from one ESPN odds item (one book).
fn espn_game_lines(it: &Value, g: &Upcoming, event: &str, start: Option<DateTime<Utc>>) -> Vec<Line> {
    let book = book_key(it["provider"]["name"].as_str().unwrap_or("espn"));
    let mk = |market: &'static str, side: &'static str, line: Option<f64>, price: i32, open_line: Option<f64>, open_price: Option<i32>| Line {
        source: "espn", game_id: g.game_id.clone(), event_ref: event.to_string(), starts_at: start, kind: "game", market,
        subject: "game".into(), subject_name: None, provider_subject: None, side, line, price, book: book.clone(), link: None, open_line, open_price,
    };
    let mut out = Vec::new();
    let (h, a) = (&it["homeTeamOdds"], &it["awayTeamOdds"]);
    // moneyline
    let ml = |t: &Value| first([american(&t["current"]["moneyLine"]["american"]), american(&t["moneyLine"])]);
    if let (Some(hp), Some(ap)) = (ml(h), ml(a)) {
        out.push(mk("moneyline", "home", None, hp, None, american(&h["open"]["moneyLine"]["american"])));
        out.push(mk("moneyline", "away", None, ap, None, american(&a["open"]["moneyLine"]["american"])));
    }
    // spread (each side's own number)
    let home_line = first([num(&h["current"]["pointSpread"]["american"]), num(&it["spread"])]);
    let away_line = first([num(&a["current"]["pointSpread"]["american"]), home_line.map(|x| -x)]);
    let sp = |t: &Value| first([american(&t["current"]["spread"]["american"]), american(&t["spreadOdds"])]);
    if let (Some(hl), Some(al), Some(hp), Some(ap)) = (home_line, away_line, sp(h), sp(a)) {
        if (hl + al).abs() < 1e-9 {
            out.push(mk("spread", "home", Some(hl), hp, num(&h["open"]["pointSpread"]["american"]), american(&h["open"]["spread"]["american"])));
            out.push(mk("spread", "away", Some(al), ap, num(&a["open"]["pointSpread"]["american"]), american(&a["open"]["spread"]["american"])));
        }
    }
    // total
    let tl = first([num(&it["current"]["total"]["american"]), num(&it["overUnder"])]);
    let op = first([american(&it["current"]["over"]["american"]), american(&it["overOdds"])]);
    let up = first([american(&it["current"]["under"]["american"]), american(&it["underOdds"])]);
    if let (Some(l), Some(o), Some(u)) = (tl, op, up) {
        let ol = num(&it["open"]["total"]["american"]);
        out.push(mk("total", "over", Some(l), o, ol, american(&it["open"]["over"]["american"])));
        out.push(mk("total", "under", Some(l), u, ol, american(&it["open"]["under"]["american"])));
    }
    out
}

/// Two-way props from ESPN's propBets list. Each prop is a pair of entries, over
/// then under (ESPN does not label them; the order was checked on lopsided lines,
/// and picks::build re-checks it against the model every cycle).
fn espn_prop_pairs<'a>(sport: &str, items: &'a [Value]) -> (Vec<(String, String, &'static str, [&'a Value; 2])>, BTreeMap<String, usize>) {
    let mut groups: BTreeMap<(String, String), Vec<&'a Value>> = BTreeMap::new();
    let mut refs: HashMap<String, String> = HashMap::new();
    let mut unmapped: BTreeMap<String, usize> = BTreeMap::new();
    for it in items {
        let Some(aref) = it["athlete"]["$ref"].as_str() else { continue };
        let ty = it["type"]["name"].as_str().unwrap_or("");
        if !ty.starts_with("Total") { continue; } // milestones / scorers are one-sided
        if espn_prop_market(sport, ty).is_none() { *unmapped.entry(ty.to_string()).or_default() += 1; continue; }
        let id = aref.split("/athletes/").nth(1).unwrap_or("").split('?').next().unwrap_or("").to_string();
        if id.is_empty() { continue; }
        refs.insert(id.clone(), aref.to_string());
        groups.entry((id, ty.to_string())).or_default().push(it);
    }
    let out = groups.into_iter().filter_map(|((id, ty), v)| {
        if v.len() != 2 { return None; }
        let line = |x: &Value| first([x["current"]["target"]["value"].as_f64(), num(&x["odds"]["total"]["value"])]);
        if line(v[0]).is_none() || line(v[0]) != line(v[1]) { return None; }
        Some((id.clone(), refs[&id].clone(), espn_prop_market(sport, &ty)?, [v[0], v[1]]))
    }).collect();
    (out, unmapped)
}

/// ESPN lines for the matched games, and ESPN's full team names (for the Odds API).
pub(crate) async fn espn(state: &State, sport: &str, games: &[Upcoming]) -> Result<(Vec<Line>, HashMap<String, (String, String)>, Value), String> {
    let (sp, lg) = espn_league(sport).ok_or("no ESPN league")?;
    let mut dates: Vec<String> = games.iter().map(|g| (g.start - Duration::hours(5)).format("%Y%m%d").to_string()).collect();
    dates.sort(); dates.dedup();
    let mut lines = Vec::new();
    let mut names: HashMap<String, (String, String)> = HashMap::new();
    let (mut events_matched, mut props_unmatched, mut errors) = (0, 0usize, Vec::new());
    let mut unmapped_all: BTreeMap<String, usize> = BTreeMap::new();
    for d in dates {
        let sb = match get_json(&state.http, &format!("https://site.api.espn.com/apis/site/v2/sports/{sp}/{lg}/scoreboard?dates={d}")).await {
            Ok(v) => v, Err(e) => { errors.push(e); continue }
        };
        for ev in sb["events"].as_array().into_iter().flatten() {
            let eid = ev["id"].as_str().unwrap_or("");
            let start = ev["date"].as_str().and_then(|s| DateTime::parse_from_rfc3339(&s.replace('Z', ":00Z")).or_else(|_| DateTime::parse_from_rfc3339(s)).ok()).map(|d| d.with_timezone(&Utc));
            let comps = ev["competitions"][0]["competitors"].as_array().cloned().unwrap_or_default();
            let team = |ha: &str| comps.iter().find(|c| c["homeAway"] == ha).map(|c| (c["team"]["abbreviation"].as_str().unwrap_or("").to_string(), c["team"]["displayName"].as_str().unwrap_or("").to_string()));
            let (Some((h, hn)), Some((a, an))) = (team("home"), team("away")) else { continue };
            let id_hint = if sport == "nba" { Some(eid) } else { None };
            let Some(g) = find_game(sport, games, id_hint, &h, &a, start) else { continue };
            events_matched += 1;
            names.insert(g.game_id.clone(), (hn, an));
            let base = format!("https://sports.core.api.espn.com/v2/sports/{sp}/leagues/{lg}/events/{eid}/competitions/{eid}/odds");
            let odds = match get_json(&state.http, &base).await { Ok(v) => v, Err(e) => { errors.push(e); continue } };
            for it in odds["items"].as_array().into_iter().flatten() {
                lines.extend(espn_game_lines(it, g, eid, start));
                let pid = it["provider"]["id"].as_str().unwrap_or("");
                if pid.is_empty() { continue; }
                let book = book_key(it["provider"]["name"].as_str().unwrap_or("espn"));
                let props = match get_json(&state.http, &format!("{base}/{pid}/propBets?limit=1000")).await { Ok(v) => v, Err(e) => { errors.push(e); continue } };
                let items = props["items"].as_array().cloned().unwrap_or_default();
                let (pairs, unmapped) = espn_prop_pairs(sport, &items);
                for (k, v) in unmapped { *unmapped_all.entry(k).or_default() += v; }
                let refs: BTreeMap<String, String> = pairs.iter().map(|(id, r, _, _)| (id.clone(), r.clone())).collect();
                let ath = espn_athletes(state, sport, &refs).await?;
                for (aid, _, market, [o, u]) in &pairs {
                    let Some((name, _)) = ath.get(aid) else { props_unmatched += 1; continue };
                    let Some(subject) = g.roster.resolve(Some(aid), name) else { props_unmatched += 1; continue };
                    let line = first([o["current"]["target"]["value"].as_f64(), num(&o["odds"]["total"]["value"])]);
                    let open_line = o["open"]["target"]["value"].as_f64();
                    for (side, x) in [("over", o), ("under", u)] {
                        let Some(price) = american(&x["odds"]["american"]["value"]) else { continue };
                        lines.push(Line { source: "espn", game_id: g.game_id.clone(), event_ref: eid.to_string(), starts_at: start, kind: "prop", market: *market,
                            subject: subject.clone(), subject_name: Some(name.clone()), provider_subject: Some(aid.clone()), side, line, price,
                            book: book.clone(), link: None, open_line, open_price: american(&x["odds"]["american"]["open"]) });
                    }
                }
            }
        }
    }
    let report = json!({ "events_matched": events_matched, "games": games.len(), "lines": lines.len(), "props_unmatched_players": props_unmatched,
        "unmapped_prop_types": unmapped_all, "errors": errors });
    Ok((lines, names, report))
}

// ---------------------------------------------------------------------------
// Budgets
// ---------------------------------------------------------------------------

async fn spent_today(db: &tokio_postgres::Client, provider: &str) -> i32 {
    db.query_opt("SELECT units FROM provider_usage WHERE provider=$1 AND day=(now() AT TIME ZONE 'utc')::date", &[&provider]).await
        .ok().flatten().map(|r| r.get(0)).unwrap_or(0)
}

async fn spend(db: &tokio_postgres::Client, provider: &str, units: i32) {
    let _ = db.execute("INSERT INTO provider_usage (provider, day, requests, units) VALUES ($1, (now() AT TIME ZONE 'utc')::date, 1, $2) \
        ON CONFLICT (provider, day) DO UPDATE SET requests = provider_usage.requests + 1, units = provider_usage.units + EXCLUDED.units", &[&provider, &units]).await;
}

/// Picks settings (learning_settings key 'picks'), with defaults.
pub(crate) async fn setting(db: &tokio_postgres::Client, key: &str, default: f64) -> f64 {
    db.query_opt("SELECT (value->>$1)::float8 FROM learning_settings WHERE key='picks'", &[&key]).await
        .ok().flatten().and_then(|r| r.get::<_, Option<f64>>(0)).unwrap_or(default)
}

/// When a provider last returned lines for a sport.
async fn last_fetch(db: &tokio_postgres::Client, source: &str, sport: &str) -> Option<DateTime<Utc>> {
    db.query_opt("SELECT max(observed_at) FROM market_lines WHERE source=$1 AND sport=$2 AND observed_at > now() - interval '2 days'", &[&source, &sport]).await
        .ok().flatten().and_then(|r| r.get(0))
}

// ---------------------------------------------------------------------------
// The Odds API (multi-book game lines)
// ---------------------------------------------------------------------------

fn oddsapi_sport(sport: &str) -> Option<&'static str> {
    match sport { "nba" => Some("basketball_nba"), "nhl" => Some("icehockey_nhl"), "nfl" => Some("americanfootball_nfl"), "mlb" => Some("baseball_mlb"), _ => None }
}

/// The books requested from The Odds API (ten = one region's credits).
const ODDSAPI_BOOKS: &str = "pinnacle,draftkings,fanduel,betmgm,betrivers,bovada,betonlineag,lowvig,betus,mybookieag";

fn oddsapi_keys() -> Vec<String> {
    std::env::var("ODDS_API_KEYS").or_else(|_| std::env::var("ODDS_API_KEY")).unwrap_or_default()
        .split(',').map(|k| k.trim().to_string()).filter(|k| !k.is_empty()).collect()
}

pub(crate) fn parse_oddsapi(sport: &str, events: &[Value], games: &[Upcoming], names: &HashMap<String, (String, String)>) -> Vec<Line> {
    let mut out = Vec::new();
    for ev in events {
        let (hn, an) = (ev["home_team"].as_str().unwrap_or(""), ev["away_team"].as_str().unwrap_or(""));
        let start = ev["commence_time"].as_str().and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|d| d.with_timezone(&Utc));
        let Some(g) = games.iter().find(|g| names.get(&g.game_id).map(|(h, a)| norm_name(h) == norm_name(hn) && norm_name(a) == norm_name(an)).unwrap_or(false)
            && start.map(|s| (s - g.start).num_minutes().abs() <= 6 * 60).unwrap_or(true)) else { continue };
        let _ = sport;
        for b in ev["bookmakers"].as_array().into_iter().flatten() {
            let book = book_key(b["key"].as_str().unwrap_or(""));
            for m in b["markets"].as_array().into_iter().flatten() {
                let market: &'static str = match m["key"].as_str() { Some("h2h") => "moneyline", Some("spreads") => "spread", Some("totals") => "total", _ => continue };
                let outs = m["outcomes"].as_array().cloned().unwrap_or_default();
                if outs.len() != 2 { continue; } // three-way (draw) markets are not two-way
                let mut sides = Vec::new();
                for o in &outs {
                    let name = o["name"].as_str().unwrap_or("");
                    let side: &'static str = match market {
                        "total" => if name.eq_ignore_ascii_case("over") { "over" } else if name.eq_ignore_ascii_case("under") { "under" } else { continue },
                        _ => if norm_name(name) == norm_name(hn) { "home" } else if norm_name(name) == norm_name(an) { "away" } else { continue },
                    };
                    let Some(price) = american(&o["price"]) else { continue };
                    let line = if market == "moneyline" { None } else { o["point"].as_f64() };
                    if market != "moneyline" && line.is_none() { continue; }
                    let link = first([o["link"].as_str(), m["link"].as_str(), b["link"].as_str()]).map(str::to_string);
                    sides.push(Line { source: "oddsapi", game_id: g.game_id.clone(), event_ref: ev["id"].as_str().unwrap_or("").to_string(), starts_at: start,
                        kind: "game", market, subject: "game".into(), subject_name: None, provider_subject: None, side, line, price, book: book.clone(), link,
                        open_line: None, open_price: None });
                }
                if sides.len() == 2 && sides[0].side != sides[1].side { out.extend(sides); }
            }
        }
    }
    out
}

async fn oddsapi(state: &State, sport: &str, games: &[Upcoming], names: &HashMap<String, (String, String)>) -> Result<(Vec<Line>, Value), String> {
    let key_sport = oddsapi_sport(sport).ok_or("no Odds API sport")?;
    let keys = oddsapi_keys();
    if keys.is_empty() { return Err("ODDS_API_KEYS not set".into()); }
    let base = std::env::var("ODDS_API_BASE_URL").unwrap_or_else(|_| "https://api.the-odds-api.com/v4".into());
    let url = format!("{base}/sports/{key_sport}/odds");
    // named books instead of a region: up to 10 cost the same as one region, and
    // Pinnacle (the sharp reference, sharp.rs) is not in the US region
    let books = std::env::var("ODDS_API_BOOKMAKERS").ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| ODDSAPI_BOOKS.to_string());
    for key in &keys {
        let r = state.http.get(&url).query(&[("apiKey", key.as_str()), ("bookmakers", books.as_str()), ("markets", "h2h,spreads,totals"), ("oddsFormat", "american"), ("includeLinks", "true")])
            .timeout(std::time::Duration::from_secs(25)).send().await.map_err(|e| format!("Odds API: {}", e.without_url()))?;
        let status = r.status();
        let cost = r.headers().get("x-requests-last").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<i32>().ok()).unwrap_or(3);
        let remaining = r.headers().get("x-requests-remaining").and_then(|v| v.to_str().ok()).map(str::to_string);
        if status.as_u16() == 401 || status.as_u16() == 429 { continue; }
        if !status.is_success() { return Err(format!("Odds API returned {status}")); }
        spend(&state.db, "oddsapi", cost).await;
        let events: Vec<Value> = r.json().await.map_err(|e| format!("Odds API body: {}", e.without_url()))?;
        let lines = parse_oddsapi(sport, &events, games, names);
        return Ok((lines, json!({ "events": events.len(), "credits_used": cost, "credits_remaining": remaining })));
    }
    Err("no usable Odds API key (invalid, out of quota or rate limited)".into())
}

// ---------------------------------------------------------------------------
// SportsGameOdds (multi-book game lines + props)
// ---------------------------------------------------------------------------

/// Our simulator stat -> SportsGameOdds statIDs to look for (combos are '+'
/// joins; the order is not documented, so both are tried).
pub(crate) fn sgo_stat_ids(sport: &str, stat: &str) -> Vec<&'static str> {
    match (sport, stat) {
        ("nba", "pr") => vec!["points+rebounds"],
        ("nba", "pa") => vec!["points+assists"],
        ("nba", "ra") => vec!["rebounds+assists", "assists+rebounds"],
        ("nba", "stocks") => vec!["blocks+steals", "steals+blocks"],
        ("nba", "fgm") => vec!["fieldGoalsMade"],
        ("nba", "fga") => vec!["fieldGoalsAttempted"],
        ("nba", "fg3a") => vec!["threePointersAttempted"],
        ("nba", "ftm") => vec!["freeThrowsMade"],
        ("nba", "fta") => vec!["freeThrowsAttempted"],
        ("nba", "oreb") => vec!["offensiveRebounds", "rebounds_offensive"],
        ("nba", "dreb") => vec!["defensiveRebounds", "rebounds_defensive"],
        ("nba", "double_double") => vec!["doubleDouble"],
        ("nba", "triple_double") => vec!["tripleDouble"],
        ("nfl", "pass_td") => vec!["passing_touchdowns"],
        ("nfl", "pass_att") => vec!["passing_attempts"],
        ("nfl", "completions") => vec!["passing_completions"],
        ("nfl", "interceptions") => vec!["passing_interceptions"],
        ("nfl", "carries") => vec!["rushing_attempts"],
        ("nfl", "targets") => vec!["receiving_targets"],
        ("nfl", "rush_td") => vec!["rushing_touchdowns"],
        ("nfl", "rec_td") => vec!["receiving_touchdowns"],
        ("nfl", "rush_rec_yds") => vec!["receiving_yards+rushing_yards", "rushing_yards+receiving_yards"],
        ("nfl", "pass_rush_yds") => vec!["passing_yards+rushing_yards", "rushing_yards+passing_yards"],
        ("nfl", "tds") => vec!["touchdowns"],
        ("nfl", "kick_pts") => vec!["kicking_totalPoints"],
        ("nfl", "fgm") => vec!["fieldGoals_made"],
        _ => sgo_stat(sport, stat).into_iter().collect(),
    }
}

/// Yes/no player markets: settle as over/under 0.5 (yes = over).
pub(crate) fn yes_no_market(stat: &str) -> bool { matches!(stat, "double_double" | "triple_double" | "tds") }

/// Our simulator stat -> SportsGameOdds statID.
pub(crate) fn sgo_stat(sport: &str, stat: &str) -> Option<&'static str> {
    Some(match (sport, stat) {
        ("nba", "pts") => "points", ("nba", "reb") => "rebounds", ("nba", "ast") => "assists", ("nba", "fg3m") => "threePointersMade",
        ("nba", "stl") => "steals", ("nba", "blk") => "blocks", ("nba", "tov") => "turnovers", ("nba", "pra") => "points+rebounds+assists",
        ("nhl", "sog") => "shots_onGoal", ("nhl", "points") => "points", ("nhl", "assists") => "assists", ("nhl", "goals") => "goals",
        ("nhl", "saves") => "goalie_saves", ("nhl", "blocks") => "blocks",
        ("nfl", "pass_yds") => "passing_yards", ("nfl", "rush_yds") => "rushing_yards", ("nfl", "rec_yds") => "receiving_yards", ("nfl", "receptions") => "receiving_receptions",
        _ => return None,
    })
}

const SGO_PROP_STATS: [(&str, &[&str]); 3] = [
    ("nba", &["pts", "reb", "ast", "fg3m", "stl", "blk", "tov", "pra", "pr", "pa", "ra", "stocks", "fgm", "fga", "fg3a", "ftm", "fta",
              "oreb", "dreb", "double_double", "triple_double"]),
    ("nhl", &["sog", "points", "assists", "goals", "saves", "blocks"]),
    ("nfl", &["pass_yds", "rush_yds", "rec_yds", "receptions", "pass_td", "pass_att", "completions", "interceptions", "carries",
              "targets", "rush_td", "rec_td", "rush_rec_yds", "pass_rush_yds", "tds", "kick_pts", "fgm"]),
];

/// Every game market name (static, for `Line::market`).
const GAME_MARKET_NAMES: [&str; 10] = crate::segments::GAME_MARKETS;

/// SportsGameOdds periodID of the first segment.
fn sgo_segment(sport: &str) -> &'static str { if sport == "nhl" { "1p" } else { "1h" } }

pub(crate) fn parse_sgo(sport: &str, events: &[Value], games: &[Upcoming]) -> Vec<Line> {
    let stats: &'static [&'static str] = SGO_PROP_STATS.iter().find(|(s, _)| *s == sport).map(|(_, v)| *v).unwrap_or(&[]);
    let mut out = Vec::new();
    for ev in events {
        let short = |s: &str| ev["teams"][s]["names"]["short"].as_str().unwrap_or("").to_string();
        let start = ev["status"]["startsAt"].as_str().and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|d| d.with_timezone(&Utc));
        let Some(g) = find_game(sport, games, None, &short("home"), &short("away"), start) else { continue };
        let Some(odds) = ev["odds"].as_object() else { continue };
        let eid = ev["eventID"].as_str().unwrap_or("").to_string();
        // (market, side, oddID, partner oddID, line field, subject, name, provider id)
        let mut sels: Vec<(&'static str, &'static str, String, String, &'static str, String, Option<String>, Option<String>)> = Vec::new();
        // full game and the first segment (SGO period "1h"; "1p" for hockey; MLB's "1h" is the first five innings)
        for (period, sfx) in [("game", ""), (sgo_segment(sport), "_1h")] {
            let m = |base: &str| -> &'static str { GAME_MARKET_NAMES.iter().find(|n| **n == format!("{base}{sfx}")).copied().unwrap_or("") };
            sels.push((m("moneyline"), "home", format!("points-home-{period}-ml-home"), format!("points-away-{period}-ml-away"), "", "game".into(), None, None));
            sels.push((m("spread"), "home", format!("points-home-{period}-sp-home"), format!("points-away-{period}-sp-away"), "spread", "game".into(), None, None));
            sels.push((m("total"), "over", format!("points-all-{period}-ou-over"), format!("points-all-{period}-ou-under"), "overUnder", "game".into(), None, None));
            sels.push((m("team_total_home"), "over", format!("points-home-{period}-ou-over"), format!("points-home-{period}-ou-under"), "overUnder", "game".into(), None, None));
            sels.push((m("team_total_away"), "over", format!("points-away-{period}-ou-over"), format!("points-away-{period}-ou-under"), "overUnder", "game".into(), None, None));
        }
        if let Some(players) = ev["players"].as_object() {
            for (pid, p) in players {
                let name = p["name"].as_str().unwrap_or("").to_string();
                let Some(ours) = g.roster.resolve(None, &name) else { continue };
                for &st in stats {
                    for sid in sgo_stat_ids(sport, st) {
                        sels.push((st, "over", format!("{sid}-{pid}-game-ou-over"), format!("{sid}-{pid}-game-ou-under"), "overUnder", ours.clone(), Some(name.clone()), Some(pid.clone())));
                        // yes/no markets (double-double, triple-double, anytime TD): yes = over 0.5
                        if yes_no_market(st) {
                            sels.push((st, "over", format!("{sid}-{pid}-game-yn-yes"), format!("{sid}-{pid}-game-yn-no"), "yesno", ours.clone(), Some(name.clone()), Some(pid.clone())));
                        }
                    }
                }
            }
        }
        let empty = serde_json::Map::new();
        for (market, side, a_id, b_id, field, subject, name, prov) in sels {
            if market.is_empty() { continue; }
            // yes/no props are often posted yes-only: keep those (a one-sided price, see picks)
            let yn = field == "yesno";
            let Some(a) = odds.get(&a_id) else { continue };
            let b = odds.get(&b_id);
            if b.is_none() && !yn { continue; }
            let Some(ab) = a["byBookmaker"].as_object() else { continue };
            let bb = b.and_then(|b| b["byBookmaker"].as_object()).unwrap_or(&empty);
            if yn {
                let kind = if subject == "game" { "game" } else { "prop" };
                for (book, av) in ab {
                    if av["available"] == false { continue; }
                    let Some(ap) = american(&av["odds"]) else { continue };
                    let link = first([av["deeplink"].as_str(), av["deepLink"].as_str(), av["link"].as_str()]).map(str::to_string);
                    let mk = |s: &'static str, price: i32, link: Option<String>| Line {
                        source: "sgo", game_id: g.game_id.clone(), event_ref: eid.clone(), starts_at: start, kind, market, subject: subject.clone(),
                        subject_name: name.clone(), provider_subject: prov.clone(), side: s, line: Some(0.5), price, book: book_key(book), link, open_line: None, open_price: None,
                    };
                    out.push(mk("over", ap, link));
                    if let Some(bv) = bb.get(book) {
                        if bv["available"] != false { if let Some(bp) = american(&bv["odds"]) { out.push(mk("under", bp, None)); } }
                    }
                }
                continue;
            }
            let other: &'static str = match side { "home" => "away", _ => "under" };
            let kind = if subject == "game" { "game" } else { "prop" };
            for (book, av) in ab {
                let Some(bv) = bb.get(book) else { continue };
                if av["available"] == false || bv["available"] == false { continue; }
                let (Some(ap), Some(bp)) = (american(&av["odds"]), american(&bv["odds"])) else { continue };
                let (al, bl) = if field.is_empty() { (None, None) } else { (num(&av[field]), num(&bv[field])) };
                if !field.is_empty() && (al.is_none() || bl.is_none()) { continue; }
                // the pair must be the same market: spread sides mirror, totals match
                match crate::segments::base_market(market) { "spread" => if (al.unwrap() + bl.unwrap()).abs() > 1e-9 { continue }, "moneyline" => {}, _ => if al != bl { continue } }
                let link = |v: &Value| first([v["deeplink"].as_str(), v["deepLink"].as_str(), v["link"].as_str()]).map(str::to_string);
                let mk = |s: &'static str, line: Option<f64>, price: i32, link: Option<String>| Line {
                    source: "sgo", game_id: g.game_id.clone(), event_ref: eid.clone(), starts_at: start, kind, market, subject: subject.clone(),
                    subject_name: name.clone(), provider_subject: prov.clone(), side: s, line, price, book: book_key(book), link, open_line: None, open_price: None,
                };
                out.push(mk(side, al, ap, link(av)));
                out.push(mk(other, bl, bp, link(bv)));
            }
        }
    }
    out
}

async fn sgo(state: &State, sport: &str, games: &[Upcoming]) -> Result<(Vec<Line>, Value), String> {
    let lg = crate::market::league(sport).ok_or("no SGO league")?;
    let keys = crate::market::keys_from_env();
    if keys.is_empty() { return Err("SPORTSGAMEODDS_API_KEY not set".into()); }
    // only the window we price (each returned event costs one of the month's entities)
    let before = games.iter().map(|g| g.start).max().unwrap_or_else(Utc::now) + Duration::hours(2);
    let url = format!("https://api.sportsgameodds.com/v2/events?leagueID={lg}&oddsAvailable=true&limit={}&startsAfter={}&startsBefore={}",
        (games.len() + 5).min(50), (Utc::now() - Duration::hours(1)).format("%Y-%m-%dT%H:%M:%SZ"), before.format("%Y-%m-%dT%H:%M:%SZ"));
    for (i, key) in keys.iter().enumerate() {
        let r = state.http.get(&url).header("x-api-key", key).timeout(std::time::Duration::from_secs(30)).send().await.map_err(|e| format!("SGO: {}", e.without_url()))?;
        if r.status().as_u16() == 429 { continue; } // this key's monthly entities or per-minute requests are used up: try the next
        if !r.status().is_success() { return Err(format!("SGO returned {}", r.status())); }
        let v: Value = r.json().await.map_err(|e| format!("SGO body: {}", e.without_url()))?;
        let events = v["data"].as_array().cloned().unwrap_or_default();
        spend(&state.db, "sgo", events.len().max(1) as i32).await;
        let lines = parse_sgo(sport, &events, games);
        return Ok((lines, json!({ "events": events.len(), "key": i + 1 })));
    }
    // every key rate limited: stop for today
    let budget = setting(&state.db, "sgo_daily_entities", 60.0).await as i32;
    spend(&state.db, "sgo", budget).await;
    Err(format!("SGO rate limit on all {} keys (monthly entity quota or per-minute limit); paused until tomorrow", keys.len()))
}

// ---------------------------------------------------------------------------
// Store + refresh
// ---------------------------------------------------------------------------

pub(crate) async fn store(db: &tokio_postgres::Client, sport: &str, lines: &[Line]) -> Result<u64, String> {
    if lines.is_empty() { return Ok(0); }
    let rows: Vec<Value> = lines.iter().map(|l| json!({
        "source": l.source, "game_id": l.game_id, "event_ref": l.event_ref, "starts_at": l.starts_at, "kind": l.kind, "market": l.market,
        "subject": l.subject, "subject_name": l.subject_name, "provider_subject": l.provider_subject, "side": l.side, "line": l.line,
        "price": l.price, "book": l.book, "link": l.link, "open_line": l.open_line, "open_price": l.open_price,
    })).collect();
    db.execute(
        "INSERT INTO market_lines (source, sport, game_id, event_ref, starts_at, kind, market, subject, subject_name, provider_subject, side, line, price, book, link, open_line, open_price) \
         SELECT x.source, $1, x.game_id, x.event_ref, x.starts_at, x.kind, x.market, x.subject, x.subject_name, x.provider_subject, x.side, x.line, x.price, x.book, x.link, x.open_line, x.open_price \
         FROM jsonb_to_recordset($2) AS x(source text, game_id text, event_ref text, starts_at timestamptz, kind text, market text, subject text, subject_name text, \
              provider_subject text, side text, line float8, price int, book text, link text, open_line float8, open_price int)",
        &[&sport, &Json(Value::Array(rows))]).await.map_err(|e| dberr(&e))
}

/// Fetch every provider that is due and within budget, for one sport.
pub(crate) async fn refresh(state: &State, sport: &str, force: bool) -> Value {
    let db = &state.db;
    let games = match upcoming(db, sport).await { Ok(g) => g, Err(e) => return json!({ "ok": false, "error": e }) };
    if games.is_empty() { return json!({ "ok": true, "games": 0, "note": "no frozen games in the next 40 hours" }); }
    let mut report = serde_json::Map::new();
    let mut ok = true;
    let mut names = HashMap::new();
    // ESPN: every call
    match espn(state, sport, &games).await {
        Ok((lines, n, rep)) => {
            names = n;
            let stored = store(db, sport, &lines).await;
            report.insert("espn".into(), json!({ "report": rep, "stored": stored.as_ref().ok(), "error": stored.err() }));
        }
        Err(e) => { ok = false; report.insert("espn".into(), json!({ "error": e })); }
    }
    // The Odds API: at most every 6 h per sport, within the daily credit budget
    let due = |t: Option<DateTime<Utc>>, h: i64| force || t.map(|t| Utc::now() - t >= Duration::hours(h)).unwrap_or(true);
    let oa_budget = setting(db, "oddsapi_daily_credits", 9.0).await as i32;
    if oddsapi_keys().is_empty() {
        report.insert("oddsapi".into(), json!({ "skipped": "ODDS_API_KEYS not set" }));
    } else if !due(last_fetch(db, "oddsapi", sport).await, 6) {
        report.insert("oddsapi".into(), json!({ "skipped": "fetched within 6 h" }));
    } else if spent_today(db, "oddsapi").await + 3 > oa_budget {
        report.insert("oddsapi".into(), json!({ "skipped": format!("daily budget of {oa_budget} credits used") }));
    } else if names.is_empty() {
        report.insert("oddsapi".into(), json!({ "skipped": "no ESPN team names to match events" }));
    } else {
        match oddsapi(state, sport, &games, &names).await {
            Ok((lines, rep)) => { let s = store(db, sport, &lines).await; report.insert("oddsapi".into(), json!({ "report": rep, "stored": s.as_ref().ok(), "error": s.err() })); }
            Err(e) => { report.insert("oddsapi".into(), json!({ "error": e })); }
        }
    }
    // SportsGameOdds: at most every 4 h per sport, within the daily entity budget
    let sgo_budget = setting(db, "sgo_daily_entities", 60.0).await as i32;
    if crate::market::keys_from_env().is_empty() {
        report.insert("sgo".into(), json!({ "skipped": "SPORTSGAMEODDS_API_KEY not set" }));
    } else if !due(last_fetch(db, "sgo", sport).await, 4) {
        report.insert("sgo".into(), json!({ "skipped": "fetched within 4 h" }));
    } else if spent_today(db, "sgo").await + games.len() as i32 > sgo_budget {
        report.insert("sgo".into(), json!({ "skipped": format!("daily budget of {sgo_budget} entities used") }));
    } else {
        match sgo(state, sport, &games).await {
            Ok((lines, rep)) => { let s = store(db, sport, &lines).await; report.insert("sgo".into(), json!({ "report": rep, "stored": s.as_ref().ok(), "error": s.err() })); }
            Err(e) => { report.insert("sgo".into(), json!({ "error": e })); }
        }
    }
    let _ = db.execute("DELETE FROM market_lines WHERE observed_at < now() - interval '21 days'", &[]).await;
    let mut seen = HashSet::new();
    let matched = games.iter().filter(|g| seen.insert(g.game_id.clone())).count();
    json!({ "ok": ok, "games": matched, "providers": report })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn up(id: &str, home: &str, away: &str, payload: Value) -> Upcoming {
        Upcoming { game_id: id.into(), start: Utc::now() + Duration::hours(3), home: home.into(), away: away.into(), roster: Roster::from_payload(&payload) }
    }

    #[test]
    fn name_keys_match_across_providers() {
        assert_eq!(name_key("Leon Draisaitl"), "ldraisaitl");
        assert_eq!(name_key("L. Draisaitl"), "ldraisaitl");
        assert_eq!(name_key("J.T. Miller"), "jmiller");
        assert_eq!(name_key("Marvin Harrison Jr."), "mharrison");
        assert_eq!(name_key("Nikola Jokić"), "njokic");
    }

    #[test]
    fn roster_resolves_ids_and_refuses_ambiguous_names() {
        let p = json!({ "home": "DET", "away": "VAN", "players": {
            "home": [{ "id": 1, "name": "A. Smith" }, { "id": 2, "name": "B. Jones" }],
            "away": [{ "id": 3, "name": "A. Smith" }, { "id": "4", "name": "C. Lee" }] },
            "goalies": [{ "id": 9, "name": "J. Gibson", "team": "DET" }] });
        let r = Roster::from_payload(&p);
        assert_eq!(r.resolve(None, "Brandon Jones"), Some("2".into()));
        assert_eq!(r.resolve(None, "Aaron Smith"), None, "two A. Smiths: ambiguous");
        assert_eq!(r.resolve(Some("4"), "whoever"), Some("4".into()));
        assert_eq!(r.resolve(None, "John Gibson"), Some("9".into()));
    }

    #[test]
    fn american_and_lines_parse() {
        assert_eq!(american(&json!("+105")), Some(105));
        assert_eq!(american(&json!("-110")), Some(-110));
        assert_eq!(american(&json!("EVEN")), Some(100));
        assert_eq!(american(&json!(-118.0)), Some(-118));
        assert_eq!(american(&json!("1.5")), None, "decimal odds are not American");
        assert_eq!(num(&json!("+1.5")), Some(1.5));
        assert_eq!(num(&json!("PK")), Some(0.0));
    }

    #[test]
    fn espn_game_item_gives_two_sided_markets() {
        let it = json!({
            "provider": { "id": "100", "name": "DraftKings" }, "spread": -3.0, "overUnder": 50.5,
            "open": { "over": { "american": "-110" }, "under": { "american": "-110" }, "total": { "american": "47.5" } },
            "current": { "over": { "american": "-115" }, "under": { "american": "-105" }, "total": { "american": "50.5" } },
            "homeTeamOdds": { "moneyLine": -155, "open": { "pointSpread": { "american": "-1.5" }, "spread": { "american": "-105" }, "moneyLine": { "american": "-120" } },
                              "current": { "pointSpread": { "american": "-3" }, "spread": { "american": "-102" }, "moneyLine": { "american": "-155" } } },
            "awayTeamOdds": { "moneyLine": 130, "open": { "pointSpread": { "american": "+1.5" }, "spread": { "american": "-115" }, "moneyLine": { "american": "+100" } },
                              "current": { "pointSpread": { "american": "+3" }, "spread": { "american": "-118" }, "moneyLine": { "american": "+130" } } }
        });
        let g = up("g1", "CHI", "NE", json!({}));
        let ls = espn_game_lines(&it, &g, "e1", None);
        assert_eq!(ls.len(), 6);
        let f = |m: &str, s: &str| ls.iter().find(|l| l.market == m && l.side == s).unwrap().clone();
        assert_eq!(f("spread", "home").line, Some(-3.0));
        assert_eq!(f("spread", "away").line, Some(3.0));
        assert_eq!(f("spread", "home").open_line, Some(-1.5));
        assert_eq!(f("total", "under").price, -105);
        assert_eq!(f("total", "over").open_line, Some(47.5));
        assert_eq!(f("moneyline", "away").price, 130);
        assert_eq!(f("moneyline", "home").book, "draftkings");
    }

    #[test]
    fn espn_props_pair_over_then_under_and_skip_one_sided() {
        let item = |ty: &str, aid: &str, price: &str, line: f64| json!({ "athlete": { "$ref": format!("http://x/athletes/{aid}?lang=en") }, "type": { "name": ty },
            "odds": { "american": { "value": price, "open": price }, "total": { "value": line.to_string() } }, "current": { "target": { "value": line } }, "open": { "target": { "value": line } } });
        let items = vec![item("Total Points", "7", "+110", 20.5), item("Total Points", "7", "-140", 20.5),
                         item("Points Milestones", "7", "+300", 25.0), item("Total Steals", "8", "-120", 1.5),
                         item("Total Dunks", "8", "-110", 2.5), item("Total Dunks", "8", "-110", 2.5)];
        let (pairs, unmapped) = espn_prop_pairs("nba", &items);
        assert_eq!(pairs.len(), 1, "steals has one entry, milestones are one-sided");
        assert_eq!(pairs[0].2, "pts");
        assert_eq!(american(&pairs[0].3[0]["odds"]["american"]["value"]), Some(110));
        assert_eq!(unmapped.get("Total Dunks"), Some(&2));
    }

    #[test]
    fn oddsapi_matches_by_full_name_and_skips_three_way() {
        let g = up("g1", "DET", "BOS", json!({}));
        let names: HashMap<String, (String, String)> = [("g1".to_string(), ("Detroit Pistons".to_string(), "Boston Celtics".to_string()))].into();
        let ev = json!([{ "id": "x", "home_team": "Detroit Pistons", "away_team": "Boston Celtics", "bookmakers": [{ "key": "fanduel", "markets": [
            { "key": "h2h", "outcomes": [{ "name": "Boston Celtics", "price": -150 }, { "name": "Detroit Pistons", "price": 130 }] },
            { "key": "spreads", "outcomes": [{ "name": "Boston Celtics", "price": -110, "point": -3.5 }, { "name": "Detroit Pistons", "price": -110, "point": 3.5 }] },
            { "key": "totals", "outcomes": [{ "name": "Over", "price": -105, "point": 221.5, "link": "https://fd/x" }, { "name": "Under", "price": -115, "point": 221.5 }] } ] },
            { "key": "book3", "markets": [{ "key": "h2h", "outcomes": [{ "name": "Boston Celtics", "price": -150 }, { "name": "Detroit Pistons", "price": 130 }, { "name": "Draw", "price": 900 }] }] }] }]);
        let ls = parse_oddsapi("nba", ev.as_array().unwrap(), &[g], &names);
        assert_eq!(ls.len(), 6);
        assert!(ls.iter().any(|l| l.market == "spread" && l.side == "home" && l.line == Some(3.5)));
        assert_eq!(ls.iter().find(|l| l.side == "over").unwrap().link.as_deref(), Some("https://fd/x"));
    }

    #[test]
    fn sgo_keeps_yes_only_double_doubles_and_combo_props() {
        let g = up("g1", "DET", "BOS", json!({ "home": "DET", "away": "BOS", "players": { "home": [{ "id": "55", "name": "Cade Cunningham" }], "away": [] } }));
        let ev = json!([{ "eventID": "E", "status": { "startsAt": (Utc::now() + Duration::hours(3)).to_rfc3339() },
            "teams": { "home": { "names": { "short": "DET" } }, "away": { "names": { "short": "BOS" } } },
            "players": { "CADE_CUNNINGHAM_1_NBA": { "name": "Cade Cunningham" } },
            "odds": {
                "doubleDouble-CADE_CUNNINGHAM_1_NBA-game-yn-yes": { "byBookmaker": { "fanduel": { "odds": "+120" } } },
                "points+rebounds-CADE_CUNNINGHAM_1_NBA-game-ou-over": { "byBookmaker": { "fanduel": { "odds": "-110", "overUnder": "32.5" } } },
                "points+rebounds-CADE_CUNNINGHAM_1_NBA-game-ou-under": { "byBookmaker": { "fanduel": { "odds": "-110", "overUnder": "32.5" } } } } }]);
        let ls = parse_sgo("nba", ev.as_array().unwrap(), &[g]);
        let dd: Vec<_> = ls.iter().filter(|l| l.market == "double_double").collect();
        assert_eq!(dd.len(), 1, "yes-only: one side, line 0.5");
        assert_eq!((dd[0].side, dd[0].line, dd[0].price), ("over", Some(0.5), 120));
        assert_eq!(ls.iter().filter(|l| l.market == "pr").count(), 2);
    }

    #[test]
    fn sgo_parses_team_totals_and_the_first_segment() {
        let g = up("g1", "DET", "BOS", json!({}));
        let ev = json!([{ "eventID": "E", "status": { "startsAt": (Utc::now() + Duration::hours(3)).to_rfc3339() },
            "teams": { "home": { "names": { "short": "DET" } }, "away": { "names": { "short": "BOS" } } },
            "odds": {
                "points-home-1h-ml-home": { "byBookmaker": { "fanduel": { "odds": "-120" } } },
                "points-away-1h-ml-away": { "byBookmaker": { "fanduel": { "odds": "+100" } } },
                "points-home-1h-sp-home": { "byBookmaker": { "fanduel": { "odds": "-110", "spread": "-1.5" } } },
                "points-away-1h-sp-away": { "byBookmaker": { "fanduel": { "odds": "-110", "spread": "+1.5" } } },
                "points-all-1h-ou-over": { "byBookmaker": { "fanduel": { "odds": "-110", "overUnder": "112.5" } } },
                "points-all-1h-ou-under": { "byBookmaker": { "fanduel": { "odds": "-110", "overUnder": "112.5" } } },
                "points-away-game-ou-over": { "byBookmaker": { "draftkings": { "odds": "-115", "overUnder": "108.5" } } },
                "points-away-game-ou-under": { "byBookmaker": { "draftkings": { "odds": "-105", "overUnder": "108.5" } } } } }]);
        let ls = parse_sgo("nba", ev.as_array().unwrap(), &[g]);
        let has = |m: &str, s: &str| ls.iter().any(|l| l.market == m && l.side == s);
        assert!(has("moneyline_1h", "home") && has("moneyline_1h", "away"));
        assert!(has("spread_1h", "away") && has("total_1h", "under"));
        assert!(has("team_total_away", "over") && has("team_total_away", "under"));
        assert_eq!(ls.len(), 8, "{ls:?}");
        // hockey's first segment is the 1st period
        let g = up("g2", "DET", "BOS", json!({}));
        let ev = json!([{ "eventID": "E", "status": { "startsAt": (Utc::now() + Duration::hours(3)).to_rfc3339() },
            "teams": { "home": { "names": { "short": "DET" } }, "away": { "names": { "short": "BOS" } } },
            "odds": { "points-all-1p-ou-over": { "byBookmaker": { "fanduel": { "odds": "+120", "overUnder": "1.5" } } },
                      "points-all-1p-ou-under": { "byBookmaker": { "fanduel": { "odds": "-150", "overUnder": "1.5" } } } } }]);
        let ls = parse_sgo("nhl", ev.as_array().unwrap(), &[g]);
        assert_eq!(ls.iter().filter(|l| l.market == "total_1h").count(), 2);
    }

    #[test]
    fn sgo_pairs_books_and_resolves_players() {
        let g = up("g1", "DET", "BOS", json!({ "home": "DET", "away": "BOS", "players": { "home": [{ "id": "55", "name": "Cade Cunningham" }], "away": [] } }));
        let ev = json!([{ "eventID": "E", "status": { "startsAt": (Utc::now() + Duration::hours(3)).to_rfc3339() },
            "teams": { "home": { "names": { "short": "DET" } }, "away": { "names": { "short": "BOS" } } },
            "players": { "CADE_CUNNINGHAM_1_NBA": { "name": "Cade Cunningham" } },
            "odds": {
                "points-CADE_CUNNINGHAM_1_NBA-game-ou-over": { "byBookmaker": { "fanduel": { "odds": "-115", "overUnder": "26.5", "deeplink": "https://fd/p" }, "draftkings": { "odds": "-110", "overUnder": "25.5" } } },
                "points-CADE_CUNNINGHAM_1_NBA-game-ou-under": { "byBookmaker": { "fanduel": { "odds": "-105", "overUnder": "26.5" }, "draftkings": { "odds": "-110", "overUnder": "26.5" } } },
                "points-home-game-sp-home": { "byBookmaker": { "pinnacle": { "odds": "-108", "spread": "-2.5" } } },
                "points-away-game-sp-away": { "byBookmaker": { "pinnacle": { "odds": "-102", "spread": "+2.5" } } } } }]);
        let ls = parse_sgo("nba", ev.as_array().unwrap(), &[g]);
        // fanduel prop pair + pinnacle spread pair; draftkings lines disagree (25.5 vs 26.5) and are dropped
        assert_eq!(ls.len(), 4, "{ls:?}");
        let over = ls.iter().find(|l| l.market == "pts" && l.side == "over").unwrap();
        assert_eq!((over.subject.as_str(), over.line, over.link.as_deref()), ("55", Some(26.5), Some("https://fd/p")));
        assert!(ls.iter().any(|l| l.market == "spread" && l.side == "away" && l.line == Some(2.5)));
    }
}
