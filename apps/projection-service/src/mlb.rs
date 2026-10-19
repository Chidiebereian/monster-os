//! MLB: data ingestion, player prop models, and the game simulator API.
//!
//! Data (MLB Stats API, statsapi.mlb.com — free, no key; MLB Advanced Media
//! copyright, personal/non-commercial use; not redistributed):
//! * schedule + final scores -> sports_games (sport 'mlb', game_id = gamePk);
//! * per-game box scores -> one player_game_logs row per player-game with the
//!   batting line, batting-order slot, starter flags and pitching line;
//! * a sample of play-by-play games -> the empirical baserunning transition
//!   table (mlb_model_params 'baserunning').
//!
//! Models:
//! * per-stat empirical-Bayes count models (mlb-{stat}-v1), identical in form
//!   to NFL/NBA/NHL, so the Projections page, Monte Carlo, hit rates and cheat
//!   sheets work for MLB;
//! * the plate-appearance game simulator (mlb_sim.rs, model mlb-pa-sim-v1):
//!   per-player outcome rates partially pooled toward the league with
//!   method-of-moments beta-binomial strengths, home/park factors, empirical
//!   starter workload, team bullpen. Simulations only use games BEFORE the
//!   simulated game's date (no leakage) and are stored with seed + inputs.

use std::collections::HashMap;
use std::time::Instant;

use actix_web::{web, HttpResponse, Responder};
use futures::stream::{self, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_postgres::types::Json;

use crate::mlb_sim::{self, Baserunning, GameInput, Move, RatePosterior, TeamInput, TransitionObs, NCAT};
use crate::{fit_count, Family, Projection, State, StatDef};

const API: &str = "https://statsapi.mlb.com/api/v1";
pub const SIM_MODEL: &str = "mlb-pa-sim-v1";

// Batter props count games with a plate appearance; pitcher props count STARTS
// only (p_gs = 1), since that is what pitcher prop markets are offered on.
pub(crate) const MLB_BATTER_STATS: &[StatDef] = &[
    StatDef { key: "hits",         label: "Hits",            column: "h",     opportunity: "pa", family: Family::Count, unit: "H" },
    StatDef { key: "total_bases",  label: "Total Bases",     column: "tb",    opportunity: "pa", family: Family::Count, unit: "TB" },
    StatDef { key: "home_runs",    label: "Home Runs",       column: "hr",    opportunity: "pa", family: Family::Count, unit: "HR" },
    StatDef { key: "runs",         label: "Runs",            column: "r",     opportunity: "pa", family: Family::Count, unit: "R" },
    StatDef { key: "rbi",          label: "RBI",             column: "rbi",   opportunity: "pa", family: Family::Count, unit: "RBI" },
    StatDef { key: "hrr",          label: "Hits+Runs+RBI",   column: "hrr",   opportunity: "pa", family: Family::Count, unit: "H+R+RBI" },
    StatDef { key: "singles",      label: "Singles",         column: "1b",    opportunity: "pa", family: Family::Count, unit: "1B" },
    StatDef { key: "doubles",      label: "Doubles",         column: "2b",    opportunity: "pa", family: Family::Count, unit: "2B" },
    StatDef { key: "walks",        label: "Walks",           column: "bb",    opportunity: "pa", family: Family::Count, unit: "BB" },
    StatDef { key: "batter_k",     label: "Batter Strikeouts", column: "k",   opportunity: "pa", family: Family::Count, unit: "K" },
    StatDef { key: "stolen_bases", label: "Stolen Bases",    column: "sb",    opportunity: "pa", family: Family::Count, unit: "SB" },
];
pub(crate) const MLB_PITCHER_STATS: &[StatDef] = &[
    StatDef { key: "pitcher_k",      label: "Pitcher Strikeouts", column: "p_k",       opportunity: "p_gs", family: Family::Count, unit: "K" },
    StatDef { key: "outs",           label: "Outs Recorded",      column: "p_outs",    opportunity: "p_gs", family: Family::Count, unit: "outs" },
    StatDef { key: "hits_allowed",   label: "Hits Allowed",       column: "p_h",       opportunity: "p_gs", family: Family::Count, unit: "H" },
    StatDef { key: "earned_runs",    label: "Earned Runs",        column: "p_er",      opportunity: "p_gs", family: Family::Count, unit: "ER" },
    StatDef { key: "walks_allowed",  label: "Walks Allowed",      column: "p_bb",      opportunity: "p_gs", family: Family::Count, unit: "BB" },
    StatDef { key: "batters_faced",  label: "Batters Faced",      column: "p_bf",      opportunity: "p_gs", family: Family::Count, unit: "BF" },
    StatDef { key: "hr_allowed",     label: "Home Runs Allowed",  column: "p_hr",      opportunity: "p_gs", family: Family::Count, unit: "HR" },
    StatDef { key: "pitches",        label: "Pitch Count",        column: "p_pitches", opportunity: "p_gs", family: Family::Count, unit: "P" },
];

fn all_stats() -> impl Iterator<Item = &'static StatDef> {
    MLB_BATTER_STATS.iter().chain(MLB_PITCHER_STATS)
}
fn stat_def(key: &str) -> Option<&'static StatDef> {
    all_stats().find(|s| s.key == key)
}
fn model_id(stat: &str) -> String {
    format!("mlb-{stat}-v1")
}
fn internal(m: impl std::fmt::Display) -> HttpResponse {
    eprintln!("projection-service(mlb): {m}");
    HttpResponse::InternalServerError().json(json!({ "error": m.to_string() }))
}
fn bad_request(m: impl Into<String>) -> HttpResponse {
    HttpResponse::BadRequest().json(json!({ "error": m.into() }))
}
fn unprocessable(m: impl Into<String>) -> HttpResponse {
    HttpResponse::UnprocessableEntity().json(json!({ "error": m.into() }))
}
fn dberr(e: &tokio_postgres::Error) -> String {
    e.as_db_error().map(|d| format!("{} ({})", d.message(), d.detail().unwrap_or(""))).unwrap_or_else(|| e.to_string())
}
fn r3(x: f64) -> f64 { (x * 1000.0).round() / 1000.0 }
fn r4(x: f64) -> f64 { (x * 10000.0).round() / 10000.0 }

async fn get_json(http: &reqwest::Client, url: &str) -> Result<Value, String> {
    let mut last = String::new();
    for attempt in 0..3u64 {
        if attempt > 0 {
            tokio_sleep(500 * attempt).await;
        }
        match http.get(url).send().await.and_then(|r| r.error_for_status()) {
            Ok(r) => match r.json::<Value>().await {
                Ok(v) => return Ok(v),
                Err(e) => last = format!("{url}: json {e}"),
            },
            Err(e) => last = format!("{url}: {e}"),
        }
    }
    Err(last)
}
async fn tokio_sleep(ms: u64) {
    actix_web::rt::time::sleep(std::time::Duration::from_millis(ms)).await;
}

// --------------------------------------------------------------------------
// Parsing (pure)
// --------------------------------------------------------------------------

fn game_type(t: &str) -> &'static str {
    match t {
        "R" => "REG",
        "F" | "D" | "L" | "W" => "POST",
        "S" | "E" => "PRE",
        "A" => "ASG",
        _ => "OTHER",
    }
}

/// One player's line in one game.
#[derive(Debug, Clone)]
pub(crate) struct LogRow {
    pub player_id: String,
    pub name: String,
    pub position: String,
    pub team: String,
    pub opponent: String,
    pub stats: Value,
}

/// Parse a boxscore into player-game rows. `abbr` maps team id -> abbreviation.
pub(crate) fn parse_boxscore(box_: &Value, abbr: &HashMap<i64, String>) -> Vec<LogRow> {
    let mut out = Vec::new();
    let side_abbr = |s: &str| {
        let id = box_["teams"][s]["team"]["id"].as_i64().unwrap_or(0);
        abbr.get(&id).cloned().or_else(|| box_["teams"][s]["team"]["abbreviation"].as_str().map(String::from)).unwrap_or_default()
    };
    let (away, home) = (side_abbr("away"), side_abbr("home"));
    for (side, team, opp, is_home) in [("away", &away, &home, 0.0), ("home", &home, &away, 1.0)] {
        let Some(players) = box_["teams"][side]["players"].as_object() else { continue };
        for p in players.values() {
            let Some(id) = p["person"]["id"].as_i64() else { continue };
            let b = &p["stats"]["batting"];
            let pi = &p["stats"]["pitching"];
            let g = |v: &Value, k: &str| v[k].as_f64().unwrap_or(0.0);
            let pa = g(b, "plateAppearances");
            let bf = g(pi, "battersFaced");
            if pa <= 0.0 && bf <= 0.0 {
                continue;
            }
            let mut m = serde_json::Map::new();
            m.insert("home".into(), json!(is_home));
            if pa > 0.0 {
                let (h, d, t, hr) = (g(b, "hits"), g(b, "doubles"), g(b, "triples"), g(b, "homeRuns"));
                let (r, rbi) = (g(b, "runs"), g(b, "rbi"));
                let order = p["battingOrder"].as_str().and_then(|s| s.parse::<i64>().ok());
                for (k, v) in [
                    ("pa", pa), ("ab", g(b, "atBats")), ("h", h), ("1b", h - d - t - hr), ("2b", d), ("3b", t), ("hr", hr),
                    ("tb", g(b, "totalBases")), ("r", r), ("rbi", rbi), ("bb", g(b, "baseOnBalls")),
                    ("ibb", g(b, "intentionalWalks")), ("hbp", g(b, "hitByPitch")), ("k", g(b, "strikeOuts")),
                    ("sb", g(b, "stolenBases")), ("cs", g(b, "caughtStealing")), ("sf", g(b, "sacFlies")),
                    ("sh", g(b, "sacBunts")), ("gidp", g(b, "groundIntoDoublePlay")),
                    ("hrr", h + r + rbi), ("xbh", d + t + hr),
                ] {
                    m.insert(k.into(), json!(v));
                }
                if let Some(o) = order {
                    m.insert("bat_order".into(), json!(o / 100));
                    m.insert("bat_start".into(), json!(if o % 100 == 0 { 1 } else { 0 }));
                }
            }
            if bf > 0.0 {
                let (h, d, t, hr) = (g(pi, "hits"), g(pi, "doubles"), g(pi, "triples"), g(pi, "homeRuns"));
                for (k, v) in [
                    ("p_bf", bf), ("p_outs", g(pi, "outs")), ("p_k", g(pi, "strikeOuts")), ("p_bb", g(pi, "baseOnBalls")),
                    ("p_hbp", g(pi, "hitBatsmen")), ("p_h", h), ("p_1b", h - d - t - hr), ("p_2b", d), ("p_3b", t),
                    ("p_hr", hr), ("p_r", g(pi, "runs")), ("p_er", g(pi, "earnedRuns")),
                    ("p_pitches", g(pi, "numberOfPitches")), ("p_gs", g(pi, "gamesStarted")),
                    ("p_win", g(pi, "wins")),
                ] {
                    m.insert(k.into(), json!(v));
                }
            }
            let pos = if pa <= 0.0 { "P".to_string() } else { p["position"]["abbreviation"].as_str().unwrap_or("").to_string() };
            out.push(LogRow {
                player_id: id.to_string(),
                name: p["person"]["fullName"].as_str().unwrap_or("").to_string(),
                position: pos,
                team: team.clone(),
                opponent: opp.clone(),
                stats: Value::Object(m),
            });
        }
    }
    out
}

fn pa_category(event_type: &str) -> Option<usize> {
    use crate::mlb_sim::*;
    Some(match event_type {
        "single" => S1,
        "double" => S2,
        "triple" => S3,
        "home_run" => HR,
        "walk" | "intent_walk" => BB,
        "hit_by_pitch" => HBP,
        "strikeout" | "strikeout_double_play" | "strikeout_triple_play" => K,
        "field_out" | "force_out" | "grounded_into_double_play" | "grounded_into_triple_play" | "double_play"
        | "triple_play" | "fielders_choice" | "fielders_choice_out" | "field_error" | "sac_fly" | "sac_bunt"
        | "sac_fly_double_play" | "sac_bunt_double_play" => OUT,
        _ => return None,
    })
}

fn base_idx(s: Option<&str>) -> Option<usize> {
    match s {
        Some("1B") => Some(1),
        Some("2B") => Some(2),
        Some("3B") => Some(3),
        _ => None,
    }
}

