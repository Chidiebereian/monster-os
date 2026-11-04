//! R4: ESPN injury reports as they are published (research-backlog R4).
//!
//! Every run reads ESPN's league injury report for each sport and writes to
//! `feature_snapshots` (write-once) only what changed since the last run: a
//! player's new status (`injury_status`, value = probability of playing), or
//! `cleared` when a listed player drops off the report. `observed_at` and
//! `available_at` are the fetch time, so the history shows what was known and
//! when; nothing is back-filled. The rows belong to no game, so `event_at` (the
//! time of the game a feature describes) is NULL; ESPN's own report date is kept
//! as its own row, `injury_reported`. v1 and v2 wrote the report date into
//! `event_at`; those immutable rows are known and excluded by the integrity check.
//! The rows are the input for decision-time features and for the news circuit
//! breaker below.
//!
//! News circuit breaker: when a player who matters to a game (the subject of a
//! pick, or a player the books post props for in that game) changes status by a
//! material amount (probability of playing moves by >= 0.25), every open pick of
//! that game published before the change gets a `news_changed` risk flag. A
//! flagged pick is shown as suspended (validation qualification), never deleted
//! or re-priced: its published price, line and ledger stay as they were, so the
//! track record still scores it. Player engines re-simulate with confirmed
//! lineups inside 90 minutes of the start (autopilot freeze).

use std::collections::HashMap;

use serde_json::{json, Value};
use tokio_postgres::types::Json;

use crate::State;

const SOURCE: &str = "espn_injuries";
const EXTRACTOR: &str = "injury_snap v3";
/// A change of at least this much in the probability of playing trips the breaker.
const MATERIAL: f64 = 0.25;
const LEAGUES: [(&str, &str); 3] = [("nba", "basketball/nba"), ("nfl", "football/nfl"), ("nhl", "hockey/nhl")];

/// ESPN designation -> probability of playing (documented priors, as nba_game.rs uses).
pub(crate) fn p_play(status: &str) -> f64 {
    match status.to_ascii_lowercase().as_str() {
        "out" | "injured reserve" | "ir" | "suspension" | "suspended" => 0.0,
        "doubtful" => 0.15, "questionable" => 0.5, "day-to-day" => 0.6, "probable" => 0.9, "cleared" | "active" => 1.0, _ => 0.75,
    }
}

/// One player's current listing on a league report.
#[derive(Clone, Default)]
pub(crate) struct Listing { pub status: String, pub team: String, pub abbr: String, pub name: String, pub date: Option<String> }

struct Entry { l: Listing, was: Option<String> }

/// player id -> current listing, from one league report.
pub(crate) fn parse(v: &Value) -> HashMap<String, Listing> {
    let mut m = HashMap::new();
    for t in v["injuries"].as_array().into_iter().flatten() {
        let team = t["displayName"].as_str().unwrap_or("").to_string();
        for i in t["injuries"].as_array().into_iter().flatten() {
            let a = &i["athlete"];
            let id = a["id"].as_str().map(str::to_string)
                .or_else(|| a["links"][0]["href"].as_str().and_then(|h| h.split("/id/").nth(1)).and_then(|x| x.split('/').next()).map(str::to_string));
            let status = i["status"].as_str().unwrap_or("").trim().to_string();
            if let (Some(id), false) = (id, status.is_empty()) {
                m.insert(id, Listing { status, team: team.clone(), abbr: a["team"]["abbreviation"].as_str().unwrap_or("").to_string(),
                    name: a["displayName"].as_str().unwrap_or("").to_string(), date: i["date"].as_str().map(str::to_string) });
            }
        }
    }
    m
}

/// Did a status change move the probability of playing enough to matter?
pub(crate) fn material(was: Option<&str>, now: &str) -> bool {
    (p_play(was.unwrap_or("active")) - p_play(now)).abs() >= MATERIAL
}

fn norm(s: &str) -> String { s.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase() }

