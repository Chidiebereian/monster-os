//! Canonical sports entities and the name resolver (migration 0040,
//! docs/architecture/monster-gpt-v2.md section 4).
//!
//! `POST /entities/build` rebuilds teams and players from the stored data:
//! - teams from ESPN's league team lists, kept only when the canonical code
//!   (`hitrates::canon`) is one the stored games use, so All-Star and exhibition
//!   codes (EAST, LEB, STRIPES, ...) never become teams; older codes of a franchise
//!   (NFL OAK/SD/STL, MLB OAK) are aliases valid for the seasons the stored games
//!   use them; a franchise the league treats as new (NHL Arizona -> Utah) is its own
//!   team entity;
//! - players from `sports_players` (NBA ESPN ids, NFL gsis ids, NHL and MLB league
//!   ids); NHL names are stored abbreviated ("C. McDavid"), so current NHL rosters
//!   (api-web.nhle.com) supply full names.
//! Ids derive from the source ids, so a rebuild keeps them. The autopilot runs it daily
//! (job `entities:build`, after the projections roster sync).
//!
//! `GET /entities/search?q=&sport=&kind=&limit=` ranks candidates;
//! `GET /entities/resolve?q=&sport=&focus=` resolves one mention or says it is
//! ambiguous (the caller then asks the user; nothing runs on a guess).

use std::collections::{HashMap, HashSet};

use actix_web::{web, HttpResponse, Responder};
use chrono::Datelike;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::State;

const LEAGUES: [(&str, &str); 4] = [("nba", "basketball/nba"), ("nfl", "football/nfl"), ("nhl", "hockey/nhl"), ("mlb", "baseball/mlb")];
const SOURCE: [(&str, &str); 4] = [("nba", "espn_site"), ("nfl", "nflverse"), ("nhl", "nhl_api"), ("mlb", "mlb_statsapi")];
/// A mention resolves when its best candidate leads the next one by this much.
const CLEAR_LEAD: f64 = 2.0;

/// Teams known only from history: (sport, stored code, entity code, name, aliases).
/// Same franchise -> the current team's entity; a new franchise -> its own entity.
const HISTORICAL: [(&str, &str, &str, &str); 5] = [
    ("nfl", "OAK", "LV", "Oakland Raiders"),
    ("nfl", "SD", "LAC", "San Diego Chargers"),
    ("nfl", "STL", "LA", "St. Louis Rams"),
    ("mlb", "OAK", "ATH", "Oakland Athletics"),
    ("nhl", "ARI", "ARI", "Arizona Coyotes"),
];
/// Earlier names of a team that kept its code: (sport, code, name, last season used).
const FORMER_NAMES: [(&str, &str, &str, i32, i32); 2] = [
    ("nfl", "WAS", "Washington Redskins", 1999, 2019),
    ("nfl", "WAS", "Washington Football Team", 2020, 2021),
];

/// Names other sources still use for a current team (no date limit): Polymarket's
/// 2027 Stanley Cup market says "Utah Hockey Club"; Kalshi calls the Athletics "A's".
const OTHER_NAMES: [(&str, &str, &str); 2] = [
    ("nhl", "UTA", "Utah Hockey Club"),
    ("mlb", "ATH", "A's"),
];
/// ESPN short locations that venues write out: the "LA Clippers" are also the
/// "Los Angeles Clippers" (without it, "Los Angeles C" matched the Lakers).
const LOCATIONS: [(&str, &str); 1] = [("LA", "Los Angeles")];