/// Replay one game's play-by-play, tracking who is on which base, and emit a
/// transition (event, bases, outs) -> where the batter and each runner ended
/// up, plus the RBI credited. Only ball-in-play events are kept; plays whose
/// recorded outs don't reconcile with the runner movements are dropped.
pub(crate) fn pbp_transitions(pbp: &Value) -> Vec<(u8, u8, u8, Move)> {
    let mut out = Vec::new();
    let Some(plays) = pbp["allPlays"].as_array() else { return out };
    let mut half: Option<(i64, bool)> = None;
    let mut occ: [Option<i64>; 4] = [None; 4]; // index 1..3 = runner id on base
    let mut outs_before: i64 = 0;
    for play in plays {
        let key = (play["about"]["inning"].as_i64().unwrap_or(0), play["about"]["isTopInning"].as_bool().unwrap_or(false));
        if half != Some(key) {
            half = Some(key);
            occ = [None; 4];
            outs_before = 0;
        }
        let batter = play["matchup"]["batter"]["id"].as_i64().unwrap_or(-1);
        let outs_after = play["count"]["outs"].as_i64().unwrap_or(outs_before);
        // group movements by runner, preserving order
        let mut by_runner: Vec<(i64, Option<usize>, Option<&str>, bool)> = Vec::new(); // id, origin, final end, out
        if let Some(rs) = play["runners"].as_array() {
            for r in rs {
                let id = r["details"]["runner"]["id"].as_i64().unwrap_or(-1);
                let origin = base_idx(r["movement"]["originBase"].as_str());
                let end = r["movement"]["end"].as_str();
                let is_out = r["movement"]["isOut"].as_bool().unwrap_or(false);
                match by_runner.iter_mut().find(|x| x.0 == id) {
                    Some(x) => { x.2 = end; x.3 = x.3 || is_out; }
                    None => by_runner.push((id, origin, end, is_out)),
                }
            }
        }
        // reconcile the start state with the runners' recorded origins
        for (id, origin, _, _) in &by_runner {
            if *id == batter && origin.is_none() {
                continue;
            }
            if let Some(o) = origin {
                for b in 1..=3 {
                    if occ[b] == Some(*id) { occ[b] = None; }
                }
                occ[*o] = Some(*id);
            }
        }
        let start_occ = occ;
        let mask: u8 = (1..=3).filter(|&b| start_occ[b].is_some()).fold(0, |m, b| m | (1 << (b - 1)));
        let ev = play["result"]["eventType"].as_str().unwrap_or("");
        let cat = pa_category(ev);
        let mut dest = [mlb_sim::D_NONE; 4];
        let mut new_occ: [Option<i64>; 4] = [None; 4];
        let mut outs_seen = 0i64;
        let mut ok = true;
        // runners who never moved stay where they were
        for b in 1..=3 {
            if let Some(id) = start_occ[b] {
                let mv = by_runner.iter().find(|x| x.0 == id);
                let d = match mv {
                    None => b as u8,
                    Some((_, _, end, out)) => {
                        if *out { mlb_sim::D_OUT } else {
                            match *end { Some("score") => mlb_sim::D_HOME, e => base_idx(e).map(|x| x as u8).unwrap_or(mlb_sim::D_OUT) }
                        }
                    }
                };
                dest[b] = d;
                if d == mlb_sim::D_OUT { outs_seen += 1; }
                if (1..=3).contains(&d) {
                    if new_occ[d as usize].is_some() { ok = false; }
                    new_occ[d as usize] = Some(id);
                }
            }
        }
        if cat.is_some() {
            let mv = by_runner.iter().find(|x| x.0 == batter && x.1.is_none());
            let d = match mv {
                None => mlb_sim::D_OUT, // batter not listed: retired without movement
                Some((_, _, end, out)) => {
                    if *out { mlb_sim::D_OUT } else {
                        match *end { Some("score") => mlb_sim::D_HOME, e => base_idx(e).map(|x| x as u8).unwrap_or(mlb_sim::D_OUT) }
                    }
                }
            };
            dest[0] = d;
            if d == mlb_sim::D_OUT { outs_seen += 1; }
            if (1..=3).contains(&d) {
                if new_occ[d as usize].is_some() { ok = false; }
                new_occ[d as usize] = Some(batter);
            }
        }
        if let (Some(c), true) = (cat, ok) {
            if let Some(slot) = mlb_sim::bip_slot(c) {
                if outs_after - outs_before == outs_seen && (0..3).contains(&outs_before) {
                    let rbi = play["result"]["rbi"].as_i64().unwrap_or(0).clamp(0, 4) as u8;
                    out.push((slot as u8, mask, outs_before as u8, Move { dest, rbi }));
                }
            }
        }
        occ = new_occ;
        outs_before = outs_after;
        if outs_before >= 3 {
            occ = [None; 4];
        }
    }
    out
}

// --------------------------------------------------------------------------
// Ingest
// --------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct IngestQuery {
    #[serde(default = "d_from")] from: i32,
    #[serde(default = "d_to")] to: i32,
    #[serde(default = "d_conc")] concurrency: usize,
    /// play-by-play games sampled (latest season) for the baserunning table
    #[serde(default = "d_pbp")] pbp_games: usize,
    /// re-download box scores already stored
    #[serde(default)] refresh: bool,
}
fn d_from() -> i32 { 2024 }
fn d_to() -> i32 { 2026 }
fn d_conc() -> usize { 6 }
fn d_pbp() -> usize { 800 }

const BOX_FIELDS: &str = "teams,away,home,team,id,abbreviation,players,person,fullName,position,battingOrder,stats,batting,pitching,\
plateAppearances,atBats,hits,doubles,triples,homeRuns,totalBases,runs,rbi,baseOnBalls,intentionalWalks,hitByPitch,strikeOuts,\
stolenBases,caughtStealing,sacFlies,sacBunts,groundIntoDoublePlay,battersFaced,outs,earnedRuns,numberOfPitches,gamesStarted,wins,hitBatsmen";
const PBP_FIELDS: &str = "allPlays,result,eventType,rbi,about,inning,isTopInning,count,outs,matchup,batter,id,runners,movement,originBase,end,isOut,details,runner";

struct Sched {
    pk: i64,
    season: i32,
    date: Option<chrono::NaiveDate>,
    home: String,
    away: String,
    status: String,
    final_: bool,
    home_score: Option<i32>,
    away_score: Option<i32>,
    gtype: &'static str,
}

async fn team_abbrs(http: &reqwest::Client, season: i32) -> Result<HashMap<i64, String>, String> {
    let v = get_json(http, &format!("{API}/teams?sportId=1&season={season}&fields=teams,id,abbreviation")).await?;
    Ok(v["teams"].as_array().map(|a| a.iter().filter_map(|t| Some((t["id"].as_i64()?, t["abbreviation"].as_str()?.to_string()))).collect()).unwrap_or_default())
}

async fn schedule(http: &reqwest::Client, url: &str, abbr: &HashMap<i64, String>) -> Result<Vec<Sched>, String> {
    let v = get_json(http, url).await?;
    let mut out = Vec::new();
    for d in v["dates"].as_array().into_iter().flatten() {
        for g in d["games"].as_array().into_iter().flatten() {
            let Some(pk) = g["gamePk"].as_i64() else { continue };
            let team = |s: &str| {
                let id = g["teams"][s]["team"]["id"].as_i64().unwrap_or(0);
                abbr.get(&id).cloned().unwrap_or_else(|| g["teams"][s]["team"]["abbreviation"].as_str().unwrap_or("").to_string())
            };
            let status = g["status"]["detailedState"].as_str().unwrap_or("").to_string();
            let final_ = g["status"]["abstractGameState"].as_str() == Some("Final")
                && !matches!(status.as_str(), "Postponed" | "Cancelled");
            out.push(Sched {
                pk,
                season: g["season"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0),
                date: g["officialDate"].as_str().and_then(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()),
                home: team("home"),
                away: team("away"),
                final_,
                home_score: g["teams"]["home"]["score"].as_i64().map(|x| x as i32),
                away_score: g["teams"]["away"]["score"].as_i64().map(|x| x as i32),
                gtype: game_type(g["gameType"].as_str().unwrap_or("")),
                status,
            });
        }
    }
    Ok(out)
}

pub async fn ingest(state: web::Data<State>, q: web::Query<IngestQuery>) -> impl Responder {
    let q = q.into_inner();
    let started = Instant::now();
    let http = &state.http;
    let have: std::collections::HashSet<String> = if q.refresh {
        Default::default()
    } else {
        match state.db.query("SELECT DISTINCT week FROM player_game_logs WHERE sport='mlb'", &[]).await {
            Ok(rows) => rows.iter().map(|r| r.get::<_, String>(0)).collect(),
            Err(e) => return internal(e),
        }
    };
    let mut report = Vec::new();
    let mut latest_finals: Vec<i64> = Vec::new();
    let mut names: HashMap<String, (String, String, String, chrono::NaiveDate)> = HashMap::new();
    for season in q.from..=q.to {
        let abbr = match team_abbrs(http, season).await { Ok(a) => a, Err(e) => return internal(e) };
        let url = format!(
            "{API}/schedule?sportId=1&season={season}&gameType=R,F,D,L,W&fields=dates,games,gamePk,season,officialDate,gameType,\
status,detailedState,abstractGameState,teams,away,home,team,id,score"
        );
        let games = match schedule(http, &url, &abbr).await { Ok(g) => g, Err(e) => return internal(e) };
        // schedule rows (idempotent upsert)
        for g in &games {
            if let Err(e) = state.db.execute(
                "INSERT INTO sports_games (sport, game_id, season, week, game_date, home_team, away_team, status, home_score, away_score, game_type) \
                 VALUES ('mlb', $1, $2, $1, $3, $4, $5, $6, $7, $8, $9) \
                 ON CONFLICT (sport, game_id) DO UPDATE SET season=EXCLUDED.season, game_date=EXCLUDED.game_date, home_team=EXCLUDED.home_team, \
                   away_team=EXCLUDED.away_team, status=EXCLUDED.status, home_score=EXCLUDED.home_score, away_score=EXCLUDED.away_score, \
                   game_type=EXCLUDED.game_type, updated_at=now()",
                &[&g.pk.to_string(), &g.season, &g.date, &g.home, &g.away, &g.status, &g.home_score, &g.away_score, &g.gtype],
            ).await { return internal(format!("schedule upsert: {}", dberr(&e))); }
        }
        let finals: Vec<&Sched> = games.iter().filter(|g| g.final_).collect();
        if season == q.to {
            latest_finals = finals.iter().filter(|g| g.gtype == "REG").map(|g| g.pk).collect();
        }
        // suspended games appear on both the original and the resumption date:
        // keep one entry per gamePk (the later, completed date)
        let mut seen = std::collections::HashSet::new();
        let mut todo: Vec<(i64, i32, Option<chrono::NaiveDate>)> = Vec::new();
        for g in finals.iter().rev() {
            if !have.contains(&g.pk.to_string()) && seen.insert(g.pk) {
                todo.push((g.pk, g.season, g.date));
            }
        }
        let (mut ok, mut failed, mut rows_written) = (0usize, 0usize, 0usize);
        // fetch concurrently, write in chunks
        for chunk in todo.chunks(60) {
            let fetched: Vec<(i64, i32, Option<chrono::NaiveDate>, Result<Value, String>)> = stream::iter(chunk.to_vec())
                .map(|(pk, s, d)| {
                    let http = http.clone();
                    async move { (pk, s, d, get_json(&http, &format!("{API}/game/{pk}/boxscore?fields={BOX_FIELDS}")).await) }
                })
                .buffer_unordered(q.concurrency.clamp(1, 12))
                .collect()
                .await;
            let mut pids = Vec::new(); let mut seasons = Vec::new(); let mut weeks = Vec::new(); let mut dates = Vec::new();
            let mut teams = Vec::new(); let mut opps = Vec::new(); let mut stats = Vec::new();
            for (pk, s, d, res) in fetched {
                let Ok(b) = res else { failed += 1; continue };
                ok += 1;
                for row in parse_boxscore(&b, &abbr) {
                    if let Some(date) = d {
                        let e = names.entry(row.player_id.clone()).or_insert((row.name.clone(), row.position.clone(), row.team.clone(), date));
                        if date >= e.3 { *e = (row.name.clone(), row.position.clone(), row.team.clone(), date); }
                    }
                    pids.push(row.player_id); seasons.push(s); weeks.push(pk.to_string()); dates.push(d);
                    teams.push(row.team); opps.push(row.opponent); stats.push(row.stats);
                }
            }
            let sj: Vec<Json<&Value>> = stats.iter().map(Json).collect();
            match state.db.execute(
                "INSERT INTO player_game_logs (sport, player_id, season, week, game_date, team, opponent, stats) \
                 SELECT 'mlb', * FROM UNNEST($1::text[], $2::int[], $3::text[], $4::date[], $5::text[], $6::text[], $7::jsonb[]) \
                 ON CONFLICT (sport, player_id, season, week) DO UPDATE SET game_date=EXCLUDED.game_date, team=EXCLUDED.team, \
                   opponent=EXCLUDED.opponent, stats=EXCLUDED.stats",
                &[&pids, &seasons, &weeks, &dates, &teams, &opps, &sj],
            ).await {
                Ok(n) => rows_written += n as usize,
                Err(e) => return internal(format!("logs insert: {}", dberr(&e))),
            }
        }
        report.push(json!({ "season": season, "scheduled": games.len(), "final": finals.len(),
            "boxscores_fetched": ok, "boxscores_failed": failed, "already_stored": finals.len() - todo.len(), "player_rows": rows_written }));
    }

    // players: latest name/position/team from this run, career span from the logs
    for (pid, (name, pos, team, _)) in &names {
        if let Err(e) = state.db.execute(
            "INSERT INTO sports_players (sport, player_id, display_name, position, team) VALUES ('mlb', $1, $2, $3, $4) \
             ON CONFLICT (sport, player_id) DO UPDATE SET display_name=EXCLUDED.display_name, position=EXCLUDED.position, \
               team=EXCLUDED.team, updated_at=now()",
            &[pid, name, pos, team],
        ).await { return internal(format!("player upsert: {e}")); }
    }
    if let Err(e) = state.db.execute(
        "UPDATE sports_players p SET first_season=a.f, last_season=a.l, games=a.n FROM \
           (SELECT player_id, min(season) f, max(season) l, count(*)::int n FROM player_game_logs WHERE sport='mlb' GROUP BY player_id) a \
         WHERE p.sport='mlb' AND p.player_id=a.player_id",
        &[],
    ).await { return internal(format!("player spans: {e}")); }

    // baserunning table from an evenly spaced sample of the latest season
    let mut pbp_report = json!(null);
    if q.pbp_games > 0 && !latest_finals.is_empty() {
        let step = (latest_finals.len() as f64 / q.pbp_games as f64).max(1.0);
        let sample: Vec<i64> = (0..q.pbp_games.min(latest_finals.len())).map(|i| latest_finals[(i as f64 * step) as usize]).collect();
        let results: Vec<Result<Value, String>> = stream::iter(sample.clone())
            .map(|pk| { let http = http.clone(); async move { get_json(&http, &format!("{API}/game/{pk}/playByPlay?fields={PBP_FIELDS}")).await } })
            .buffer_unordered(q.concurrency.clamp(1, 12))
            .collect().await;
        let mut counts: HashMap<(u8, u8, u8, Move), f64> = HashMap::new();
        let mut used = 0usize;
        for r in results.into_iter().flatten() {
            used += 1;
            for (slot, bases, outs, mv) in pbp_transitions(&r) {
                *counts.entry((slot, bases, outs, mv)).or_insert(0.0) += 1.0;
            }
        }
        let obs: Vec<TransitionObs> = counts.into_iter().map(|((slot, bases, outs, mv), n)| TransitionObs { slot, bases, outs, mv, n }).collect();
        let total: f64 = obs.iter().map(|o| o.n).sum();
        let source = format!("statsapi playByPlay, {used} regular-season games from {}", q.to);
        if let Err(e) = state.db.execute(
            "INSERT INTO mlb_model_params (key, params, source) VALUES ($3, $1, $2) \
             ON CONFLICT (key) DO UPDATE SET params=EXCLUDED.params, source=EXCLUDED.source, fitted_at=now()",
            &[&Json(json!({ "obs": obs })), &source, &format!("baserunning:{}", q.to)],
        ).await { return internal(format!("baserunning store: {e}")); }
        pbp_report = json!({ "games": used, "ball_in_play_transitions": total, "distinct_cells": obs.len() });
    }

    HttpResponse::Ok().json(json!({
        "sport": "mlb", "seasons": report, "players_touched": names.len(), "baserunning": pbp_report,
        "elapsed_s": started.elapsed().as_secs(),
        "source": "statsapi.mlb.com (MLB Advanced Media; personal, non-commercial use)",
    }))
}

