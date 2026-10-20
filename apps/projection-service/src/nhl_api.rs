//! NHL game data from the NHL's public API (api-web.nhle.com).
//!
//! * `/v1/schedule/{date}` (a week per call) -> game ids
//! * `/v1/gamecenter/{id}/play-by-play` -> every shot attempt (goal, shot on
//!   goal, missed, blocked) with rink coordinates, shot type, shooter, goalie
//!   and the on-ice situation code; penalties with durations
//! * `/v1/gamecenter/{id}/boxscore` -> player TOI, goals, assists, SOG, hits,
//!   PIM, blocks and goalie lines (incl. who started)
//!
//! Coordinates are normalised so the shooting team always attacks the net at
//! x = +89 ft (y = 0). Strength is from the shooting team's view: EV, PP, SH,
//! or EN (opponent's net empty).

use std::collections::HashMap;

use actix_web::{web, HttpResponse, Responder};
use chrono::NaiveDate;
use futures::stream::{self, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_postgres::types::ToSql;

use crate::State;

const API: &str = "https://api-web.nhle.com/v1";

fn dberr(e: &tokio_postgres::Error) -> String {
    e.as_db_error().map(|d| format!("{} ({})", d.message(), d.detail().unwrap_or(""))).unwrap_or_else(|| e.to_string())
}

#[derive(Debug, Default, Clone)]
pub struct Game {
    pub id: i64, pub season: i32, pub game_type: i32, pub date: Option<NaiveDate>, pub start: Option<chrono::DateTime<chrono::Utc>>,
    pub away: String, pub home: String, pub away_score: Option<i32>, pub home_score: Option<i32>, pub completed: bool, pub last_period: Option<String>,
}
#[derive(Debug, Default, Clone)]
pub struct BoxRow {
    pub team: String, pub player_id: i64, pub name: String, pub pos: String, pub toi: Option<i32>,
    pub goals: Option<i32>, pub assists: Option<i32>, pub sog: Option<i32>, pub hits: Option<i32>, pub pim: Option<i32>, pub blocks: Option<i32>,
    pub pp_goals: Option<i32>, pub starter: bool, pub saves: Option<i32>, pub shots_against: Option<i32>, pub goals_against: Option<i32>,
}
#[derive(Debug, Default, Clone)]
pub struct ShotRow {
    pub event_id: i32, pub team: String, pub shooter: Option<i64>, pub goalie: Option<i64>, pub period: i32, pub period_type: String, pub sec: i32,
    pub kind: String, pub strength: String, pub skaters: String, pub x: Option<f64>, pub y: Option<f64>, pub shot_type: Option<String>,
    pub assist1: Option<i64>, pub assist2: Option<i64>, pub empty_net: bool,
}
#[derive(Debug, Default, Clone)]
pub struct PenRow { pub event_id: i32, pub team: String, pub player: Option<i64>, pub period: i32, pub sec: i32, pub duration: i32, pub kind: Option<String> }

fn mmss(s: &str) -> Option<i32> {
    let mut it = s.split(':');
    let m: i32 = it.next()?.trim().parse().ok()?;
    let ss: i32 = it.next()?.trim().parse().ok()?;
    Some(m * 60 + ss)
}

/// Strength from the shooting team's view, from the 4-digit situation code
/// (away goalie, away skaters, home skaters, home goalie).
pub fn strength(code: &str, shooter_home: bool) -> (String, String, bool) {
    let d: Vec<i32> = code.chars().filter_map(|c| c.to_digit(10).map(|x| x as i32)).collect();
    if d.len() != 4 { return ("EV".into(), "5v5".into(), false); }
    let (ag, ask, hsk, hg) = (d[0], d[1], d[2], d[3]);
    let (own, opp, opp_goalie) = if shooter_home { (hsk, ask, ag) } else { (ask, hsk, hg) };
    let empty = opp_goalie == 0;
    let s = if empty { "EN" } else if own > opp { "PP" } else if own < opp { "SH" } else { "EV" };
    (s.into(), format!("{own}v{opp}"), empty)
}

/// Parse play-by-play (pure). Returns the game header, shots and penalties.
pub fn parse_pbp(v: &Value) -> Option<(Game, Vec<ShotRow>, Vec<PenRow>)> {
    let mut g = Game { id: v["id"].as_i64()?, season: v["season"].as_i64()? as i32, game_type: v["gameType"].as_i64().unwrap_or(2) as i32, ..Default::default() };
    g.date = v["gameDate"].as_str().and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok());
    g.start = v["startTimeUTC"].as_str().and_then(|d| chrono::DateTime::parse_from_rfc3339(d).ok()).map(|d| d.with_timezone(&chrono::Utc));
    g.away = v["awayTeam"]["abbrev"].as_str()?.to_string();
    g.home = v["homeTeam"]["abbrev"].as_str()?.to_string();
    let home_id = v["homeTeam"]["id"].as_i64().unwrap_or(-1);
    g.away_score = v["awayTeam"]["score"].as_i64().map(|x| x as i32);
    g.home_score = v["homeTeam"]["score"].as_i64().map(|x| x as i32);
    g.completed = matches!(v["gameState"].as_str(), Some("OFF") | Some("FINAL"));
    g.last_period = v["gameOutcome"]["lastPeriodType"].as_str().map(String::from);
    let mut shots = Vec::new();
    let mut pens = Vec::new();
    for p in v["plays"].as_array().into_iter().flatten() {
        let kind = match p["typeDescKey"].as_str().unwrap_or("") {
            "goal" => "goal", "shot-on-goal" => "sog", "missed-shot" => "missed", "blocked-shot" => "blocked", "penalty" => "penalty", _ => continue,
        };
        let d = &p["details"];
        let owner = d["eventOwnerTeamId"].as_i64().unwrap_or(0);
        let period = p["periodDescriptor"]["number"].as_i64().unwrap_or(0) as i32;
        let sec = p["timeInPeriod"].as_str().and_then(mmss).unwrap_or(0);
        let event_id = p["eventId"].as_i64().unwrap_or(0) as i32;
        if kind == "penalty" {
            let team = if owner == home_id { g.home.clone() } else { g.away.clone() };
            pens.push(PenRow { event_id, team, player: d["committedByPlayerId"].as_i64(), period, sec,
                duration: d["duration"].as_i64().unwrap_or(2) as i32, kind: d["descKey"].as_str().map(String::from) });
            continue;
        }
        // the event owner is the shooting team for every attempt type (verified: blocked shots' shooters are on the owner team)
        let shooter_home = owner == home_id;
        let team = if shooter_home { g.home.clone() } else { g.away.clone() };
        let (st, sk, en) = strength(p["situationCode"].as_str().unwrap_or("1551"), shooter_home);
        // normalise: shooting team attacks +x
        let home_def_left = p["homeTeamDefendingSide"].as_str() == Some("left");
        let attacks_right = if shooter_home { home_def_left } else { !home_def_left };
        let (x, y) = match (d["xCoord"].as_f64(), d["yCoord"].as_f64()) {
            (Some(x), Some(y)) => if attacks_right { (Some(x), Some(y)) } else { (Some(-x), Some(-y)) },
            _ => (None, None),
        };
        shots.push(ShotRow {
            event_id, team, shooter: d["shootingPlayerId"].as_i64().or(d["scoringPlayerId"].as_i64()), goalie: d["goalieInNetId"].as_i64(),
            period, period_type: p["periodDescriptor"]["periodType"].as_str().unwrap_or("REG").to_string(), sec,
            kind: kind.into(), strength: st, skaters: sk, x, y, shot_type: d["shotType"].as_str().map(String::from),
            assist1: d["assist1PlayerId"].as_i64(), assist2: d["assist2PlayerId"].as_i64(), empty_net: en,
        });
    }
    Some((g, shots, pens))
}