/// Lowercase letters and digits only, accents folded ("Jokić" -> "jokic").
pub(crate) fn norm(s: &str) -> String {
    s.chars().flat_map(|c| c.to_lowercase()).filter_map(|c| {
        let f = match c {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => 'a', 'ç' | 'ć' | 'č' => 'c', 'ď' | 'đ' => 'd',
            'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ė' | 'ę' | 'ě' => 'e', 'ğ' => 'g', 'ì' | 'í' | 'î' | 'ï' | 'ī' | 'į' | 'ı' => 'i',
            'ł' | 'ľ' | 'ĺ' => 'l', 'ñ' | 'ń' | 'ň' => 'n', 'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ő' => 'o',
            'ř' | 'ŕ' => 'r', 'ś' | 'š' | 'ş' | 'ș' => 's', 'ť' | 'ţ' | 'ț' => 't', 'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ů' | 'ű' | 'ų' => 'u',
            'ý' | 'ÿ' => 'y', 'ź' | 'ż' | 'ž' => 'z', c => c,
        };
        f.is_ascii_alphanumeric().then_some(f)
    }).collect()
}

/// Last name of "First Last", "F. Last" or "First Last Jr.".
fn last_name(full: &str) -> Option<String> {
    let parts: Vec<&str> = full.split_whitespace().filter(|p| !matches!(norm(p).as_str(), "jr" | "sr" | "ii" | "iii" | "iv")).collect();
    (parts.len() >= 2).then(|| parts[parts.len() - 1].to_string())
}

fn levenshtein(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + usize::from(a[i - 1] != b[j - 1]));
        }
        prev = cur;
    }
    prev[b.len()]
}

// ------------------------------------------------------------------ build

#[derive(Default)]
struct Batch { ents: Vec<(String, String, String, String, Option<String>, Value)>, ids: Vec<(String, String, String)>, aliases: Vec<(String, String, f32, Option<chrono::NaiveDate>, Option<chrono::NaiveDate>)> }

impl Batch {
    fn alias(&mut self, id: &str, alias: &str, w: f32, from: Option<chrono::NaiveDate>, to: Option<chrono::NaiveDate>) {
        if !norm(alias).is_empty() { self.aliases.push((id.to_string(), alias.trim().to_string(), w, from, to)); }
    }
}

fn season_start(y: i32) -> Option<chrono::NaiveDate> { chrono::NaiveDate::from_ymd_opt(y, 1, 1) }
fn season_end(y: i32) -> Option<chrono::NaiveDate> { chrono::NaiveDate::from_ymd_opt(y + 1, 6, 30) }

pub async fn build(state: web::Data<State>) -> impl Responder {
    match build_all(&state).await {
        Ok(v) => HttpResponse::Ok().json(v),
        Err(e) => crate::internal(e),
    }
}

