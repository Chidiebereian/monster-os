//! Game hub: the Games page (schedule by week/day with lines + model) and the
//! per-game matchup page, for every sport.
//!
//!   GET /hub/{sport}/schedule?season=&week=   (NFL)  | ?date=YYYY-MM-DD (others)
//!   GET /hub/{sport}/game/{espn_event_id}
//!
//! Schedule, records, venue, weather, injuries, last-five, ATS records and
//! standings come from ESPN's public site API (cached a few minutes). Our side:
//! the champion engine's frozen prediction (`prediction_ledger`) for model score /
//! line / total / win %, prop projections with P(over) at each book line, recorded
//! lines from every book (`market_lines`), AI picks (`ai_picks`), and team rankings
//! from our warehouses (NFL: nfl_hub.rs; NBA/NHL/MLB: below).

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use actix_web::{web, HttpResponse, Responder};
use chrono::{DateTime, Datelike, Duration as CDuration, NaiveDate, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_postgres::types::Json;

use crate::hitrates::canon;
use crate::State;

fn dberr(e: &tokio_postgres::Error) -> String {
    e.as_db_error().map(|d| d.message().to_string()).unwrap_or_else(|| e.to_string())
}
fn r1(x: f64) -> f64 { (x * 10.0).round() / 10.0 }
fn r3(x: f64) -> f64 { (x * 1000.0).round() / 1000.0 }
fn err(e: impl std::fmt::Display) -> HttpResponse { HttpResponse::InternalServerError().json(json!({ "error": e.to_string() })) }

pub const SPORTS: &[&str] = &["nfl", "nba", "nhl", "mlb"];

fn espn_path(sport: &str) -> Option<&'static str> {
    match sport { "nfl" => Some("football/nfl"), "nba" => Some("basketball/nba"), "nhl" => Some("hockey/nhl"), "mlb" => Some("baseball/mlb"), _ => None }
}

/// Local (US Eastern) calendar date of a UTC kickoff.
fn et_date(t: DateTime<Utc>) -> NaiveDate {
    let dst = (3..=10).contains(&t.month()) || (t.month() == 11 && t.day() < 2);
    (t - CDuration::hours(if dst { 4 } else { 5 })).date_naive()
}

// ---------------------------------------------------------------------------
// ESPN (cached)
// ---------------------------------------------------------------------------

fn espn_cache() -> &'static Mutex<HashMap<String, (Instant, Value)>> {
    static C: OnceLock<Mutex<HashMap<String, (Instant, Value)>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

async fn espn(state: &State, url: &str, ttl: Duration) -> Result<Value, String> {
    if let Some((t, v)) = espn_cache().lock().unwrap().get(url) {
        if t.elapsed() < ttl { return Ok(v.clone()); }
    }
    let r = state.http.get(url).timeout(Duration::from_secs(20)).send().await.map_err(|e| format!("espn: {}", e.without_url()))?;
    if !r.status().is_success() { return Err(format!("espn returned {}", r.status())); }
    let v: Value = r.json().await.map_err(|e| format!("espn body: {}", e.without_url()))?;
    let mut c = espn_cache().lock().unwrap();
    if c.len() > 400 { c.retain(|_, (t, _)| t.elapsed() < Duration::from_secs(600)); }
    c.insert(url.to_string(), (Instant::now(), v.clone()));
    Ok(v)
}

fn s(v: &Value) -> Option<String> { v.as_str().map(|x| x.to_string()).filter(|x| !x.is_empty()) }
fn num(v: &Value) -> Option<f64> { v.as_f64().or_else(|| v.as_str().and_then(|x| x.trim().trim_start_matches('+').parse().ok())) }

fn team_json(sport: &str, c: &Value) -> Value {
    let t = &c["team"];
    let abbr = s(&t["abbreviation"]).unwrap_or_default();
    let record = c["records"].as_array().and_then(|r| r.iter().find(|x| x["type"] == "total" || x["name"] == "overall"))
        .and_then(|x| s(&x["summary"])).or_else(|| c["record"].as_array().and_then(|r| r.first()).and_then(|x| s(&x["displayValue"])));
    json!({
        "id": s(&t["id"]), "abbr": canon(sport, &abbr), "espn_abbr": abbr, "name": s(&t["displayName"]), "short": s(&t["shortDisplayName"]),
        "logo": s(&t["logo"]).or_else(|| t["logos"].as_array().and_then(|l| l.first()).and_then(|l| s(&l["href"]))),
        "color": s(&t["color"]), "record": record,
        "score": num(&c["score"]).or_else(|| num(&c["score"]["value"])).or_else(|| {
            // the summary header drops `score` mid-game: add up the periods
            let ls: Vec<f64> = c["linescores"].as_array()?.iter().filter_map(|l| num(&l["displayValue"]).or_else(|| num(&l["value"]))).collect();
            if ls.is_empty() { None } else { Some(ls.iter().sum()) }
        }),
        "home_away": s(&c["homeAway"]),
    })
}

fn espn_odds(c: &Value) -> Value {
    let Some(o) = c["odds"].as_array().and_then(|a| a.first()) else { return Value::Null };
    let ml = |side: &str| crate::lines::american(&o[format!("{side}TeamOdds")]["moneyLine"])
        .or_else(|| crate::lines::american(&o["moneyline"][side]["close"]["odds"]))
        .or_else(|| crate::lines::american(&o["moneyline"][side]["open"]["odds"]));
    json!({ "provider": s(&o["provider"]["name"]), "details": s(&o["details"]), "spread_home": num(&o["spread"]), "total": num(&o["overUnder"]),
        "home_ml": ml("home"), "away_ml": ml("away") })
}

/// ESPN times come as "2026-11-01T18:00Z" (no seconds) or full RFC 3339.
fn parse_espn_time(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc))
        .or_else(|| chrono::NaiveDateTime::parse_from_str(s.trim_end_matches('Z'), "%Y-%m-%dT%H:%M").ok().map(|n| n.and_utc()))
}

/// Kickoff times from ESPN for slate games that have none (the NFL games table
/// stores dates only). Games are matched by "AWAY @ HOME" team codes and saved
/// to game_kickoffs: ledger rows are immutable, and the line fetcher only looks
/// at games starting in the next 40 hours, so a NULL start hid them from picks.
pub(crate) async fn fill_kickoffs(state: &State, sport: &str, date: NaiveDate, games: &mut [crate::learning::SlateGame]) {
    if games.iter().all(|g| g.start.is_some()) { return; }
    let Some(path) = espn_path(sport) else { return };
    let url = format!("https://site.api.espn.com/apis/site/v2/sports/{path}/scoreboard?dates={}&limit=100", date.format("%Y%m%d"));
    let sb = match espn(state, &url, Duration::from_secs(600)).await { Ok(v) => v, Err(e) => { eprintln!("kickoffs {sport} {date}: {e}"); return } };
    let mut starts: HashMap<(String, String), DateTime<Utc>> = HashMap::new();
    for e in sb["events"].as_array().into_iter().flatten() {
        let Some(ev) = event_json(sport, e) else { continue };
        let Some(t) = ev["start"].as_str().and_then(parse_espn_time) else { continue };
        if let (Some(a), Some(h)) = (ev["away"]["abbr"].as_str(), ev["home"]["abbr"].as_str()) { starts.insert((a.to_string(), h.to_string()), t); }
    }
    for g in games.iter_mut().filter(|g| g.start.is_none()) {
        let Some((a, h)) = g.label.split_once(" @ ") else { continue };
        let Some(t) = starts.get(&(canon(sport, a.trim()), canon(sport, h.trim()))).copied() else { continue };
        g.start = Some(t);
        if let Err(e) = state.db.execute("INSERT INTO game_kickoffs (sport, game_id, starts_at) VALUES ($1,$2,$3) \
            ON CONFLICT (sport, game_id) DO UPDATE SET starts_at = EXCLUDED.starts_at, updated_at = now()", &[&sport, &g.id, &t]).await {
            eprintln!("kickoffs {sport} {}: {}", g.id, dberr(&e));
        }
    }
}

fn event_json(sport: &str, e: &Value) -> Option<Value> {
    let c = e["competitions"].as_array()?.first()?;
    let comps = c["competitors"].as_array()?;
    let home = comps.iter().find(|x| x["homeAway"] == "home")?;
    let away = comps.iter().find(|x| x["homeAway"] == "away")?;
    let date = s(&e["date"])?;
    let start = parse_espn_time(&date);
    let st = &e["status"]["type"];
    Some(json!({
        "id": s(&e["id"]), "start": start, "local_date": start.map(et_date), "name": s(&e["name"]), "short": s(&e["shortName"]),
        "status": { "state": s(&st["state"]), "detail": s(&st["shortDetail"]).or_else(|| s(&st["detail"])), "completed": st["completed"].as_bool().unwrap_or(false) },
        "venue": { "name": s(&c["venue"]["fullName"]), "city": s(&c["venue"]["address"]["city"]), "state": s(&c["venue"]["address"]["state"]),
                   "indoor": c["venue"]["indoor"].as_bool() },
        "weather": if e["weather"].is_object() { json!({ "text": s(&e["weather"]["displayValue"]), "temp": num(&e["weather"]["temperature"]),
                   "condition": s(&e["weather"]["conditionId"]) }) } else { Value::Null },
        "broadcast": c["broadcasts"].as_array().and_then(|b| b.first()).and_then(|b| b["names"].as_array()).and_then(|n| n.first()).and_then(s),
        "neutral": c["neutralSite"].as_bool().unwrap_or(false),
        "note": c["notes"].as_array().and_then(|n| n.first()).and_then(|n| s(&n["headline"])),
        "home": team_json(sport, home), "away": team_json(sport, away), "espn_odds": espn_odds(c),
    }))
}

// ---------------------------------------------------------------------------
// Our model (prediction ledger) + recorded lines
// ---------------------------------------------------------------------------

fn mean_of(v: &Value) -> Option<f64> {
    if let Some(x) = v.as_f64() { return Some(x); }
    if let Some(m) = v["mean"].as_f64() { return Some(m); }
    let pmf = v["pmf"].as_array()?;
    let off = v["pmf_offset"].as_f64().unwrap_or(0.0);
    Some(pmf.iter().enumerate().map(|(i, p)| (i as f64 + off) * p.as_f64().unwrap_or(0.0)).sum())
}

/// P(value > line), P(value < line) from a stored pmf.
pub fn p_over_under(stat: &Value, line: f64) -> Option<(f64, f64)> {
    if let Some(l) = stat["lines"].as_array().and_then(|ls| ls.iter().find(|l| (l["line"].as_f64().unwrap_or(f64::NAN) - line).abs() < 1e-9)) {
        return Some((l["over"].as_f64()?, l["under"].as_f64()?));
    }
    let pmf = stat["pmf"].as_array()?;
    let off = stat["pmf_offset"].as_f64().unwrap_or(0.0);
    let (mut o, mut u) = (0.0, 0.0);
    for (i, p) in pmf.iter().enumerate() {
        let x = i as f64 + off;
        let p = p.as_f64().unwrap_or(0.0);
        if x > line { o += p } else if x < line { u += p }
    }
    Some((o, u))
}

#[derive(Clone)]
struct Ledger { id: i64, game_id: String, date: NaiveDate, home: String, away: String, engine: String, version: String, payload: Value }

async fn ledgers(db: &tokio_postgres::Client, sport: &str, from: NaiveDate, to: NaiveDate, full: bool) -> Result<Vec<Ledger>, String> {
    let engines = vec![sport.to_string(), format!("{sport}_team")];
    let sel = if full { "payload" } else {
        "jsonb_build_object('home', coalesce(payload->'home'->>'team', payload->>'home'), 'away', coalesce(payload->'away'->>'team', payload->>'away'), 'win', payload->'win', \
          'hs', payload->'score'->'home'->'mean', 'as', payload->'score'->'away'->'mean', 'total', \
          CASE WHEN jsonb_typeof(payload->'total') = 'number' THEN payload->'total' ELSE payload->'total'->'mean' END, \
          'margin', CASE WHEN jsonb_typeof(payload->'margin_home') = 'number' THEN payload->'margin_home' ELSE payload->'margin_home'->'mean' END)" };
    let rows = db.query(&format!(
        "SELECT DISTINCT ON (game_id) id, game_id, game_date, engine, model_version, {sel} FROM prediction_ledger \
          WHERE sport = ANY($1) AND game_date BETWEEN $2 AND $3 ORDER BY game_id, (sport = $4) DESC, id DESC"),
        &[&engines, &from, &to, &sport]).await.map_err(|e| dberr(&e))?;
    Ok(rows.iter().map(|r| {
        let p: Json<Value> = r.get(5);
        Ledger { id: r.get(0), game_id: r.get(1), date: r.get(2), engine: r.get(3), version: r.get(4),
            home: canon(sport, team_code(&p.0["home"])), away: canon(sport, team_code(&p.0["away"])), payload: p.0 }
    }).collect())
}

fn team_code(v: &Value) -> &str { v.as_str().or_else(|| v["team"].as_str()).unwrap_or("") }

/// Players in a frozen prediction: `players.{side}` (NFL/NBA/NHL) or MLB's batters + starter.
fn ledger_players(p: &Value, side: &str) -> Vec<Value> {
    if let Some(a) = p["players"][side].as_array() { return a.clone(); }
    let mut v: Vec<Value> = p[side]["batters"].as_array().cloned().unwrap_or_default().into_iter().map(|mut b| {
        if b["id"].is_null() { b["id"] = b["player_id"].clone(); }
        if b["pos"].is_null() { b["pos"] = b["position"].clone(); }
        b
    }).collect();
    if p[side]["starter"].is_object() {
        let mut s = p[side]["starter"].clone();
        s["id"] = s["player_id"].clone();
        s["pos"] = json!("SP");
        s["role"] = json!("starter");
        v.insert(0, s);
    }
    v
}

