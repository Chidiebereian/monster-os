//! R4: ESPN injury reports as they are published (research-backlog R4).
//!
//! Every run reads ESPN's league injury report for each sport and writes to
//! `feature_snapshots` (write-once) only what changed since the last run: a
//! player's new status (`injury_status`, value = probability of playing), or
//! `cleared` when a listed player drops off the report. `observed_at` and
//! `available_at` are the fetch time, so the history shows what was known and
//! when; nothing is back-filled. `event_at` is ESPN's own report date when given.
//! The rows are the input for the news circuit breaker (picks of a game whose
//! starter changes status go to leans) and for decision-time features.

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::State;

const SOURCE: &str = "espn_injuries";
const EXTRACTOR: &str = "injury_snap v1";
const LEAGUES: [(&str, &str); 3] = [("nba", "basketball/nba"), ("nfl", "football/nfl"), ("nhl", "hockey/nhl")];

/// ESPN designation -> probability of playing (documented priors, as nba_game.rs uses).
pub(crate) fn p_play(status: &str) -> f64 {
    match status.to_ascii_lowercase().as_str() {
        "out" | "injured reserve" | "ir" | "suspension" | "suspended" => 0.0,
        "doubtful" => 0.15, "questionable" => 0.5, "day-to-day" => 0.6, "probable" => 0.9, "cleared" | "active" => 1.0, _ => 0.75,
    }
}

struct Entry { status: String, team: String, event_at: Option<chrono::DateTime<chrono::Utc>> }

/// player id -> current listing, from one league report.
pub(crate) fn parse(v: &Value) -> HashMap<String, (String, String, Option<String>)> {
    let mut m = HashMap::new();
    for t in v["injuries"].as_array().into_iter().flatten() {
        let team = t["displayName"].as_str().unwrap_or("").to_string();
        for i in t["injuries"].as_array().into_iter().flatten() {
            let id = i["athlete"]["id"].as_str().map(str::to_string)
                .or_else(|| i["athlete"]["links"][0]["href"].as_str().and_then(|h| h.split("/id/").nth(1)).and_then(|x| x.split('/').next()).map(str::to_string));
            let status = i["status"].as_str().unwrap_or("").trim().to_string();
            if let (Some(id), false) = (id, status.is_empty()) {
                m.insert(id, (status, team.clone(), i["date"].as_str().map(str::to_string)));
            }
        }
    }
    m
}

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
        let mut changes: Vec<(String, Entry)> = Vec::new();
        for (id, (status, team, date)) in &report {
            if last.get(id).map(|s| !s.eq_ignore_ascii_case(status)).unwrap_or(true) {
                let event_at = date.as_deref().and_then(|d| chrono::DateTime::parse_from_rfc3339(d).ok()).map(|d| d.with_timezone(&chrono::Utc))
                    .or_else(|| date.as_deref().and_then(|d| chrono::NaiveDateTime::parse_from_str(d, "%Y-%m-%dT%H:%MZ").ok()).map(|d| d.and_utc()));
                changes.push((id.clone(), Entry { status: status.clone(), team: team.clone(), event_at }));
            }
        }
        // listed before, gone now: back to available
        let cleared: Vec<String> = last.iter().filter(|(id, s)| !report.contains_key(*id) && !s.eq_ignore_ascii_case("cleared")).map(|(id, _)| id.clone()).collect();
        for id in &cleared { changes.push((id.clone(), Entry { status: "cleared".into(), team: String::new(), event_at: None })); }
        let mut written = 0u64;
        for (id, e) in &changes {
            written += db.execute(
                "INSERT INTO feature_snapshots (sport, game_id, subject, feature_set, name, value, value_text, event_at, observed_at, available_at, source, quality, extractor) \
                 VALUES ($1, '', $2, 'availability', 'injury_status', $3, $4, $5, $6, $6, $7, 'observed', $8) ON CONFLICT DO NOTHING",
                &[&sport, id, &p_play(&e.status), &e.status, &e.event_at, &now, &SOURCE, &EXTRACTOR]).await.map_err(|x| x.to_string())?;
            if !e.team.is_empty() {
                written += db.execute(
                    "INSERT INTO feature_snapshots (sport, game_id, subject, feature_set, name, value_text, observed_at, available_at, source, quality, extractor) \
                     VALUES ($1, '', $2, 'availability', 'injury_team', $3, $4, $4, $5, 'observed', $6) ON CONFLICT DO NOTHING",
                    &[&sport, id, &e.team, &now, &SOURCE, &EXTRACTOR]).await.map_err(|x| x.to_string())?;
            }
        }
        out.insert(sport.into(), json!({ "listed": report.len(), "status_changes": changes.len() - cleared.len(), "cleared": cleared.len(), "rows": written }));
    }
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_league_report_and_maps_status_to_play_probability() {
        let v = json!({ "injuries": [{ "displayName": "Boston Celtics", "injuries": [
            { "status": "Out", "date": "2026-11-01T18:02Z", "athlete": { "id": "4065648" } },
            { "status": "Questionable", "athlete": { "links": [{ "href": "https://www.espn.com/nba/player/_/id/3917376/x" }] } },
            { "status": "", "athlete": { "id": "1" } }] }] });
        let m = parse(&v);
        assert_eq!(m.len(), 2);
        assert_eq!(m["4065648"].0, "Out");
        assert_eq!(m["3917376"].1, "Boston Celtics");
        assert_eq!(p_play("Out"), 0.0);
        assert_eq!(p_play("questionable"), 0.5);
        assert_eq!(p_play("cleared"), 1.0);
    }
}