pub fn parse_box(v: &Value, away: &str, home: &str) -> Vec<BoxRow> {
    let mut out = Vec::new();
    for (side, team) in [("awayTeam", away), ("homeTeam", home)] {
        let t = &v["playerByGameStats"][side];
        for grp in ["forwards", "defense", "goalies"] {
            for p in t[grp].as_array().into_iter().flatten() {
                let i = |k: &str| p[k].as_i64().map(|x| x as i32);
                out.push(BoxRow {
                    team: team.to_string(), player_id: p["playerId"].as_i64().unwrap_or(0), name: p["name"]["default"].as_str().unwrap_or("").to_string(),
                    pos: p["position"].as_str().unwrap_or("").to_string(), toi: p["toi"].as_str().and_then(mmss),
                    goals: i("goals"), assists: i("assists"), sog: i("sog"), hits: i("hits"), pim: i("pim"), blocks: i("blockedShots"), pp_goals: i("powerPlayGoals"),
                    starter: p["starter"].as_bool().unwrap_or(false), saves: i("saves"), shots_against: i("shotsAgainst"), goals_against: i("goalsAgainst"),
                });
            }
        }
    }
    out
}

async fn exec_rows(db: &tokio_postgres::Client, head: &str, ncol: usize, rows: Vec<Vec<Box<dyn ToSql + Sync + Send>>>) -> Result<(), String> {
    for chunk in rows.chunks(300) {
        let mut sql = String::from(head);
        let mut ps: Vec<&(dyn ToSql + Sync)> = Vec::new();
        for (i, r) in chunk.iter().enumerate() {
            if i > 0 { sql.push(','); }
            sql.push('(');
            for k in 0..ncol { if k > 0 { sql.push(','); } sql.push_str(&format!("${}", i * ncol + k + 1)); }
            sql.push(')');
            for b in r { ps.push(b.as_ref() as &(dyn ToSql + Sync)); }
        }
        sql.push_str(" ON CONFLICT DO NOTHING");
        db.execute(sql.as_str(), &ps).await.map_err(|e| dberr(&e))?;
    }
    Ok(())
}

