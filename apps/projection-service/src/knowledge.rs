//! Sports facts for MonsterGPT (docs/architecture/monster-gpt-v2.md section 6):
//! standings, champions, a player's games and a team's results, computed from the
//! stored games and logs. Read-only; every response says which table and source it
//! came from, how many rows it used and the date of the newest one.
//!
//!   GET /kb/standings?sport=&season=          league table from final scores
//!   GET /kb/champions?sport=&season=&limit=   last game of each postseason
//!   GET /kb/player_stats?entity=&stats=&last=&season=
//!   GET /kb/results?entity=&last=&opponent=&season=&upcoming=true  (upcoming: scheduled games, soonest first)
//!
//! Seasons are labelled by their start year (2025 = 2025-26 in the NBA and NHL);
//! MLB seasons are calendar years. Player logs hold regular-season games only for
//! the NBA, NHL and NFL and include the postseason for MLB (data-foundation.md).

use std::collections::HashMap;

use actix_web::{web, HttpResponse, Responder};
use chrono::NaiveDate;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::State;

fn dberr(e: &tokio_postgres::Error) -> String { e.to_string() }
fn bad(msg: impl Into<String>) -> HttpResponse { HttpResponse::BadRequest().json(json!({ "error": msg.into() })) }
fn r3(x: f64) -> f64 { (x * 1000.0).round() / 1000.0 }

pub(crate) fn source_of(sport: &str) -> &'static str {
    match sport { "nfl" => "nflverse", "nhl" => "nhl_api", "mlb" => "mlb_statsapi", _ => "espn_site" }
}
/// Which games a sport's player logs include.
pub(crate) fn log_game_types(sport: &str) -> &'static str {
    match sport { "mlb" => "regular season and postseason", _ => "regular season only" }
}
pub(crate) fn season_label(sport: &str, start: i32) -> String {
    match sport { "nba" | "nhl" => format!("{start}-{:02}", (start + 1) % 100), _ => start.to_string() }
}

// ------------------------------------------------------------------ standings (pure)

/// One final: (home, away, home score, away score, decided in OT/SO).
pub(crate) type Final = (String, String, i32, i32, bool);

#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Row { pub team: String, pub w: u32, pub l: u32, pub t: u32, pub otl: u32, pub pf: i64, pub pa: i64 }

impl Row {
    fn gp(&self) -> u32 { self.w + self.l + self.t + self.otl }
    /// NHL: points (2 a win, 1 an overtime or shootout loss); elsewhere win share with ties as half.
    fn pts(&self) -> u32 { 2 * self.w + self.otl }
    fn pct(&self) -> f64 { if self.gp() == 0 { 0.0 } else { (self.w as f64 + 0.5 * self.t as f64) / self.gp() as f64 } }
}

pub(crate) fn standings(sport: &str, finals: &[Final]) -> Vec<Row> {
    let mut m: HashMap<String, Row> = HashMap::new();
    for (h, a, hs, as_, ot) in finals {
        for (team, me, them) in [(h, *hs, *as_), (a, *as_, *hs)] {
            let r = m.entry(team.clone()).or_insert_with(|| Row { team: team.clone(), ..Default::default() });
            r.pf += me as i64;
            r.pa += them as i64;
            if me > them { r.w += 1 } else if me == them { r.t += 1 } else if sport == "nhl" && *ot { r.otl += 1 } else { r.l += 1 }
        }
    }
    let mut v: Vec<Row> = m.into_values().collect();
    if sport == "nhl" {
        // points, then fewer games played, then wins
        v.sort_by(|a, b| b.pts().cmp(&a.pts()).then(a.gp().cmp(&b.gp())).then(b.w.cmp(&a.w)).then(a.team.cmp(&b.team)));
    } else {
        v.sort_by(|a, b| b.pct().total_cmp(&a.pct()).then(b.w.cmp(&a.w)).then(a.team.cmp(&b.team)));
    }
    v
}

fn row_json(sport: &str, r: &Row, leader: &Row, names: &HashMap<String, String>) -> Value {
    let gb = ((leader.w as f64 - r.w as f64) + (r.l as f64 - leader.l as f64)) / 2.0;
    let mut v = json!({ "team": r.team, "name": names.get(&r.team), "gp": r.gp(), "w": r.w, "l": r.l,
        "points_for": r.pf, "points_against": r.pa, "diff": r.pf - r.pa });
    match sport {
        "nhl" => { v["otl"] = json!(r.otl); v["pts"] = json!(r.pts()); }
        "nfl" => { v["t"] = json!(r.t); v["pct"] = json!(r3(r.pct())); }
        _ => { v["pct"] = json!(r3(r.pct())); v["gb"] = json!(gb); }
    }
    v
}