pub(crate) async fn snapshot(state: &State) -> Result<Value, String> {
    let db = &state.db;
    let now = chrono::Utc::now();
    let mut out = serde_json::Map::new();
    for (sport, path) in LEAGUES {
        let url = format!("https://site.api.espn.com/apis/site/v2/sports/{path}/injuries");
        let report = match crate::nba_espn::get_json(&state.http, &url).await {
            Ok(v) => parse(&v),
            Err(e) => { out.insert(sport.into(), json!({ "error": e })); continue; }
        };
        if report.is_empty() { out.insert(sport.into(), json!({ "listed": 0, "note": "empty report; nothing recorded" })); continue; }
        // last recorded status per player
        let last: HashMap<String, String> = db.query(
            "SELECT DISTINCT ON (subject) subject, value_text FROM feature_snapshots \
             WHERE sport=$1 AND source=$2 AND name='injury_status' ORDER BY subject, observed_at DESC, id DESC", &[&sport, &SOURCE]).await
            .map_err(|e| e.to_string())?.iter().map(|r| (r.get(0), r.get::<_, Option<String>>(1).unwrap_or_default())).collect();
        let first_run = last.is_empty();
        let mut changes: Vec<(String, Entry)> = Vec::new();
        for (id, l) in &report {
            if last.get(id).map(|s| !s.eq_ignore_ascii_case(&l.status)).unwrap_or(true) {
                changes.push((id.clone(), Entry { l: l.clone(), was: last.get(id).cloned() }));
            }
        }
        // listed before, gone now: back to available (name and team from the last listing; recorded from v2 on)
        let cleared: Vec<String> = last.iter().filter(|(id, s)| !report.contains_key(*id) && !s.eq_ignore_ascii_case("cleared")).map(|(id, _)| id.clone()).collect();
        for id in &cleared {
            let who = db.query_opt("SELECT max(value_text) FILTER (WHERE name='injury_player'), max(value_text) FILTER (WHERE name='injury_team_abbr') \
                 FROM (SELECT DISTINCT ON (name) name, value_text FROM feature_snapshots WHERE sport=$1 AND source=$2 AND subject=$3 \
                       AND name IN ('injury_player','injury_team_abbr') ORDER BY name, observed_at DESC) x", &[&sport, &SOURCE, id]).await
                .ok().flatten().map(|r| (r.get::<_, Option<String>>(0).unwrap_or_default(), r.get::<_, Option<String>>(1).unwrap_or_default())).unwrap_or_default();
            changes.push((id.clone(), Entry { l: Listing { status: "cleared".into(), name: who.0, abbr: who.1, ..Default::default() }, was: last.get(id).cloned() }));
        }
        let mut written = 0u64;
        for (id, e) in &changes {
            written += db.execute(
                "INSERT INTO feature_snapshots (sport, game_id, subject, feature_set, name, value, value_text, observed_at, available_at, source, quality, extractor) \
                 VALUES ($1, '', $2, 'availability', 'injury_status', $3, $4, $5, $5, $6, 'observed', $7) ON CONFLICT DO NOTHING",
                &[&sport, id, &p_play(&e.l.status), &e.l.status, &now, &SOURCE, &EXTRACTOR]).await.map_err(|x| x.to_string())?;
            if e.l.status == "cleared" { continue; }
            let reported = e.l.date.clone().unwrap_or_default();
            for (name, text) in [("injury_team", &e.l.team), ("injury_team_abbr", &e.l.abbr), ("injury_player", &e.l.name), ("injury_reported", &reported)] {
                if text.is_empty() { continue; }
                written += db.execute(
                    "INSERT INTO feature_snapshots (sport, game_id, subject, feature_set, name, value_text, observed_at, available_at, source, quality, extractor) \
                     VALUES ($1, '', $2, 'availability', $3, $4, $5, $5, $6, 'observed', $7) ON CONFLICT DO NOTHING",
                    &[&sport, id, &name, text, &now, &SOURCE, &EXTRACTOR]).await.map_err(|x| x.to_string())?;
            }
        }
        // the first report ever read is a baseline, not news
        let tripped = if first_run { json!({ "skipped": "baseline run" }) } else { breaker(db, sport, &changes, now).await? };
        out.insert(sport.into(), json!({ "listed": report.len(), "status_changes": changes.len() - cleared.len(), "cleared": cleared.len(), "rows": written, "breaker": tripped }));
    }
    Ok(Value::Object(out))
}