pub(crate) async fn build_all(state: &State) -> Result<Value, String> {
    let db = &state.db;
    let mut b = Batch::default();
    let mut report = serde_json::Map::new();
    // stored team codes per sport (canonical), with the seasons each raw code was used
    let mut stored: HashMap<String, HashSet<String>> = HashMap::new();
    let mut code_seasons: HashMap<(String, String), (i32, i32)> = HashMap::new();
    for r in db.query("SELECT sport, home_team, min(season), max(season) FROM sports_games WHERE sport IN ('nba','nfl','mlb') GROUP BY 1, 2 \
                       UNION ALL SELECT 'nhl', home, min(season / 10000), max(season / 10000) FROM nhl_games_api GROUP BY 2", &[]).await.map_err(|e| e.to_string())? {
        let (sport, code): (String, String) = (r.get(0), r.get(1));
        stored.entry(sport.clone()).or_default().insert(crate::hitrates::canon(&sport, &code));
        code_seasons.insert((sport, code), (r.get(2), r.get(3)));
    }
    for (sport, path) in LEAGUES {
        let v = crate::nba_espn::get_json(&state.http, &format!("https://site.api.espn.com/apis/site/v2/sports/{path}/teams")).await?;
        let mut kept = 0;
        for t in v["sports"][0]["leagues"][0]["teams"].as_array().into_iter().flatten() {
            let t = &t["team"];
            let abbr = t["abbreviation"].as_str().unwrap_or("");
            let code = team_code(sport, abbr);
            if !stored.get(sport).map(|s| s.contains(&code)).unwrap_or(false) { continue; }
            let id = format!("tm:{sport}:{code}");
            let name = t["displayName"].as_str().unwrap_or(abbr).to_string();
            b.ents.push((id.clone(), "team".into(), sport.into(), name.clone(), None,
                json!({ "code": code, "location": t["location"], "nickname": t["name"], "espn_id": t["id"] })));
            if let Some(eid) = t["id"].as_str() { b.ids.push((id.clone(), "espn_site".into(), format!("{sport}:team:{eid}"))); }
            b.alias(&id, &name, 1.0, None, None);
            for (k, w) in [("name", 1.0), ("shortDisplayName", 0.9), ("location", 0.6)] {
                if let Some(s) = t[k].as_str() { b.alias(&id, s, w, None, None); }
            }
            b.alias(&id, abbr, 0.8, None, None);
            b.alias(&id, &code, 0.8, None, None);
            if let Some((_, long)) = LOCATIONS.iter().find(|l| Some(l.0) == t["location"].as_str()) {
                b.alias(&id, long, 0.6, None, None);
                if let Some(nick) = t["name"].as_str() { b.alias(&id, &format!("{long} {nick}"), 1.0, None, None); }
            }
            kept += 1;
        }
        // stored raw codes that canon maps onto a kept team (NY -> NYK, LA -> LAR ...)
        for ((s, raw), _) in &code_seasons {
            if s == sport && HISTORICAL.iter().all(|h| !(h.0 == sport && h.1 == raw)) {
                let code = team_code(sport, raw);
                if b.ents.iter().any(|e| e.0 == format!("tm:{sport}:{code}")) && *raw != code { b.alias(&format!("tm:{sport}:{code}"), raw, 0.8, None, None); }
            }
        }
        report.insert(sport.into(), json!({ "teams": kept }));
    }
    for (sport, raw, code, name) in HISTORICAL {
        let Some(&(from, to)) = code_seasons.get(&(sport.to_string(), raw.to_string())) else { continue };
        let id = format!("tm:{sport}:{code}");
        if !b.ents.iter().any(|e| e.0 == id) {
            b.ents.push((id.clone(), "team".into(), sport.into(), name.into(), None, json!({ "code": code, "historical": true, "seasons": [from, to] })));
        }
        b.alias(&id, name, 1.0, season_start(from), season_end(to));
        b.alias(&id, raw, 0.8, season_start(from), season_end(to));
        // a franchise that is its own entity also answers to its nickname ("Coyotes")
        if raw == code {
            if let Some(nick) = name.split_whitespace().last() { b.alias(&id, nick, 1.0, season_start(from), season_end(to)); }
        }
    }
    for (sport, code, name, from, to) in FORMER_NAMES {
        b.alias(&format!("tm:{sport}:{code}"), name, 1.0, season_start(from), season_end(to));
    }
    for (sport, code, name) in OTHER_NAMES {
        let id = format!("tm:{sport}:{code}");
        if b.ents.iter().any(|e| e.0 == id) { b.alias(&id, name, 1.0, None, None); }
    }
    // NHL full names from the current rosters (the stored names are "C. McDavid")
    let nhl_full = nhl_full_names(state, stored.get("nhl").cloned().unwrap_or_default()).await;
    let teams: HashSet<String> = b.ents.iter().map(|e| e.0.clone()).collect();
    let src: HashMap<&str, &str> = SOURCE.into_iter().collect();
    let rows = db.query("SELECT sport, player_id, display_name, position, team, first_season, last_season, games FROM sports_players \
                         WHERE sport IN ('nba','nfl','nhl','mlb') AND display_name <> ''", &[]).await.map_err(|e| e.to_string())?;
    for r in &rows {
        let (sport, pid, shown): (String, String, String) = (r.get(0), r.get(1), r.get(2));
        let full = if sport == "nhl" { nhl_full.get(&pid).cloned().unwrap_or_else(|| shown.clone()) } else { shown.clone() };
        let id = format!("pl:{sport}:{pid}");
        let team = r.get::<_, Option<String>>(4).map(|t| format!("tm:{sport}:{}", crate::hitrates::canon(&sport, &t))).filter(|t| teams.contains(t));
        let (first, last): (Option<i32>, Option<i32>) = (r.get(5), r.get(6));
        b.ents.push((id.clone(), "player".into(), sport.clone(), full.clone(), team,
            json!({ "position": r.get::<_, Option<String>>(3), "first_season": first.filter(|s| *s > 0), "last_season": last, "games": r.get::<_, Option<i32>>(7) })));
        b.ids.push((id.clone(), src[sport.as_str()].into(), format!("{sport}:player:{pid}")));
        b.alias(&id, &full, 1.0, None, None);
        if shown != full { b.alias(&id, &shown, 0.9, None, None); }
        if let Some(l) = last_name(&full) { b.alias(&id, &l, 0.5, None, None); }
    }
    let counts = write(db, &b).await?;
    report.insert("written".into(), counts);
    report.insert("nhl_full_names".into(), json!(nhl_full.len()));
    Ok(Value::Object(report))
}