fn model_json(l: &Ledger) -> Value {
    let p = &l.payload;
    let hs = p["hs"].as_f64().or_else(|| mean_of(&p["score"]["home"]));
    let aw = p["as"].as_f64().or_else(|| mean_of(&p["score"]["away"]));
    let total = p["total"].as_f64().or_else(|| mean_of(&p["total"])).or_else(|| Some(hs? + aw?));
    let margin = p["margin"].as_f64().or_else(|| mean_of(&p["margin_home"])).or_else(|| Some(hs? - aw?));
    let (wh, wa) = (p["win"]["home"].as_f64(), p["win"]["away"].as_f64());
    let tie = p["win"]["tie"].as_f64().unwrap_or(0.0);
    // regulation ties go to overtime in the engines that report them; split evenly for display
    let (wh, wa) = match (wh, wa) { (Some(h), Some(a)) => (Some(h + tie / 2.0), Some(a + tie / 2.0)), x => x };
    json!({ "ledger_id": l.id, "game_id": l.game_id, "engine": l.engine, "model_version": l.version,
        "home_score": hs.map(r1), "away_score": aw.map(r1), "total": total.map(r1), "spread_home": margin.map(|m| r1(-m)),
        "win_home": wh.map(r3), "win_away": wa.map(r3) })
}

fn match_ledger<'a>(sport: &str, ls: &'a [Ledger], ev: &Value, game_id: Option<&str>) -> Option<&'a Ledger> {
    if let Some(g) = game_id { if let Some(l) = ls.iter().find(|l| l.game_id == g) { return Some(l); } }
    let id = ev["id"].as_str().unwrap_or("");
    if let Some(l) = ls.iter().find(|l| l.game_id == id) { return Some(l); }
    let (h, a) = (ev["home"]["abbr"].as_str().unwrap_or(""), ev["away"]["abbr"].as_str().unwrap_or(""));
    let d: Option<NaiveDate> = ev["local_date"].as_str().and_then(|x| x.parse().ok());
    let same = |l: &&Ledger| canon(sport, &l.home) == h && canon(sport, &l.away) == a;
    ls.iter().filter(same).find(|l| Some(l.date) == d)
        .or_else(|| if sport == "mlb" { None } else { ls.iter().filter(same).find(|l| d.map(|d| (l.date - d).num_days().abs() <= 1).unwrap_or(false)) })
}

#[derive(Clone, Debug)]
struct Quote { market: String, side: String, line: Option<f64>, price: i32, book: String, link: Option<String>, open_line: Option<f64>, open_price: Option<i32> }

async fn game_lines(db: &tokio_postgres::Client, sport: &str, game_ids: &[String]) -> Result<HashMap<String, Vec<Quote>>, String> {
    if game_ids.is_empty() { return Ok(HashMap::new()); }
    let rows = db.query(
        "SELECT DISTINCT ON (game_id, market, side, book) game_id, market, side, line, price, book, link, open_line, open_price \
           FROM market_lines WHERE sport=$1 AND kind='game' AND game_id = ANY($2) AND observed_at > now() - interval '4 days' \
          ORDER BY game_id, market, side, book, observed_at DESC", &[&sport, &game_ids]).await.map_err(|e| dberr(&e))?;
    let mut out: HashMap<String, Vec<Quote>> = HashMap::new();
    for r in rows {
        out.entry(r.get(0)).or_default().push(Quote { market: r.get(1), side: r.get(2), line: r.get(3), price: r.get(4), book: r.get(5),
            link: r.get(6), open_line: r.get(7), open_price: r.get(8) });
    }
    Ok(out)
}

/// Best price for each side at the most-quoted line.
fn best_lines(qs: &[Quote]) -> Value {
    let mut out = json!({});
    for (market, sides) in [("moneyline", ["home", "away"]), ("spread", ["home", "away"]), ("total", ["over", "under"])] {
        let mut m = json!({});
        for side in sides {
            let cand: Vec<&Quote> = qs.iter().filter(|q| q.market == market && q.side == side).collect();
            if cand.is_empty() { continue; }
            let mut count: BTreeMap<i64, usize> = BTreeMap::new();
            for q in &cand { *count.entry((q.line.unwrap_or(0.0) * 10.0).round() as i64).or_default() += 1; }
            let main = count.iter().max_by_key(|(_, n)| **n).map(|(l, _)| *l).unwrap_or(0);
            if let Some(b) = cand.iter().filter(|q| (q.line.unwrap_or(0.0) * 10.0).round() as i64 == main).max_by_key(|q| q.price) {
                m[side] = json!({ "line": b.line, "price": b.price, "book": b.book, "link": b.link, "books": cand.len() });
            }
        }
        out[market] = m;
    }
    out
}

// ---------------------------------------------------------------------------
// Schedule
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct ScheduleQ { season: Option<i32>, week: Option<i32>, date: Option<String> }

pub async fn schedule(state: web::Data<State>, path: web::Path<String>, q: web::Query<ScheduleQ>) -> impl Responder {
    match schedule_inner(&state, &path, &q).await { Ok(v) => HttpResponse::Ok().json(v), Err(e) => err(e) }
}

async fn schedule_inner(state: &State, sport: &str, q: &ScheduleQ) -> Result<Value, String> {
    let path = espn_path(sport).ok_or("unknown sport")?;
    let today = et_date(Utc::now());
    let mut meta = json!({ "sport": sport });
    let url = if sport == "nfl" {
        let season = q.season.unwrap_or_else(|| crate::nfl_hub::season_of(today));
        let weeks = crate::nfl_hub::weeks(&state.db, season).await?;
        let week = q.week.unwrap_or_else(|| weeks.iter()
            .find(|w| w["end"].as_str().and_then(|d| d.parse::<NaiveDate>().ok()).map(|d| d >= today).unwrap_or(false))
            .or(weeks.last()).and_then(|w| w["week"].as_i64()).unwrap_or(1) as i32);
        let (stype, wk) = if week >= 19 { (3, if week >= 22 { 5 } else { week - 18 }) } else { (2, week) };
        meta["season"] = json!(season);
        meta["week"] = json!(week);
        meta["weeks"] = json!(weeks);
        format!("https://site.api.espn.com/apis/site/v2/sports/{path}/scoreboard?seasontype={stype}&week={wk}&dates={season}")
    } else {
        let date = q.date.as_deref().and_then(|d| d.parse::<NaiveDate>().ok()).unwrap_or(today);
        meta["date"] = json!(date);
        meta["days"] = json!((-3..=6).map(|i| date + CDuration::days(i)).collect::<Vec<_>>());
        format!("https://site.api.espn.com/apis/site/v2/sports/{path}/scoreboard?dates={}&limit=100", date.format("%Y%m%d"))
    };
    let sb = espn(state, &url, Duration::from_secs(30)).await?;
    let mut events: Vec<Value> = sb["events"].as_array().map(|a| a.iter().filter_map(|e| event_json(sport, e)).collect()).unwrap_or_default();
    if let Some(byes) = sb["week"]["teamsOnBye"].as_array() {
        meta["byes"] = json!(byes.iter().filter_map(|t| s(&t["abbreviation"])).map(|a| canon(sport, &a)).collect::<Vec<_>>());
    }
    // nflverse ids for NFL games
    let mut nfl_ids: HashMap<String, String> = HashMap::new();
    if sport == "nfl" {
        let ids: Vec<String> = events.iter().filter_map(|e| s(&e["id"])).collect();
        for r in state.db.query("SELECT espn_id, game_id FROM nfl_game_info WHERE espn_id = ANY($1)", &[&ids]).await.map_err(|e| dberr(&e))? {
            nfl_ids.insert(r.get(0), r.get(1));
        }
    }
    let dates: Vec<NaiveDate> = events.iter().filter_map(|e| e["local_date"].as_str().and_then(|d| d.parse().ok())).collect();
    let (lo, hi) = (dates.iter().min().copied().unwrap_or(today), dates.iter().max().copied().unwrap_or(today));
    let ls = ledgers(&state.db, sport, lo - CDuration::days(1), hi + CDuration::days(1), false).await?;
    let mut gids = Vec::new();
    for e in events.iter_mut() {
        let nid = e["id"].as_str().and_then(|i| nfl_ids.get(i)).cloned();
        let l = match_ledger(sport, &ls, e, nid.as_deref());
        let gid = nid.or_else(|| l.map(|l| l.game_id.clone())).or_else(|| s(&e["id"]));
        e["game_id"] = json!(gid);
        e["model"] = l.map(model_json).unwrap_or(Value::Null);
        if let Some(g) = gid { gids.push(g); }
    }
    let lines = game_lines(&state.db, sport, &gids).await?;
    let picks = pick_counts(&state.db, sport, &gids).await?;
    for e in events.iter_mut() {
        let gid = e["game_id"].as_str().unwrap_or("").to_string();
        e["lines"] = lines.get(&gid).map(|q| best_lines(q)).unwrap_or(Value::Null);
        e["picks"] = json!(picks.get(&gid).copied().unwrap_or(0));
    }
    events.sort_by(|a, b| a["start"].as_str().cmp(&b["start"].as_str()));
    meta["games"] = json!(events);
    Ok(meta)
}

async fn pick_counts(db: &tokio_postgres::Client, sport: &str, gids: &[String]) -> Result<HashMap<String, i64>, String> {
    let rows = db.query(&format!("SELECT game_id, count(*) FROM ai_picks WHERE sport=$1 AND game_id = ANY($2) AND tier <> 'hidden' AND {} GROUP BY 1", crate::governance::VISIBLE),
        &[&sport, &gids]).await.map_err(|e| dberr(&e))?;
    Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
}

// ---------------------------------------------------------------------------
// Game page
// ---------------------------------------------------------------------------

pub async fn game(state: web::Data<State>, path: web::Path<(String, String)>) -> impl Responder {
    let (sport, id) = path.into_inner();
    match game_inner(&state, &sport, &id).await { Ok(v) => HttpResponse::Ok().json(v), Err(e) => err(e) }
}

fn summary_team_list(v: &Value, f: impl Fn(&Value) -> Value) -> Vec<Value> {
    v.as_array().map(|a| a.iter().map(|x| json!({ "team": s(&x["team"]["abbreviation"]), "logo": s(&x["team"]["logo"]), "data": f(x) })).collect()).unwrap_or_default()
}