// --------------------------------------------------------------------------
// Fit: per-stat prop models + simulator parameters
// --------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct FitQuery {
    #[serde(default = "d_min_games")] min_games: usize,
    #[serde(default = "d_prior_k")] prior_k: f64,
    #[serde(default = "d_halflife")] halflife: f64,
}
fn d_min_games() -> usize { 15 }
fn d_prior_k() -> f64 { 10.0 }
fn d_halflife() -> f64 { 120.0 }

fn fval(v: &Value, k: &str) -> f64 {
    v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0)
}
/// Per-PA outcome counts [K, BB, HBP, 1B, 2B, 3B, HR, OUT] from a batting line.
fn bat_counts(s: &Value) -> [f64; NCAT] {
    let (pa, k, bb, hbp) = (fval(s, "pa"), fval(s, "k"), fval(s, "bb"), fval(s, "hbp"));
    let (s1, s2, s3, hr) = (fval(s, "1b"), fval(s, "2b"), fval(s, "3b"), fval(s, "hr"));
    [k, bb, hbp, s1, s2, s3, hr, (pa - k - bb - hbp - s1 - s2 - s3 - hr).max(0.0)]
}
/// Same categories from a pitching line (per batter faced).
fn pit_counts(s: &Value) -> [f64; NCAT] {
    let (bf, k, bb, hbp) = (fval(s, "p_bf"), fval(s, "p_k"), fval(s, "p_bb"), fval(s, "p_hbp"));
    let (s1, s2, s3, hr) = (fval(s, "p_1b"), fval(s, "p_2b"), fval(s, "p_3b"), fval(s, "p_hr"));
    [k, bb, hbp, s1, s2, s3, hr, (bf - k - bb - hbp - s1 - s2 - s3 - hr).max(0.0)]
}
fn add(a: &mut [f64; NCAT], b: &[f64; NCAT], w: f64) {
    for c in 0..NCAT { a[c] += w * b[c]; }
}
fn rates(c: &[f64; NCAT]) -> [f64; NCAT] {
    let n: f64 = c.iter().sum();
    let mut r = [0.0; NCAT];
    if n > 0.0 { for i in 0..NCAT { r[i] = c[i] / n; } }
    r
}

pub async fn fit(state: web::Data<State>, q: web::Query<FitQuery>) -> impl Responder {
    let q = q.into_inner();
    let rows = match state.db.query(
        "SELECT player_id, season, week, team, opponent, stats FROM player_game_logs WHERE sport='mlb' \
         ORDER BY player_id, game_date NULLS LAST, week",
        &[],
    ).await { Ok(r) => r, Err(e) => return internal(e) };
    if rows.is_empty() {
        return internal("no MLB game logs stored; POST /ingest/mlb first");
    }
    let latest: i32 = rows.iter().map(|r| r.get::<_, i32>("season")).max().unwrap_or(0);

    // ---- 1. per-stat empirical-Bayes count models (same as other sports)
    let mut players: Vec<(String, Vec<Value>)> = Vec::new();
    for r in &rows {
        let pid: String = r.get("player_id");
        let s: Json<Value> = r.get("stats");
        match players.last_mut() {
            Some((p, v)) if *p == pid => v.push(s.0),
            _ => players.push((pid, vec![s.0])),
        }
    }
    let mut league: HashMap<&str, f64> = HashMap::new();
    for s in all_stats() {
        let (mut sum, mut cnt) = (0.0, 0.0);
        for (_, games) in &players {
            for g in games {
                if fval(g, s.opportunity) >= 1.0 { sum += fval(g, s.column); cnt += 1.0; }
            }
        }
        league.insert(s.key, if cnt > 0.0 { sum / cnt } else { 0.0 });
    }
    for s in all_stats() {
        if let Err(e) = state.db.execute(
            "INSERT INTO models (id, name, sport, version, type, status) VALUES ($1, $2, 'mlb', '1', $3, 'active') ON CONFLICT (id) DO NOTHING",
            &[&model_id(s.key), &format!("MLB {} (EB, recency-weighted)", s.label), &s.family.model_type()],
        ).await { return internal(format!("register {}: {e}", s.key)); }
    }
    if let Err(e) = state.db.execute(
        "INSERT INTO models (id, name, sport, version, type, status) VALUES ($1, 'MLB plate-appearance game simulator', 'mlb', '1', 'monte_carlo', 'active') ON CONFLICT (id) DO NOTHING",
        &[&SIM_MODEL],
    ).await { return internal(format!("register sim: {e}")); }
    let mut by_stat: HashMap<&str, usize> = HashMap::new();
    for (pid, games) in &players {
        for s in all_stats() {
            let vals: Vec<f64> = games.iter().filter(|g| fval(g, s.opportunity) >= 1.0).map(|g| fval(g, s.column)).collect();
            if vals.len() < q.min_games { continue; }
            let proj = fit_count(&vals, *league.get(s.key).unwrap_or(&0.0), q.prior_k, q.halflife);
            if let Err(e) = persist_projection(&state.db, pid, s, &proj).await { return internal(e); }
            *by_stat.entry(s.key).or_insert(0) += 1;
        }
    }

    // ---- 2. simulator parameters, one set per season (a game in season S is
    // simulated with parameters from a season that had finished before it)
    let seasons: std::collections::BTreeSet<i32> = rows.iter().map(|r| r.get::<_, i32>("season")).collect();
    let mut params = json!(null);
    for &season in &seasons {
        let p = season_params(&rows, season);
        if let Err(e) = state.db.execute(
            "INSERT INTO mlb_model_params (key, params, source) VALUES ($1, $2, $3)              ON CONFLICT (key) DO UPDATE SET params=EXCLUDED.params, source=EXCLUDED.source, fitted_at=now()",
            &[&format!("sim:{season}"), &Json(&p), &format!("player_game_logs (mlb), season {season}")],
        ).await { return internal(format!("store sim params: {e}")); }
        if season == latest { params = p; }
    }

    let summary: Vec<Value> = all_stats().map(|s| json!({
        "stat": s.key, "league_mean": r3(*league.get(s.key).unwrap_or(&0.0)), "players_fitted": by_stat.get(s.key).copied().unwrap_or(0),
    })).collect();
    HttpResponse::Ok().json(json!({ "sport": "mlb", "players_seen": players.len(), "stats": summary, "sim_params": params }))
}

/// Season totals per player: batters (all PAs) and pitchers (starts only).
fn season_units(rows: &[tokio_postgres::Row], season: i32) -> (HashMap<String, [f64; NCAT]>, HashMap<String, [f64; NCAT]>) {
    let mut bat: HashMap<String, [f64; NCAT]> = HashMap::new();
    let mut sp: HashMap<String, [f64; NCAT]> = HashMap::new();
    for r in rows.iter().filter(|r| r.get::<_, i32>("season") == season) {
        let s: Json<Value> = r.get("stats");
        let pid: String = r.get("player_id");
        if fval(&s.0, "pa") > 0.0 {
            add(bat.entry(pid.clone()).or_insert([0.0; NCAT]), &bat_counts(&s.0), 1.0);
        }
        if fval(&s.0, "p_gs") >= 1.0 {
            add(sp.entry(pid).or_insert([0.0; NCAT]), &pit_counts(&s.0), 1.0);
        }
    }
    (bat, sp)
}

/// Pooling strength chosen for FORECASTING: for players seen in both seasons,
/// the kappa whose shrunk prior-season rate best predicts next season's rate
/// (squared error weighted by next-season trials). Within-season spread
/// overstates how much of a rate carries over, so this is typically larger.
fn forecast_kappa(prev: &HashMap<String, [f64; NCAT]>, next: &HashMap<String, [f64; NCAT]>, min_trials: f64) -> Option<[f64; NCAT]> {
    let pairs: Vec<(&[f64; NCAT], &[f64; NCAT])> = prev.iter()
        .filter_map(|(k, a)| next.get(k).map(|b| (a, b)))
        .filter(|(a, b)| a.iter().sum::<f64>() >= min_trials && b.iter().sum::<f64>() >= min_trials)
        .collect();
    if pairs.len() < 30 {
        return None;
    }
    let mut lg = [0.0; NCAT];
    for (a, _) in &pairs { add(&mut lg, a, 1.0); }
    let lg = rates(&lg);
    let mut out = [0.0; NCAT];
    for c in 0..NCAT {
        let mut best = (f64::INFINITY, 0.0);
        for step in 0..=60 {
            let kappa = 10f64.powf(1.0 + step as f64 * 3.0 / 60.0); // 10 .. 10,000
            let sse: f64 = pairs.iter().map(|(a, b)| {
                let (na, nb) = (a.iter().sum::<f64>(), b.iter().sum::<f64>());
                let pred = (a[c] + kappa * lg[c]) / (na + kappa);
                nb * (b[c] / nb - pred).powi(2)
            }).sum();
            if sse < best.0 { best = (sse, kappa); }
        }
        out[c] = best.1.round();
    }
    Some(out)
}