/// ESPN's abbreviation in the stored games' canonical code (ESPN uses UTAH for Utah's NHL team).
fn team_code(sport: &str, abbr: &str) -> String {
    match (sport, abbr) { ("nhl", "UTAH") => "UTA".into(), _ => crate::hitrates::canon(sport, abbr) }
}

async fn nhl_full_names(state: &State, teams: HashSet<String>) -> HashMap<String, String> {
    use futures::StreamExt;
    let http = state.http.clone();
    let lists = futures::stream::iter(teams.into_iter().map(|abbr| {
        let http = http.clone();
        async move {
            let Ok(v) = crate::nba_espn::get_json(&http, &format!("https://api-web.nhle.com/v1/roster/{abbr}/current")).await else { return Vec::new() };
            ["forwards", "defensemen", "goalies"].iter().flat_map(|g| v[*g].as_array().cloned().unwrap_or_default()).filter_map(|p| {
                let id = p["id"].as_i64()?;
                let name = format!("{} {}", p["firstName"]["default"].as_str()?, p["lastName"]["default"].as_str()?);
                Some((id.to_string(), name))
            }).collect::<Vec<_>>()
        }
    })).buffer_unordered(6).collect::<Vec<_>>().await;
    lists.into_iter().flatten().collect()
}

async fn write(db: &tokio_postgres::Client, b: &Batch) -> Result<Value, String> {
    let e = |x: tokio_postgres::Error| x.to_string();
    db.batch_execute("BEGIN").await.map_err(e)?;
    let res: Result<(), tokio_postgres::Error> = async {
        db.execute("DELETE FROM entities WHERE kind IN ('team','player')", &[]).await?;
        for chunk in b.ents.chunks(2000) {
            let (ids, kinds, sports, names, teams, attrs): (Vec<&str>, Vec<&str>, Vec<&str>, Vec<&str>, Vec<Option<&str>>, Vec<tokio_postgres::types::Json<&Value>>) = (
                chunk.iter().map(|x| x.0.as_str()).collect(), chunk.iter().map(|x| x.1.as_str()).collect(), chunk.iter().map(|x| x.2.as_str()).collect(),
                chunk.iter().map(|x| x.3.as_str()).collect(), chunk.iter().map(|x| x.4.as_deref()).collect(), chunk.iter().map(|x| tokio_postgres::types::Json(&x.5)).collect());
            db.execute("INSERT INTO entities (entity_id, kind, sport, name, team_id, attrs) SELECT * FROM UNNEST($1::text[], $2::text[], $3::text[], $4::text[], $5::text[], $6::jsonb[]) \
                        ON CONFLICT (entity_id) DO NOTHING", &[&ids, &kinds, &sports, &names, &teams, &attrs]).await?;
        }
        for chunk in b.ids.chunks(4000) {
            let (a, s, i): (Vec<&str>, Vec<&str>, Vec<&str>) = (chunk.iter().map(|x| x.0.as_str()).collect(), chunk.iter().map(|x| x.1.as_str()).collect(), chunk.iter().map(|x| x.2.as_str()).collect());
            db.execute("INSERT INTO entity_ids (entity_id, source, source_id) SELECT * FROM UNNEST($1::text[], $2::text[], $3::text[]) ON CONFLICT DO NOTHING", &[&a, &s, &i]).await?;
        }
        for chunk in b.aliases.chunks(4000) {
            let ids: Vec<&str> = chunk.iter().map(|x| x.0.as_str()).collect();
            let al: Vec<&str> = chunk.iter().map(|x| x.1.as_str()).collect();
            let nm: Vec<String> = chunk.iter().map(|x| norm(&x.1)).collect();
            let w: Vec<f32> = chunk.iter().map(|x| x.2).collect();
            let (f, t): (Vec<Option<chrono::NaiveDate>>, Vec<Option<chrono::NaiveDate>>) = (chunk.iter().map(|x| x.3).collect(), chunk.iter().map(|x| x.4).collect());
            db.execute("INSERT INTO entity_aliases (entity_id, alias, norm, weight, valid_from, valid_to) \
                        SELECT * FROM UNNEST($1::text[], $2::text[], $3::text[], $4::real[], $5::date[], $6::date[]) ON CONFLICT (entity_id, norm) DO NOTHING",
                       &[&ids, &al, &nm, &w, &f, &t]).await?;
        }
        Ok(())
    }.await;
    match res {
        Ok(()) => db.batch_execute("COMMIT").await.map_err(e)?,
        Err(x) => { let _ = db.batch_execute("ROLLBACK").await; return Err(x.to_string()); }
    }
    let r = db.query_one("SELECT (SELECT count(*) FROM entities WHERE kind='team'), (SELECT count(*) FROM entities WHERE kind='player'), \
                          (SELECT count(*) FROM entity_aliases), (SELECT count(*) FROM entity_ids)", &[]).await.map_err(e)?;
    Ok(json!({ "teams": r.get::<_, i64>(0), "players": r.get::<_, i64>(1), "aliases": r.get::<_, i64>(2), "source_ids": r.get::<_, i64>(3) }))
}