async fn store(db: &tokio_postgres::Client, g: &Game, shots: Vec<ShotRow>, pens: Vec<PenRow>, boxes: Vec<BoxRow>) -> Result<(), String> {
    db.execute(
        "INSERT INTO nhl_games_api (game_id, season, game_type, game_date, start_time, away, home, away_score, home_score, completed, last_period, updated_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11, now()) ON CONFLICT (game_id) DO UPDATE SET season=EXCLUDED.season, game_type=EXCLUDED.game_type, \
         game_date=EXCLUDED.game_date, start_time=EXCLUDED.start_time, away=EXCLUDED.away, home=EXCLUDED.home, away_score=EXCLUDED.away_score, \
         home_score=EXCLUDED.home_score, completed=EXCLUDED.completed, last_period=EXCLUDED.last_period, updated_at=now()",
        &[&g.id, &g.season, &g.game_type, &g.date, &g.start, &g.away, &g.home, &g.away_score, &g.home_score, &g.completed, &g.last_period]).await.map_err(|e| dberr(&e))?;
    if !g.completed { return Ok(()); }
    for t in ["nhl_box_api", "nhl_shots_api", "nhl_penalties_api"] {
        db.execute(&format!("DELETE FROM {t} WHERE game_id = $1"), &[&g.id]).await.map_err(|e| dberr(&e))?;
    }
    let b = |x: Box<dyn ToSql + Sync + Send>| x;
    exec_rows(db, "INSERT INTO nhl_box_api (game_id, team, player_id, name, pos, toi_sec, goals, assists, sog, hits, pim, blocks, pp_goals, starter, saves, shots_against, goals_against) VALUES ", 17,
        boxes.into_iter().map(|r| vec![b(Box::new(g.id)), b(Box::new(r.team)), b(Box::new(r.player_id)), b(Box::new(r.name)), b(Box::new(r.pos)), b(Box::new(r.toi)),
            b(Box::new(r.goals)), b(Box::new(r.assists)), b(Box::new(r.sog)), b(Box::new(r.hits)), b(Box::new(r.pim)), b(Box::new(r.blocks)), b(Box::new(r.pp_goals)),
            b(Box::new(r.starter)), b(Box::new(r.saves)), b(Box::new(r.shots_against)), b(Box::new(r.goals_against))]).collect()).await?;
    exec_rows(db, "INSERT INTO nhl_shots_api (game_id, event_id, team, shooter, goalie, period, period_type, sec, kind, strength, skaters, x, y, shot_type, assist1, assist2, empty_net) VALUES ", 17,
        shots.into_iter().map(|r| vec![b(Box::new(g.id)), b(Box::new(r.event_id)), b(Box::new(r.team)), b(Box::new(r.shooter)), b(Box::new(r.goalie)), b(Box::new(r.period)),
            b(Box::new(r.period_type)), b(Box::new(r.sec)), b(Box::new(r.kind)), b(Box::new(r.strength)), b(Box::new(r.skaters)), b(Box::new(r.x)), b(Box::new(r.y)),
            b(Box::new(r.shot_type)), b(Box::new(r.assist1)), b(Box::new(r.assist2)), b(Box::new(r.empty_net))]).collect()).await?;
    exec_rows(db, "INSERT INTO nhl_penalties_api (game_id, event_id, team, player, period, sec, duration, kind) VALUES ", 8,
        pens.into_iter().map(|r| vec![b(Box::new(g.id)), b(Box::new(r.event_id)), b(Box::new(r.team)), b(Box::new(r.player)), b(Box::new(r.period)),
            b(Box::new(r.sec)), b(Box::new(r.duration)), b(Box::new(r.kind))]).collect()).await?;
    Ok(())
}