/// Flag the open picks of every game a material status change touches (module docs).
async fn breaker(db: &tokio_postgres::Client, sport: &str, changes: &[(String, Entry)], now: chrono::DateTime<chrono::Utc>) -> Result<Value, String> {
    let news: Vec<&Entry> = changes.iter().map(|(_, e)| e).filter(|e| !e.l.name.is_empty() && !e.l.abbr.is_empty() && material(e.was.as_deref(), &e.l.status)).collect();
    if news.is_empty() { return Ok(json!({ "material_changes": 0, "picks_flagged": 0 })); }
    let picks = db.query("SELECT id, game_id, home, away, subject_name, risk_flags FROM ai_picks \
        WHERE sport=$1 AND status='open' AND starts_at > now() AND published_at < $2", &[&sport, &now]).await.map_err(|e| e.to_string())?;
    // players the books post props for, per game: the ones a game's price depends on
    let priced: std::collections::HashSet<(String, String)> = db.query(
        "SELECT DISTINCT game_id, subject_name FROM market_lines WHERE sport=$1 AND kind='prop' AND game_id IS NOT NULL AND subject_name IS NOT NULL \
         AND observed_at > now() - interval '2 days'", &[&sport]).await.map_err(|e| e.to_string())?
        .iter().map(|r| (r.get::<_, String>(0), norm(&r.get::<_, String>(1)))).collect();
    let canon = |c: &str| crate::hitrates::canon(sport, c);
    let (mut flagged, mut players) = (0u64, Vec::new());
    for e in &news {
        let (team, who, to) = (canon(&e.l.abbr), norm(&e.l.name), e.l.status.as_str());
        let was = e.was.as_deref().unwrap_or("not listed");
        let mut hit = 0u64;
        for r in &picks {
            let (game, home, away, subject): (String, Option<String>, Option<String>, String) = (r.get(1), r.get(2), r.get(3), r.get(4));
            let in_game = [home, away].iter().flatten().any(|t| canon(t) == team);
            if !in_game || !(norm(&subject) == who || priced.contains(&(game, who.clone()))) { continue; }
            let flags: Json<Value> = r.get(5);
            if flags.0.as_array().into_iter().flatten().any(|f| f["code"] == "news_changed" && f["player"] == e.l.name.as_str() && f["to"] == to) { continue; }
            let flag = json!([{ "code": "news_changed", "player": e.l.name, "team": e.l.abbr, "from": was, "to": to, "at": now,
                "text": format!("{} ({}) went from {was} to {to} after this pick was published; its price did not know that", e.l.name, e.l.abbr) }]);
            hit += db.execute("UPDATE ai_picks SET risk_flags = risk_flags || $2, updated_at=now() WHERE id=$1 AND status='open'",
                &[&r.get::<_, i64>(0), &Json(&flag)]).await.map_err(|x| x.to_string())?;
        }
        if hit > 0 { players.push(json!({ "player": e.l.name, "team": e.l.abbr, "from": was, "to": to, "picks": hit })); }
        flagged += hit;
    }
    Ok(json!({ "material_changes": news.len(), "picks_flagged": flagged, "players": players }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_league_report_and_maps_status_to_play_probability() {
        let v = json!({ "injuries": [{ "displayName": "Boston Celtics", "injuries": [
            { "status": "Out", "date": "2026-11-01T18:02Z", "athlete": { "id": "4065648", "displayName": "Jayson Tatum", "team": { "abbreviation": "BOS" } } },
            { "status": "Questionable", "athlete": { "links": [{ "href": "https://www.espn.com/nba/player/_/id/3917376/x" }] } },
            { "status": "", "athlete": { "id": "1" } }] }] });
        let m = parse(&v);
        assert_eq!(m.len(), 2);
        assert_eq!(m["4065648"].status, "Out");
        assert_eq!(m["3917376"].team, "Boston Celtics");
        assert_eq!((m["4065648"].name.as_str(), m["4065648"].abbr.as_str()), ("Jayson Tatum", "BOS"));
        assert_eq!(p_play("Out"), 0.0);
        assert_eq!(p_play("questionable"), 0.5);
        assert_eq!(p_play("cleared"), 1.0);
    }

    #[test]
    fn only_material_changes_trip_the_breaker() {
        assert!(material(None, "Out"));                  // newly listed as out
        assert!(material(Some("Questionable"), "Out"));
        assert!(material(Some("Out"), "cleared"));        // back from injury
        assert!(!material(Some("Questionable"), "Day-To-Day"));
        assert!(!material(Some("Probable"), "cleared"));
        assert!(!material(None, "Probable"));
        assert_eq!(norm("Karl-Anthony Towns"), norm("karl anthony towns"));
    }
}