async fn game_inner(state: &State, sport: &str, id: &str) -> Result<Value, String> {
    let path = espn_path(sport).ok_or("unknown sport")?;
    let sum = espn(state, &format!("https://site.api.espn.com/apis/site/v2/sports/{path}/summary?event={id}"), Duration::from_secs(180)).await?;
    let comp = &sum["header"]["competitions"][0];
    let mut ev = json!({ "id": id, "competitions": [comp], "date": comp["date"], "name": sum["header"]["name"],
        "status": comp["status"], "shortName": sum["header"]["shortName"] });
    let abbr = |side: &str| comp["competitors"].as_array().and_then(|a| a.iter().find(|c| c["homeAway"] == side))
        .and_then(|c| s(&c["team"]["abbreviation"])).unwrap_or_default();
    if ev["shortName"].is_null() { ev["shortName"] = json!(format!("{} @ {}", abbr("away"), abbr("home"))); }
    if ev["name"].is_null() { ev["name"] = ev["shortName"].clone(); }
    ev["weather"] = sum["gameInfo"]["weather"].clone();
    let mut e = event_json(sport, &ev).ok_or("event not found")?;
    e["venue"] = json!({ "name": s(&sum["gameInfo"]["venue"]["fullName"]), "city": s(&sum["gameInfo"]["venue"]["address"]["city"]),
        "state": s(&sum["gameInfo"]["venue"]["address"]["state"]), "grass": sum["gameInfo"]["venue"]["grass"].as_bool(),
        "image": sum["gameInfo"]["venue"]["images"][0]["href"].as_str() });
    if e["weather"].is_null() && sum["gameInfo"]["weather"].is_object() {
        let w = &sum["gameInfo"]["weather"];
        e["weather"] = json!({ "text": s(&w["displayValue"]), "temp": num(&w["temperature"]), "condition": s(&w["conditionId"]) });
    }
    let home = e["home"]["abbr"].as_str().unwrap_or("").to_string();
    let away = e["away"]["abbr"].as_str().unwrap_or("").to_string();
    let day: Option<NaiveDate> = e["local_date"].as_str().and_then(|d| d.parse().ok());

    // ids + model
    let nfl_key = if sport == "nfl" { Some(crate::nfl_hub::resolve(&state.db, Some(id), &home, &away, day).await?) } else { None };
    let d0 = day.unwrap_or_else(|| et_date(Utc::now()));
    let ls = ledgers(&state.db, sport, d0 - CDuration::days(1), d0 + CDuration::days(1), true).await?;
    let led = match_ledger(sport, &ls, &e, nfl_key.as_ref().and_then(|k| k.game_id.as_deref())).cloned();
    let gid = nfl_key.as_ref().and_then(|k| k.game_id.clone()).or_else(|| led.as_ref().map(|l| l.game_id.clone())).unwrap_or_else(|| id.to_string());
    e["game_id"] = json!(gid);
    let model = led.as_ref().map(|l| {
        let mut m = model_json(&Ledger { payload: json!({ "hs": mean_of(&l.payload["score"]["home"]), "as": mean_of(&l.payload["score"]["away"]),
            "total": mean_of(&l.payload["total"]), "margin": mean_of(&l.payload["margin_home"]), "win": l.payload["win"] }), ..l.clone() });
        let q = |v: &Value| json!({ "q10": v["q10"], "q50": v["q50"], "q90": v["q90"] });
        m["home_range"] = q(&l.payload["score"]["home"]);
        m["away_range"] = q(&l.payload["score"]["away"]);
        m["total_range"] = q(&l.payload["total"]);
        m["overtime"] = l.payload["win"]["overtime"].clone();
        if l.payload["h1"].is_object() {
            m["first_half"] = json!({ "win": l.payload["h1"]["win"], "home": mean_of(&l.payload["h1"]["home"]).map(r1),
                "away": mean_of(&l.payload["h1"]["away"]).map(r1), "total": mean_of(&l.payload["h1"]["total"]).map(r1) });
        }
        m
    });
    e["model"] = model.clone().unwrap_or(Value::Null);

    // lines (every book) + best
    let mut lines = game_lines(&state.db, sport, &[gid.clone()]).await?.remove(&gid).unwrap_or_default();
    if lines.is_empty() { lines = pickcenter_quotes(&sum["pickcenter"]); }
    e["lines"] = best_lines(&lines);
    let books: Vec<Value> = lines.iter().map(|q| json!({ "market": q.market, "side": q.side, "line": q.line, "price": q.price, "book": q.book,
        "link": q.link, "open_line": q.open_line, "open_price": q.open_price })).collect();

    // model vs market on the main lines
    let mut edges = Vec::new();
    if let (Some(l), Some(m)) = (&led, &model) {
        let p = &l.payload;
        for (market, side) in [("moneyline", "home"), ("moneyline", "away"), ("spread", "home"), ("spread", "away"), ("total", "over"), ("total", "under")] {
            let Some(b) = e["lines"][market][side].as_object() else { continue };
            let price = b["price"].as_i64().unwrap_or(0) as i32;
            let line = b["line"].as_f64();
            let pm = match (market, side) {
                ("moneyline", "home") => m["win_home"].as_f64(),
                ("moneyline", "away") => m["win_away"].as_f64(),
                ("spread", sd) => line.and_then(|ln| {
                    // home covers when margin_home + line_home > 0
                    let lh = if sd == "home" { ln } else { -ln };
                    let (o, u) = p_over_under(&p["margin_home"], -lh)?;
                    Some(if sd == "home" { o / (o + u).max(1e-9) } else { u / (o + u).max(1e-9) })
                }),
                ("total", sd) => line.and_then(|ln| { let (o, u) = p_over_under(&p["total"], ln)?; Some(if sd == "over" { o / (o + u).max(1e-9) } else { u / (o + u).max(1e-9) }) }),
                _ => None,
            };
            let imp = implied(price);
            if let Some(pm) = pm {
                edges.push(json!({ "market": market, "side": side, "line": line, "price": price, "book": b["book"], "p_model": r3(pm),
                    "p_implied": r3(imp), "edge": r3(pm - imp), "ev": r3(pm * payout(price) - (1.0 - pm)) }));
            }
        }
    }

    // projections (players in the frozen sim) with book props
    let props = prop_lines(&state.db, sport, &gid).await?;
    let subj: Vec<String> = props.keys().map(|k| k.0.clone()).collect::<std::collections::HashSet<_>>().into_iter().collect();
    // the AI Picks blend + sanity check, so hub probabilities match what Picks would publish
    let pricer = crate::picks::PropPricer::load(&state.db, sport, &subj).await.ok();
    let mut proj_by_id: HashMap<String, Value> = HashMap::new();
    let projections = led.as_ref().map(|l| {
        let mut out = json!({});
        for side in ["away", "home"] {
            let team = if side == "home" { &home } else { &away };
            let list: Vec<Value> = ledger_players(&l.payload, side).iter().map(|pl| {
                let pid = pl["id"].as_str().map(|x| x.to_string()).or_else(|| pl["id"].as_i64().map(|x| x.to_string())).unwrap_or_default();
                let mut stats = json!({});
                if let Some(st) = pl["stats"].as_object() {
                    for (k, v) in st {
                        let mut row = json!({ "mean": mean_of(v).map(|x| (x * 100.0).round() / 100.0), "q10": v["q10"], "q50": v["q50"], "q90": v["q90"] });
                        if let Some(qs) = props.get(&(pid.clone(), k.clone())) {
                            row["books"] = json!(qs.iter().map(|q| {
                                let p_raw = q.line.and_then(|ln| p_over_under(v, ln))
                                    .map(|(o, u)| { let t = (o + u).max(1e-9); if q.side == "over" { o / t } else { u / t } });
                                // market: no-vig against the other side at the same book and line
                                let p_market = match qs.iter().find(|x| x.book == q.book && x.side != q.side && x.line == q.line) {
                                    Some(o) => { let (a, b) = (implied(q.price), implied(o.price)); a / (a + b) }
                                    None => implied(q.price),
                                };
                                let (p_model, flag) = match (p_raw, &pricer, q.line) {
                                    (Some(pr), Some(pz), Some(ln)) => {
                                        let naive = pz.naive_over(&pid, k, ln).map(|o| if q.side == "over" { o } else { 1.0 - o });
                                        (Some(pz.blend(sport, k, pr, naive, p_market)), (pr - p_market > pz.max_edge).then_some("implausible"))
                                    }
                                    (pr, _, _) => (pr, None),
                                };
                                json!({ "side": q.side, "line": q.line, "price": q.price, "book": q.book, "link": q.link,
                                    "p_model": p_model.map(r3), "p_raw": p_raw.map(r3), "p_market": r3(p_market), "flag": flag })
                            }).collect::<Vec<_>>());
                            if let Some((avg, n)) = pricer.as_ref().and_then(|pz| pz.recent(&pid, k)) {
                                row["recent"] = json!({ "avg": r1(avg), "games": n });
                            }
                        }
                        stats[k] = row;
                    }
                }
                let summary = json!({ "name": pl["name"], "pos": pl["pos"], "stats": stats.as_object().map(|o| o.iter()
                    .map(|(k, v)| (k.clone(), v["mean"].clone())).collect::<serde_json::Map<_, _>>()) });
                proj_by_id.insert(pid.clone(), summary);
                json!({ "id": pid, "name": pl["name"], "pos": pl["pos"], "role": pl["role"], "team": team, "stats": stats })
            }).collect();
            out[side] = json!(list);
        }
        out
    });

    // picks
    let picks: Vec<Value> = state.db.query(
        &format!("SELECT id, kind, market, subject_name, side, line, book, price, p_model, p_market, edge, ev, grade, tier, writeup, link, projection, published_at \
           FROM ai_picks WHERE sport=$1 AND game_id=$2 AND {} ORDER BY (tier='hidden'), edge DESC LIMIT 40", crate::governance::VISIBLE), &[&sport, &gid]).await.map_err(|e| dberr(&e))?
        .iter().map(|r| json!({ "id": r.get::<_, i64>(0), "kind": r.get::<_, String>(1), "market": r.get::<_, String>(2), "subject": r.get::<_, String>(3),
            "side": r.get::<_, String>(4), "line": r.get::<_, Option<f64>>(5), "book": r.get::<_, String>(6), "price": r.get::<_, i32>(7),
            "p_model": r.get::<_, f64>(8), "p_market": r.get::<_, f64>(9), "edge": r.get::<_, f64>(10), "ev": r.get::<_, f64>(11),
            "grade": r.get::<_, String>(12), "tier": r.get::<_, String>(13), "writeup": r.get::<_, String>(14), "link": r.get::<_, Option<String>>(15),
            "projection": r.get::<_, Option<f64>>(16), "published_at": r.get::<_, DateTime<Utc>>(17) })).collect();

    // ESPN context
    let injuries = summary_team_list(&sum["injuries"], |x| json!(x["injuries"].as_array().map(|a| a.iter().map(|i| json!({
        "name": s(&i["athlete"]["displayName"]), "pos": s(&i["athlete"]["position"]["abbreviation"]), "status": s(&i["status"]),
        "detail": s(&i["details"]["type"]).or_else(|| s(&i["type"]["description"])), "side": s(&i["details"]["side"]),
        "return": s(&i["details"]["returnDate"]), "headshot": s(&i["athlete"]["headshot"]["href"]) })).collect::<Vec<_>>()).unwrap_or_default()));
    let last_five = summary_team_list(&sum["lastFiveGames"], |x| json!(x["events"].as_array().map(|a| a.iter().map(|g| json!({
        "date": s(&g["gameDate"]), "opponent": s(&g["opponent"]["abbreviation"]), "at": s(&g["atVs"]), "result": s(&g["gameResult"]),
        "score": s(&g["score"]) })).collect::<Vec<_>>()).unwrap_or_default()));
    let ats = summary_team_list(&sum["againstTheSpread"], |x| json!(x["records"].as_array().map(|a| a.iter().map(|r| json!({
        "type": s(&r["type"]), "summary": s(&r["summary"]) })).collect::<Vec<_>>()).unwrap_or_default()));
    let leaders = summary_team_list(&sum["leaders"], |x| json!(x["leaders"].as_array().map(|a| a.iter().filter_map(|c| {
        let l = c["leaders"].as_array()?.first()?;
        Some(json!({ "category": s(&c["displayName"]), "name": s(&l["athlete"]["displayName"]), "value": s(&l["displayValue"]),
            "headshot": s(&l["athlete"]["headshot"]["href"]) }))
    }).collect::<Vec<_>>()).unwrap_or_default()));
    let predictor = if sum["predictor"].is_object() {
        json!({ "home": num(&sum["predictor"]["homeTeam"]["gameProjection"]), "away": num(&sum["predictor"]["awayTeam"]["gameProjection"]) })
    } else { Value::Null };
    let standings = sum["standings"]["groups"].as_array().map(|gs| gs.iter().map(|g| json!({ "header": s(&g["header"]),
        "teams": g["standings"]["entries"].as_array().map(|es| es.iter().map(|t| json!({ "team": s(&t["team"]),
            "stats": t["stats"].as_array().map(|st| st.iter().filter_map(|x| Some((s(&x["abbreviation"]).or_else(|| s(&x["shortDisplayName"]))?, x["displayValue"].clone())))
                .collect::<serde_json::Map<_, _>>()) })).collect::<Vec<_>>()) })).collect::<Vec<_>>());

    // analytics
    let analysis = match sport {
        "nfl" => crate::nfl_hub::analysis(&state.db, nfl_key.as_ref().unwrap(), &proj_by_id).await.unwrap_or_else(|e| json!({ "error": e })),
        _ => team_analysis(&state.db, sport, &home, &away, day, projections.as_ref()).await.unwrap_or_else(|e| json!({ "error": e })),
    };

    let mut out = json!({
        "sport": sport, "event": e, "books": books, "edges": edges, "projections": projections, "picks": picks,
        "espn": { "injuries": injuries, "last_five": last_five, "ats": ats, "leaders": leaders, "predictor": predictor, "standings": standings },
        "analysis": analysis,
    });
    out["report"] = report(&out);
    Ok(out)
}

/// ESPN's pickcenter (DraftKings) as quotes, for games we haven't recorded lines for yet.
fn pickcenter_quotes(pc: &Value) -> Vec<Quote> {
    let mut out = Vec::new();
    for o in pc.as_array().into_iter().flatten() {
        let book = s(&o["provider"]["name"]).unwrap_or_else(|| "espn".into()).to_lowercase();
        let q = |market: &str, side: &str, line: Option<f64>, price: Option<i32>| price.map(|p| Quote { market: market.into(), side: side.into(),
            line, price: p, book: book.clone(), link: None, open_line: None, open_price: None });
        let spread = num(&o["spread"]);
        let total = num(&o["overUnder"]);
        out.extend([
            q("moneyline", "home", None, crate::lines::american(&o["homeTeamOdds"]["moneyLine"])),
            q("moneyline", "away", None, crate::lines::american(&o["awayTeamOdds"]["moneyLine"])),
            q("spread", "home", spread, crate::lines::american(&o["homeTeamOdds"]["spreadOdds"])),
            q("spread", "away", spread.map(|x| -x), crate::lines::american(&o["awayTeamOdds"]["spreadOdds"])),
            q("total", "over", total, crate::lines::american(&o["overOdds"])),
            q("total", "under", total, crate::lines::american(&o["underOdds"])),
        ].into_iter().flatten());
    }
    out
}

fn implied(price: i32) -> f64 { if price < 0 { -price as f64 / (-price as f64 + 100.0) } else { 100.0 / (price as f64 + 100.0) } }
fn payout(price: i32) -> f64 { if price < 0 { 100.0 / -price as f64 } else { price as f64 / 100.0 } }