/// Canonical code -> display name, from the entities table.
async fn team_names(db: &tokio_postgres::Client, sport: &str) -> HashMap<String, String> {
    db.query("SELECT attrs->>'code', name FROM entities WHERE kind='team' AND sport=$1", &[&sport]).await
        .map(|rows| rows.iter().filter_map(|r| Some((r.get::<_, Option<String>>(0)?, r.get::<_, String>(1)))).collect()).unwrap_or_default()
}

#[derive(Deserialize)]
pub struct StandingsQ { sport: String, season: Option<i32> }

pub async fn standings_http(state: web::Data<State>, q: web::Query<StandingsQ>) -> impl Responder {
    let sport = q.sport.to_lowercase();
    if !matches!(sport.as_str(), "nba" | "nfl" | "nhl" | "mlb") { return bad("sport must be nba, nfl, nhl or mlb"); }
    match standings_for(&state.db, &sport, q.season).await { Ok(v) => HttpResponse::Ok().json(v), Err(e) => crate::internal(e) }
}

async fn standings_for(db: &tokio_postgres::Client, sport: &str, season: Option<i32>) -> Result<Value, String> {
    let rows = if sport == "nhl" {
        let s = season.map(|y| y * 10000 + y + 1);
        db.query("SELECT home, away, home_score, away_score, last_period IN ('OT','SO'), game_date, season / 10000 FROM nhl_games_api \
                  WHERE game_type = 2 AND completed AND home_score IS NOT NULL \
                    AND season = COALESCE($1, (SELECT max(season) FROM nhl_games_api WHERE game_type = 2 AND completed))", &[&s]).await
    } else {
        db.query("SELECT home_team, away_team, home_score, away_score, false, game_date, season FROM sports_games \
                  WHERE sport = $1 AND game_type = 'REG' AND home_score IS NOT NULL AND away_score IS NOT NULL \
                    AND season = COALESCE($2, (SELECT max(season) FROM sports_games WHERE sport = $1 AND game_type = 'REG' AND home_score IS NOT NULL))",
                 &[&sport, &season]).await
    }.map_err(|e| dberr(&e))?;
    let canon = |c: &str| crate::hitrates::canon(sport, c);
    let finals: Vec<Final> = rows.iter().map(|r| (canon(&r.get::<_, String>(0)), canon(&r.get::<_, String>(1)), r.get(2), r.get(3), r.get::<_, Option<bool>>(4).unwrap_or(false))).collect();
    let as_of: Option<NaiveDate> = rows.iter().map(|r| r.get::<_, NaiveDate>(5)).max();
    let season_start: Option<i32> = rows.first().map(|r| r.get(6));
    let names = team_names(db, sport).await;
    // a season with no regular-season game left to play is final: its table never goes stale
    let remaining: i64 = match season_start {
        Some(s) if sport == "nhl" => db.query_one("SELECT count(*) FROM nhl_games_api WHERE game_type = 2 AND NOT completed AND season = $1", &[&(s * 10000 + s + 1)]).await.map(|r| r.get(0)).unwrap_or(1),
        Some(s) => db.query_one("SELECT count(*) FROM sports_games WHERE sport = $1 AND game_type = 'REG' AND home_score IS NULL AND season = $2", &[&sport, &s]).await.map(|r| r.get(0)).unwrap_or(1),
        None => 1,
    };
    let table = standings(sport, &finals);
    let out: Vec<Value> = table.iter().map(|r| row_json(sport, r, &table[0], &names)).collect();
    Ok(json!({ "sport": sport, "season": season_start, "season_label": season_start.map(|s| season_label(sport, s)),
        "scope": "whole league, regular season (no division or conference data is stored)",
        "ranking": if sport == "nhl" { "points (2 per win, 1 per overtime/shootout loss)" } else { "win percentage" },
        "games": finals.len(), "as_of": as_of, "complete": remaining == 0, "standings": out,
        "source": { "source_id": source_of(sport), "table": if sport == "nhl" { "nhl_games_api" } else { "sports_games" } } }))
}

// ------------------------------------------------------------------ champions