// ------------------------------------------------------------------ resolve

#[derive(Clone, Debug)]
pub(crate) struct Cand { pub id: String, pub kind: String, pub sport: String, pub name: String, pub team: Option<String>, pub score: f64, pub matched: String, pub active: bool, pub quality: f64 }

/// Close spellings only count when nothing matched the mention as written: "LeBron" is a
/// prefix of LeBron James, so De León (one letter off) is not a rival.
pub(crate) fn drop_fuzzy_when_written_matches(c: Vec<Cand>) -> Vec<Cand> {
    if c.iter().any(|x| x.quality >= 2.0) { c.into_iter().filter(|x| x.quality >= 1.0).collect() } else { c }
}

/// How well one alias matches the mention: exact 3, prefix 2, contained 1, close spelling 1 - 0.3 per edit.
pub(crate) fn match_score(q: &str, alias: &str) -> Option<f64> {
    if q.is_empty() { return None; }
    if alias == q { return Some(3.0); }
    if alias.starts_with(q) && q.len() >= 3 { return Some(2.0); }
    if q.len() >= 4 && alias.contains(q) { return Some(1.0); }
    if q.len() >= 6 && alias.len().abs_diff(q.len()) <= 2 {
        let d = levenshtein(q, alias);
        if d <= 2 { return Some(1.0 - 0.3 * d as f64); }
    }
    None
}

/// Resolved / ambiguous / none, from scored candidates (one per entity, best alias).
pub(crate) fn decide(mut c: Vec<Cand>) -> Value {
    c.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.name.cmp(&b.name)));
    let show = |x: &Cand| json!({ "entity_id": x.id, "kind": x.kind, "sport": x.sport, "name": x.name, "team_id": x.team, "score": (x.score * 100.0).round() / 100.0, "matched": x.matched });
    match c.as_slice() {
        [] => json!({ "status": "none", "candidates": [] }),
        [one] => json!({ "status": "resolved", "entity": show(one), "candidates": [show(one)] }),
        [a, b, ..] if a.score - b.score >= CLEAR_LEAD => json!({ "status": "resolved", "entity": show(a), "candidates": c.iter().take(4).map(show).collect::<Vec<_>>() }),
        _ => json!({ "status": "ambiguous", "candidates": c.iter().take(4).map(show).collect::<Vec<_>>() }),
    }
}