/// Simulator parameters from one season of box scores: league per-PA outcome
/// rates, home/away factors, beta-binomial pooling strengths (batters and
/// pitchers), reliability-shrunk park factors, starter batters-faced
/// distribution and the earned share of runs.
fn season_params(rows: &[tokio_postgres::Row], season: i32) -> Value {
    let mut lg_bat = [0.0; NCAT];
    let mut home_bat = [0.0; NCAT];
    let mut away_bat = [0.0; NCAT];
    let mut bat_units: HashMap<String, [f64; NCAT]> = HashMap::new();
    let mut pit_units: HashMap<String, [f64; NCAT]> = HashMap::new();
    let mut bf_hist = vec![0u32; 61];
    let (mut r_sum, mut er_sum) = (0.0, 0.0);
    // park: rates in games at team X's park vs in X's road games (both teams' PAs)
    let mut at_park: HashMap<String, [f64; NCAT]> = HashMap::new();
    let mut road: HashMap<String, [f64; NCAT]> = HashMap::new();
    // lineup slot usage: (game, team, slot) -> (slot PAs, starter PAs)
    let mut slots: HashMap<(String, String, i64), (f64, f64)> = HashMap::new();
    let mut bench_bat = [0.0; NCAT];
    for r in rows.iter().filter(|r| r.get::<_, i32>("season") == season) {
        let s: Json<Value> = r.get("stats");
        let s = &s.0;
        let pid: String = r.get("player_id");
        let team: String = r.get::<_, Option<String>>("team").unwrap_or_default();
        let opp: String = r.get::<_, Option<String>>("opponent").unwrap_or_default();
        if fval(s, "pa") > 0.0 {
            let c = bat_counts(s);
            add(&mut lg_bat, &c, 1.0);
            if let Some(order) = s.get("bat_order").and_then(|v| v.as_f64()) {
                let e = slots.entry((r.get::<_, String>("week"), team.clone(), order as i64)).or_insert((0.0, 0.0));
                e.0 += fval(s, "pa");
                if fval(s, "bat_start") >= 1.0 { e.1 += fval(s, "pa") } else { add(&mut bench_bat, &c, 1.0) }
            }
            if fval(s, "home") >= 1.0 { add(&mut home_bat, &c, 1.0) } else { add(&mut away_bat, &c, 1.0) }
            add(bat_units.entry(pid.clone()).or_insert([0.0; NCAT]), &c, 1.0);
            let (park, visitor) = if fval(s, "home") >= 1.0 { (team.clone(), opp.clone()) } else { (opp.clone(), team.clone()) };
            add(at_park.entry(park.clone()).or_insert([0.0; NCAT]), &c, 1.0);
            // this game is a road game for the visitor
            add(road.entry(visitor).or_insert([0.0; NCAT]), &c, 1.0);
        }
        if fval(s, "p_bf") > 0.0 {
            add(pit_units.entry(pid).or_insert([0.0; NCAT]), &pit_counts(s), 1.0);
            r_sum += fval(s, "p_r");
            er_sum += fval(s, "p_er");
            if fval(s, "p_gs") >= 1.0 {
                bf_hist[(fval(s, "p_bf") as usize).min(60)] += 1;
            }
        }
    }
    let lg = rates(&lg_bat);
    let mut env_home = [1.0; NCAT];
    let mut env_away = [1.0; NCAT];
    let (rh, ra) = (rates(&home_bat), rates(&away_bat));
    for c in 0..NCAT {
        if lg[c] > 0.0 { env_home[c] = rh[c] / lg[c]; env_away[c] = ra[c] / lg[c]; }
    }
    let kappa_of = |units: &HashMap<String, [f64; NCAT]>| -> Vec<f64> {
        (0..NCAT).map(|c| {
            let u: Vec<(f64, f64)> = units.values().map(|x| (x[c], x.iter().sum::<f64>())).collect();
            mlb_sim::mom_kappa(&u, 100.0).unwrap_or(200.0)
        }).collect()
    };
    let k_bat_within = kappa_of(&bat_units);
    let k_pit_within = kappa_of(&pit_units);
    // prefer the year-over-year (forecasting) strengths when the previous
    // season is stored: (season-1 -> season) pairs
    let (pb, pp) = season_units(rows, season - 1);
    let (cb, cp) = season_units(rows, season);
    let fk_bat = forecast_kappa(&pb, &cb, 150.0);
    let fk_pit = forecast_kappa(&pp, &cp, 150.0);
    let k_bat: Vec<f64> = fk_bat.map(|k| k.to_vec()).unwrap_or_else(|| k_bat_within.clone());
    let k_pit: Vec<f64> = fk_pit.map(|k| k.to_vec()).unwrap_or_else(|| k_pit_within.clone());
    // park factors, shrunk by their estimated reliability across parks
    let mut park_raw: Vec<(String, [f64; NCAT], [f64; NCAT])> = Vec::new(); // team, pf, sampling var
    for (team, cp) in &at_park {
        let Some(cr) = road.get(team) else { continue };
        let (rp, rr) = (rates(cp), rates(cr));
        let (np, nr) = (cp.iter().sum::<f64>(), cr.iter().sum::<f64>());
        let mut pf = [1.0; NCAT];
        let mut var = [0.0; NCAT];
        for c in 0..NCAT {
            if rr[c] > 0.0 && rp[c] > 0.0 {
                pf[c] = rp[c] / rr[c];
                var[c] = pf[c].powi(2) * (1.0 / (rp[c] * np) + 1.0 / (rr[c] * nr));
            }
        }
        park_raw.push((team.clone(), pf, var));
    }
    let mut park: serde_json::Map<String, Value> = serde_json::Map::new();
    let mut reliability = [0.0; NCAT];
    if park_raw.len() >= 10 {
        for c in 0..NCAT {
            let n = park_raw.len() as f64;
            let mean = park_raw.iter().map(|p| p.1[c]).sum::<f64>() / n;
            let var_obs = park_raw.iter().map(|p| (p.1[c] - mean).powi(2)).sum::<f64>() / (n - 1.0);
            let samp = park_raw.iter().map(|p| p.2[c]).sum::<f64>() / n;
            reliability[c] = if var_obs > 0.0 { ((var_obs - samp) / var_obs).clamp(0.0, 1.0) } else { 0.0 };
        }
        for (team, pf, _) in &park_raw {
            let shrunk: Vec<f64> = (0..NCAT).map(|c| r4(1.0 + reliability[c] * (pf[c] - 1.0))).collect();
            park.insert(team.clone(), json!(shrunk));
        }
    }
    let earned_share = if r_sum > 0.0 { er_sum / r_sum } else { 0.9 };
    // Kaplan-Meier style hazard of a starting batter being replaced before his
    // next PA, given n PAs taken and his slot coming up again (s == T: censored)
    let mut sub_hazard = vec![0.0f64; 8];
    for n in 1..8usize {
        let nf = n as f64;
        let at_risk = slots.values().filter(|(t, st)| *st >= nf && *t > nf).count() as f64;
        let events = slots.values().filter(|(t, st)| *st == nf && *t > nf).count() as f64;
        sub_hazard[n] = if at_risk >= 50.0 { r4(events / at_risk) } else { sub_hazard[n - 1] };
    }
    let starter_share = {
        let (t, st): (f64, f64) = slots.values().fold((0.0, 0.0), |a, x| (a.0 + x.0, a.1 + x.1));
        if t > 0.0 { st / t } else { 1.0 }
    };
    json!({
        "sub_hazard": sub_hazard,
        "starter_share_of_slot_pa": r4(starter_share),
        "bench": rates(&bench_bat).iter().map(|x| r4(*x)).collect::<Vec<_>>(),
        "bench_pa": bench_bat.iter().sum::<f64>(),
        "season": season,
        "categories": mlb_sim::CATS,
        "league": lg.iter().map(|x| r4(*x)).collect::<Vec<_>>(),
        "env_home": env_home.iter().map(|x| r4(*x)).collect::<Vec<_>>(),
        "env_away": env_away.iter().map(|x| r4(*x)).collect::<Vec<_>>(),
        "kappa_batter": k_bat.iter().map(|x| x.round()).collect::<Vec<_>>(),
        "kappa_pitcher": k_pit.iter().map(|x| x.round()).collect::<Vec<_>>(),
        "kappa_source": if fk_bat.is_some() && fk_pit.is_some() { format!("year-over-year {} -> {season}", season - 1) } else { "within-season method of moments (no prior season stored)".to_string() },
        "kappa_batter_within_season": k_bat_within.iter().map(|x| x.round()).collect::<Vec<_>>(),
        "kappa_pitcher_within_season": k_pit_within.iter().map(|x| x.round()).collect::<Vec<_>>(),
        "park_factors": park,
        "park_reliability": reliability.iter().map(|x| r3(*x)).collect::<Vec<_>>(),
        "starter_bf_hist": bf_hist,
        "earned_share": r4(earned_share),
        "pa": lg_bat.iter().sum::<f64>(),
    })
}

async fn persist_projection(db: &tokio_postgres::Client, pid: &str, s: &StatDef, p: &Projection) -> Result<(), String> {
    let mid = model_id(s.key);
    db.execute(
        "INSERT INTO player_projections (sport, player_id, stat, game_id, model_id, proj_mean, proj_sd, dispersion, sample_games, career_mean, recent_mean, family, unit) \
         VALUES ('mlb', $1, $2, '', $3, $4, $5, $6, $7, $8, $9, $10, $11) \
         ON CONFLICT (sport, player_id, stat, game_id) DO UPDATE SET model_id=EXCLUDED.model_id, proj_mean=EXCLUDED.proj_mean, proj_sd=EXCLUDED.proj_sd, \
           dispersion=EXCLUDED.dispersion, sample_games=EXCLUDED.sample_games, career_mean=EXCLUDED.career_mean, recent_mean=EXCLUDED.recent_mean, family=EXCLUDED.family, unit=EXCLUDED.unit, as_of=now()",
        &[&pid, &s.key, &mid, &p.proj_mean, &p.proj_sd, &p.dispersion, &p.sample_games, &p.career_mean, &p.recent_mean, &s.family.as_str(), &s.unit],
    ).await.map_err(|e| format!("persist projection {pid}/{}: {e}", s.key))?;
    let q05 = (p.proj_mean - 1.645 * p.proj_sd).max(0.0);
    let q95 = p.proj_mean + 1.645 * p.proj_sd;
    db.execute(
        "INSERT INTO posterior_features (model_id, parameter, mean, sd, q05, q95, rhat, ess) VALUES ($1, $2, $3, $4, $5, $6, 1.0, NULL) \
         ON CONFLICT (model_id, parameter) DO UPDATE SET mean=EXCLUDED.mean, sd=EXCLUDED.sd, q05=EXCLUDED.q05, q95=EXCLUDED.q95",
        &[&mid, &format!("{}:{pid}", s.key), &p.proj_mean, &p.proj_sd, &q05, &q95],
    ).await.map(|_| ()).map_err(|e| format!("persist posterior {pid}/{}: {e}", s.key))
}

// --------------------------------------------------------------------------
// Read endpoints for the per-stat models (mirror NBA/NHL)
// --------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct ListQuery { limit: Option<i64>, min_games: Option<i32>, q: Option<String>, teams: Option<String> }

pub async fn list_stat(state: web::Data<State>, path: web::Path<String>, query: web::Query<ListQuery>) -> impl Responder {
    let stat = path.into_inner();
    let Some(def) = stat_def(&stat) else { return bad_request("unknown stat"); };
    let limit = query.limit.unwrap_or(50).clamp(1, 500);
    let min_games = query.min_games.unwrap_or(0);
    let search = query.q.clone().unwrap_or_default();
    let teams: Option<Vec<String>> = query.teams.as_ref().and_then(|s| {
        let v: Vec<String> = s.split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect();
        if v.is_empty() { None } else { Some(v) }
    });
    // default to players active in the latest stored season
    let min_last: Option<i32> = state.db.query_opt("SELECT max(last_season)::int AS c FROM sports_players WHERE sport='mlb'", &[])
        .await.ok().flatten().and_then(|r| r.get::<_, Option<i32>>("c"));
    let sql = "SELECT p.player_id, pl.display_name, pl.team, pl.position, pl.last_season, p.proj_mean, p.proj_sd, p.dispersion, \
                      p.sample_games, p.career_mean, p.recent_mean, p.market_line, p.market_implied, p.model_id, p.family, p.unit \
               FROM player_projections p JOIN sports_players pl ON pl.sport=p.sport AND pl.player_id=p.player_id \
               WHERE p.sport='mlb' AND p.stat=$1 AND p.sample_games >= $2 AND ($3='' OR pl.display_name ILIKE '%'||$3||'%') \
                 AND ($5::text[] IS NULL OR pl.team = ANY($5)) AND ($6::int IS NULL OR pl.last_season >= $6 OR $3<>'') \
               ORDER BY p.proj_mean DESC LIMIT $4";
    match state.db.query(sql, &[&def.key, &min_games, &search, &limit, &teams, &min_last]).await {
        Ok(rows) => {
            let out: Vec<Value> = rows.iter().map(proj_row_json).collect();
            HttpResponse::Ok().json(json!({ "sport": "mlb", "stat": def.key, "label": def.label, "unit": def.unit, "family": def.family.as_str(), "model_id": model_id(def.key), "count": out.len(), "projections": out }))
        }
        Err(e) => internal(e),
    }
}

pub async fn get_player_stat(state: web::Data<State>, path: web::Path<(String, String)>) -> impl Responder {
    let (stat, pid) = path.into_inner();
    let Some(def) = stat_def(&stat) else { return bad_request("unknown stat"); };
    let row = match state.db.query_opt(
        "SELECT p.player_id, pl.display_name, pl.team, pl.position, pl.last_season, p.proj_mean, p.proj_sd, p.dispersion, p.sample_games, p.career_mean, p.recent_mean, p.market_line, p.market_implied, p.model_id, p.family, p.unit \
         FROM player_projections p JOIN sports_players pl ON pl.sport=p.sport AND pl.player_id=p.player_id WHERE p.sport='mlb' AND p.stat=$1 AND p.player_id=$2",
        &[&def.key, &pid],
    ).await {
        Ok(Some(r)) => r,
        Ok(None) => return bad_request("no projection for that player/stat"),
        Err(e) => return internal(e),
    };
    let logs = state.db.query(
        "SELECT season, week, opponent, game_date, stats FROM player_game_logs WHERE sport='mlb' AND player_id=$1 \
           AND COALESCE((stats->>$2)::float8, 0) >= 1 ORDER BY game_date DESC NULLS LAST, week DESC LIMIT 20",
        &[&pid, &def.opportunity],
    ).await.unwrap_or_default();
    let recent: Vec<Value> = logs.iter().map(|r| {
        let stats: Json<Value> = r.get("stats");
        json!({ "season": r.get::<_, i32>("season"), "week": r.get::<_, String>("week"),
                "date": r.get::<_, Option<chrono::NaiveDate>>("game_date").map(|d| d.to_string()),
                "opponent": r.get::<_, Option<String>>("opponent"), "value": stats.0.get(def.column).cloned().unwrap_or(Value::Null) })
    }).collect();
    HttpResponse::Ok().json(json!({ "stat": def.key, "unit": def.unit, "projection": proj_row_json(&row), "recent_games": recent }))
}

pub async fn list_stats(_state: web::Data<State>) -> impl Responder {
    let out: Vec<Value> = all_stats().map(|s| json!({
        "key": s.key, "label": s.label, "unit": s.unit, "family": s.family.as_str(), "model_id": model_id(s.key),
        "group": if s.opportunity == "p_gs" { "pitcher" } else { "batter" },
    })).collect();
    HttpResponse::Ok().json(json!({ "sport": "mlb", "stats": out }))
}

fn proj_row_json(r: &tokio_postgres::Row) -> Value {
    let f = |c: &str| r.get::<_, Option<f64>>(c);
    json!({
        "player_id": r.get::<_, String>("player_id"), "name": r.get::<_, String>("display_name"),
        "team": r.get::<_, Option<String>>("team"), "position": r.get::<_, Option<String>>("position"),
        "last_season": r.get::<_, Option<i32>>("last_season"),
        "proj_mean": f("proj_mean"), "proj_sd": f("proj_sd"), "dispersion": f("dispersion"),
        "sample_games": r.get::<_, Option<i32>>("sample_games"), "career_mean": f("career_mean"), "recent_mean": f("recent_mean"),
        "market_line": f("market_line"), "market_implied": f("market_implied"), "model_id": r.get::<_, Option<String>>("model_id"),
        "family": r.get::<_, Option<String>>("family"), "unit": r.get::<_, Option<String>>("unit"),
    })
}

// --------------------------------------------------------------------------
// Slate + game simulation
// --------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct SlateQuery { date: Option<String>, days: Option<i64> }