#[derive(Deserialize)]
pub struct ChampionsQ { sport: String, season: Option<i32>, #[serde(default = "d_one")] limit: i64 }
fn d_one() -> i64 { 1 }

pub async fn champions_http(state: web::Data<State>, q: web::Query<ChampionsQ>) -> impl Responder {
    let sport = q.sport.to_lowercase();
    if !matches!(sport.as_str(), "nba" | "nfl" | "nhl" | "mlb") { return bad("sport must be nba, nfl, nhl or mlb"); }
    match champions_for(&state.db, &sport, q.season, q.limit.clamp(1, 30)).await { Ok(v) => HttpResponse::Ok().json(v), Err(e) => crate::internal(e) }
}

async fn champions_for(db: &tokio_postgres::Client, sport: &str, season: Option<i32>, limit: i64) -> Result<Value, String> {
    // every postseason game of the seasons asked for; the last one decided the title
    let rows = if sport == "nhl" {
        db.query("SELECT season / 10000, game_date, home, away, home_score, away_score FROM nhl_games_api \
                  WHERE game_type = 3 AND completed AND home_score IS NOT NULL AND ($1::int IS NULL OR season / 10000 = $1) ORDER BY game_date", &[&season]).await
    } else {
        let types: &[&str] = if sport == "nfl" { &["SB"] } else { &["POST"] };
        db.query("SELECT season, game_date, home_team, away_team, home_score, away_score FROM sports_games \
                  WHERE sport = $1 AND game_type = ANY($2) AND home_score IS NOT NULL AND ($3::int IS NULL OR season = $3) ORDER BY game_date",
                 &[&sport, &types, &season]).await
    }.map_err(|e| dberr(&e))?;
    let names = team_names(db, sport).await;
    let canon = |c: &str| crate::hitrates::canon(sport, c);
    let mut by_season: std::collections::BTreeMap<i32, Vec<(NaiveDate, String, String, i32, i32)>> = Default::default();
    for r in &rows {
        by_season.entry(r.get(0)).or_default().push((r.get(1), canon(&r.get::<_, String>(2)), canon(&r.get::<_, String>(3)), r.get(4), r.get(5)));
    }
    let mut out = Vec::new();
    for (s, games) in by_season.iter().rev().take(limit as usize) {
        let (date, h, a, hs, as_) = games.last().unwrap().clone();
        if hs == as_ { continue; }
        let (champ, runner) = if hs > as_ { (h.clone(), a.clone()) } else { (a.clone(), h.clone()) };
        // the final series: games between the two finalists in that postseason
        let series: Vec<_> = games.iter().filter(|g| (g.1 == champ && g.2 == runner) || (g.1 == runner && g.2 == champ)).collect();
        let wins = |t: &str| series.iter().filter(|g| (g.1 == t && g.3 > g.4) || (g.2 == t && g.4 > g.3)).count();
        out.push(json!({ "season": s, "season_label": season_label(sport, *s), "champion": champ, "champion_name": names.get(&champ),
            "runner_up": runner, "runner_up_name": names.get(&runner),
            "final_game": { "date": date, "home": h, "away": a, "home_score": hs, "away_score": as_ },
            "series": if sport == "nfl" { Value::Null } else { json!({ "champion_wins": wins(&champ), "runner_up_wins": wins(&runner), "games": series.len() }) } }));
    }
    Ok(json!({ "sport": sport, "champions": out, "event": match sport { "nfl" => "Super Bowl", "nba" => "NBA Finals", "nhl" => "Stanley Cup Final", _ => "World Series" },
        "source": { "source_id": source_of(sport), "table": if sport == "nhl" { "nhl_games_api" } else { "sports_games" } },
        "note": "the champion is the winner of the last stored postseason game of the season" }))
}

// ------------------------------------------------------------------ player stats

#[derive(Debug, PartialEq)]
pub(crate) struct Summary { pub n: usize, pub mean: f64, pub median: f64, pub sd: f64, pub min: f64, pub max: f64 }

pub(crate) fn summarize(xs: &[f64]) -> Option<Summary> {
    if xs.is_empty() { return None; }
    let n = xs.len();
    let mean = xs.iter().sum::<f64>() / n as f64;
    let mut s = xs.to_vec();
    s.sort_by(|a, b| a.total_cmp(b));
    let median = if n % 2 == 1 { s[n / 2] } else { (s[n / 2 - 1] + s[n / 2]) / 2.0 };
    let sd = if n > 1 { (xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1) as f64).sqrt() } else { 0.0 };
    Some(Summary { n, mean, median, sd, min: s[0], max: s[n - 1] })
}