#[derive(Deserialize)]
pub struct SearchQ { q: String, sport: Option<String>, kind: Option<String>, #[serde(default)] focus: Option<String>, #[serde(default = "d_limit")] limit: usize }
fn d_limit() -> usize { 10 }

async fn candidates(db: &tokio_postgres::Client, q: &SearchQ) -> Result<Vec<Cand>, String> {
    // "the Lakers" is the Lakers
    let raw = q.q.trim();
    let raw = ["the ", "The ", "THE "].iter().fold(raw, |t, a| t.strip_prefix(a).unwrap_or(t));
    let n = norm(raw);
    if n.is_empty() || n.len() > 60 { return Ok(Vec::new()); }
    let focus: HashSet<String> = q.focus.as_deref().unwrap_or("").split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    let first = n.chars().next().map(|c| c.to_string()).unwrap_or_default();
    let rows = db.query(
        "SELECT a.entity_id, a.norm, a.weight, e.kind, e.sport, e.name, e.team_id, (e.attrs->>'last_season')::int, a.valid_to \
         FROM entity_aliases a JOIN entities e USING (entity_id) \
         WHERE ($2::text IS NULL OR e.sport = $2) AND ($3::text IS NULL OR e.kind = $3) \
           AND (a.norm = $1 OR a.norm LIKE $1 || '%' OR (length($1) >= 4 AND a.norm LIKE '%' || $1 || '%') \
                OR (length($1) >= 6 AND left(a.norm, 1) = $4 AND abs(length(a.norm) - length($1)) <= 2)) \
         LIMIT 3000", &[&n, &q.sport, &q.kind, &first]).await.map_err(|e| e.to_string())?;
    let this_year = chrono::Utc::now().year();
    let mut best: HashMap<String, Cand> = HashMap::new();
    for r in &rows {
        let (id, alias, w): (String, String, f32) = (r.get(0), r.get(1), r.get(2));
        let Some(m) = match_score(&n, &alias) else { continue };
        let kind: String = r.get(3);
        let team: Option<String> = r.get(6);
        let active = kind == "team" && r.get::<_, Option<chrono::NaiveDate>>(8).is_none() || r.get::<_, Option<i32>>(7).map(|s| s >= this_year - 1).unwrap_or(false);
        let score = m * w as f64 + if active { 1.0 } else { 0.0 }
            + if focus.contains(&id) || team.as_ref().map(|t| focus.contains(t)).unwrap_or(false) { 3.0 } else { 0.0 };
        let c = Cand { id: id.clone(), kind, sport: r.get(4), name: r.get(5), team, score, matched: alias, active, quality: m };
        if best.get(&id).map(|b| b.score < c.score).unwrap_or(true) { best.insert(id, c); }
    }
    Ok(drop_fuzzy_when_written_matches(best.into_values().collect()))
}

pub async fn search(state: web::Data<State>, q: web::Query<SearchQ>) -> impl Responder {
    match candidates(&state.db, &q).await {
        Ok(mut c) => {
            c.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.name.cmp(&b.name)));
            let limit = q.limit.clamp(1, 50);
            HttpResponse::Ok().json(json!({ "q": q.q, "results": c.iter().take(limit).map(|x| json!({ "entity_id": x.id, "kind": x.kind, "sport": x.sport,
                "name": x.name, "team_id": x.team, "active": x.active, "score": (x.score * 100.0).round() / 100.0 })).collect::<Vec<_>>() }))
        }
        Err(e) => crate::internal(e),
    }
}