async fn prop_lines(db: &tokio_postgres::Client, sport: &str, gid: &str) -> Result<HashMap<(String, String), Vec<Quote>>, String> {
    let rows = db.query(
        "SELECT DISTINCT ON (subject, market, side, book) subject, market, side, line, price, book, link FROM market_lines \
          WHERE sport=$1 AND kind='prop' AND game_id=$2 AND observed_at > now() - interval '3 days' \
          ORDER BY subject, market, side, book, observed_at DESC", &[&sport, &gid]).await.map_err(|e| dberr(&e))?;
    let mut out: HashMap<(String, String), Vec<Quote>> = HashMap::new();
    for r in rows {
        out.entry((r.get(0), r.get(1))).or_default().push(Quote { market: r.get(1), side: r.get(2), line: r.get(3), price: r.get(4),
            book: r.get(5), link: r.get(6), open_line: None, open_price: None });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// NBA / NHL / MLB team rankings from our warehouses
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Dir { Up, Down, Usage }

struct Def { key: &'static str, label: &'static str, help: &'static str, dir: Dir, fmt: &'static str }

fn defs(sport: &str) -> &'static [Def] {
    const NBA: &[Def] = &[
        Def { key: "pts", label: "Points / game", help: "Points per game", dir: Dir::Up, fmt: "d1" },
        Def { key: "ortg", label: "Offensive rating", help: "Points per 100 possessions", dir: Dir::Up, fmt: "d1" },
        Def { key: "net", label: "Net rating", help: "Offensive minus defensive rating (offense column only)", dir: Dir::Up, fmt: "d1" },
        Def { key: "pace", label: "Pace", help: "Possessions per game", dir: Dir::Usage, fmt: "d1" },
        Def { key: "efg", label: "Effective FG %", help: "(FGM + 0.5 x 3PM) / FGA", dir: Dir::Up, fmt: "pct" },
        Def { key: "tov_pct", label: "Turnover %", help: "Turnovers per possession", dir: Dir::Down, fmt: "pct" },
        Def { key: "orb_pct", label: "Offensive rebound %", help: "Share of available offensive rebounds grabbed", dir: Dir::Up, fmt: "pct" },
        Def { key: "ftr", label: "Free-throw rate", help: "FTA per FGA", dir: Dir::Up, fmt: "pct" },
        Def { key: "fg3a_rate", label: "3PA rate", help: "Share of shots from three", dir: Dir::Usage, fmt: "pct" },
        Def { key: "fg3_pct", label: "3P %", help: "Three-point percentage", dir: Dir::Up, fmt: "pct" },
        Def { key: "fg3m", label: "3PM / game", help: "Threes made per game", dir: Dir::Up, fmt: "d1" },
        Def { key: "reb", label: "Rebounds / game", help: "Total rebounds per game", dir: Dir::Up, fmt: "d1" },
        Def { key: "ast", label: "Assists / game", help: "Assists per game", dir: Dir::Up, fmt: "d1" },
        Def { key: "stl", label: "Steals / game", help: "Steals per game", dir: Dir::Up, fmt: "d1" },
        Def { key: "blk", label: "Blocks / game", help: "Blocks per game", dir: Dir::Up, fmt: "d1" },
        Def { key: "pts_g", label: "Points / game - guards", help: "Points per game by guards", dir: Dir::Up, fmt: "d1" },
        Def { key: "pts_f", label: "Points / game - forwards", help: "Points per game by forwards", dir: Dir::Up, fmt: "d1" },
        Def { key: "pts_c", label: "Points / game - centers", help: "Points per game by centers", dir: Dir::Up, fmt: "d1" },
        Def { key: "reb_c", label: "Rebounds / game - centers", help: "Rebounds per game by centers", dir: Dir::Up, fmt: "d1" },
        Def { key: "ast_g", label: "Assists / game - guards", help: "Assists per game by guards", dir: Dir::Up, fmt: "d1" },
        Def { key: "fg3m_g", label: "3PM / game - guards", help: "Threes per game by guards", dir: Dir::Up, fmt: "d1" },
    ];
    const NHL: &[Def] = &[
        Def { key: "gf", label: "Goals / game", help: "Goals per game", dir: Dir::Up, fmt: "d2" },
        Def { key: "sf", label: "Shots on goal / game", help: "Shots on goal per game", dir: Dir::Up, fmt: "d1" },
        Def { key: "sh_pct", label: "Shooting %", help: "Goals per shot on goal", dir: Dir::Up, fmt: "pct" },
        Def { key: "sv_pct", label: "Save %", help: "Saves per shot faced (offense column: own goalies)", dir: Dir::Up, fmt: "pct" },
        Def { key: "shot_share", label: "Shot share", help: "Share of the game's shots on goal (offense column only)", dir: Dir::Up, fmt: "pct" },
        Def { key: "ppg", label: "Power-play goals / game", help: "Power-play goals per game", dir: Dir::Up, fmt: "d2" },
        Def { key: "hits", label: "Hits / game", help: "Hits per game", dir: Dir::Usage, fmt: "d1" },
        Def { key: "blocks", label: "Blocked shots / game", help: "Blocked shots per game", dir: Dir::Usage, fmt: "d1" },
        Def { key: "pim", label: "Penalty minutes / game", help: "PIM per game", dir: Dir::Down, fmt: "d1" },
        Def { key: "pts_f", label: "Points / game - forwards", help: "Goals + assists per game by forwards", dir: Dir::Up, fmt: "d2" },
        Def { key: "pts_d", label: "Points / game - defense", help: "Goals + assists per game by defensemen", dir: Dir::Up, fmt: "d2" },
        Def { key: "sog_f", label: "Shots / game - forwards", help: "Shots on goal per game by forwards", dir: Dir::Up, fmt: "d1" },
        Def { key: "sog_d", label: "Shots / game - defense", help: "Shots on goal per game by defensemen", dir: Dir::Up, fmt: "d1" },
    ];
    const MLB: &[Def] = &[
        Def { key: "r", label: "Runs / game", help: "Runs per game", dir: Dir::Up, fmt: "d2" },
        Def { key: "avg", label: "Batting average", help: "Hits per at-bat", dir: Dir::Up, fmt: "d3" },
        Def { key: "obp", label: "On-base %", help: "(H + BB + HBP) / (AB + BB + HBP + SF)", dir: Dir::Up, fmt: "d3" },
        Def { key: "slg", label: "Slugging %", help: "Total bases per at-bat", dir: Dir::Up, fmt: "d3" },
        Def { key: "ops", label: "OPS", help: "On-base plus slugging", dir: Dir::Up, fmt: "d3" },
        Def { key: "hr", label: "Home runs / game", help: "Home runs per game", dir: Dir::Up, fmt: "d2" },
        Def { key: "k_pct", label: "Strikeout %", help: "Strikeouts per plate appearance", dir: Dir::Down, fmt: "pct" },
        Def { key: "bb_pct", label: "Walk %", help: "Walks per plate appearance", dir: Dir::Up, fmt: "pct" },
        Def { key: "sb", label: "Stolen bases / game", help: "Stolen bases per game", dir: Dir::Usage, fmt: "d2" },
        Def { key: "h", label: "Hits / game", help: "Hits per game", dir: Dir::Up, fmt: "d2" },
    ];
    match sport { "nba" => NBA, "nhl" => NHL, "mlb" => MLB, _ => &[] }
}

/// team -> (offense values, defense-allowed values, games)
type Table = BTreeMap<String, (HashMap<&'static str, f64>, HashMap<&'static str, f64>, i64)>;

fn ranks_cache() -> &'static Mutex<HashMap<String, (Instant, Value)>> {
    static C: OnceLock<Mutex<HashMap<String, (Instant, Value)>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

async fn nba_table(db: &tokio_postgres::Client, before: NaiveDate) -> Result<(Table, String), String> {
    let season: i32 = db.query_one("SELECT coalesce(max(season), 0) FROM nba_games_espn WHERE completed AND season_type=2 AND game_date < $1", &[&before])
        .await.map_err(|e| dberr(&e))?.get(0);
    let n: i64 = db.query_one("SELECT count(*) FROM nba_games_espn WHERE completed AND season_type=2 AND season=$1 AND game_date < $2", &[&season, &before])
        .await.map_err(|e| dberr(&e))?.get(0);
    // until ~5 games a team, lean on last season too
    let seasons = if n < 75 { vec![season - 1, season] } else { vec![season] };
    let rows = db.query(
        "WITH g AS (SELECT espn_id, home, away FROM nba_games_espn WHERE completed AND season_type=2 AND season = ANY($1) AND game_date < $2), \
         t AS (SELECT b.espn_id, b.team, sum(pts)::float8 pts, sum(fgm)::float8 fgm, sum(fga)::float8 fga, sum(fg3m)::float8 fg3m, sum(fg3a)::float8 fg3a, \
               sum(ftm)::float8 ftm, sum(fta)::float8 fta, sum(oreb)::float8 oreb, sum(dreb)::float8 dreb, sum(ast)::float8 ast, sum(tov)::float8 tov, \
               sum(stl)::float8 stl, sum(blk)::float8 blk, \
               sum(pts) FILTER (WHERE pos LIKE '%G%')::float8 pts_g, sum(pts) FILTER (WHERE pos LIKE '%F%')::float8 pts_f, \
               sum(pts) FILTER (WHERE pos LIKE '%C%')::float8 pts_c, sum(oreb+dreb) FILTER (WHERE pos LIKE '%C%')::float8 reb_c, \
               sum(ast) FILTER (WHERE pos LIKE '%G%')::float8 ast_g, sum(fg3m) FILTER (WHERE pos LIKE '%G%')::float8 fg3m_g \
               FROM nba_box_espn b JOIN g USING (espn_id) WHERE NOT b.dnp GROUP BY 1,2) \
         SELECT a.team, o.team, a.pts,a.fgm,a.fga,a.fg3m,a.fg3a,a.ftm,a.fta,a.oreb,a.dreb,a.ast,a.tov,a.stl,a.blk, \
                o.pts,o.fgm,o.fga,o.fg3m,o.fg3a,o.ftm,o.fta,o.oreb,o.dreb,o.ast,o.tov,o.stl,o.blk, \
                coalesce(a.pts_g,0),coalesce(a.pts_f,0),coalesce(a.pts_c,0),coalesce(a.reb_c,0),coalesce(a.ast_g,0),coalesce(a.fg3m_g,0) \
           FROM t a JOIN t o ON a.espn_id = o.espn_id AND a.team <> o.team", &[&seasons, &before]).await.map_err(|e| dberr(&e))?;
    let mut sums: BTreeMap<String, ([f64; 13], [f64; 13], [f64; 6], i64)> = BTreeMap::new();
    let mut allowed_pos: BTreeMap<String, [f64; 6]> = BTreeMap::new();
    for r in &rows {
        let team: String = canon("nba", &r.get::<_, String>(0));
        let opp: String = canon("nba", &r.get::<_, String>(1));
        let e = sums.entry(team.clone()).or_insert(([0.0; 13], [0.0; 13], [0.0; 6], 0));
        for i in 0..13 { e.0[i] += r.get::<_, f64>(2 + i); e.1[i] += r.get::<_, f64>(15 + i); }
        for i in 0..6 { e.2[i] += r.get::<_, f64>(28 + i); }
        e.3 += 1;
        let ap = allowed_pos.entry(opp).or_insert([0.0; 6]);
        for i in 0..6 { ap[i] += r.get::<_, f64>(28 + i); }
    }
    let mut table = Table::new();
    for (team, (a, o, pp, n)) in &sums {
        let g = *n as f64;
        let side = |x: &[f64; 13], y: &[f64; 13]| -> HashMap<&'static str, f64> {
            let poss = x[2] - x[7] + x[10] + 0.44 * x[6];
            let opp_poss = y[2] - y[7] + y[10] + 0.44 * y[6];
            let mut m = HashMap::new();
            m.insert("pts", x[0] / g);
            m.insert("ortg", x[0] / poss * 100.0);
            m.insert("net", x[0] / poss * 100.0 - y[0] / opp_poss * 100.0);
            m.insert("pace", (poss + opp_poss) / 2.0 / g);
            m.insert("efg", (x[1] + 0.5 * x[3]) / x[2]);
            m.insert("tov_pct", x[10] / poss);
            m.insert("orb_pct", x[7] / (x[7] + y[8]));
            m.insert("ftr", x[6] / x[2]);
            m.insert("fg3a_rate", x[4] / x[2]);
            m.insert("fg3_pct", x[3] / x[4]);
            m.insert("fg3m", x[3] / g);
            m.insert("reb", (x[7] + x[8]) / g);
            m.insert("ast", x[9] / g);
            m.insert("stl", x[11] / g);
            m.insert("blk", x[12] / g);
            m
        };
        let mut off = side(a, o);
        let mut def = side(o, a);
        def.remove("net");
        for (i, k) in ["pts_g", "pts_f", "pts_c", "reb_c", "ast_g", "fg3m_g"].iter().enumerate() {
            off.insert(k, pp[i] / g);
            if let Some(ap) = allowed_pos.get(team) { def.insert(k, ap[i] / g); }
        }
        table.insert(team.clone(), (off, def, *n));
    }
    Ok((table, format!("{} regular season{}", seasons.iter().map(|s| format!("{}-{:02}", s - 1, s % 100)).collect::<Vec<_>>().join(" + "),
        if seasons.len() > 1 { " (early season: last season included)" } else { "" })))
}

async fn nhl_table(db: &tokio_postgres::Client, before: NaiveDate) -> Result<(Table, String), String> {
    let season: i32 = db.query_one("SELECT coalesce(max(season), 0) FROM nhl_games_api WHERE completed AND game_type=2 AND game_date < $1", &[&before])
        .await.map_err(|e| dberr(&e))?.get(0);
    let n: i64 = db.query_one("SELECT count(*) FROM nhl_games_api WHERE completed AND game_type=2 AND season=$1 AND game_date < $2", &[&season, &before])
        .await.map_err(|e| dberr(&e))?.get(0);
    let seasons = if n < 80 { vec![season - 10001, season] } else { vec![season] };
    let rows = db.query(
        "WITH g AS (SELECT game_id, home, away, home_score, away_score FROM nhl_games_api WHERE completed AND game_type=2 AND season = ANY($1) AND game_date < $2), \
         t AS (SELECT b.game_id, b.team, sum(coalesce(sog,0))::float8 sog, sum(coalesce(hits,0))::float8 hits, sum(coalesce(blocks,0))::float8 blocks, \
               sum(coalesce(pim,0))::float8 pim, sum(coalesce(pp_goals,0))::float8 ppg, sum(coalesce(saves,0))::float8 saves, \
               sum(coalesce(shots_against,0))::float8 sa, \
               sum(coalesce(goals,0)+coalesce(assists,0)) FILTER (WHERE pos IN ('C','L','R','F'))::float8 pts_f, \
               sum(coalesce(goals,0)+coalesce(assists,0)) FILTER (WHERE pos = 'D')::float8 pts_d, \
               sum(coalesce(sog,0)) FILTER (WHERE pos IN ('C','L','R','F'))::float8 sog_f, sum(coalesce(sog,0)) FILTER (WHERE pos = 'D')::float8 sog_d \
               FROM nhl_box_api b JOIN g USING (game_id) GROUP BY 1,2) \
         SELECT a.team, o.team, CASE WHEN a.team = g.home THEN g.home_score ELSE g.away_score END::float8, \
                a.sog, a.hits, a.blocks, a.pim, a.ppg, a.saves, a.sa, coalesce(a.pts_f,0), coalesce(a.pts_d,0), coalesce(a.sog_f,0), coalesce(a.sog_d,0), \
                CASE WHEN o.team = g.home THEN g.home_score ELSE g.away_score END::float8, \
                o.sog, o.hits, o.blocks, o.pim, o.ppg, o.saves, o.sa, coalesce(o.pts_f,0), coalesce(o.pts_d,0), coalesce(o.sog_f,0), coalesce(o.sog_d,0) \
           FROM t a JOIN t o ON a.game_id = o.game_id AND a.team <> o.team JOIN g ON g.game_id = a.game_id", &[&seasons, &before]).await.map_err(|e| dberr(&e))?;
    let mut sums: BTreeMap<String, ([f64; 12], [f64; 12], i64)> = BTreeMap::new();
    for r in &rows {
        let e = sums.entry(canon("nhl", &r.get::<_, String>(0))).or_insert(([0.0; 12], [0.0; 12], 0));
        for i in 0..12 { e.0[i] += r.get::<_, f64>(2 + i); e.1[i] += r.get::<_, f64>(14 + i); }
        e.2 += 1;
    }
    let mut table = Table::new();
    for (team, (a, o, n)) in &sums {
        let g = *n as f64;
        let side = |x: &[f64; 12], y: &[f64; 12], own: bool| -> HashMap<&'static str, f64> {
            let mut m = HashMap::new();
            m.insert("gf", x[0] / g);
            m.insert("sf", x[1] / g);
            m.insert("sh_pct", if x[1] > 0.0 { x[0] / x[1] } else { 0.0 });
            if x[7] > 0.0 { m.insert("sv_pct", x[6] / x[7]); }
            if own { m.insert("shot_share", x[1] / (x[1] + y[1]).max(1.0)); }
            m.insert("ppg", x[5] / g);
            m.insert("hits", x[2] / g);
            m.insert("blocks", x[3] / g);
            m.insert("pim", x[4] / g);
            m.insert("pts_f", x[8] / g);
            m.insert("pts_d", x[9] / g);
            m.insert("sog_f", x[10] / g);
            m.insert("sog_d", x[11] / g);
            m
        };
        table.insert(team.clone(), (side(a, o, true), side(o, a, false), *n));
    }
    Ok((table, format!("{} regular season{}", seasons.iter().map(|s| format!("{}-{:02}", s / 10000, s % 100)).collect::<Vec<_>>().join(" + "),
        if seasons.len() > 1 { " (early season: last season included)" } else { "" })))
}

