//! NBA game data from ESPN's public site API.
//!
//! * scoreboard (`/scoreboard?dates=YYYYMMDD`) -> the day's game ids
//! * summary (`/summary?event=ID`) -> header (teams, score, status), pickcenter
//!   (pregame spread / total), full player box scores (incl. starters and DNP
//!   reasons) and play-by-play (every field-goal attempt with court x/y).
//!
//! Stored in `nba_games_espn`, `nba_box_espn`, `nba_shots_espn`. These feed the
//! NBA possession simulator (`nba_sim.rs`) and the broadcast court.

use std::collections::HashMap;

use actix_web::{web, HttpResponse, Responder};
use chrono::NaiveDate;
use futures::stream::{self, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_postgres::types::ToSql;

use crate::State;

const SITE: &str = "https://site.api.espn.com/apis/site/v2/sports/basketball/nba";

fn internal(m: impl std::fmt::Display) -> HttpResponse {
    eprintln!("projection-service(nba-espn): {m}");
    HttpResponse::InternalServerError().json(json!({ "error": m.to_string() }))
}
fn dberr(e: &tokio_postgres::Error) -> String {
    e.as_db_error().map(|d| format!("{} ({})", d.message(), d.detail().unwrap_or(""))).unwrap_or_else(|| e.to_string())
}

pub async fn get_json(http: &reqwest::Client, url: &str) -> Result<Value, String> {
    let mut last = String::new();
    for attempt in 0..3u64 {
        if attempt > 0 {
            actix_web::rt::time::sleep(std::time::Duration::from_millis(400 * attempt)).await;
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

#[derive(Debug, Default, Clone)]
pub struct BoxRow {
    pub team: String,
    pub player_id: String,
    pub name: String,
    pub pos: String,
    pub starter: bool,
    pub dnp: bool,
    pub reason: Option<String>,
    pub min: Option<f64>,
    pub s: [Option<i32>; 13], // pts fgm fga fg3m fg3a ftm fta oreb dreb ast tov stl blk  (pf separately)
    pub pf: Option<i32>,
}

#[derive(Debug, Default, Clone)]
pub struct Shot {
    pub seq: i32,
    pub player_id: String,
    pub team: Option<String>,
    pub period: i32,
    pub x: f64,
    pub y: f64,
    pub made: bool,
    pub value: i32,
}

#[derive(Debug, Default, Clone)]
pub struct GameRow {
    pub espn_id: String,
    pub season: i32,
    pub season_type: i32,
    pub date: Option<NaiveDate>,
    pub start: Option<chrono::DateTime<chrono::Utc>>,
    pub away: String,
    pub home: String,
    pub away_score: Option<i32>,
    pub home_score: Option<i32>,
    pub completed: bool,
    pub spread: Option<f64>,
    pub over_under: Option<f64>,
    pub periods: Option<i32>,
    pub boxes: Vec<BoxRow>,
    pub shots: Vec<Shot>,
}

fn made_att(s: &str) -> (Option<i32>, Option<i32>) {
    let mut it = s.split('-');
    let m = it.next().and_then(|x| x.trim().parse().ok());
    let a = it.next().and_then(|x| x.trim().parse().ok());
    (m, a)
}

/// Parse one ESPN game summary (pure).
pub fn parse_summary(id: &str, sm: &Value) -> Option<GameRow> {
    let h = &sm["header"];
    let c = &h["competitions"][0];
    let mut g = GameRow { espn_id: id.to_string(), ..Default::default() };
    g.season = h["season"]["year"].as_i64()? as i32;
    g.season_type = h["season"]["type"].as_i64().unwrap_or(2) as i32;
    g.start = c["date"].as_str().and_then(|d| chrono::DateTime::parse_from_rfc3339(&d.replace('Z', ":00Z")).ok()
        .or_else(|| chrono::DateTime::parse_from_rfc3339(d).ok())).map(|d| d.with_timezone(&chrono::Utc));
    // ESPN dates are UTC; the game's local date is a few hours earlier
    g.date = g.start.map(|d| (d - chrono::Duration::hours(6)).date_naive());
    let mut team_by_id: HashMap<String, String> = HashMap::new();
    for comp in c["competitors"].as_array()? {
        let abbr = comp["team"]["abbreviation"].as_str()?.to_string();
        team_by_id.insert(comp["team"]["id"].as_str().unwrap_or("").to_string(), abbr.clone());
        let score = comp["score"].as_str().and_then(|x| x.parse().ok());
        if comp["homeAway"] == "home" { g.home = abbr; g.home_score = score; } else { g.away = abbr; g.away_score = score; }
    }
    g.completed = c["status"]["type"]["completed"].as_bool().unwrap_or(false);
    g.periods = c["status"]["period"].as_i64().map(|x| x as i32);
    if let Some(pc) = sm["pickcenter"].as_array().and_then(|a| a.first()) {
        g.spread = pc["spread"].as_f64();
        g.over_under = pc["overUnder"].as_f64();
    }
    for tp in sm["boxscore"]["players"].as_array().into_iter().flatten() {
        let team = tp["team"]["abbreviation"].as_str().unwrap_or("").to_string();
        let Some(st) = tp["statistics"].as_array().and_then(|a| a.first()) else { continue };
        let labels: Vec<&str> = st["labels"].as_array().map(|a| a.iter().filter_map(|x| x.as_str()).collect()).unwrap_or_default();
        let col = |n: &str| labels.iter().position(|l| *l == n);
        for a in st["athletes"].as_array().into_iter().flatten() {
            let stats: Vec<&str> = a["stats"].as_array().map(|v| v.iter().filter_map(|x| x.as_str()).collect()).unwrap_or_default();
            let get = |n: &str| col(n).and_then(|i| stats.get(i).copied());
            let int = |n: &str| get(n).and_then(|x| x.parse::<i32>().ok());
            let (fgm, fga) = get("FG").map(made_att).unwrap_or((None, None));
            let (f3m, f3a) = get("3PT").map(made_att).unwrap_or((None, None));
            let (ftm, fta) = get("FT").map(made_att).unwrap_or((None, None));
            let dnp = a["didNotPlay"].as_bool().unwrap_or(false) || stats.is_empty();
            g.boxes.push(BoxRow {
                team: team.clone(),
                player_id: a["athlete"]["id"].as_str().unwrap_or("").to_string(),
                name: a["athlete"]["displayName"].as_str().unwrap_or("").to_string(),
                pos: a["athlete"]["position"]["abbreviation"].as_str().unwrap_or("").to_string(),
                starter: a["starter"].as_bool().unwrap_or(false),
                dnp,
                reason: a["reason"].as_str().map(String::from).filter(|r| !r.is_empty()),
                min: get("MIN").and_then(|x| x.parse::<f64>().ok()),
                s: [int("PTS"), fgm, fga, f3m, f3a, ftm, fta, int("OREB"), int("DREB"), int("AST"), int("TO"), int("STL"), int("BLK")],
                pf: int("PF"),
            });
        }
    }
    for (i, p) in sm["plays"].as_array().into_iter().flatten().enumerate() {
        if !p["shootingPlay"].as_bool().unwrap_or(false) { continue; }
        let ty = p["type"]["text"].as_str().unwrap_or("");
        if ty.contains("Free Throw") { continue; }
        let (Some(x), Some(y)) = (p["coordinate"]["x"].as_f64(), p["coordinate"]["y"].as_f64()) else { continue };
        if !(0.0..=50.0).contains(&x) || !(-5.0..=94.0).contains(&y) { continue; }
        let Some(pid) = p["participants"][0]["athlete"]["id"].as_str() else { continue };
        let text = p["text"].as_str().unwrap_or("");
        g.shots.push(Shot {
            seq: p["sequenceNumber"].as_str().and_then(|s| s.parse().ok()).unwrap_or(i as i32),
            player_id: pid.to_string(),
            team: p["team"]["id"].as_str().and_then(|t| team_by_id.get(t).cloned()),
            period: p["period"]["number"].as_i64().unwrap_or(0) as i32,
            x, y,
            made: p["scoringPlay"].as_bool().unwrap_or(false),
            value: if text.contains("three point") { 3 } else { 2 },
        });
    }
    Some(g)
}

async fn store(db: &tokio_postgres::Client, g: &GameRow) -> Result<(), String> {
    db.execute(
        "INSERT INTO nba_games_espn (espn_id, season, season_type, game_date, start_time, away, home, away_score, home_score, completed, spread, over_under, periods, updated_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13, now()) ON CONFLICT (espn_id) DO UPDATE SET season=EXCLUDED.season, season_type=EXCLUDED.season_type, \
         game_date=EXCLUDED.game_date, start_time=EXCLUDED.start_time, away=EXCLUDED.away, home=EXCLUDED.home, away_score=EXCLUDED.away_score, home_score=EXCLUDED.home_score, \
         completed=EXCLUDED.completed, spread=COALESCE(EXCLUDED.spread, nba_games_espn.spread), over_under=COALESCE(EXCLUDED.over_under, nba_games_espn.over_under), \
         periods=EXCLUDED.periods, updated_at=now()",
        &[&g.espn_id, &g.season, &g.season_type, &g.date, &g.start, &g.away, &g.home, &g.away_score, &g.home_score, &g.completed, &g.spread, &g.over_under, &g.periods],
    ).await.map_err(|e| dberr(&e))?;
    if !g.completed { return Ok(()); }
    db.execute("DELETE FROM nba_box_espn WHERE espn_id = $1", &[&g.espn_id]).await.map_err(|e| dberr(&e))?;
    db.execute("DELETE FROM nba_shots_espn WHERE espn_id = $1", &[&g.espn_id]).await.map_err(|e| dberr(&e))?;
    for chunk in g.boxes.chunks(40) {
        let mut sql = String::from("INSERT INTO nba_box_espn (espn_id, team, player_id, name, pos, starter, dnp, reason, min, pts, fgm, fga, fg3m, fg3a, ftm, fta, oreb, dreb, ast, tov, stl, blk, pf) VALUES ");
        let mut ps: Vec<Box<dyn ToSql + Sync + Send>> = Vec::new();
        for (i, b) in chunk.iter().enumerate() {
            if i > 0 { sql.push(','); }
            sql.push('(');
            for k in 0..23 { if k > 0 { sql.push(','); } sql.push_str(&format!("${}", i * 23 + k + 1)); }
            sql.push(')');
            ps.push(Box::new(g.espn_id.clone())); ps.push(Box::new(b.team.clone())); ps.push(Box::new(b.player_id.clone()));
            ps.push(Box::new(b.name.clone())); ps.push(Box::new(b.pos.clone())); ps.push(Box::new(b.starter)); ps.push(Box::new(b.dnp));
            ps.push(Box::new(b.reason.clone())); ps.push(Box::new(b.min));
            for v in b.s { ps.push(Box::new(v)); }
            ps.push(Box::new(b.pf));
        }
        sql.push_str(" ON CONFLICT DO NOTHING");
        let refs: Vec<&(dyn ToSql + Sync)> = ps.iter().map(|b| b.as_ref() as &(dyn ToSql + Sync)).collect();
        db.execute(sql.as_str(), &refs).await.map_err(|e| format!("box: {}", dberr(&e)))?;
    }
    for chunk in g.shots.chunks(200) {
        let mut sql = String::from("INSERT INTO nba_shots_espn (espn_id, seq, player_id, team, period, x, y, made, value) VALUES ");
        let mut ps: Vec<Box<dyn ToSql + Sync + Send>> = Vec::new();
        for (i, s) in chunk.iter().enumerate() {
            if i > 0 { sql.push(','); }
            sql.push('(');
            for k in 0..9 { if k > 0 { sql.push(','); } sql.push_str(&format!("${}", i * 9 + k + 1)); }
            sql.push(')');
            ps.push(Box::new(g.espn_id.clone())); ps.push(Box::new(s.seq)); ps.push(Box::new(s.player_id.clone())); ps.push(Box::new(s.team.clone()));
            ps.push(Box::new(s.period)); ps.push(Box::new(s.x)); ps.push(Box::new(s.y)); ps.push(Box::new(s.made)); ps.push(Box::new(s.value));
        }
        sql.push_str(" ON CONFLICT DO NOTHING");
        let refs: Vec<&(dyn ToSql + Sync)> = ps.iter().map(|b| b.as_ref() as &(dyn ToSql + Sync)).collect();
        db.execute(sql.as_str(), &refs).await.map_err(|e| format!("shots: {}", dberr(&e)))?;
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct IngestQuery {
    from: String,
    to: String,
    #[serde(default = "d_conc")] concurrency: usize,
    /// re-fetch games already stored as completed
    #[serde(default)] refresh: bool,
}
fn d_conc() -> usize { 8 }

/// POST /ingest/nba/espn?from=2025-10-21&to=2026-04-12 — every game in the range.
pub async fn ingest(state: web::Data<State>, q: web::Query<IngestQuery>) -> impl Responder {
    let (Ok(from), Ok(to)) = (NaiveDate::parse_from_str(&q.from, "%Y-%m-%d"), NaiveDate::parse_from_str(&q.to, "%Y-%m-%d")) else {
        return HttpResponse::BadRequest().json(json!({ "error": "from/to must be YYYY-MM-DD" }));
    };
    let t0 = std::time::Instant::now();
    let done: std::collections::HashSet<String> = if q.refresh { Default::default() } else {
        state.db.query("SELECT espn_id FROM nba_games_espn WHERE completed AND game_date BETWEEN $1 AND $2", &[&from, &to]).await
            .map(|r| r.iter().map(|x| x.get(0)).collect()).unwrap_or_default()
    };
    let mut ids: Vec<String> = Vec::new();
    let mut d = from;
    let mut days = Vec::new();
    while d <= to { days.push(d); d += chrono::Duration::days(1); }
    let boards: Vec<Result<Value, String>> = stream::iter(days.iter().map(|d| {
        let http = state.http.clone();
        let url = format!("{SITE}/scoreboard?dates={}", d.format("%Y%m%d"));
        async move { get_json(&http, &url).await }
    })).buffer_unordered(q.concurrency.clamp(1, 16)).collect().await;
    for b in boards {
        let b = match b { Ok(b) => b, Err(e) => return internal(e) };
        for ev in b["events"].as_array().into_iter().flatten() {
            if let Some(id) = ev["id"].as_str() {
                let completed = ev["status"]["type"]["completed"].as_bool().unwrap_or(false);
                if !(completed && done.contains(id)) { ids.push(id.to_string()); }
            }
        }
    }
    ids.sort();
    ids.dedup();
    let results: Vec<(String, Result<Value, String>)> = stream::iter(ids.iter().map(|id| {
        let http = state.http.clone();
        let url = format!("{SITE}/summary?event={id}");
        let id = id.clone();
        async move { (id, get_json(&http, &url).await) }
    })).buffer_unordered(q.concurrency.clamp(1, 16)).collect().await;
    let (mut stored, mut completed, mut failed) = (0, 0, Vec::new());
    for (id, r) in results {
        let parsed = r.ok().and_then(|v| parse_summary(&id, &v));
        match parsed {
            Some(g) => {
                if let Err(e) = store(&state.db, &g).await { failed.push(json!({ "id": id, "error": e })); continue; }
                stored += 1;
                completed += g.completed as i32;
            }
            None => failed.push(json!({ "id": id, "error": "fetch/parse failed" })),
        }
    }
    HttpResponse::Ok().json(json!({
        "source": "ESPN site API (scoreboard + summary)", "from": from, "to": to, "days": days.len(),
        "games_fetched": ids.len(), "stored": stored, "completed": completed, "skipped_already_stored": done.len(),
        "failed": failed.len(), "failures": failed.into_iter().take(10).collect::<Vec<_>>(), "secs": t0.elapsed().as_secs(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_a_summary() {
        let sm = json!({
            "header": { "season": { "year": 2026, "type": 2 }, "competitions": [{
                "date": "2026-01-15T19:00Z", "status": { "type": { "completed": true }, "period": 4 },
                "competitors": [ { "homeAway": "home", "team": { "id": "19", "abbreviation": "ORL" }, "score": "118" },
                                 { "homeAway": "away", "team": { "id": "29", "abbreviation": "MEM" }, "score": "111" } ] }] },
            "pickcenter": [{ "spread": -5.5, "overUnder": 227.5 }],
            "boxscore": { "players": [{ "team": { "abbreviation": "MEM" }, "statistics": [{
                "labels": ["MIN","PTS","FG","3PT","FT","REB","AST","TO","STL","BLK","OREB","DREB","PF","+/-"],
                "athletes": [
                    { "athlete": { "id": "1", "displayName": "A B", "position": { "abbreviation": "F" } }, "starter": true, "didNotPlay": false,
                      "stats": ["33","30","12-22","3-5","3-3","3","1","4","2","2","0","3","3","-21"] },
                    { "athlete": { "id": "2", "displayName": "C D" }, "starter": false, "didNotPlay": true, "reason": "COACH'S DECISION", "stats": [] } ] }] }] },
            "plays": [
                { "shootingPlay": true, "scoringPlay": true, "type": { "text": "Jump Shot" }, "text": "A B makes 25-foot three point jumper",
                  "coordinate": { "x": 38, "y": 22 }, "participants": [{ "athlete": { "id": "1" } }], "team": { "id": "29" }, "period": { "number": 1 }, "sequenceNumber": "12" },
                { "shootingPlay": true, "scoringPlay": true, "type": { "text": "Free Throw - 1 of 2" }, "text": "A B makes free throw",
                  "coordinate": { "x": -214748340, "y": -214748365 }, "participants": [{ "athlete": { "id": "1" } }] } ]
        });
        let g = parse_summary("401", &sm).unwrap();
        assert_eq!((g.home.as_str(), g.away.as_str(), g.home_score, g.away_score), ("ORL", "MEM", Some(118), Some(111)));
        assert_eq!(g.spread, Some(-5.5));
        assert_eq!(g.boxes.len(), 2);
        assert_eq!(g.boxes[0].s[1..5], [Some(12), Some(22), Some(3), Some(5)]);
        assert_eq!(g.boxes[0].s[7], Some(0));
        assert!(g.boxes[1].dnp && g.boxes[1].reason.as_deref() == Some("COACH'S DECISION"));
        assert_eq!(g.shots.len(), 1);
        assert_eq!((g.shots[0].value, g.shots[0].made, g.shots[0].team.as_deref()), (3, true, Some("MEM")));
        assert_eq!(g.date, NaiveDate::from_ymd_opt(2026, 1, 15));
    }
}