pub async fn resolve(state: web::Data<State>, q: web::Query<SearchQ>) -> impl Responder {
    match candidates(&state.db, &q).await {
        Ok(c) => HttpResponse::Ok().json(decide(c)),
        Err(e) => crate::internal(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(id: &str, name: &str, score: f64) -> Cand {
        Cand { id: id.into(), kind: "player".into(), sport: "nba".into(), name: name.into(), team: None, score, matched: String::new(), active: true, quality: 3.0 }
    }

    #[test]
    fn close_spellings_do_not_compete_with_a_name_as_written() {
        let fuzzy = |id: &str, name: &str| Cand { quality: 0.7, score: 1.2, ..cand(id, name, 0.0) };
        let kept = drop_fuzzy_when_written_matches(vec![Cand { quality: 2.0, ..cand("pl:nba:1", "LeBron James", 3.0) }, fuzzy("pl:mlb:2", "José De León")]);
        assert_eq!(kept.len(), 1);
        assert_eq!(decide(kept)["status"], "resolved");
        // a typo with no written match keeps its close spellings (and may be ambiguous)
        assert_eq!(drop_fuzzy_when_written_matches(vec![fuzzy("a", "Nikola Jokic"), fuzzy("b", "Nikola Jovic")]).len(), 2);
    }

    #[test]
    fn names_fold_accents_and_punctuation() {
        assert_eq!(norm("Nikola Jokić"), "nikolajokic");
        assert_eq!(norm("Shai Gilgeous-Alexander"), "shaigilgeousalexander");
        assert_eq!(norm("C. McDavid"), "cmcdavid");
        assert_eq!(last_name("Jaren Jackson Jr."), Some("Jackson".into()));
        assert_eq!(last_name("C. McDavid"), Some("McDavid".into()));
        assert_eq!(last_name("Nuggets"), None);
    }

    #[test]
    fn match_quality_orders_exact_prefix_contained_and_close_spellings() {
        assert_eq!(match_score("lebronjames", "lebronjames"), Some(3.0));
        assert_eq!(match_score("lebron", "lebronjames"), Some(2.0));
        assert_eq!(match_score("mcdavid", "connormcdavid"), Some(1.0));
        assert_eq!(match_score("jokicc", "jokic"), Some(0.7), "a doubled letter is one edit");
        assert_eq!(match_score("jokix", "jokic"), None, "five letters is too short for a fuzzy match");
        assert!(match_score("nikolajokis", "nikolajokic").unwrap() > 0.0, "one wrong letter still matches");
        assert_eq!(match_score("ja", "james"), None, "two letters never match by prefix");
    }

    #[test]
    fn a_shared_name_is_ambiguous_and_a_clear_leader_resolves() {
        // "James": several players share the last-name alias (0.5 x exact 3 + active 1)
        let amb = decide(vec![cand("pl:nba:1", "LeBron James", 2.5), cand("pl:nba:2", "Jalen James", 2.5), cand("pl:nba:3", "Mike James", 1.5)]);
        assert_eq!(amb["status"], "ambiguous");
        assert_eq!(amb["candidates"].as_array().unwrap().len(), 3);
        // in focus (+3) the conversation's player wins clearly
        let r = decide(vec![cand("pl:nba:1", "LeBron James", 5.5), cand("pl:nba:2", "Jalen James", 2.5)]);
        assert_eq!((r["status"].as_str(), r["entity"]["entity_id"].as_str()), (Some("resolved"), Some("pl:nba:1")));
        assert_eq!(decide(vec![])["status"], "none");
        assert_eq!(decide(vec![cand("pl:nba:9", "Nikola Jokic", 4.0)])["status"], "resolved");
    }

    #[test]
    fn historical_teams_and_new_franchises_are_kept_apart() {
        assert!(HISTORICAL.iter().any(|h| h == &("nfl", "OAK", "LV", "Oakland Raiders")), "a relocation keeps the franchise");
        assert!(HISTORICAL.iter().any(|h| h.0 == "nhl" && h.1 == "ARI" && h.2 == "ARI"), "Arizona is not Utah: a new franchise");
        assert_eq!(team_code("nhl", "UTAH"), "UTA");
        assert_eq!(team_code("nba", "NY"), "NYK");
    }
}