#[derive(Deserialize)]
pub struct IngestQuery { from: String, to: String, #[serde(default = "d_c")] concurrency: usize, #[serde(default)] refresh: bool }
fn d_c() -> usize { 8 }

/// POST /ingest/nhl/api?from=2024-10-04&to=2025-06-20
pub async fn ingest(state: web::Data<State>, q: web::Query<IngestQuery>) -> impl Responder {
    let (Ok(from), Ok(to)) = (NaiveDate::parse_from_str(&q.from, "%Y-%m-%d"), NaiveDate::parse_from_str(&q.to, "%Y-%m-%d")) else {
        return HttpResponse::BadRequest().json(json!({ "error": "from/to must be YYYY-MM-DD" }));
    };
    let t0 = std::time::Instant::now();
    let done: std::collections::HashSet<i64> = if q.refresh { Default::default() } else {
        state.db.query("SELECT game_id FROM nhl_games_api WHERE completed AND game_date BETWEEN $1 AND $2", &[&from, &to]).await
            .map(|r| r.iter().map(|x| x.get(0)).collect()).unwrap_or_default()
    };
    let mut weeks = Vec::new();
    let mut d = from;
    while d <= to { weeks.push(d); d += chrono::Duration::days(7); }
    let mut games: HashMap<i64, bool> = HashMap::new(); // id -> completed per schedule
    let boards: Vec<Result<Value, String>> = stream::iter(weeks.into_iter().map(|w| {
        let http = state.http.clone();
        async move { crate::nba_espn::get_json(&http, &format!("{API}/schedule/{w}")).await }
    })).buffer_unordered(4).collect().await;
    for b in boards {
        let b = match b { Ok(b) => b, Err(e) => return HttpResponse::InternalServerError().json(json!({ "error": e })) };
        for day in b["gameWeek"].as_array().into_iter().flatten() {
            let Some(date) = day["date"].as_str().and_then(|x| NaiveDate::parse_from_str(x, "%Y-%m-%d").ok()) else { continue };
            if date < from || date > to { continue; }
            for g in day["games"].as_array().into_iter().flatten() {
                let (Some(id), gt) = (g["id"].as_i64(), g["gameType"].as_i64().unwrap_or(0)) else { continue };
                if !(1..=3).contains(&gt) { continue; }
                let completed = matches!(g["gameState"].as_str(), Some("OFF") | Some("FINAL"));
                if !(completed && done.contains(&id)) { games.insert(id, completed); }
            }
        }
    }
    let ids: Vec<(i64, bool)> = games.into_iter().collect();
    let fetched: Vec<(i64, Result<(Value, Option<Value>), String>)> = stream::iter(ids.iter().map(|(id, completed)| {
        let http = state.http.clone();
        let (id, completed) = (*id, *completed);
        async move {
            let pbp = crate::nba_espn::get_json(&http, &format!("{API}/gamecenter/{id}/play-by-play")).await;
            let bx = if completed { crate::nba_espn::get_json(&http, &format!("{API}/gamecenter/{id}/boxscore")).await.ok() } else { None };
            (id, pbp.map(|p| (p, bx)))
        }
    })).buffer_unordered(q.concurrency.clamp(1, 16)).collect().await;
    let (mut stored, mut completed, mut failed) = (0, 0, Vec::new());
    for (id, r) in fetched {
        let Ok((pbp, bx)) = r else { failed.push(id); continue };
        let Some((g, shots, pens)) = parse_pbp(&pbp) else { failed.push(id); continue };
        let boxes = bx.map(|b| parse_box(&b, &g.away, &g.home)).unwrap_or_default();
        match store(&state.db, &g, shots, pens, boxes).await {
            Ok(()) => { stored += 1; completed += g.completed as i32; }
            Err(e) => { eprintln!("nhl store {id}: {e}"); failed.push(id); }
        }
    }
    HttpResponse::Ok().json(json!({ "source": "NHL API (api-web.nhle.com)", "from": from, "to": to, "games": ids.len(), "stored": stored,
        "completed": completed, "failed": failed.len(), "failed_ids": failed.into_iter().take(10).collect::<Vec<_>>(), "secs": t0.elapsed().as_secs() }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strength_codes() {
        assert_eq!(strength("1551", true), ("EV".into(), "5v5".into(), false));
        assert_eq!(strength("1451", true).0, "PP");   // home 5 vs away 4
        assert_eq!(strength("1451", false).0, "SH");
        assert_eq!(strength("0651", false).0, "PP");  // away pulled its goalie: 6 skaters v 5
        assert_eq!(strength("1560", false).0, "EN");  // home net empty, away shooting
    }
    #[test]
    fn parses_pbp() {
        let v = json!({ "id": 2025020740, "season": 20252026, "gameType": 2, "gameDate": "2026-01-15", "gameState": "OFF",
            "awayTeam": { "id": 8, "abbrev": "MTL", "score": 3 }, "homeTeam": { "id": 7, "abbrev": "BUF", "score": 4 },
            "gameOutcome": { "lastPeriodType": "REG" },
            "plays": [
              { "eventId": 55, "periodDescriptor": { "number": 1, "periodType": "REG" }, "timeInPeriod": "00:26", "situationCode": "1551",
                "homeTeamDefendingSide": "left", "typeDescKey": "shot-on-goal", "details": { "xCoord": 31, "yCoord": 33, "shotType": "snap", "shootingPlayerId": 1, "goalieInNetId": 9, "eventOwnerTeamId": 7 } },
              { "eventId": 95, "periodDescriptor": { "number": 1, "periodType": "REG" }, "timeInPeriod": "03:11", "situationCode": "1551",
                "homeTeamDefendingSide": "left", "typeDescKey": "goal", "details": { "xCoord": -83, "yCoord": -8, "shotType": "wrist", "scoringPlayerId": 2, "assist1PlayerId": 3, "eventOwnerTeamId": 8 } },
              { "eventId": 99, "periodDescriptor": { "number": 1, "periodType": "REG" }, "timeInPeriod": "04:00", "situationCode": "1551",
                "homeTeamDefendingSide": "left", "typeDescKey": "blocked-shot", "details": { "xCoord": -60, "yCoord": 5, "shootingPlayerId": 4, "eventOwnerTeamId": 8 } },
              { "eventId": 156, "periodDescriptor": { "number": 1, "periodType": "REG" }, "timeInPeriod": "03:46", "situationCode": "1551",
                "typeDescKey": "penalty", "details": { "duration": 2, "committedByPlayerId": 5, "eventOwnerTeamId": 8, "descKey": "high-sticking" } } ] });
        let (g, shots, pens) = parse_pbp(&v).unwrap();
        assert_eq!((g.away.as_str(), g.home.as_str(), g.completed), ("MTL", "BUF", true));
        assert_eq!(shots.len(), 3);
        assert_eq!((shots[0].team.as_str(), shots[0].x), ("BUF", Some(31.0)));       // home defends left -> attacks right
        assert_eq!((shots[1].team.as_str(), shots[1].x, shots[1].kind.as_str()), ("MTL", Some(83.0), "goal")); // away attacks left -> flipped
        assert_eq!(shots[1].assist1, Some(3));
        assert_eq!((shots[2].team.as_str(), shots[2].x), ("MTL", Some(60.0)));      // MTL attempt, blocked; MTL attacks left -> flipped
        assert_eq!(pens[0].team, "MTL");
    }
}