async fn mlb_table(db: &tokio_postgres::Client, before: NaiveDate) -> Result<(Table, String), String> {
    let season: i32 = db.query_one("SELECT coalesce(max(season), 0) FROM player_game_logs WHERE sport='mlb' AND game_date < $1", &[&before])
        .await.map_err(|e| dberr(&e))?.get(0);
    let rows = db.query(
        "SELECT team, opponent, count(DISTINCT week)::float8, \
                sum((stats->>'r')::float8), sum((stats->>'h')::float8), sum((stats->>'hr')::float8), sum((stats->>'ab')::float8), \
                sum((stats->>'bb')::float8), sum((stats->>'hbp')::float8), sum((stats->>'sf')::float8), sum((stats->>'tb')::float8), \
                sum((stats->>'k')::float8), sum((stats->>'pa')::float8), sum((stats->>'sb')::float8) \
           FROM player_game_logs WHERE sport='mlb' AND season=$1 AND game_date < $2 AND stats ? 'pa' AND team IS NOT NULL AND opponent IS NOT NULL \
          GROUP BY 1,2", &[&season, &before]).await.map_err(|e| dberr(&e))?;
    let mut off: BTreeMap<String, [f64; 12]> = BTreeMap::new();
    let mut def: BTreeMap<String, [f64; 12]> = BTreeMap::new();
    for r in &rows {
        let (t, o) = (canon("mlb", &r.get::<_, String>(0)), canon("mlb", &r.get::<_, String>(1)));
        let v: Vec<f64> = (2..14).map(|i| r.get::<_, Option<f64>>(i).unwrap_or(0.0)).collect();
        for (m, k) in [(&mut off, t), (&mut def, o)] {
            let e = m.entry(k).or_insert([0.0; 12]);
            for i in 0..12 { e[i] += v[i]; }
        }
    }
    let side = |x: &[f64; 12]| -> HashMap<&'static str, f64> {
        let g = x[0].max(1.0);
        let obp = (x[2] + x[5] + x[6]) / (x[4] + x[5] + x[6] + x[7]).max(1.0);
        let slg = x[8] / x[4].max(1.0);
        let mut m = HashMap::new();
        m.insert("r", x[1] / g); m.insert("h", x[2] / g); m.insert("hr", x[3] / g); m.insert("avg", x[2] / x[4].max(1.0));
        m.insert("obp", obp); m.insert("slg", slg); m.insert("ops", obp + slg); m.insert("k_pct", x[9] / x[10].max(1.0));
        m.insert("bb_pct", x[5] / x[10].max(1.0)); m.insert("sb", x[11] / g);
        m
    };
    let mut table = Table::new();
    for (t, x) in &off {
        let d = def.get(t).map(side).unwrap_or_default();
        table.insert(t.clone(), (side(x), d, x[0] as i64));
    }
    Ok((table, format!("{season} season")))
}

fn rank_value(table: &Table, def: &Def, team: &str, off: bool) -> Value {
    let get = |t: &(HashMap<&'static str, f64>, HashMap<&'static str, f64>, i64)| if off { t.0.get(def.key) } else { t.1.get(def.key) }.copied();
    // drop exhibition / all-star sides that only show up a handful of times
    let most = table.values().map(|t| t.2).max().unwrap_or(0);
    let mut vals: Vec<(&String, f64)> = table.iter().filter(|(_, t)| t.2 * 4 >= most)
        .filter_map(|(k, t)| get(t).filter(|v| v.is_finite()).map(|v| (k, v))).collect();
    let desc = match (def.dir, off) { (Dir::Usage, _) => true, (Dir::Up, true) | (Dir::Down, false) => true, _ => false };
    vals.sort_by(|a, b| if desc { b.1.total_cmp(&a.1) } else { a.1.total_cmp(&b.1) });
    let lg = if vals.is_empty() { 0.0 } else { vals.iter().map(|x| x.1).sum::<f64>() / vals.len() as f64 };
    match vals.iter().position(|(k, _)| k.as_str() == team) {
        Some(i) => {
            let v = vals[i].1;
            json!({ "key": def.key, "label": def.label, "help": def.help, "fmt": def.fmt, "value": (v * 10000.0).round() / 10000.0, "rank": i + 1,
                "of": vals.len(), "vs_league": if lg.abs() > 1e-9 { r1((v - lg) / lg.abs() * 100.0) } else { 0.0 }, "usage": def.dir == Dir::Usage })
        }
        None => json!({ "key": def.key, "label": def.label, "help": def.help, "fmt": def.fmt, "value": null, "rank": null, "of": vals.len() }),
    }
}

async fn team_analysis(db: &tokio_postgres::Client, sport: &str, home: &str, away: &str, day: Option<NaiveDate>, proj: Option<&Value>) -> Result<Value, String> {
    let before = day.unwrap_or_else(|| et_date(Utc::now()));
    let ck = format!("{sport}:{before}");
    let cached = ranks_cache().lock().unwrap().get(&ck).filter(|(t, _)| t.elapsed() < Duration::from_secs(1800)).map(|x| x.1.clone());
    let (table, label): (Table, String) = match cached {
        Some(v) => serde_json::from_value::<(BTreeMap<String, (HashMap<String, f64>, HashMap<String, f64>, i64)>, String)>(v)
            .map(|(t, l)| (t.into_iter().map(|(k, (a, b, n))| (k, (intern(sport, a), intern(sport, b), n))).collect(), l))
            .map_err(|e| e.to_string())?,
        None => {
            let (t, l) = match sport { "nba" => nba_table(db, before).await?, "nhl" => nhl_table(db, before).await?, "mlb" => mlb_table(db, before).await?, _ => return Ok(Value::Null) };
            let ser: BTreeMap<String, (HashMap<String, f64>, HashMap<String, f64>, i64)> = t.iter()
                .map(|(k, (a, b, n))| (k.clone(), (a.iter().map(|(x, y)| (x.to_string(), *y)).collect(), b.iter().map(|(x, y)| (x.to_string(), *y)).collect(), *n))).collect();
            ranks_cache().lock().unwrap().insert(ck, (Instant::now(), json!((ser, l.clone()))));
            (t, l)
        }
    };
    let ds = defs(sport);
    let block = |team: &str, off: bool| -> Vec<Value> { ds.iter().map(|d| rank_value(&table, d, team, off)).collect() };
    let sort_rank = |mut v: Vec<Value>| { v.retain(|r| !r["rank"].is_null()); v.sort_by_key(|r| r["rank"].as_u64().unwrap_or(99)); v };
    let games = |t: &str| table.get(t).map(|x| x.2).unwrap_or(0);
    let gap_table = &table;
    Ok(json!({
        "data_note": format!("Team ranks from our warehouse: {label}, games before this one."),
        "season_label": label,
        "stands_out": [stands_out_generic(sport, gap_table, away, home), stands_out_generic(sport, gap_table, home, away)],
        "has_ball": [has_ball_generic(sport, gap_table, away, home), has_ball_generic(sport, gap_table, home, away)],
        "player_matchups": proj.map(|p| player_matchups_generic(sport, gap_table, p, home, away)),
        "rankings": [ { "team": home, "faces": away, "games": games(home), "rows": sort_rank(block(home, false)) },
                      { "team": away, "faces": home, "games": games(away), "rows": sort_rank(block(away, false)) } ],
        "team_stats": { "rows": ds.iter().map(|d| json!({ "key": d.key, "label": d.label, "help": d.help,
            "home_off": rank_value(&table, d, home, true), "home_def": rank_value(&table, d, home, false),
            "away_off": rank_value(&table, d, away, true), "away_def": rank_value(&table, d, away, false) })).collect::<Vec<_>>() },
    }))
}

fn intern(sport: &str, m: HashMap<String, f64>) -> HashMap<&'static str, f64> {
    let ds = defs(sport);
    m.into_iter().filter_map(|(k, v)| ds.iter().find(|d| d.key == k).map(|d| (d.key, v))).collect()
}

// ---------------------------------------------------------------------------
// NBA / NHL / MLB: offense vs defense, what stands out, player matchups
// ---------------------------------------------------------------------------

fn def_of(sport: &str, key: &str) -> Option<&'static Def> { defs(sport).iter().find(|d| d.key == key) }

/// Lower-case a label for mid-sentence use, keeping acronyms ("3PM", "OPS").
fn lc(label: &str) -> String {
    let w: Vec<&str> = label.split(' ').collect();
    w.iter().map(|x| if x.chars().filter(|c| c.is_ascii_uppercase()).count() > 1 { x.to_string() } else { x.to_lowercase() }).collect::<Vec<_>>().join(" ")
}

fn fmt_val(def: &Def, v: f64) -> String {
    match def.fmt { "pct" => format!("{:.1}%", v * 100.0), "d3" => format!("{v:.3}"), "d2" => format!("{v:.2}"), _ => format!("{v:.1}") }
}

/// (offense value, offense rank, defense-allowed value, defense rank, teams)
fn cells(table: &Table, def: &Def, off: &str, dfn: &str) -> Option<(f64, u64, f64, u64, u64)> {
    let o = rank_value(table, def, off, true);
    let d = rank_value(table, def, dfn, false);
    Some((o["value"].as_f64()?, o["rank"].as_u64()?, d["value"].as_f64()?, d["rank"].as_u64()?, o["of"].as_u64().unwrap_or(30)))
}

/// One-line read of a metric: what the offense does and what the defense allows.
fn metric_note(sport: &str, table: &Table, key: &str, off: &str, dfn: &str) -> Option<String> {
    let def = def_of(sport, key)?;
    let (ov, or, dv, dr, n) = cells(table, def, off, dfn)?;
    let label = lc(def.label);
    let lean = if def.dir == Dir::Usage { String::new() } else if or + 5 <= dr {
        format!(" Edge {off}: its offense ranks #{or} against a defense ranked #{dr} of {n}.")
    } else if dr + 5 <= or { format!(" Edge {dfn}: its defense ranks #{dr} against an offense ranked #{or}.") } else { " Fairly even.".into() };
    Some(format!("{off} sits at {} in {label} (#{or} of {n}); {dfn} allows {} (#{dr}).{lean}", fmt_val(def, ov), fmt_val(def, dv)))
}

fn side_rows(sport: &str, table: &Table, off: &str, dfn: &str, keys: &[&str]) -> Vec<Value> {
    keys.iter().filter_map(|k| def_of(sport, k)).map(|d| json!({ "key": d.key, "label": d.label, "help": d.help,
        "offense": rank_value(table, d, off, true), "defense": rank_value(table, d, dfn, false) })).collect()
}

type SectionDef = (&'static str, &'static [&'static str], &'static str, &'static str);

/// (title, metric keys, lead metric for the note, matchup group to explore)
fn sections(sport: &str) -> &'static [SectionDef] {
    const NBA: &[SectionDef] = &[
        ("Scoring efficiency", &["ortg", "pts", "efg", "fg3_pct", "ftr"], "ortg", "scoring"),
        ("Possessions & ball security", &["pace", "tov_pct", "orb_pct", "stl", "reb"], "tov_pct", "rebounding"),
        ("Shot profile & who scores", &["fg3a_rate", "fg3m", "pts_g", "pts_f", "pts_c", "ast_g"], "fg3m", "threes"),
    ];
    const NHL: &[SectionDef] = &[
        ("Creating shots", &["sf", "sog_f", "sog_d"], "sf", "shots"),
        ("Finishing", &["gf", "sh_pct", "pts_f", "pts_d"], "gf", "points"),
        ("Special teams & physical play", &["ppg", "pim", "hits", "blocks"], "ppg", "points"),
    ];
    const MLB: &[SectionDef] = &[
        ("Getting on base", &["avg", "obp", "bb_pct", "k_pct", "h"], "obp", "hitting"),
        ("Power", &["slg", "ops", "hr", "r"], "slg", "hitting"),
        ("On the bases", &["sb"], "sb", "pitching"),
    ];
    match sport { "nba" => NBA, "nhl" => NHL, "mlb" => MLB, _ => &[] }
}

fn has_ball_generic(sport: &str, table: &Table, off: &str, dfn: &str) -> Value {
    let secs: Vec<Value> = sections(sport).iter().map(|(title, keys, lead, explore)| json!({
        "title": title, "note": metric_note(sport, table, lead, off, dfn).unwrap_or_default(), "explore": explore,
        "rows": side_rows(sport, table, off, dfn, keys) })).collect();
    json!({ "offense": off, "defense": dfn, "sections": secs })
}

fn stands_out_generic(sport: &str, table: &Table, off: &str, dfn: &str) -> Value {
    let keys: &[&str] = match sport {
        "nba" => &["ortg", "efg", "tov_pct", "orb_pct", "fg3_pct", "ftr"],
        "nhl" => &["gf", "sf", "sh_pct", "ppg"],
        "mlb" => &["obp", "slg", "k_pct", "hr", "r"],
        _ => &[],
    };
    let best = keys.iter().filter_map(|k| { let d = def_of(sport, k)?; let c = cells(table, d, off, dfn)?; Some((d, c)) })
        .max_by_key(|(_, c)| (c.3 as i64 - c.1 as i64).abs());
    match best {
        Some((d, (ov, or, dv, dr, n))) => {
            let g = dr as i64 - or as i64;
            json!({ "team": off, "edge": g, "text": format!(
                "{off} ranks #{or} of {n} in {} ({}); {dfn} ranks #{dr} in what it allows ({}). That's the biggest rank {} for {off}'s offense in this game.",
                lc(d.label), fmt_val(d, ov), fmt_val(d, dv), if g >= 0 { "advantage" } else { "mismatch against it" }) })
        }
        None => json!({ "team": off, "edge": 0, "text": "Not enough games yet to compare." }),
    }
}

struct Group { key: &'static str, label: &'static str, cols: &'static [(&'static str, &'static str)], who: fn(&str, &str) -> Option<(&'static str, bool)> }