/// Upcoming games with probable pitchers and (when posted) official lineups.
pub async fn slate(state: web::Data<State>, q: web::Query<SlateQuery>) -> impl Responder {
    let start = q.date.as_deref().and_then(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
        .unwrap_or_else(|| chrono::Utc::now().date_naive());
    let end = start + chrono::Duration::days(q.days.unwrap_or(7).clamp(0, 30));
    let abbr = team_abbrs(&state.http, start.format("%Y").to_string().parse().unwrap_or(2026)).await.unwrap_or_default();
    let url = format!("{API}/schedule?sportId=1&startDate={start}&endDate={end}&hydrate=probablePitcher,lineups,venue");
    let v = match get_json(&state.http, &url).await { Ok(v) => v, Err(e) => return internal(e) };
    let mut games = Vec::new();
    for d in v["dates"].as_array().into_iter().flatten() {
        for g in d["games"].as_array().into_iter().flatten() {
            let team = |s: &str| abbr.get(&g["teams"][s]["team"]["id"].as_i64().unwrap_or(0)).cloned().unwrap_or_default();
            let pp = |s: &str| g["teams"][s]["probablePitcher"].as_object().map(|p| json!({ "id": p.get("id"), "name": p.get("fullName") }));
            let lineup = |key: &str| g["lineups"][key].as_array().map(|a| a.len()).unwrap_or(0);
            games.push(json!({
                "game_pk": g["gamePk"], "date": g["officialDate"], "start": g["gameDate"], "status": g["status"]["detailedState"],
                "game_type": game_type(g["gameType"].as_str().unwrap_or("")),
                "away": team("away"), "home": team("home"), "venue": g["venue"]["name"],
                "away_probable": pp("away"), "home_probable": pp("home"),
                "lineups_posted": { "away": lineup("awayPlayers") == 9, "home": lineup("homePlayers") == 9 },
            }));
        }
    }
    HttpResponse::Ok().json(json!({ "sport": "mlb", "from": start.to_string(), "to": end.to_string(), "count": games.len(), "games": games }))
}

#[derive(Deserialize)]
pub struct SimQuery {
    #[serde(default = "d_n")] n: usize,
    seed: Option<u64>,
    #[serde(default = "d_draws")] draws: usize,
    /// override lineups / starters: comma-separated MLB person ids
    away_lineup: Option<String>,
    home_lineup: Option<String>,
    away_sp: Option<String>,
    home_sp: Option<String>,
    #[serde(default = "d_bat_hl")] batter_halflife: f64,
    #[serde(default = "d_pit_hl")] pitcher_halflife: f64,
    #[serde(default = "d_hook_starts")] hook_starts: usize,
    #[serde(default = "d_hook_prior")] hook_prior: f64,
    /// store the run in mlb_sim_runs (default true)
    store: Option<bool>,
}
fn d_n() -> usize { 100_000 }
fn d_draws() -> usize { 200 }
fn d_bat_hl() -> f64 { 150.0 }
fn d_pit_hl() -> f64 { 40.0 }
fn d_hook_starts() -> usize { 10 } // tuned on the 2025 walk-forward, validated on 2026
fn d_hook_prior() -> f64 { 1.0 }
pub const PRODUCTION_N: usize = 100_000;

struct Params {
    league: [f64; NCAT],
    env_home: [f64; NCAT],
    env_away: [f64; NCAT],
    k_bat: [f64; NCAT],
    k_pit: [f64; NCAT],
    park: HashMap<String, [f64; NCAT]>,
    bf_hist: Vec<u32>,
    earned_share: f64,
    season: i64,
    sub_hazard: Vec<f64>,
    bench: [f64; NCAT],
}
fn arr(v: &Value) -> Option<[f64; NCAT]> {
    let a = v.as_array()?;
    if a.len() != NCAT { return None; }
    let mut o = [0.0; NCAT];
    for i in 0..NCAT { o[i] = a[i].as_f64()?; }
    Some(o)
}
/// Parameters from the latest season whose data finished before the game:
/// the previous season for a regular-season game, the same season for a
/// postseason game. Returns (params, baserunning table, provenance).
async fn load_params(db: &tokio_postgres::Client, season: i32, postseason: bool) -> Result<(Params, Baserunning, Value), String> {
    let max = if postseason { season } else { season - 1 };
    let sql = "SELECT key, params, fitted_at::text AS t, source FROM mlb_model_params \
               WHERE key LIKE $1 || ':%' AND split_part(key, ':', 2)::int <= $2 \
               ORDER BY split_part(key, ':', 2)::int DESC LIMIT 1";
    let row = db.query_opt(sql, &[&"sim", &max]).await.map_err(|e| e.to_string())?
        .ok_or(format!("no simulator parameters for a season <= {max}; POST /fit/mlb (needs that season's box scores)"))?;
    let p: Json<Value> = row.get("params");
    let p = p.0;
    let br_row = db.query_opt(sql, &[&"baserunning", &max]).await.map_err(|e| e.to_string())?;
    let (br, br_key) = match br_row {
        Some(r) => {
            let v: Json<Value> = r.get("params");
            let obs: Vec<TransitionObs> = serde_json::from_value(v.0["obs"].clone()).map_err(|e| format!("baserunning: {e}"))?;
            (Baserunning::from_obs(&obs), Some((r.get::<_, String>("key"), r.get::<_, Option<String>>("source"))))
        }
        None => (Baserunning::default(), None),
    };
    let park = p["park_factors"].as_object().map(|m| m.iter().filter_map(|(k, v)| Some((k.clone(), arr(v)?))).collect()).unwrap_or_default();
    let provenance = json!({
        "sim_params": row.get::<_, String>("key"), "sim_params_fitted_at": row.get::<_, String>("t"),
        "baserunning": br_key.as_ref().map(|k| k.0.clone()), "baserunning_source": br_key.as_ref().and_then(|k| k.1.clone()),
    });
    Ok((Params {
        league: arr(&p["league"]).ok_or("league rates missing")?,
        env_home: arr(&p["env_home"]).unwrap_or([1.0; NCAT]),
        env_away: arr(&p["env_away"]).unwrap_or([1.0; NCAT]),
        k_bat: arr(&p["kappa_batter"]).ok_or("kappa missing")?,
        k_pit: arr(&p["kappa_pitcher"]).ok_or("kappa missing")?,
        park,
        bf_hist: p["starter_bf_hist"].as_array().map(|a| a.iter().map(|x| x.as_u64().unwrap_or(0) as u32).collect()).unwrap_or_default(),
        earned_share: p["earned_share"].as_f64().unwrap_or(0.9),
        season: p["season"].as_i64().unwrap_or(0),
        sub_hazard: p["sub_hazard"].as_array().map(|a| a.iter().map(|x| x.as_f64().unwrap_or(0.0)).collect()).unwrap_or_default(),
        bench: arr(&p["bench"]).unwrap_or(arr(&p["league"]).unwrap_or([0.0; NCAT])),
    }, br, provenance))
}

/// Recency-weighted outcome counts for one player from games BEFORE `cutoff`.
/// For a starting pitcher only his STARTS are used (relief outings run higher
/// K rates), unless he has under 100 BF as a starter.
async fn player_counts(db: &tokio_postgres::Client, pid: &str, pitcher: bool, cutoff: chrono::NaiveDate, hl: f64, hook_starts: usize)
    -> Result<([f64; NCAT], usize, Vec<u32>), String> {
    let key = if pitcher { "p_bf" } else { "pa" };
    let rows = db.query(
        "SELECT stats FROM player_game_logs WHERE sport='mlb' AND player_id=$1 AND game_date < $2 \
           AND COALESCE((stats->>$3)::float8,0) > 0 ORDER BY game_date DESC, week DESC LIMIT 400",
        &[&pid, &cutoff, &key],
    ).await.map_err(|e| e.to_string())?;
    let stats: Vec<Value> = rows.iter().map(|r| r.get::<_, Json<Value>>("stats").0).collect();
    let start_bf: f64 = stats.iter().filter(|s| fval(s, "p_gs") >= 1.0).map(|s| fval(s, "p_bf")).sum();
    let starts_only = pitcher && start_bf >= 100.0;
    let used: Vec<&Value> = stats.iter().filter(|s| !starts_only || fval(s, "p_gs") >= 1.0).collect();
    let mut c = [0.0; NCAT];
    let mut bf = Vec::new();
    for (i, s) in used.iter().enumerate() {
        let w = 0.5f64.powf(i as f64 / hl);
        add(&mut c, &if pitcher { pit_counts(s) } else { bat_counts(s) }, w);
        if pitcher && fval(s, "p_gs") >= 1.0 && bf.len() < hook_starts {
            bf.push(fval(s, "p_bf") as u32);
        }
    }
    Ok((c, used.len(), bf))
}

/// Team bullpen: all relief appearances for the team before the cutoff.
async fn bullpen_counts(db: &tokio_postgres::Client, team: &str, cutoff: chrono::NaiveDate, hl: f64) -> Result<([f64; NCAT], usize), String> {
    let rows = db.query(
        "SELECT stats FROM player_game_logs WHERE sport='mlb' AND team=$1 AND game_date < $2 \
           AND COALESCE((stats->>'p_bf')::float8,0) > 0 AND COALESCE((stats->>'p_gs')::float8,0) = 0 \
         ORDER BY game_date DESC, week DESC LIMIT 600",
        &[&team, &cutoff],
    ).await.map_err(|e| e.to_string())?;
    let mut c = [0.0; NCAT];
    for (i, r) in rows.iter().enumerate() {
        let s: Json<Value> = r.get("stats");
        add(&mut c, &pit_counts(&s.0), 0.5f64.powf(i as f64 / hl));
    }
    Ok((c, rows.len()))
}

/// The team's batting order from its most recent game before the cutoff.
async fn last_lineup(db: &tokio_postgres::Client, team: &str, cutoff: chrono::NaiveDate) -> Result<Vec<String>, String> {
    let rows = db.query(
        "WITH g AS (SELECT week FROM player_game_logs WHERE sport='mlb' AND team=$1 AND game_date < $2 \
                    ORDER BY game_date DESC, week DESC LIMIT 1) \
         SELECT player_id FROM player_game_logs l JOIN g ON g.week=l.week WHERE l.sport='mlb' AND l.team=$1 \
           AND (l.stats->>'bat_start')::float8 = 1 ORDER BY (l.stats->>'bat_order')::float8",
        &[&team, &cutoff],
    ).await.map_err(|e| e.to_string())?;
    Ok(rows.iter().map(|r| r.get::<_, String>(0)).collect())
}

async fn names_of(db: &tokio_postgres::Client, ids: &[String]) -> HashMap<String, (String, String)> {
    db.query("SELECT player_id, display_name, COALESCE(position,'') FROM sports_players WHERE sport='mlb' AND player_id = ANY($1)", &[&ids])
        .await.unwrap_or_default().iter()
        .map(|r| (r.get::<_, String>(0), (r.get::<_, String>(1), r.get::<_, String>(2)))).collect()
}

fn ids(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(|p| p["id"].as_i64().map(|x| x.to_string())).collect()).unwrap_or_default()
}
fn split_ids(s: &Option<String>) -> Option<Vec<String>> {
    s.as_ref().map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())
}

/// Standard prop lines reported for each simulated stat (any other line can be
/// read off the returned pmf).
fn default_lines(stat: &str) -> &'static [f64] {
    match stat {
        "h" => &[0.5, 1.5, 2.5], "tb" => &[0.5, 1.5, 2.5, 3.5], "hr" => &[0.5], "r" => &[0.5, 1.5], "rbi" => &[0.5, 1.5],
        "hrr" => &[0.5, 1.5, 2.5, 3.5], "1b" => &[0.5, 1.5], "2b" => &[0.5], "bb" => &[0.5, 1.5], "k" => &[0.5, 1.5, 4.5, 5.5, 6.5, 7.5],
        "outs" => &[14.5, 15.5, 16.5, 17.5, 18.5], "er" => &[1.5, 2.5, 3.5], "bf" => &[20.5, 22.5, 24.5], "pa" => &[3.5, 4.5],
        _ => &[0.5],
    }
}

fn dist_json(h: &mlb_sim::Hist, stat: &str) -> Value {
    let lines: Vec<Value> = default_lines(stat).iter().map(|&l| {
        let p = h.line(l);
        json!({ "line": l, "over": r4(p.over), "under": r4(p.under), "push": r4(p.push), "mcse": r4(p.mcse_over) })
    }).collect();
    json!({
        "mean": r3(h.mean()), "median": h.quantile(0.5), "sd": r3(h.sd()),
        "p10": h.quantile(0.10), "p90": h.quantile(0.90), "p025": h.quantile(0.025), "p975": h.quantile(0.975),
        "pmf": h.pmf().iter().map(|x| r4(*x)).collect::<Vec<_>>(), "lines": lines,
    })
}

/// Everything the simulator needs for one game, built only from games before
/// `cutoff`, plus a JSON description of those inputs and any data flags.
struct Built {
    game: GameInput,
    inputs: serde_json::Map<String, Value>,
    flags: Vec<String>,
}

/// Starter workload: the exit point is drawn from his last `starts` starts,
/// mixed with the league distribution at a weight of `prior` starts.
#[derive(Clone, Copy)]
struct Hook {
    starts: usize,
    prior: f64,
}

struct Side<'a> {
    team: &'a str,
    lineup: &'a [String],
    starter: &'a str,
    lineup_source: &'a str,
    starter_source: &'a str,
}