/// Stats a player log has for this sport (the keys stored in `stats`).
pub(crate) fn default_stats(sport: &str) -> &'static [&'static str] {
    match sport {
        "nba" => &["pts", "reb", "ast"], "nhl" => &["goals", "assists", "points", "shots"],
        "mlb" => &["h", "hr", "rbi", "r"], _ => &["passing_yards", "rushing_yards", "receiving_yards", "tds"],
    }
}

#[derive(Deserialize)]
pub struct PlayerQ { entity: String, stats: Option<String>, last: Option<i64>, season: Option<i32> }

pub async fn player_stats_http(state: web::Data<State>, q: web::Query<PlayerQ>) -> impl Responder {
    let parts: Vec<&str> = q.entity.split(':').collect();
    let (sport, pid) = match parts.as_slice() { ["pl", s, id] if matches!(*s, "nba" | "nfl" | "nhl" | "mlb") => (s.to_string(), id.to_string()), _ => return bad("entity must be pl:<sport>:<id>") };
    let stats: Vec<String> = q.stats.as_deref().map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty() && x.len() <= 30 && x.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')).collect())
        .filter(|v: &Vec<String>| !v.is_empty()).unwrap_or_else(|| default_stats(&sport).iter().map(|s| s.to_string()).collect());
    let last = q.last.unwrap_or(10).clamp(1, 200);
    let db = &state.db;
    let name = db.query_opt("SELECT name, team_id FROM entities WHERE entity_id = $1", &[&q.entity]).await.ok().flatten();
    let Some(name) = name else { return HttpResponse::NotFound().json(json!({ "error": "unknown entity" })) };
    let order = if sport == "nfl" { "season DESC, (CASE WHEN week ~ '^[0-9]+$' THEN week::int END) DESC NULLS LAST" } else { "game_date DESC" };
    let rows = match db.query(&format!("SELECT season, week, game_date, team, opponent, stats FROM player_game_logs \
            WHERE sport = $1 AND player_id = $2 AND ($3::int IS NULL OR season = $3) ORDER BY {order} LIMIT $4"),
            &[&sport, &pid, &q.season, &(if q.season.is_some() { 400 } else { last })]).await {
        Ok(r) => r, Err(e) => return crate::internal(dberr(&e)),
    };
    let games: Vec<Value> = rows.iter().map(|r| {
        let s: tokio_postgres::types::Json<Value> = r.get(5);
        json!({ "season": r.get::<_, i32>(0), "week": r.get::<_, Option<String>>(1), "date": r.get::<_, Option<NaiveDate>>(2),
            "team": r.get::<_, Option<String>>(3), "opponent": r.get::<_, Option<String>>(4),
            "stats": stats.iter().map(|k| (k.clone(), s.0.get(k).cloned().unwrap_or(Value::Null))).collect::<serde_json::Map<_, _>>() })
    }).collect();
    let mut summary = serde_json::Map::new();
    let mut missing = Vec::new();
    for k in &stats {
        let xs: Vec<f64> = games.iter().filter_map(|g| g["stats"][k].as_f64()).collect();
        match summarize(&xs) {
            Some(s) => { summary.insert(k.clone(), json!({ "n": s.n, "mean": r3(s.mean), "median": s.median, "sd": r3(s.sd), "min": s.min, "max": s.max, "total": r3(xs.iter().sum()) })); }
            None => missing.push(k.clone()),
        }
    }
    HttpResponse::Ok().json(json!({ "entity": q.entity, "name": name.get::<_, String>(0), "team_id": name.get::<_, Option<String>>(1), "sport": sport,
        "window": match q.season { Some(s) => format!("season {}", season_label(&sport, s)), None => format!("last {last} games") },
        "games_found": games.len(), "game_types": log_game_types(&sport), "summary": summary, "missing_stats": missing, "games": games,
        "first_date": games.last().map(|g| g["date"].clone()), "last_date": games.first().map(|g| g["date"].clone()),
        "source": { "source_id": source_of(&sport), "table": "player_game_logs" } }))
}

// ------------------------------------------------------------------ team results