fn nba_pos(pos: &str, g: &'static str, f: &'static str, c: &'static str) -> &'static str { if pos.contains('C') { c } else if pos.contains('G') { g } else { f } }

fn groups(sport: &str) -> Vec<Group> {
    match sport {
        "nba" => vec![
            Group { key: "scoring", label: "Scoring", cols: &[("pts", "Pts"), ("pra", "PRA"), ("fga", "FGA"), ("fta", "FTA"), ("min", "Min")],
                who: |pos, _| Some((nba_pos(pos, "pts_g", "pts_f", "pts_c"), false)) },
            Group { key: "rebounding", label: "Rebounding & defense", cols: &[("reb", "Reb"), ("oreb", "OReb"), ("dreb", "DReb"), ("blk", "Blk"), ("stl", "Stl")],
                who: |pos, _| Some((if pos.contains('C') { "reb_c" } else { "reb" }, false)) },
            Group { key: "playmaking", label: "Playmaking", cols: &[("ast", "Ast"), ("pa", "P+A"), ("ra", "R+A"), ("tov", "TO")],
                who: |pos, _| Some((if pos.contains('G') { "ast_g" } else { "ast" }, false)) },
            Group { key: "threes", label: "Threes", cols: &[("fg3m", "3PM"), ("fg3a", "3PA")],
                who: |pos, _| Some((if pos.contains('G') { "fg3m_g" } else { "fg3m" }, false)) },
        ],
        "nhl" => vec![
            Group { key: "points", label: "Points", cols: &[("points", "Pts"), ("goals", "G"), ("assists", "A")],
                who: |pos, _| if pos == "G" { None } else { Some((if pos == "D" { "pts_d" } else { "pts_f" }, false)) } },
            Group { key: "shots", label: "Shots & blocks", cols: &[("sog", "SOG"), ("blocks", "Blk")],
                who: |pos, _| if pos == "G" { None } else { Some((if pos == "D" { "sog_d" } else { "sog_f" }, false)) } },
        ],
        "mlb" => vec![
            Group { key: "hitting", label: "Hitting", cols: &[("h", "H"), ("tb", "TB"), ("hr", "HR"), ("hrr", "H+R+RBI"), ("bb", "BB"), ("k", "K")],
                who: |_, role| if role == "starter" { None } else { Some(("ops", false)) } },
            Group { key: "pitching", label: "Starting pitchers", cols: &[("k", "K"), ("outs", "Outs"), ("er", "ER"), ("h", "H allowed"), ("bb", "BB")],
                who: |_, role| if role == "starter" { Some(("k_pct", true)) } else { None } },
        ],
        _ => vec![],
    }
}

/// Player groups: projected stats (with the main book line and the model's
/// probability) next to how the opponent ranks at what it allows that player type.
fn player_matchups_generic(sport: &str, table: &Table, proj: &Value, home: &str, away: &str) -> Value {
    let out: Vec<Value> = groups(sport).iter().map(|g| {
        let mut rows: Vec<(f64, Value)> = Vec::new();
        for side in ["away", "home"] {
            let (team, opp) = if side == "home" { (home, away) } else { (away, home) };
            for p in proj[side].as_array().into_iter().flatten() {
                let pos = p["pos"].as_str().unwrap_or("");
                let role = p["role"].as_str().unwrap_or("");
                let Some((key, opp_off)) = (g.who)(pos, role) else { continue };
                // skip deep bench: NBA under 12 minutes, MLB under 3 PA
                let st = &p["stats"];
                if sport == "nba" && st["min"]["mean"].as_f64().unwrap_or(0.0) < 12.0 { continue; }
                if sport == "mlb" && role != "starter" && st["pa"]["mean"].as_f64().unwrap_or(0.0) < 3.0 { continue; }
                let mut cols = serde_json::Map::new();
                for (k, _) in g.cols {
                    let v = &st[*k];
                    if v.is_null() { continue; }
                    let over = v["books"].as_array().and_then(|b| b.iter().find(|q| q["side"] == "over")).cloned();
                    cols.insert(k.to_string(), json!({ "mean": v["mean"], "line": over.as_ref().map(|q| q["line"].clone()),
                        "price": over.as_ref().map(|q| q["price"].clone()), "book": over.as_ref().map(|q| q["book"].clone()),
                        "p_over": over.as_ref().map(|q| q["p_model"].clone()) }));
                }
                if cols.is_empty() { continue; }
                let sort = st[g.cols[0].0]["mean"].as_f64().unwrap_or(0.0);
                let opp_allows = def_of(sport, key).map(|d| rank_value(table, d, opp, opp_off)).unwrap_or(Value::Null);
                rows.push((sort, json!({ "id": p["id"], "name": p["name"], "pos": pos, "team": team, "opponent": opp, "stats": cols,
                    "opp_allows": opp_allows, "opp_side": if opp_off { "offense" } else { "defense" } })));
            }
        }
        rows.sort_by(|a, b| b.0.total_cmp(&a.0));
        json!({ "key": g.key, "label": g.label, "cols": g.cols.iter().map(|(k, l)| json!({ "key": k, "label": l })).collect::<Vec<_>>(),
            "rows": rows.into_iter().map(|r| r.1).take(40).collect::<Vec<_>>() })
    }).collect();
    json!(out)
}

// ---------------------------------------------------------------------------
// Game report: a written rundown of one game from the payload (every sport)
// ---------------------------------------------------------------------------

fn odds_s(p: i64) -> String { if p > 0 { format!("+{p}") } else { p.to_string() } }
fn pct0(x: f64) -> String { format!("{:.0}%", x * 100.0) }

pub fn report(g: &Value) -> Value {
    let e = &g["event"];
    let (h, a) = (e["home"]["abbr"].as_str().unwrap_or("HOME"), e["away"]["abbr"].as_str().unwrap_or("AWAY"));
    let mut secs: Vec<Value> = Vec::new();
    let mut push = |title: &str, paras: Vec<String>| {
        let p: Vec<String> = paras.into_iter().filter(|x| !x.is_empty()).collect();
        if !p.is_empty() { secs.push(json!({ "title": title, "paragraphs": p })); }
    };

    // the number
    let m = &e["model"];
    let mut num = Vec::new();
    if let (Some(hs), Some(as_), Some(wh)) = (m["home_score"].as_f64(), m["away_score"].as_f64(), m["win_home"].as_f64()) {
        let (fav, pf) = if wh >= 0.5 { (h, wh) } else { (a, 1.0 - wh) };
        num.push(format!("Our {} simulation has {a} {as_:.1}, {h} {hs:.1} (total {:.1}). {fav} wins {} of simulated games.",
            m["engine"].as_str().unwrap_or("model"), hs + as_, pct0(pf)));
    } else {
        num.push("No frozen model forecast for this game yet; the autopilot freezes one before it starts.".into());
    }
    let q = |mk: &str, sd: &str| -> Option<String> {
        let b = &e["lines"][mk][sd];
        let price = b["price"].as_i64()?;
        let line = b["line"].as_f64().map(|l| if mk == "spread" && l > 0.0 { format!("+{l} ") } else { format!("{l} ") }).unwrap_or_default();
        Some(format!("{line}{} ({})", odds_s(price), b["book"].as_str().unwrap_or("")))
    };
    let mut market = Vec::new();
    if let (Some(x), Some(y)) = (q("moneyline", "away"), q("moneyline", "home")) { market.push(format!("moneyline {a} {x} / {h} {y}")); }
    if let Some(x) = q("spread", "home") { market.push(format!("{h} {x}")); }
    if let Some(x) = q("total", "over") { market.push(format!("total o{x}")); }
    if !market.is_empty() { num.push(format!("Best prices: {}.", market.join("; "))); }
    let edges: Vec<&Value> = g["edges"].as_array().into_iter().flatten().collect();
    if let Some(best) = edges.iter().filter(|x| x["ev"].as_f64().unwrap_or(-1.0) > 0.0)
        .max_by(|x, y| x["ev"].as_f64().unwrap_or(0.0).total_cmp(&y["ev"].as_f64().unwrap_or(0.0))) {
        let side = match best["side"].as_str().unwrap_or("") { "home" => h.to_string(), "away" => a.to_string(), s => s.to_string() };
        num.push(format!("Biggest model-vs-price gap: {} {side}{} at {} ({}), model {} vs {} implied. Game-line models rarely beat closing lines in our backtests, so treat this as a lead, not a bet.",
            best["market"].as_str().unwrap_or(""), best["line"].as_f64().map(|l| format!(" {l}")).unwrap_or_default(),
            odds_s(best["price"].as_i64().unwrap_or(0)), best["book"].as_str().unwrap_or(""),
            pct0(best["p_model"].as_f64().unwrap_or(0.0)), pct0(best["p_implied"].as_f64().unwrap_or(0.0))));
    } else if !edges.is_empty() {
        num.push("The model doesn't beat the best available price on any main line.".into());
    }
    push("The number", num);

    // matchup keys
    let an = &g["analysis"];
    let mut keys: Vec<String> = an["stands_out"].as_array().into_iter().flatten().filter_map(|x| x["text"].as_str().map(|s| s.to_string())).collect();
    for hb in an["has_ball"].as_array().into_iter().flatten() {
        if let Some(n) = hb["sections"][0]["note"].as_str() { keys.push(n.to_string()); }
    }
    push("Matchup keys", keys);

    // injuries
    let is_q = |i: &Value| i["status"].as_str().map(|s| { let l = s.to_lowercase(); l.contains("questionable") || l.contains("day-to-day") || l.contains("probable") }).unwrap_or(false);
    let inj: Vec<String> = g["espn"]["injuries"].as_array().into_iter().flatten().filter_map(|t| {
        let items = t["data"].as_array()?;
        if items.is_empty() { return None; }
        let out: Vec<String> = items.iter().filter(|i| !is_q(i)).take(5).map(|i| format!("{} ({})", i["name"].as_str().unwrap_or(""), i["pos"].as_str().unwrap_or(""))).collect();
        let qs = items.iter().filter(|i| is_q(i)).count();
        Some(format!("{}: {}{}", t["team"].as_str().unwrap_or(""),
            if out.is_empty() { "no one ruled out".into() } else { format!("out/doubtful {}", out.join(", ")) },
            if qs > 0 { format!("; {qs} more questionable or day-to-day") } else { String::new() }))
    }).collect();
    push("Injuries", if inj.is_empty() { vec!["No injuries listed by ESPN.".into()] } else { inj });

    // form
    let form: Vec<String> = g["espn"]["last_five"].as_array().into_iter().flatten().filter_map(|t| {
        let rs: Vec<&str> = t["data"].as_array()?.iter().filter_map(|x| x["result"].as_str()).collect();
        if rs.is_empty() { return None; }
        let w = rs.iter().filter(|r| **r == "W").count();
        let ats = g["espn"]["ats"].as_array().and_then(|v| v.iter().find(|x| x["team"] == t["team"]))
            .and_then(|x| x["data"].as_array().and_then(|d| d.first()).map(|r| format!(", ATS {}", r["summary"].as_str().unwrap_or(""))));
        Some(format!("{} is {w}-{} in its last {} ({}){}.", t["team"].as_str().unwrap_or(""), rs.len() - w, rs.len(), rs.join(""), ats.unwrap_or_default()))
    }).collect();
    push("Form", form);

    // props to watch: the blended probability (same as AI Picks) against the
    // no-vig market, positive EV at the quoted price; implausible gaps set aside
    let mut props: Vec<(f64, String, String)> = Vec::new();
    let mut aside: Vec<(f64, String, String)> = Vec::new();
    for side in ["away", "home"] {
        for p in g["projections"][side].as_array().into_iter().flatten() {
            let name = p["name"].as_str().unwrap_or("").to_string();
            for (k, st) in p["stats"].as_object().into_iter().flatten() {
                for b in st["books"].as_array().into_iter().flatten() {
                    let (Some(pm), Some(price), Some(line)) = (b["p_model"].as_f64(), b["price"].as_i64(), b["line"].as_f64()) else { continue };
                    let mk = b["p_market"].as_f64().unwrap_or_else(|| implied(price as i32));
                    let proj = st["mean"].as_f64().unwrap_or(0.0);
                    let recent = st["recent"]["avg"].as_f64().map(|a| match st["recent"]["games"].as_u64().unwrap_or(0) {
                        1 => format!(", only 1 recent game ({a:.1})"), n => format!(", last-{n} avg {a:.1}") }).unwrap_or_default();
                    let sd = b["side"].as_str().unwrap_or("");
                    if b["flag"] == "implausible" {
                        let raw = b["p_raw"].as_f64().unwrap_or(pm);
                        aside.push((raw - mk, name.clone(), format!("{name} {sd} {line} {}: raw model {} vs market {} (projection {proj:.1}{recent})",
                            crate::picks::stat_label(k).to_lowercase(), pct0(raw), pct0(mk))));
                        continue;
                    }
                    let ev = pm * payout(price as i32) - (1.0 - pm);
                    if pm - mk > 0.02 && ev > 0.0 {
                        props.push((pm - mk, name.clone(), format!("{name} {sd} {line} {} at {} ({}): {} vs {} market, projection {proj:.1}{recent}.",
                            crate::picks::stat_label(k).to_lowercase(), odds_s(price), b["book"].as_str().unwrap_or(""), pct0(pm), pct0(mk))));
                    }
                }
            }
        }
    }
    let top = |mut v: Vec<(f64, String, String)>, n: usize| -> Vec<String> {
        v.sort_by(|x, y| y.0.total_cmp(&x.0));
        let mut seen = std::collections::HashSet::new();
        v.into_iter().filter(|(_, k, _)| seen.insert(k.clone())).take(n).map(|x| x.2).collect()
    };
    let mut watch = top(props, 5);
    let aside = top(aside, 3);
    if !watch.is_empty() || !aside.is_empty() {
        if watch.is_empty() { watch.push("No prop clears the price once the model is blended with the player's history and the market.".into()); }
        else { watch.push("Probabilities are blended like AI Picks (model, the player's own history, the no-vig market), before its track-record grade caps.".into()); }
        if !aside.is_empty() {
            watch.push(format!("Set aside (model and market disagree by more than {} points, which is almost always a minutes, role or data problem rather than an edge): {}.",
                25, aside.join("; ")));
        }
        push("Props to watch", watch);
    }

    // picks
    let picks: Vec<&Value> = g["picks"].as_array().into_iter().flatten().filter(|p| p["tier"] != "hidden").collect();
    if !picks.is_empty() {
        let top: Vec<String> = picks.iter().take(3).map(|p| format!("{} {} {}{} {} ({}, edge {:+.1}%)", p["grade"].as_str().unwrap_or(""),
            p["subject"].as_str().unwrap_or(""), p["side"].as_str().unwrap_or(""), p["line"].as_f64().map(|l| format!(" {l}")).unwrap_or_default(),
            odds_s(p["price"].as_i64().unwrap_or(0)), p["book"].as_str().unwrap_or(""), p["edge"].as_f64().unwrap_or(0.0) * 100.0)).collect();
        push("AI picks", vec![format!("{} published for this game. Top: {}.", picks.len(), top.join("; "))]);
    }
    json!({ "generated_at": Utc::now(), "sections": secs })
}

// ---------------------------------------------------------------------------
// Live scoreboard: GET /hub/{sport}/game/{id}/live (ESPN, ~15 s cache)
// ---------------------------------------------------------------------------

/// "Shea Theodore" / "S. Theodore" / "Kenneth Walker III" -> "s theodore" / "k walker".
pub fn name_key(name: &str) -> String {
    let clean: String = name.chars().map(|c| match c {
        'á' | 'à' | 'ä' | 'â' | 'ã' => 'a', 'é' | 'è' | 'ë' | 'ê' => 'e', 'í' | 'ì' | 'ï' | 'î' => 'i', 'ó' | 'ò' | 'ö' | 'ô' | 'õ' => 'o',
        'ú' | 'ù' | 'ü' | 'û' => 'u', 'ñ' => 'n', 'ç' => 'c', 'Á' => 'a', 'É' => 'e', 'Í' => 'i', 'Ó' => 'o', 'Ú' => 'u',
        '.' | '\'' | ',' => ' ', '-' => ' ', c => c,
    }).collect::<String>().to_lowercase();
    let parts: Vec<&str> = clean.split_whitespace().filter(|w| !matches!(*w, "jr" | "sr" | "ii" | "iii" | "iv" | "v")).collect();
    match (parts.first(), parts.last()) {
        (Some(f), Some(l)) if parts.len() > 1 => format!("{} {}", &f[..f.char_indices().nth(1).map(|x| x.0).unwrap_or(f.len())], l),
        (Some(f), _) => f.to_string(),
        _ => String::new(),
    }
}

fn split_pair(s: &str) -> Option<(f64, f64)> {
    let (a, b) = s.split_once(['-', '/'])?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

/// ESPN box-score row -> our stat keys (the same keys as the projections).
fn live_stats(sport: &str, group: &str, labels: &[String], vals: &[String]) -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    let get = |l: &str| labels.iter().position(|x| x == l).and_then(|i| vals.get(i)).map(|s| s.as_str());
    let n = |l: &str| get(l).and_then(|s| s.trim_start_matches('+').parse::<f64>().ok());
    let mut put = |k: &str, v: Option<f64>| if let Some(v) = v { m.insert(k.into(), json!(v)); };
    match sport {
        "nba" => {
            let (pts, reb, ast, stl, blk) = (n("PTS"), n("REB"), n("AST"), n("STL"), n("BLK"));
            put("min", n("MIN")); put("pts", pts); put("reb", reb); put("ast", ast); put("stl", stl); put("blk", blk); put("tov", n("TO"));
            put("oreb", n("OREB")); put("dreb", n("DREB"));
            if let Some((a, b)) = get("FG").and_then(split_pair) { put("fgm", Some(a)); put("fga", Some(b)); }
            if let Some((a, b)) = get("3PT").and_then(split_pair) { put("fg3m", Some(a)); put("fg3a", Some(b)); }
            if let Some((a, b)) = get("FT").and_then(split_pair) { put("ftm", Some(a)); put("fta", Some(b)); }
            if let (Some(p), Some(r), Some(a)) = (pts, reb, ast) { put("pra", Some(p + r + a)); put("pr", Some(p + r)); put("pa", Some(p + a)); put("ra", Some(r + a)); }
            if let (Some(s), Some(b)) = (stl, blk) { put("stocks", Some(s + b)); }
        }
        "nhl" => {
            if group == "goalies" { put("saves", n("SV")); put("goals_against", n("GA")); }
            else {
                let (g, a) = (n("G"), n("A"));
                put("goals", g); put("assists", a); put("sog", n("SOG")); put("blocks", n("BS")); put("hits", n("HT"));
                if let (Some(g), Some(a)) = (g, a) { put("points", Some(g + a)); }
            }
        }
        "mlb" => {
            if labels.iter().any(|l| l == "IP") {
                // pitching: innings "5.2" = 17 outs
                if let Some(ip) = get("IP") { let (w, f) = ip.split_once('.').unwrap_or((ip, "0")); if let (Ok(w), Ok(f)) = (w.parse::<f64>(), f.parse::<f64>()) { put("outs", Some(w * 3.0 + f)); } }
                put("k", n("K")); put("h", n("H")); put("er", n("ER")); put("r", n("R")); put("bb", n("BB")); put("hr", n("HR"));
            } else {
                let (h, r, rbi) = (n("H"), n("R"), n("RBI"));
                put("h", h); put("r", r); put("rbi", rbi); put("hr", n("HR")); put("bb", n("BB")); put("k", n("K")); put("ab", n("AB"));
                if let (Some(h), Some(r), Some(b)) = (h, r, rbi) { put("hrr", Some(h + r + b)); }
            }
        }
        "nfl" => match group {
            "passing" => {
                if let Some((c, a)) = get("C/ATT").and_then(split_pair) { put("completions", Some(c)); put("pass_att", Some(a)); }
                put("pass_yds", n("YDS")); put("pass_td", n("TD")); put("interceptions", n("INT"));
            }
            "rushing" => { put("carries", n("CAR")); put("rush_yds", n("YDS")); put("rush_td", n("TD")); }
            "receiving" => { put("receptions", n("REC")); put("rec_yds", n("YDS")); put("rec_td", n("TD")); put("targets", n("TGTS")); }
            "kicking" => {
                if let Some((a, _)) = get("FG").and_then(split_pair) { put("fgm", Some(a)); }
                if let Some((a, _)) = get("XP").and_then(split_pair) { put("xpm", Some(a)); }
            }
            _ => {}
        },
        _ => {}
    }
    m
}

pub async fn live(state: web::Data<State>, path: web::Path<(String, String)>) -> impl Responder {
    let (sport, id) = path.into_inner();
    match live_inner(&state, &sport, &id).await { Ok(v) => HttpResponse::Ok().json(v), Err(e) => err(e) }
}

async fn live_inner(state: &State, sport: &str, id: &str) -> Result<Value, String> {
    let path = espn_path(sport).ok_or("unknown sport")?;
    let sum = espn(state, &format!("https://site.api.espn.com/apis/site/v2/sports/{path}/summary?event={id}"), Duration::from_secs(15)).await?;
    let comp = &sum["header"]["competitions"][0];
    let st = &comp["status"];
    let teams: Vec<Value> = comp["competitors"].as_array().into_iter().flatten().map(|c| {
        let lines: Vec<Option<f64>> = c["linescores"].as_array().into_iter().flatten().map(|l| num(&l["displayValue"]).or_else(|| num(&l["value"]))).collect();
        let score = num(&c["score"]).or_else(|| if lines.is_empty() { None } else { Some(lines.iter().flatten().sum()) });
        json!({ "side": c["homeAway"], "id": s(&c["team"]["id"]), "abbr": canon(sport, &s(&c["team"]["abbreviation"]).unwrap_or_default()),
            "logo": s(&c["team"]["logos"][0]["href"]).or_else(|| s(&c["team"]["logo"])), "score": score, "linescores": lines,
            "possession": c["possession"].as_bool().unwrap_or(false) })
    }).collect();

    // box score: raw ESPN groups for display + our stat keys per player
    let mut players: Vec<Value> = Vec::new();
    let boxes: Vec<Value> = sum["boxscore"]["players"].as_array().into_iter().flatten().map(|t| {
        let team = canon(sport, &s(&t["team"]["abbreviation"]).unwrap_or_default());
        let groups: Vec<Value> = t["statistics"].as_array().into_iter().flatten().filter_map(|g| {
            let labels: Vec<String> = g["labels"].as_array()?.iter().filter_map(|x| x.as_str().map(|x| x.to_string())).collect();
            let gname = s(&g["name"]).or_else(|| s(&g["type"])).unwrap_or_else(|| if labels.iter().any(|l| l == "IP") { "pitching".into() } else { "batting".into() });
            let rows: Vec<Value> = g["athletes"].as_array().into_iter().flatten().filter_map(|a| {
                let vals: Vec<String> = a["stats"].as_array()?.iter().map(|x| x.as_str().unwrap_or("").to_string()).collect();
                let name = s(&a["athlete"]["displayName"])?;
                let pid = s(&a["athlete"]["id"]);
                if !vals.is_empty() {
                    players.push(json!({ "id": pid, "name": name, "key": name_key(&name), "team": team, "group": gname,
                        "stats": live_stats(sport, &gname, &labels, &vals) }));
                }
                Some(json!({ "id": pid, "name": name, "short": s(&a["athlete"]["shortName"]), "pos": s(&a["athlete"]["position"]["abbreviation"]),
                    "starter": a["starter"].as_bool().unwrap_or(false), "dnp": a["didNotPlay"].as_bool().unwrap_or(false) || vals.is_empty(),
                    "reason": s(&a["reason"]), "stats": vals }))
            }).collect();
            if rows.is_empty() { return None; }
            let totals: Option<Vec<String>> = g["totals"].as_array().map(|v| v.iter().map(|x| x.as_str().unwrap_or("").to_string()).collect());
            Some(json!({ "name": gname, "title": s(&g["text"]).or_else(|| s(&g["displayName"])), "labels": labels, "rows": rows, "totals": totals }))
        }).collect();
        json!({ "team": team, "logo": s(&t["team"]["logo"]), "groups": groups })
    }).collect();
    let team_stats: Vec<Value> = sum["boxscore"]["teams"].as_array().into_iter().flatten().map(|t| json!({
        "team": canon(sport, &s(&t["team"]["abbreviation"]).unwrap_or_default()),
        "stats": t["statistics"].as_array().into_iter().flatten().filter_map(|x| Some(json!({ "label": s(&x["label"]).or_else(|| s(&x["name"]))?, "value": s(&x["displayValue"]) })))
            .collect::<Vec<_>>() })).collect();

    // plays, newest first (NFL keeps them inside drives; MLB drops pitch-by-pitch rows)
    let mut raw: Vec<Value> = sum["plays"].as_array().cloned().unwrap_or_default();
    if raw.is_empty() {
        for d in sum["drives"]["previous"].as_array().into_iter().flatten() { raw.extend(d["plays"].as_array().cloned().unwrap_or_default()); }
        if let Some(c) = sum["drives"]["current"]["plays"].as_array() { raw.extend(c.iter().cloned()); }
    }
    let plays: Vec<Value> = raw.iter().rev().filter(|p| {
        let ty = p["type"]["type"].as_str().unwrap_or("");
        !(sport == "mlb" && (p["summaryType"] == "P" || ty.contains("batterpitcher") || ty == "start-inning"))
            && p["text"].as_str().map(|t| !t.is_empty()).unwrap_or(false)
    }).take(40).map(|p| json!({
        "id": p["id"], "period": p["period"]["number"], "period_label": s(&p["period"]["displayValue"]), "clock": s(&p["clock"]["displayValue"]),
        "text": p["text"], "away": p["awayScore"], "home": p["homeScore"], "scoring": p["scoringPlay"].as_bool().unwrap_or(false),
        "team_id": s(&p["team"]["id"]) })).collect();

    // ESPN's live win probability (home), thinned to <= 120 points
    let wp: Vec<f64> = sum["winprobability"].as_array().into_iter().flatten().filter_map(|x| x["homeWinPercentage"].as_f64()).collect();
    let step = (wp.len() / 120).max(1);
    let series: Vec<f64> = wp.iter().step_by(step).map(|x| r3(*x)).chain(wp.last().map(|x| r3(*x))).collect();

    Ok(json!({
        "id": id, "sport": sport, "fetched_at": Utc::now(),
        "status": { "state": s(&st["type"]["state"]), "detail": s(&st["type"]["shortDetail"]).or_else(|| s(&st["type"]["detail"])),
            "clock": s(&st["displayClock"]), "period": st["period"], "completed": st["type"]["completed"].as_bool().unwrap_or(false) },
        "teams": teams, "box": boxes, "team_stats": team_stats, "players": players, "plays": plays,
        "win_prob": if wp.is_empty() { Value::Null } else { json!({ "home": wp.last(), "series": series }) },
        "situation": if sum["situation"].is_object() { json!({ "down_distance": s(&sum["situation"]["shortDownDistanceText"]).or_else(|| s(&sum["situation"]["downDistanceText"])),
            "possession": s(&sum["situation"]["possession"]), "last_play": s(&sum["situation"]["lastPlay"]["text"]),
            "balls": sum["situation"]["balls"], "strikes": sum["situation"]["strikes"], "outs": sum["situation"]["outs"],
            "on_first": sum["situation"]["onFirst"].is_object(), "on_second": sum["situation"]["onSecond"].is_object(), "on_third": sum["situation"]["onThird"].is_object() }) } else { Value::Null },
    }))
}

// ---------------------------------------------------------------------------
// MonsterGPT brief: a compact summary of one game for the LLM
// ---------------------------------------------------------------------------

fn fmt_rank_row(m: &Value) -> String {
    format!("{} {} (#{})", m["label"].as_str().unwrap_or(""), m["value"], m["rank"])
}

/// Compact game summary (a few KB) from the full game payload.
pub fn brief(g: &Value) -> Value {
    let e = &g["event"];
    let (h, a) = (e["home"]["abbr"].as_str().unwrap_or(""), e["away"]["abbr"].as_str().unwrap_or(""));
    let best = |m: &str, sd: &str| {
        let q = &e["lines"][m][sd];
        if q.is_object() { json!({ "line": q["line"], "price": q["price"], "book": q["book"] }) } else { Value::Null }
    };
    let is_q = |i: &Value| i["status"].as_str().map(|s| s.to_lowercase().contains("questionable")).unwrap_or(false);
    let inj: Vec<Value> = g["espn"]["injuries"].as_array().into_iter().flatten().map(|t| {
        let items: Vec<Value> = t["data"].as_array().cloned().unwrap_or_default();
        json!({ "team": t["team"],
            "out_or_doubtful": items.iter().filter(|i| !is_q(i)).take(6)
                .map(|i| format!("{} ({}, {})", i["name"].as_str().unwrap_or(""), i["pos"].as_str().unwrap_or(""), i["status"].as_str().unwrap_or("")))
                .collect::<Vec<_>>(),
            "questionable": items.iter().filter(|i| is_q(i)).take(6).map(|i| i["name"].clone()).collect::<Vec<_>>() })
    }).collect();
    let an = &g["analysis"];
    let ranks: Vec<Value> = an["rankings"].as_array().into_iter().flatten().map(|r| {
        let rows: Vec<Value> = r["rows"].as_array().cloned().unwrap_or_default();
        json!({ "defense": r["team"], "games": r["games"],
            "best_allowed": rows.iter().take(4).map(fmt_rank_row).collect::<Vec<_>>(),
            "worst_allowed": rows.iter().rev().take(4).map(fmt_rank_row).collect::<Vec<_>>() })
    }).collect();
    let mut proj: Vec<Value> = Vec::new();
    for sd in ["away", "home"] {
        for p in g["projections"][sd].as_array().cloned().unwrap_or_default().into_iter().take(5) {
            let mut stats = serde_json::Map::new();
            if let Some(o) = p["stats"].as_object() {
                let mut v: Vec<(&String, &Value)> = o.iter().filter(|(_, s)| s["mean"].as_f64().unwrap_or(0.0) > 0.05).collect();
                v.sort_by_key(|(_, s)| std::cmp::Reverse(s["books"].is_array()));
                for (k, s) in v.into_iter().take(4) {
                    let line = s["books"].as_array().and_then(|b| b.iter().find(|q| q["side"] == "over")).cloned();
                    stats.insert(k.clone(), json!({ "mean": s["mean"], "line": line.as_ref().map(|q| q["line"].clone()),
                        "p_over": line.as_ref().map(|q| q["p_model"].clone()) }));
                }
            }
            proj.push(json!({ "name": p["name"], "team": p["team"], "pos": p["pos"], "stats": stats }));
        }
    }
    let notes: Vec<Value> = an["has_ball"].as_array().into_iter().flatten()
        .flat_map(|hb| hb["sections"].as_array().cloned().unwrap_or_default().into_iter().map(|s| s["note"].clone())).collect();
    let picks: Vec<Value> = g["picks"].as_array().into_iter().flatten().filter(|p| p["tier"] != "hidden").take(5).map(|p| json!({
        "pick": format!("{} {} {}", p["subject"].as_str().unwrap_or(""), p["side"].as_str().unwrap_or(""), p["line"].as_f64().map(|l| l.to_string()).unwrap_or_default()),
        "market": p["market"], "price": p["price"], "book": p["book"], "grade": p["grade"], "p_model": p["p_model"], "edge": p["edge"] })).collect();
    let mut score = serde_json::Map::new();
    score.insert(a.to_string(), e["model"]["away_score"].clone());
    score.insert(h.to_string(), e["model"]["home_score"].clone());
    let mut win = serde_json::Map::new();
    win.insert(a.to_string(), e["model"]["win_away"].clone());
    win.insert(h.to_string(), e["model"]["win_home"].clone());
    json!({
        "game": format!("{a} @ {h}"), "sport": g["sport"], "start": e["start"], "status": e["status"]["detail"], "venue": e["venue"]["name"],
        "weather": e["weather"], "records": { "away": e["away"]["record"], "home": e["home"]["record"] },
        "model": { "engine": e["model"]["engine"], "score": score, "total": e["model"]["total"], "spread_home": e["model"]["spread_home"], "win_prob": win },
        "best_lines": { "moneyline": { "away": best("moneyline", "away"), "home": best("moneyline", "home") },
                        "spread": { "away": best("spread", "away"), "home": best("spread", "home") },
                        "total": { "over": best("total", "over"), "under": best("total", "under") } },
        "model_vs_market": g["edges"],
        "stands_out": an["stands_out"].as_array().map(|v| v.iter().map(|x| x["text"].clone()).collect::<Vec<_>>()),
        "matchup_notes": notes,
        "season_records_su_ats_ou": an["season_records"], "defense_rankings": ranks, "injuries": inj, "top_projections": proj,
        "ai_picks": picks, "data_note": an["data_note"],
    })
}

pub async fn game_brief(state: web::Data<State>, path: web::Path<(String, String)>) -> impl Responder {
    let (sport, id) = path.into_inner();
    match game_inner(&state, &sport, &id).await { Ok(v) => HttpResponse::Ok().json(brief(&v)), Err(e) => err(e) }
}

#[derive(Deserialize)]
pub struct BriefQ { q: String, sport: Option<String> }

/// GET /hub/brief?q=<question>[&sport=]: find games the question names (team
/// name, nickname, city or code) on the current slates and return their briefs.
pub async fn find_brief(state: web::Data<State>, q: web::Query<BriefQ>) -> impl Responder {
    let words: Vec<String> = q.q.split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() >= 2).map(|w| w.to_string()).collect();
    let text = format!(" {} ", words.iter().map(|w| w.to_lowercase()).collect::<Vec<_>>().join(" "));
    let sports: Vec<&str> = match q.sport.as_deref() { Some(s) if SPORTS.contains(&s) => vec![s], _ => SPORTS.to_vec() };
    let mut hits: Vec<(usize, String, String)> = Vec::new();
    for sport in sports {
        let Ok(sch) = schedule_inner(&state, sport, &ScheduleQ { season: None, week: None, date: None }).await else { continue };
        for g in sch["games"].as_array().into_iter().flatten() {
            if g["status"]["completed"].as_bool().unwrap_or(false) { continue; }
            let mut score = 0;
            for side in ["home", "away"] {
                let t = &g[side];
                let abbr = t["abbr"].as_str().unwrap_or("");
                let espn = t["espn_abbr"].as_str().unwrap_or("");
                let name = t["name"].as_str().unwrap_or("").to_lowercase();
                let short = t["short"].as_str().unwrap_or("").to_lowercase();
                let city_hit = name.rsplit_once(' ').map(|(city, _)| city.len() > 3 && text.contains(&format!(" {city} "))).unwrap_or(false);
                let hit = words.iter().any(|w| w == abbr || w == espn)
                    || (!short.is_empty() && text.contains(&format!(" {short} ")))
                    || (!name.is_empty() && text.contains(&format!(" {name} ")))
                    || city_hit;
                if hit { score += 1; }
            }
            if score > 0 { hits.push((score, sport.to_string(), g["id"].as_str().unwrap_or("").to_string())); }
        }
    }
    hits.sort_by(|a, b| b.0.cmp(&a.0));
    let top = hits.first().map(|h| h.0).unwrap_or(0);
    let mut out = Vec::new();
    for (_, sport, id) in hits.into_iter().filter(|h| h.0 == top).take(2) {
        if let Ok(v) = game_inner(&state, &sport, &id).await { out.push(brief(&v)); }
    }
    HttpResponse::Ok().json(json!({ "games": out }))
}