#[allow(clippy::too_many_arguments)]
async fn build_input(
    db: &tokio_postgres::Client, params: &Params, baserunning: Baserunning, away: Side<'_>, home: Side<'_>,
    cutoff: chrono::NaiveDate, season: i32, bat_hl: f64, pit_hl: f64, hook: Hook,
) -> Result<Built, String> {
    let mut flags: Vec<String> = Vec::new();
    let mut inputs = serde_json::Map::new();
    let mut teams: Vec<TeamInput> = Vec::new();
    let park_team = home.team.to_string();
    for (label, s) in [("away", &away), ("home", &home)] {
        if s.lineup.len() != 9 {
            return Err(format!("{label} lineup has {} batters (need 9)", s.lineup.len()));
        }
        let mut lineup: Vec<RatePosterior> = Vec::new();
        let mut bat_meta = Vec::new();
        for pid in s.lineup {
            let (c, games, _) = player_counts(db, pid, false, cutoff, bat_hl, 0).await?;
            if games == 0 { flags.push(format!("{label} batter {pid} has no MLB history before {cutoff}: league-average prior")); }
            let post = mlb_sim::shrink(&c, &params.league, &params.k_bat);
            bat_meta.push(json!({ "player_id": pid, "games": games, "eff_pa": r3(c.iter().sum()), "rates": post.mean.iter().map(|x| r4(*x)).collect::<Vec<_>>() }));
            lineup.push(post);
        }
        let (c, games, bf) = player_counts(db, s.starter, true, cutoff, pit_hl, hook.starts).await?;
        if games == 0 { flags.push(format!("{label} starter {} has no MLB history before {cutoff}: league-average prior", s.starter)); }
        if bf.is_empty() { flags.push(format!("{label} starter {} has no prior starts: league workload distribution", s.starter)); }
        let starter = mlb_sim::shrink(&c, &params.league, &params.k_pit);
        let (bc, bgames) = bullpen_counts(db, s.team, cutoff, 150.0).await?;
        let bullpen = mlb_sim::shrink(&bc, &params.league, &params.k_pit);
        // environment: home/away factor x the home team's park, per category
        let side_env = if label == "home" { params.env_home } else { params.env_away };
        let pf = params.park.get(park_team.as_str()).copied();
        if pf.is_none() && label == "home" { flags.push(format!("no park factor for {park_team}: neutral park")); }
        let pf = pf.unwrap_or([1.0; NCAT]);
        let mut env = [1.0; NCAT];
        for i in 0..NCAT { env[i] = side_env[i] * pf[i]; }
        inputs.insert(label.to_string(), json!({
            "team": s.team, "lineup_source": s.lineup_source, "starter_source": s.starter_source,
            "lineup": bat_meta,
            "starter": { "player_id": s.starter, "appearances": games, "eff_bf": r3(c.iter().sum()), "starts_for_hook": bf.len(), "rates": starter.mean.iter().map(|x| r4(*x)).collect::<Vec<_>>() },
            "bullpen": { "relief_appearances": bgames, "rates": bullpen.mean.iter().map(|x| r4(*x)).collect::<Vec<_>>() },
            "bat_env": env.iter().map(|x| r4(*x)).collect::<Vec<_>>(),
        }));
        let lineup: [RatePosterior; 9] = lineup.try_into().map_err(|_| "lineup size".to_string())?;
        let bench = RatePosterior { mean: params.bench, conc: 2000.0 };
        teams.push(TeamInput { lineup, starter, bullpen, bench, starter_bf: bf, bat_env: env });
    }
    let league_bf: Vec<u32> = params.bf_hist.iter().enumerate().flat_map(|(bf, &n)| std::iter::repeat(bf as u32).take(n as usize)).collect();
    if baserunning.counts.is_empty() { flags.push("no baserunning table: station-to-station advancement".into()); }
    if params.sub_hazard.is_empty() { flags.push("no substitution model in these parameters: starters bat all game (refit)".into()); }
    let home_in = teams.pop().unwrap();
    let away_in = teams.pop().unwrap();
    Ok(Built {
        game: GameInput {
            away: away_in, home: home_in, league: params.league, league_bf, hook_prior_starts: hook.prior,
            baserunning, earned_share: params.earned_share, innings: 9, ghost_runner: season >= 2020,
            sub_hazard: params.sub_hazard.clone(),
        },
        inputs,
        flags,
    })
}

pub async fn simulate(state: web::Data<State>, path: web::Path<String>, q: web::Query<SimQuery>) -> impl Responder {
    let pk = path.into_inner();
    let q = q.into_inner();
    if q.n == 0 || q.n > 1_000_000 {
        return bad_request("n must be between 1 and 1,000,000");
    }
    let t0 = Instant::now();
    let db = &state.db;

    // game context (teams, date, probable pitchers, posted lineups)
    let url = format!("{API}/schedule?gamePk={pk}&hydrate=probablePitcher,lineups,venue");
    let sched = match get_json(&state.http, &url).await { Ok(v) => v, Err(e) => return internal(e) };
    let Some(g) = sched["dates"][0]["games"][0].as_object().cloned() else { return bad_request("unknown gamePk") };
    let g = Value::Object(g);
    let Some(date) = g["officialDate"].as_str().and_then(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()) else {
        return internal("game has no date");
    };
    let season: i32 = g["season"].as_str().and_then(|s| s.parse().ok()).unwrap_or(date.format("%Y").to_string().parse().unwrap_or(0));
    let gtype = game_type(g["gameType"].as_str().unwrap_or(""));
    let (params, baserunning, provenance) = match load_params(db, season, gtype == "POST").await { Ok(x) => x, Err(e) => return unprocessable(e) };
    let abbr = team_abbrs(&state.http, season).await.unwrap_or_default();
    let team = |s: &str| abbr.get(&g["teams"][s]["team"]["id"].as_i64().unwrap_or(0)).cloned().unwrap_or_default();
    let (away, home) = (team("away"), team("home"));
    let cutoff = date; // strictly earlier games only

    // lineups: query override > official posted lineup > last game's order (projected)
    let mut lineups: Vec<(Vec<String>, &str)> = Vec::new();
    for (side, key, ov, team_code) in [("away", "awayPlayers", &q.away_lineup, &away), ("home", "homePlayers", &q.home_lineup, &home)] {
        let posted = ids(&g["lineups"][key]);
        let (lu, src) = if let Some(o) = split_ids(ov) {
            (o, "override")
        } else if posted.len() == 9 {
            (posted, "official (posted by MLB)")
        } else {
            match last_lineup(db, team_code, cutoff).await {
                Ok(l) => (l, "projected (team's previous game batting order)"),
                Err(e) => return internal(e),
            }
        };
        if lu.len() != 9 {
            return unprocessable(format!("{side} lineup has {} batters (need 9); pass {side}_lineup", lu.len()));
        }
        lineups.push((lu, src));
    }
    let mut starters: Vec<(String, &str)> = Vec::new();
    for (side, ov) in [("away", &q.away_sp), ("home", &q.home_sp)] {
        let probable = g["teams"][side]["probablePitcher"]["id"].as_i64().map(|x| x.to_string());
        match (ov.clone(), probable) {
            (Some(o), _) => starters.push((o, "override")),
            (None, Some(p)) => starters.push((p, "probable (announced by MLB)")),
            (None, None) => return unprocessable(format!("no announced {side} starter yet; pass {side}_sp=<MLB person id>")),
        }
    }
    let built = match build_input(
        db, &params, baserunning,
        Side { team: &away, lineup: &lineups[0].0, starter: &starters[0].0, lineup_source: lineups[0].1, starter_source: starters[0].1 },
        Side { team: &home, lineup: &lineups[1].0, starter: &starters[1].0, lineup_source: lineups[1].1, starter_source: starters[1].1 },
        cutoff, season, q.batter_halflife, q.pitcher_halflife, Hook { starts: q.hook_starts, prior: q.hook_prior },
    ).await { Ok(b) => b, Err(e) => return unprocessable(e) };
    let Built { game, inputs: inputs_json, flags } = built;

    let seed = q.seed.unwrap_or_else(|| mlb_sim::derive_seed(0x6d6c62, &[&pk, &chrono::Utc::now().timestamp_millis().to_string()]));
    let (n, draws) = (q.n, q.draws);
    let sim_t = Instant::now();
    let out = match web::block(move || mlb_sim::simulate(&game, n, draws, seed)).await {
        Ok(o) => o,
        Err(e) => return internal(format!("simulation thread: {e}")),
    };
    let sim_ms = sim_t.elapsed().as_millis() as u64;

    // names for the response
    let mut all_ids: Vec<String> = lineups.iter().flat_map(|l| l.0.clone()).collect();
    all_ids.extend(starters.iter().map(|s| s.0.clone()));
    let names = names_of(db, &all_ids).await;
    let name = |id: &str| names.get(id).map(|x| x.0.clone()).unwrap_or_else(|| id.to_string());
    let mut sides = serde_json::Map::new();
    for (si, side) in ["away", "home"].iter().enumerate() {
        let batters: Vec<Value> = lineups[si].0.iter().enumerate().map(|(slot, pid)| {
            let stats: serde_json::Map<String, Value> = mlb_sim::BAT_STATS.iter().enumerate()
                .map(|(k, s)| (s.to_string(), dist_json(&out.bat[si][slot][k], s))).collect();
            json!({ "slot": slot + 1, "player_id": pid, "name": name(pid), "position": names.get(pid).map(|x| x.1.clone()), "stats": stats })
        }).collect();
        let sp = &starters[si].0;
        let sp_stats: serde_json::Map<String, Value> = mlb_sim::PIT_STATS.iter().enumerate()
            .map(|(k, s)| (s.to_string(), dist_json(&out.starter[si][k], s))).collect();
        sides.insert(side.to_string(), json!({
            "team": if si == 0 { &away } else { &home },
            "runs": dist_json(&out.team_runs[si], "runs"),
            "starter": { "player_id": sp, "name": name(sp), "stats": sp_stats },
            "batters": batters,
        }));
    }
    let totals: Vec<Value> = [6.5, 7.0, 7.5, 8.0, 8.5, 9.0, 9.5, 10.5].iter().map(|&l| {
        let p = out.total_runs.line(l);
        json!({ "line": l, "over": r4(p.over), "under": r4(p.under), "push": r4(p.push), "mcse": r4(p.mcse_over) })
    }).collect();
    let n_valid = out.valid;
    let home_win = out.home_win();
    let result = json!({
        "game": { "game_pk": pk, "date": date.to_string(), "season": season, "game_type": gtype, "away": away, "home": home,
                  "venue": g["venue"]["name"], "status": g["status"]["detailedState"], "start": g["gameDate"] },
        "simulation": {
            "model_version": SIM_MODEL, "provenance": provenance, "params_season": params.season,
            "requested": out.requested, "valid": n_valid, "rejected": out.rejected, "parameter_draws": out.draws,
            "seed": seed.to_string(), "seed_strategy": "per-draw sub-seeds = FNV-1a(seed, 'draw', i) -> splitmix64 -> xoshiro256**",
            "production_grade": n_valid >= PRODUCTION_N,
            "sim_ms": sim_ms, "data_cutoff": cutoff.to_string(), "extra_inning_rate": r4(out.extra_innings as f64 / n_valid.max(1) as f64),
            "note": "mcse is Monte Carlo sampling error only; it says nothing about model accuracy (see /backtest/mlb/sim).",
        },
        "team": {
            "home_win": r4(home_win), "away_win": r4(1.0 - home_win),
            "home_runline_minus_1_5": r4(out.home_cover(-1.5)), "away_runline_plus_1_5": r4(1.0 - out.home_cover(-1.5)),
            "away_runline_minus_1_5": r4(1.0 - out.home_cover(1.5)), "home_runline_plus_1_5": r4(out.home_cover(1.5)),
            "total": dist_json(&out.total_runs, "total"), "totals": totals,
        },
        "away": sides["away"], "home": sides["home"],
        "flags": flags,
        "limitations": LIMITATIONS,
    });
    let sim_id = format!("mlb-{pk}-{seed:016x}");
    if q.store.unwrap_or(true) {
        if let Err(e) = db.execute(
            "INSERT INTO mlb_sim_runs (sim_id, game_pk, model_version, seed, n_requested, n_valid, n_rejected, data_cutoff, inputs, result) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) ON CONFLICT (sim_id) DO NOTHING",
            &[&sim_id, &pk, &SIM_MODEL, &(seed as i64), &(out.requested as i32), &(n_valid as i32), &(out.rejected as i32), &cutoff,
              &Json(Value::Object(inputs_json.clone())), &Json(&result)],
        ).await { eprintln!("projection-service(mlb): store sim run: {}", dberr(&e)); }
    }
    let mut body = result;
    body["sim_id"] = json!(sim_id);
    body["inputs"] = Value::Object(inputs_json);
    body["simulation"]["total_ms"] = json!(t0.elapsed().as_millis() as u64);
    HttpResponse::Ok().json(body)
}

const LIMITATIONS: [&str; 5] = [
    "plate-appearance model (no pitch-by-pitch); no handedness/platoon splits; substitutions are a league-wide hazard by PA number, not player-specific",
    "stolen-base attempts not simulated (SB props use the per-stat model)",
    "starter exits on an empirical batters-faced draw, not pitch count or in-game performance",
    "bullpen = team aggregate reliever; no weather, roof or umpire inputs",
    "earned runs = runs x league earned share (no error reconstruction)",
];

// --------------------------------------------------------------------------
// Walk-forward backtest of the game simulator
// --------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct BacktestQuery {
    #[serde(default = "d_bt_season")] season: i32,
    /// first date tested (players need some history first)
    from: Option<String>,
    to: Option<String>,
    #[serde(default = "d_bt_games")] games: usize,
    /// simulations per game: research setting, NOT the 100k production run
    #[serde(default = "d_bt_n")] n: usize,
    #[serde(default = "d_bt_draws")] draws: usize,
    #[serde(default = "d_bat_hl")] batter_halflife: f64,
    #[serde(default = "d_pit_hl")] pitcher_halflife: f64,
    #[serde(default = "d_hook_starts")] hook_starts: usize,
    #[serde(default = "d_hook_prior")] hook_prior: f64,
    #[serde(default = "d_bt_seed")] seed: u64,
}
fn d_bt_season() -> i32 { 2026 }
fn d_bt_games() -> usize { 150 }
fn d_bt_n() -> usize { 10_000 }
fn d_bt_draws() -> usize { 50 }
fn d_bt_seed() -> u64 { 20261019 }