#[derive(Deserialize)]
pub struct ResultsQ { entity: String, last: Option<i64>, opponent: Option<String>, season: Option<i32>, #[serde(default)] upcoming: bool }

/// The stored codes of a team entity (raw codes canon maps to it, plus historical codes).
async fn codes_of(db: &tokio_postgres::Client, entity: &str) -> Result<(String, Vec<String>), String> {
    let parts: Vec<&str> = entity.split(':').collect();
    let ["tm", sport, code] = parts.as_slice() else { return Err("entity must be tm:<sport>:<code>".into()) };
    let rows = db.query("SELECT alias FROM entity_aliases WHERE entity_id = $1 AND alias ~ '^[A-Z.]{2,5}$'", &[&entity]).await.map_err(|e| dberr(&e))?;
    let mut codes: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
    codes.push(code.to_string());
    codes.sort();
    codes.dedup();
    Ok((sport.to_string(), codes))
}

pub async fn results_http(state: web::Data<State>, q: web::Query<ResultsQ>) -> impl Responder {
    let db = &state.db;
    let (sport, codes) = match codes_of(db, &q.entity).await { Ok(x) => x, Err(e) => return bad(e) };
    let opp = match &q.opponent { Some(o) => match codes_of(db, o).await { Ok(x) => Some(x.1), Err(e) => return bad(e) }, None => None };
    let last = q.last.unwrap_or(10).clamp(1, 200);
    if q.upcoming {
        let names = team_names(db, &sport).await;
        let rows = if sport == "nhl" {
            db.query("SELECT game_date, home, away, start_time, game_id::text FROM nhl_games_api WHERE game_type IN (2, 3) AND NOT completed                       AND start_time > now() - interval '4 hours' AND (home = ANY($1) OR away = ANY($1)) ORDER BY start_time LIMIT $2", &[&codes, &last]).await
        } else {
            db.query("SELECT g.game_date, g.home_team, g.away_team, k.starts_at, g.game_id FROM sports_games g LEFT JOIN game_kickoffs k ON k.sport = g.sport AND k.game_id = g.game_id                       WHERE g.sport = $3 AND g.home_score IS NULL AND g.game_date >= (now() AT TIME ZONE 'America/New_York')::date                         AND (g.home_team = ANY($1) OR g.away_team = ANY($1)) ORDER BY g.game_date LIMIT $2", &[&codes, &last, &sport]).await
        };
        let rows = match rows { Ok(r) => r, Err(e) => return crate::internal(dberr(&e)) };
        let team_name = db.query_opt("SELECT name FROM entities WHERE entity_id = $1", &[&q.entity]).await.ok().flatten().map(|r| r.get::<_, String>(0));
        let games: Vec<Value> = rows.iter().map(|r| {
            let (h, a): (String, String) = (r.get(1), r.get(2));
            let home = codes.contains(&h);
            let opp = crate::hitrates::canon(&sport, if home { &a } else { &h });
            let (hc, ac) = (crate::hitrates::canon(&sport, &h), crate::hitrates::canon(&sport, &a));
            json!({ "game_id": r.get::<_, String>(4), "date": r.get::<_, NaiveDate>(0), "start": r.get::<_, Option<chrono::DateTime<chrono::Utc>>>(3), "home": home,
                    "home_team": hc, "away_team": ac, "home_name": names.get(&hc), "away_name": names.get(&ac),
                    "opponent": opp, "opponent_name": names.get(&opp) })
        }).collect();
        return HttpResponse::Ok().json(json!({ "entity": q.entity, "name": team_name, "sport": sport, "upcoming": games,
            "source": { "source_id": source_of(&sport), "table": if sport == "nhl" { "nhl_games_api" } else { "sports_games" } } }));
    }
    let rows = if sport == "nhl" {
        db.query("SELECT game_date, home, away, home_score, away_score, CASE game_type WHEN 3 THEN 'POST' ELSE 'REG' END, last_period FROM nhl_games_api \
                  WHERE game_type IN (2, 3) AND completed AND home_score IS NOT NULL AND (home = ANY($1) OR away = ANY($1)) \
                    AND ($2::text[] IS NULL OR home = ANY($2) OR away = ANY($2)) AND ($3::int IS NULL OR season / 10000 = $3) ORDER BY game_date DESC LIMIT $4",
                 &[&codes, &opp, &q.season, &last]).await
    } else {
        db.query("SELECT game_date, home_team, away_team, home_score, away_score, game_type, NULL::text FROM sports_games \
                  WHERE sport = $5 AND game_type <> 'PRE' AND home_score IS NOT NULL AND (home_team = ANY($1) OR away_team = ANY($1)) \
                    AND ($2::text[] IS NULL OR home_team = ANY($2) OR away_team = ANY($2)) AND ($3::int IS NULL OR season = $3) ORDER BY game_date DESC LIMIT $4",
                 &[&codes, &opp, &q.season, &last, &sport]).await
    };
    let rows = match rows { Ok(r) => r, Err(e) => return crate::internal(dberr(&e)) };
    let names = team_names(db, &sport).await;
    let (mut w, mut l, mut t, mut pf, mut pa) = (0, 0, 0, 0i64, 0i64);
    let games: Vec<Value> = rows.iter().map(|r| {
        let (h, a, hs, as_): (String, String, i32, i32) = (r.get(1), r.get(2), r.get(3), r.get(4));
        let home = codes.contains(&h);
        let (me, them, vs) = if home { (hs, as_, a.clone()) } else { (as_, hs, h.clone()) };
        if me > them { w += 1 } else if me < them { l += 1 } else { t += 1 }
        pf += me as i64; pa += them as i64;
        let opp = crate::hitrates::canon(&sport, &vs);
        json!({ "date": r.get::<_, NaiveDate>(0), "opponent_name": names.get(&opp), "opponent": opp, "home": home, "score": format!("{me}-{them}"),
            "result": if me > them { "W" } else if me < them { "L" } else { "T" }, "game_type": r.get::<_, String>(5), "ended": r.get::<_, Option<String>>(6) })
    }).collect();
    let n = games.len().max(1) as f64;
    let team_name = db.query_opt("SELECT name FROM entities WHERE entity_id = $1", &[&q.entity]).await.ok().flatten().map(|r| r.get::<_, String>(0));
    HttpResponse::Ok().json(json!({ "entity": q.entity, "name": team_name, "sport": sport, "games_found": games.len(), "record": { "w": w, "l": l, "t": t },
        "avg_points_for": r3(pf as f64 / n), "avg_points_against": r3(pa as f64 / n), "games": games,
        "source": { "source_id": source_of(&sport), "table": if sport == "nhl" { "nhl_games_api" } else { "sports_games" } } }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(h: &str, a: &str, hs: i32, as_: i32, ot: bool) -> Final { (h.into(), a.into(), hs, as_, ot) }

    #[test]
    fn nhl_standings_give_a_point_for_an_overtime_loss() {
        let t = standings("nhl", &[f("TOR", "BOS", 3, 2, true), f("BOS", "MTL", 4, 1, false), f("MTL", "TOR", 2, 1, false)]);
        let row = |c: &str| t.iter().find(|r| r.team == c).unwrap().clone();
        assert_eq!((row("BOS").w, row("BOS").otl, row("BOS").pts()), (1, 1, 3));
        assert_eq!((row("TOR").w, row("TOR").l, row("TOR").pts()), (1, 1, 2));
        assert_eq!(t[0].team, "BOS", "most points first");
    }

    #[test]
    fn other_leagues_rank_by_win_share_with_ties_as_half() {
        let t = standings("nfl", &[f("KC", "BUF", 20, 20, false), f("KC", "DEN", 30, 10, false), f("BUF", "DEN", 14, 17, false)]);
        assert_eq!(t[0].team, "KC");
        assert_eq!((t[0].w, t[0].t, t[0].l), (1, 1, 0));
        assert!((t[0].pct() - 0.75).abs() < 1e-9);
        let leader = t[0].clone();
        let den = t.iter().find(|r| r.team == "DEN").unwrap();
        assert_eq!((den.w, den.l), (1, 1));
        assert_eq!(row_json("nba", den, &leader, &HashMap::new())["gb"], json!(0.5), "(1 - 1 + 1 - 0) / 2");
    }

    #[test]
    fn summaries_match_a_hand_calculation() {
        let s = summarize(&[26.0, 18.0, 21.0, 19.0]).unwrap();
        assert_eq!((s.n, s.mean, s.median, s.min, s.max), (4, 21.0, 20.0, 18.0, 26.0));
        assert!((s.sd - 3.5590).abs() < 1e-3, "sample sd {}", s.sd);
        assert_eq!(summarize(&[]), None);
    }

    #[test]
    fn seasons_are_labelled_the_way_leagues_say_them() {
        assert_eq!(season_label("nba", 2025), "2025-26");
        assert_eq!(season_label("nhl", 2009), "2009-10");
        assert_eq!(season_label("mlb", 2025), "2025");
        assert_eq!(log_game_types("mlb"), "regular season and postseason");
        assert_eq!(log_game_types("nba"), "regular season only");
    }
}