// ---------------------------------------------------------------------------
// Autopilot
// ---------------------------------------------------------------------------

/// Keep the hub fresh: NFL efficiency/charting/schedule, and pre-warm today's schedules.
pub async fn refresh(state: &State) -> Value {
    let today = et_date(Utc::now());
    let season = crate::nfl_hub::season_of(today);
    let nfl = crate::nfl_hub::ingest(state, &[season]).await;
    let mut warmed = json!({});
    for sport in SPORTS {
        let q = ScheduleQ { season: None, week: None, date: None };
        warmed[*sport] = json!(schedule_inner(state, sport, &q).await.map(|v| v["games"].as_array().map(|a| a.len()).unwrap_or(0)).map_err(|e| e));
    }
    json!({ "nfl": nfl.unwrap_or_else(|e| json!({ "error": e })), "schedules": warmed })
}

/// Every minute (autopilot): live data for games in progress, so the Live tab and
/// scores are warm, and the full game page for games starting within 90 minutes.
pub async fn live_warm(state: &State) -> Value {
    let mut out = json!({});
    for sport in SPORTS {
        let q = ScheduleQ { season: None, week: None, date: None };
        let Ok(sch) = schedule_inner(state, sport, &q).await else { continue };
        let (mut live, mut soon) = (0, 0);
        for g in sch["games"].as_array().into_iter().flatten() {
            let Some(id) = g["id"].as_str() else { continue };
            match g["status"]["state"].as_str() {
                Some("in") => { if live_inner(state, sport, id).await.is_ok() { live += 1; } }
                Some("pre") => {
                    let mins = g["start"].as_str().and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                        .map(|t| (t.with_timezone(&Utc) - Utc::now()).num_minutes()).unwrap_or(i64::MAX);
                    if (0..=90).contains(&mins) && game_inner(state, sport, id).await.is_ok() { soon += 1; }
                }
                _ => {}
            }
        }
        out[*sport] = json!({ "live": live, "starting_soon": soon });
    }
    out
}