/// Binary forecast record: predicted probability, baseline probability, outcome.
#[derive(Default)]
struct Binary {
    p: Vec<f64>,
    base: Vec<f64>,
    y: Vec<f64>,
}
impl Binary {
    fn push(&mut self, p: f64, base: f64, y: bool) {
        self.p.push(p);
        self.base.push(base);
        self.y.push(if y { 1.0 } else { 0.0 });
    }
    fn report(&self, label: &str, baseline: &str) -> Value {
        let n = self.y.len();
        if n == 0 {
            return json!({ "market": label, "n": 0 });
        }
        let brier = |p: &[f64]| p.iter().zip(&self.y).map(|(p, y)| (p - y).powi(2)).sum::<f64>() / n as f64;
        let ll = |p: &[f64]| -p.iter().zip(&self.y).map(|(p, y)| {
            let p = p.clamp(1e-4, 1.0 - 1e-4);
            y * p.ln() + (1.0 - y) * (1.0 - p).ln()
        }).sum::<f64>() / n as f64;
        // paired standard error of the Brier difference (model - baseline)
        let d: Vec<f64> = (0..n).map(|i| (self.p[i] - self.y[i]).powi(2) - (self.base[i] - self.y[i]).powi(2)).collect();
        let dm = d.iter().sum::<f64>() / n as f64;
        let dse = (d.iter().map(|x| (x - dm).powi(2)).sum::<f64>() / (n as f64 - 1.0).max(1.0) / n as f64).sqrt();
        // reliability: 10 equal-width buckets
        let mut buckets = Vec::new();
        for b in 0..10 {
            let (lo, hi) = (b as f64 / 10.0, (b + 1) as f64 / 10.0);
            let idx: Vec<usize> = (0..n).filter(|&i| self.p[i] >= lo && (self.p[i] < hi || (b == 9 && self.p[i] <= 1.0))).collect();
            if idx.is_empty() { continue; }
            let m = idx.len() as f64;
            buckets.push(json!({ "bucket": format!("{lo:.1}-{hi:.1}"), "n": idx.len(),
                "mean_pred": r3(idx.iter().map(|&i| self.p[i]).sum::<f64>() / m),
                "observed": r3(idx.iter().map(|&i| self.y[i]).sum::<f64>() / m) }));
        }
        let ece = buckets.iter().map(|b| b["n"].as_f64().unwrap() / n as f64 * (b["mean_pred"].as_f64().unwrap() - b["observed"].as_f64().unwrap()).abs()).sum::<f64>();
        json!({
            "market": label, "n": n, "base_rate": r3(self.y.iter().sum::<f64>() / n as f64),
            "model": { "brier": r4(brier(&self.p)), "log_loss": r4(ll(&self.p)), "mean_pred": r3(self.p.iter().sum::<f64>() / n as f64), "ece": r4(ece) },
            "baseline": { "name": baseline, "brier": r4(brier(&self.base)), "log_loss": r4(ll(&self.base)) },
            "brier_diff_model_minus_baseline": r4(dm), "brier_diff_se": r4(dse),
            "calibration": buckets,
        })
    }
}

#[derive(Default)]
struct Numeric {
    pred: Vec<f64>,
    base: Vec<f64>,
    y: Vec<f64>,
    in80: Vec<bool>,
}
impl Numeric {
    fn report(&self, label: &str, baseline: &str) -> Value {
        let n = self.y.len();
        if n == 0 { return json!({ "stat": label, "n": 0 }); }
        let mae = |p: &[f64]| p.iter().zip(&self.y).map(|(p, y)| (p - y).abs()).sum::<f64>() / n as f64;
        let rmse = |p: &[f64]| (p.iter().zip(&self.y).map(|(p, y)| (p - y).powi(2)).sum::<f64>() / n as f64).sqrt();
        let bias = self.pred.iter().zip(&self.y).map(|(p, y)| p - y).sum::<f64>() / n as f64;
        // calibration by tercile of the prediction: is the error a level shift
        // or concentrated in the extremes (over-confident spread)?
        let mut idx: Vec<usize> = (0..n).collect();
        idx.sort_by(|&a, &b| self.pred[a].total_cmp(&self.pred[b]));
        let terciles: Vec<Value> = idx.chunks(n.div_ceil(3).max(1)).map(|c| {
            let m = c.len() as f64;
            json!({ "n": c.len(), "mean_pred": r3(c.iter().map(|&i| self.pred[i]).sum::<f64>() / m),
                    "mean_actual": r3(c.iter().map(|&i| self.y[i]).sum::<f64>() / m),
                    "baseline_mean": r3(c.iter().map(|&i| self.base[i]).sum::<f64>() / m) })
        }).collect();
        json!({
            "by_prediction_tercile": terciles,
            "stat": label, "n": n, "actual_mean": r3(self.y.iter().sum::<f64>() / n as f64),
            "model": { "mae": r3(mae(&self.pred)), "rmse": r3(rmse(&self.pred)), "bias": r3(bias),
                       "coverage_80": r3(self.in80.iter().filter(|x| **x).count() as f64 / n as f64) },
            "baseline": { "name": baseline, "mae": r3(mae(&self.base)), "rmse": r3(rmse(&self.base)) },
        })
    }
}

/// P(X >= k) from a stored distribution of past per-game values, with a
/// Beta(1,1) (add-one) correction so a short history never gives 0 or 1.
fn freq_at_least(vals: &[f64], k: f64) -> f64 {
    let hits = vals.iter().filter(|v| **v >= k).count() as f64;
    (hits + 1.0) / (vals.len() as f64 + 2.0)
}

/// Pregame-knowable inputs and actual results for one completed game, read
/// from the stored box scores: starting lineups (batting order 1-9), starting
/// pitchers, and each starter's actual line.
struct Played {
    date: chrono::NaiveDate,
    teams: [String; 2],
    lineups: [Vec<(String, Value)>; 2],
    starters: [(String, Value); 2],
    runs: [f64; 2],
}

async fn played_game(db: &tokio_postgres::Client, pk: &str) -> Result<Option<Played>, String> {
    let g = db.query_opt(
        "SELECT game_date, home_team, away_team, home_score, away_score FROM sports_games WHERE sport='mlb' AND game_id=$1", &[&pk],
    ).await.map_err(|e| e.to_string())?;
    let Some(g) = g else { return Ok(None) };
    let (Some(date), Some(hs), Some(as_)) = (g.get::<_, Option<chrono::NaiveDate>>(0), g.get::<_, Option<i32>>(3), g.get::<_, Option<i32>>(4)) else { return Ok(None) };
    let home: String = g.get::<_, Option<String>>(1).unwrap_or_default();
    let away: String = g.get::<_, Option<String>>(2).unwrap_or_default();
    let rows = db.query("SELECT player_id, team, stats FROM player_game_logs WHERE sport='mlb' AND week=$1", &[&pk]).await.map_err(|e| e.to_string())?;
    let mut lineups: [Vec<(f64, String, Value)>; 2] = [Vec::new(), Vec::new()];
    let mut starters: [Option<(String, Value)>; 2] = [None, None];
    for r in &rows {
        let team: String = r.get::<_, Option<String>>(1).unwrap_or_default();
        let side = if team == away { 0 } else if team == home { 1 } else { continue };
        let s: Json<Value> = r.get(2);
        let pid: String = r.get(0);
        if fval(&s.0, "bat_start") >= 1.0 {
            lineups[side].push((fval(&s.0, "bat_order"), pid.clone(), s.0.clone()));
        }
        if fval(&s.0, "p_gs") >= 1.0 {
            starters[side] = Some((pid, s.0.clone()));
        }
    }
    for l in lineups.iter_mut() { l.sort_by(|a, b| a.0.total_cmp(&b.0)); }
    let (Some(sa), Some(sh)) = (starters[0].take(), starters[1].take()) else { return Ok(None) };
    if lineups[0].len() != 9 || lineups[1].len() != 9 { return Ok(None); }
    let [la, lh] = lineups;
    Ok(Some(Played {
        date, teams: [away, home],
        lineups: [la.into_iter().map(|x| (x.1, x.2)).collect(), lh.into_iter().map(|x| (x.1, x.2)).collect()],
        starters: [sa, sh], runs: [as_ as f64, hs as f64],
    }))
}

/// Prior per-game values of one stat for a player (games before the cutoff
/// where he had the opportunity), newest first.
async fn prior_values(db: &tokio_postgres::Client, pid: &str, opp: &str, col: &str, cutoff: chrono::NaiveDate) -> Vec<f64> {
    db.query(
        "SELECT (stats->>$3)::float8 FROM player_game_logs WHERE sport='mlb' AND player_id=$1 AND game_date < $2 \
           AND COALESCE((stats->>$4)::float8,0) >= 1 ORDER BY game_date DESC LIMIT 162",
        &[&pid, &cutoff, &col, &opp],
    ).await.unwrap_or_default().iter().map(|r| r.get::<_, Option<f64>>(0).unwrap_or(0.0)).collect()
}