pub async fn refresh_http(state: web::Data<State>) -> impl Responder { HttpResponse::Ok().json(refresh(&state).await) }

#[cfg(test)]
mod tests {
    #[test]
    fn live_name_keys_and_stats() {
        assert_eq!(name_key("Shea Theodore"), "s theodore");
        assert_eq!(name_key("S. Theodore"), "s theodore");
        assert_eq!(name_key("Kenneth Walker III"), "k walker");
        assert_eq!(name_key("Yandy D\u{ed}az"), "y diaz");
        let l: Vec<String> = ["MIN", "PTS", "FG", "3PT", "REB", "AST", "STL", "BLK"].iter().map(|x| x.to_string()).collect();
        let v: Vec<String> = ["30", "21", "8-15", "3-7", "9", "4", "1", "2"].iter().map(|x| x.to_string()).collect();
        let m = live_stats("nba", "", &l, &v);
        assert_eq!(m["pra"], json!(34.0));
        assert_eq!(m["fg3m"], json!(3.0));
        let l: Vec<String> = ["IP", "H", "K"].iter().map(|x| x.to_string()).collect();
        let v: Vec<String> = ["5.2", "4", "7"].iter().map(|x| x.to_string()).collect();
        assert_eq!(live_stats("mlb", "pitching", &l, &v)["outs"], json!(17.0));
    }

    #[test]
    fn label_case_keeps_acronyms() {
        assert_eq!(lc("3PM / game - guards"), "3PM / game - guards");
        assert_eq!(lc("Offensive rating"), "offensive rating");
        assert_eq!(lc("OPS"), "OPS");
    }

    use super::*;

    #[test]
    fn pmf_tail_probabilities() {
        let st = json!({ "pmf": [0.2, 0.3, 0.5], "pmf_offset": 0, "lines": [] });
        let (o, u) = p_over_under(&st, 0.5).unwrap();
        assert!((o - 0.8).abs() < 1e-9 && (u - 0.2).abs() < 1e-9);
        let (o, u) = p_over_under(&st, 1.0).unwrap();
        assert!((o - 0.5).abs() < 1e-9 && (u - 0.2).abs() < 1e-9); // push at 1 excluded
        assert!((mean_of(&st).unwrap() - 1.3).abs() < 1e-9);
    }

    #[test]
    fn best_line_uses_consensus_line() {
        let q = |line: f64, price: i32, book: &str| Quote { market: "spread".into(), side: "home".into(), line: Some(line), price, book: book.into(),
            link: None, open_line: None, open_price: None };
        let v = best_lines(&[q(-3.0, -110, "a"), q(-3.0, -105, "b"), q(-2.5, -130, "c")]);
        assert_eq!(v["spread"]["home"]["book"], "b");
        assert_eq!(v["spread"]["home"]["line"], -3.0);
    }

    #[test]
    fn et_date_rolls_back_late_games() {
        let t = DateTime::parse_from_rfc3339("2026-11-01T00:30:00Z").unwrap().with_timezone(&Utc);
        assert_eq!(et_date(t), NaiveDate::from_ymd_opt(2026, 10, 31).unwrap());
    }
}