pub async fn backtest(state: web::Data<State>, q: web::Query<BacktestQuery>) -> impl Responder {
    let q = q.into_inner();
    let t0 = Instant::now();
    let db = &state.db;
    let (params, baserunning, provenance) = match load_params(db, q.season, false).await { Ok(x) => x, Err(e) => return unprocessable(e) };
    let from = q.from.as_deref().and_then(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
        .unwrap_or_else(|| chrono::NaiveDate::from_ymd_opt(q.season, 5, 1).unwrap());
    let to = q.to.as_deref().and_then(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
        .unwrap_or_else(|| chrono::NaiveDate::from_ymd_opt(q.season, 12, 31).unwrap());
    let pks: Vec<String> = match db.query(
        "SELECT game_id FROM sports_games WHERE sport='mlb' AND season=$1 AND game_type='REG' AND home_score IS NOT NULL \
           AND game_date BETWEEN $2 AND $3 ORDER BY game_date, game_id",
        &[&q.season, &from, &to],
    ).await { Ok(r) => r.iter().map(|r| r.get::<_, String>(0)).collect(), Err(e) => return internal(e) };
    if pks.is_empty() { return unprocessable("no completed games in that window"); }
    let step = (pks.len() as f64 / q.games.max(1) as f64).max(1.0);
    let sample: Vec<&String> = (0..q.games.min(pks.len())).map(|i| &pks[(i as f64 * step) as usize]).collect();

    let mut home_win = Binary::default();
    let mut over85 = Binary::default();
    let mut total_runs = Numeric::default();
    let mut hit1 = Binary::default();
    let mut tb2 = Binary::default();
    let mut hr1 = Binary::default();
    let mut hrr2 = Binary::default();
    let mut bk1 = Binary::default();
    let mut sp_k = Numeric::default();
    let mut sp_outs = Numeric::default();
    let mut k_over = Binary::default();
    let mut outs_over = Binary::default();
    let mut bat_hits = Numeric::default();
    let (mut sims, mut skipped, mut rejected, mut sim_ms) = (0usize, 0usize, 0usize, 0u128);
    let league_home_win = 0.53; // overwritten below with the prior season's rate
    let lhw: f64 = db.query_opt(
        "SELECT avg((home_score > away_score)::int)::float8 FROM sports_games WHERE sport='mlb' AND season=$1 AND game_type='REG' AND home_score IS NOT NULL",
        &[&(q.season - 1)],
    ).await.ok().flatten().and_then(|r| r.get::<_, Option<f64>>(0)).unwrap_or(league_home_win);
    let lover: f64 = db.query_opt(
        "SELECT avg(((home_score + away_score) > 8.5)::int)::float8 FROM sports_games WHERE sport='mlb' AND season=$1 AND game_type='REG' AND home_score IS NOT NULL",
        &[&(q.season - 1)],
    ).await.ok().flatten().and_then(|r| r.get::<_, Option<f64>>(0)).unwrap_or(0.5);
    let lruns: f64 = db.query_opt(
        "SELECT avg(home_score + away_score)::float8 FROM sports_games WHERE sport='mlb' AND season=$1 AND game_type='REG' AND home_score IS NOT NULL",
        &[&(q.season - 1)],
    ).await.ok().flatten().and_then(|r| r.get::<_, Option<f64>>(0)).unwrap_or(8.5);

    for pk in sample {
        let played = match played_game(db, pk).await { Ok(Some(p)) => p, Ok(None) => { skipped += 1; continue } Err(e) => return internal(e) };
        let la: Vec<String> = played.lineups[0].iter().map(|x| x.0.clone()).collect();
        let lh: Vec<String> = played.lineups[1].iter().map(|x| x.0.clone()).collect();
        let built = match build_input(
            db, &params, baserunning.clone(),
            Side { team: &played.teams[0], lineup: &la, starter: &played.starters[0].0, lineup_source: "actual starting lineup", starter_source: "actual starter" },
            Side { team: &played.teams[1], lineup: &lh, starter: &played.starters[1].0, lineup_source: "actual starting lineup", starter_source: "actual starter" },
            played.date, q.season, q.batter_halflife, q.pitcher_halflife, Hook { starts: q.hook_starts, prior: q.hook_prior },
        ).await { Ok(b) => b, Err(_) => { skipped += 1; continue } };
        let game = built.game;
        let (n, draws, seed) = (q.n, q.draws, mlb_sim::derive_seed(q.seed, &[pk]));
        let ts = Instant::now();
        let out = match web::block(move || mlb_sim::simulate(&game, n, draws, seed)).await { Ok(o) => o, Err(e) => return internal(e) };
        sim_ms += ts.elapsed().as_millis();
        sims += 1;
        rejected += out.rejected;

        // team markets
        home_win.push(out.home_win(), lhw, played.runs[1] > played.runs[0]);
        let tot = played.runs[0] + played.runs[1];
        over85.push(out.total_runs.line(8.5).over, lover, tot > 8.5);
        total_runs.pred.push(out.total_runs.mean());
        total_runs.base.push(lruns);
        total_runs.y.push(tot);
        total_runs.in80.push((out.total_runs.quantile(0.1) as f64) <= tot && tot <= out.total_runs.quantile(0.9) as f64);

        // batters
        let bi = |s: &str| mlb_sim::BAT_STATS.iter().position(|x| *x == s).unwrap();
        for side in 0..2 {
            for (slot, (pid, actual)) in played.lineups[side].iter().enumerate() {
                let h = &out.bat[side][slot];
                let hist_h = prior_values(db, pid, "pa", "h", played.date).await;
                let hist_tb = prior_values(db, pid, "pa", "tb", played.date).await;
                let hist_hr = prior_values(db, pid, "pa", "hr", played.date).await;
                let hist_hrr = prior_values(db, pid, "pa", "hrr", played.date).await;
                let hist_k = prior_values(db, pid, "pa", "k", played.date).await;
                hit1.push(h[bi("h")].line(0.5).over, freq_at_least(&hist_h, 1.0), fval(actual, "h") >= 1.0);
                tb2.push(h[bi("tb")].line(1.5).over, freq_at_least(&hist_tb, 2.0), fval(actual, "tb") >= 2.0);
                hr1.push(h[bi("hr")].line(0.5).over, freq_at_least(&hist_hr, 1.0), fval(actual, "hr") >= 1.0);
                hrr2.push(h[bi("hrr")].line(1.5).over, freq_at_least(&hist_hrr, 2.0), fval(actual, "hrr") >= 2.0);
                bk1.push(h[bi("k")].line(0.5).over, freq_at_least(&hist_k, 1.0), fval(actual, "k") >= 1.0);
                let hm = h[bi("h")].mean();
                let y = fval(actual, "h");
                bat_hits.pred.push(hm);
                bat_hits.base.push(if hist_h.is_empty() { 0.8 } else { hist_h.iter().sum::<f64>() / hist_h.len() as f64 });
                bat_hits.y.push(y);
                bat_hits.in80.push(h[bi("h")].quantile(0.1) as f64 <= y && y <= h[bi("h")].quantile(0.9) as f64);
            }
            // starting pitcher
            let (pid, actual) = &played.starters[side];
            let sp = &out.starter[side];
            let pi = |s: &str| mlb_sim::PIT_STATS.iter().position(|x| *x == s).unwrap();
            let hist_k = prior_values(db, pid, "p_gs", "p_k", played.date).await;
            let hist_outs = prior_values(db, pid, "p_gs", "p_outs", played.date).await;
            let mean_or = |v: &[f64], d: f64| if v.is_empty() { d } else { v.iter().sum::<f64>() / v.len() as f64 };
            let (yk, yo) = (fval(actual, "p_k"), fval(actual, "p_outs"));
            sp_k.pred.push(sp[pi("k")].mean()); sp_k.base.push(mean_or(&hist_k, 4.8)); sp_k.y.push(yk);
            sp_k.in80.push(sp[pi("k")].quantile(0.1) as f64 <= yk && yk <= sp[pi("k")].quantile(0.9) as f64);
            sp_outs.pred.push(sp[pi("outs")].mean()); sp_outs.base.push(mean_or(&hist_outs, 15.4)); sp_outs.y.push(yo);
            sp_outs.in80.push(sp[pi("outs")].quantile(0.1) as f64 <= yo && yo <= sp[pi("outs")].quantile(0.9) as f64);
            // a line near the median of the pitcher's own history, like a book would hang
            let kline = (mean_or(&hist_k, 4.8)).floor() + 0.5;
            let oline = (mean_or(&hist_outs, 15.4)).floor() + 0.5;
            k_over.push(sp[pi("k")].line(kline).over, freq_at_least(&hist_k, kline.ceil()), yk > kline);
            outs_over.push(sp[pi("outs")].line(oline).over, freq_at_least(&hist_outs, oline.ceil()), yo > oline);
        }
    }

    let report = json!({
        "sport": "mlb", "model_version": SIM_MODEL, "season": q.season, "window": [from.to_string(), to.to_string()],
        "settings": { "batter_halflife": q.batter_halflife, "pitcher_halflife": q.pitcher_halflife, "hook_starts": q.hook_starts, "hook_prior": q.hook_prior },
        "games_simulated": sims, "games_skipped": skipped, "sims_per_game": q.n, "parameter_draws": q.draws,
        "rejected_sims": rejected, "research_mode": q.n < PRODUCTION_N, "provenance": provenance,
        "method": "walk-forward: each game simulated with its actual starting lineups and starters (known pregame), player rates from games BEFORE that date only, and simulator parameters from the previous season. Outcomes compared to the box score.",
        "baselines": {
            "team": "previous-season league rates (home win %, over 8.5 %, mean total)",
            "player": "the player's own frequency in prior games (up to 162, add-one smoothed) — a season/career-to-date rate model",
        },
        "team": [home_win.report("home win", "prior-season home win rate"), over85.report("total over 8.5", "prior-season over 8.5 rate")],
        "team_numeric": [total_runs.report("total runs", "prior-season mean total")],
        "batter": [
            hit1.report("1+ hits", "own prior rate"), tb2.report("2+ total bases", "own prior rate"),
            hr1.report("1+ home runs", "own prior rate"), hrr2.report("2+ H+R+RBI", "own prior rate"),
            bk1.report("1+ strikeouts", "own prior rate"),
        ],
        "batter_numeric": [bat_hits.report("hits", "own prior mean")],
        "pitcher": [k_over.report("starter K over own-avg line", "own prior rate"), outs_over.report("starter outs over own-avg line", "own prior rate")],
        "pitcher_numeric": [sp_k.report("starter strikeouts", "own prior mean"), sp_outs.report("starter outs", "own prior mean")],
        "notes": [
            "Brier/log loss: lower is better. brier_diff < 0 means the simulator beat the baseline; compare it to ~2x brier_diff_se.",
            "No historical sportsbook prices are stored, so this measures forecast quality, not betting ROI.",
        ],
        "elapsed_s": t0.elapsed().as_secs(), "sim_ms_total": sim_ms as u64,
        "run_at": chrono::Utc::now().to_rfc3339(),
    });
    // keep the latest substantial run per season so the dashboard can show it
    if sims >= 100 {
        if let Err(e) = db.execute(
            "INSERT INTO mlb_model_params (key, params, source) VALUES ($1, $2, 'walk-forward backtest')              ON CONFLICT (key) DO UPDATE SET params=EXCLUDED.params, source=EXCLUDED.source, fitted_at=now()",
            &[&format!("backtest:{}", q.season), &Json(&report)],
        ).await { eprintln!("projection-service(mlb): store backtest: {}", dberr(&e)); }
    }
    HttpResponse::Ok().json(report)
}

/// Latest stored walk-forward backtest (one per season).
pub async fn latest_backtest(state: web::Data<State>) -> impl Responder {
    match state.db.query("SELECT params FROM mlb_model_params WHERE key LIKE 'backtest:%' ORDER BY key DESC", &[]).await {
        Ok(rows) => {
            let runs: Vec<Value> = rows.iter().map(|r| r.get::<_, Json<Value>>(0).0).collect();
            HttpResponse::Ok().json(json!({ "sport": "mlb", "runs": runs }))
        }
        Err(e) => internal(e),
    }
}

pub async fn get_sim(state: web::Data<State>, path: web::Path<String>) -> impl Responder {
    let id = path.into_inner();
    match state.db.query_opt("SELECT result, inputs, created_at::text AS t FROM mlb_sim_runs WHERE sim_id=$1", &[&id]).await {
        Ok(Some(r)) => {
            let mut v: Json<Value> = r.get("result");
            let inputs: Json<Value> = r.get("inputs");
            v.0["sim_id"] = json!(id);
            v.0["inputs"] = inputs.0;
            v.0["created_at"] = json!(r.get::<_, String>("t"));
            HttpResponse::Ok().json(v.0)
        }
        Ok(None) => HttpResponse::NotFound().json(json!({ "error": "no such simulation" })),
        Err(e) => internal(e),
    }
}

pub async fn params(state: web::Data<State>) -> impl Responder {
    match state.db.query("SELECT key, params, source, fitted_at::text AS t FROM mlb_model_params WHERE key NOT LIKE 'backtest:%' ORDER BY key", &[]).await {
        Ok(rows) => {
            let out: Vec<Value> = rows.iter().map(|r| {
                let key: String = r.get("key");
                let p: Json<Value> = r.get("params");
                // the transition table is large; summarise it
                let params = if key == "baserunning" {
                    let n = p.0["obs"].as_array().map(|a| a.len()).unwrap_or(0);
                    let total: f64 = p.0["obs"].as_array().map(|a| a.iter().map(|o| o["n"].as_f64().unwrap_or(0.0)).sum()).unwrap_or(0.0);
                    json!({ "distinct_moves": n, "transitions": total })
                } else { p.0 };
                json!({ "key": key, "source": r.get::<_, Option<String>>("source"), "fitted_at": r.get::<_, String>("t"), "params": params })
            }).collect();
            HttpResponse::Ok().json(json!({ "sport": "mlb", "model_version": SIM_MODEL, "params": out }))
        }
        Err(e) => internal(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boxscore_rows_are_derived_consistently() {
        let b = json!({ "teams": {
            "away": { "team": { "id": 1 }, "players": {
                "ID10": { "person": { "id": 10, "fullName": "A Batter" }, "position": { "abbreviation": "SS" }, "battingOrder": "100",
                          "stats": { "batting": { "plateAppearances": 5, "atBats": 4, "hits": 2, "doubles": 1, "triples": 0, "homeRuns": 1,
                                                  "totalBases": 7, "runs": 1, "rbi": 2, "baseOnBalls": 1, "strikeOuts": 1 }, "pitching": {} } },
                "ID11": { "person": { "id": 11, "fullName": "Bench Guy" }, "position": { "abbreviation": "C" },
                          "stats": { "batting": {}, "pitching": {} } } } },
            "home": { "team": { "id": 2 }, "players": {
                "ID20": { "person": { "id": 20, "fullName": "S Pitcher" }, "position": { "abbreviation": "P" },
                          "stats": { "batting": {}, "pitching": { "battersFaced": 25, "outs": 18, "strikeOuts": 7, "baseOnBalls": 2,
                                                                  "hits": 5, "doubles": 1, "triples": 0, "homeRuns": 1, "runs": 3, "earnedRuns": 2,
                                                                  "numberOfPitches": 95, "gamesStarted": 1, "hitBatsmen": 0 } } } } } } });
        let abbr: HashMap<i64, String> = [(1, "NYY".to_string()), (2, "BOS".to_string())].into();
        let rows = parse_boxscore(&b, &abbr);
        assert_eq!(rows.len(), 2, "bench player without a PA is skipped");
        let bat = rows.iter().find(|r| r.player_id == "10").unwrap();
        assert_eq!((bat.team.as_str(), bat.opponent.as_str()), ("NYY", "BOS"));
        assert_eq!(fval(&bat.stats, "1b"), 0.0); // 2 hits = 1 double + 1 HR
        assert_eq!(fval(&bat.stats, "hrr"), 5.0);
        assert_eq!(fval(&bat.stats, "bat_order"), 1.0);
        assert_eq!(fval(&bat.stats, "bat_start"), 1.0);
        assert_eq!(fval(&bat.stats, "home"), 0.0);
        let c = bat_counts(&bat.stats);
        assert_eq!(c.iter().sum::<f64>(), 5.0, "categories partition the PAs");
        let p = rows.iter().find(|r| r.player_id == "20").unwrap();
        assert_eq!(p.position, "P");
        assert_eq!(fval(&p.stats, "p_1b"), 3.0);
        assert_eq!(pit_counts(&p.stats).iter().sum::<f64>(), 25.0);
        assert_eq!(fval(&p.stats, "home"), 1.0);
    }

    #[test]
    fn pbp_replay_tracks_runners_and_outs() {
        let pbp = json!({ "allPlays": [
            // single, batter to 1B
            { "result": { "eventType": "single", "rbi": 0 }, "about": { "inning": 1, "isTopInning": true }, "count": { "outs": 0 },
              "matchup": { "batter": { "id": 1 } },
              "runners": [ { "movement": { "originBase": null, "end": "1B", "isOut": false }, "details": { "runner": { "id": 1 } } } ] },
            // double: runner from 1B scores, batter to 2B
            { "result": { "eventType": "double", "rbi": 1 }, "about": { "inning": 1, "isTopInning": true }, "count": { "outs": 0 },
              "matchup": { "batter": { "id": 2 } },
              "runners": [ { "movement": { "originBase": null, "end": "2B", "isOut": false }, "details": { "runner": { "id": 2 } } },
                           { "movement": { "originBase": "1B", "end": "score", "isOut": false }, "details": { "runner": { "id": 1 } } } ] },
            // groundout, runner on 2B holds (not listed)
            { "result": { "eventType": "field_out", "rbi": 0 }, "about": { "inning": 1, "isTopInning": true }, "count": { "outs": 1 },
              "matchup": { "batter": { "id": 3 } },
              "runners": [ { "movement": { "originBase": null, "end": null, "isOut": true }, "details": { "runner": { "id": 3 } } } ] },
            // sac fly: runner... still on 2B, moves to 3B; batter out
            { "result": { "eventType": "sac_fly", "rbi": 0 }, "about": { "inning": 1, "isTopInning": true }, "count": { "outs": 2 },
              "matchup": { "batter": { "id": 4 } },
              "runners": [ { "movement": { "originBase": null, "end": null, "isOut": true }, "details": { "runner": { "id": 4 } } },
                           { "movement": { "originBase": "2B", "end": "3B", "isOut": false }, "details": { "runner": { "id": 2 } } } ] }
        ]});
        let t = pbp_transitions(&pbp);
        assert_eq!(t.len(), 4);
        // single from empty bases, 0 outs
        assert_eq!((t[0].0, t[0].1, t[0].2), (0, 0, 0));
        assert_eq!(t[0].3.dest[0], 1);
        // double with runner on 1B: runner scores, rbi 1
        assert_eq!((t[1].0, t[1].1), (1, 0b001));
        assert_eq!(t[1].3.dest[1], mlb_sim::D_HOME);
        assert_eq!(t[1].3.rbi, 1);
        // groundout with runner on 2B who holds
        assert_eq!((t[2].0, t[2].1, t[2].2), (3, 0b010, 0));
        assert_eq!(t[2].3.dest[2], 2);
        assert_eq!(t[2].3.dest[0], mlb_sim::D_OUT);
        // next play starts with 1 out, runner on 2B
        assert_eq!((t[3].1, t[3].2), (0b010, 1));
        assert_eq!(t[3].3.dest[2], 3);
    }
}
