//! NBA referee crews: free-throw tendencies and today's assignments.
//!
//! Each referee's tendency is a ridge estimate of the free-throw attempts his
//! games produce above the season's average (both teams), crew = mean of the
//! three, shrunk to 0 with LAMBDA games of prior, games weighted by
//! 0.5^(age / 540 days), regulation games only (overtime runs long). Fits are
//! taken as of the first of each month (games strictly before it).
//!
//! Walk-forward on 2022-2026 regular seasons (5,810 games): a crew's predicted
//! FTA effect comes true one for one (slope +0.95 +/- 0.13) and crews differ by
//! about +/- 1.1 FTA a game; per predicted FTA the game total beats the closing
//! total by +0.69 +/- 0.28 points (3,240 games with lines), i.e. the market
//! prices part of it. Crews have no measurable effect on pace.
//!
//! Crews come from ESPN summaries (completed games, `nba_espn.rs`) and from
//! official.nba.com's assignments page (posted ~9:00 ET on game day).
//!
//!   POST /fit/nba/refs?from=&to=      monthly fits (missing ones)
//!   POST /ingest/nba/refs              today's assignments from official.nba.com

use std::collections::HashMap;

use actix_web::{web, HttpResponse, Responder};
use chrono::{Datelike, NaiveDate};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_postgres::types::Json;

use crate::State;

pub const LAMBDA: f64 = 100.0;
const HALF_LIFE_DAYS: f64 = 540.0;
const YEARS: i64 = 4;
pub const METHOD: &str = "crew-fta-ridge-v1";
const ASSIGNMENTS: &str = "https://official.nba.com/referee-assignments/";

fn dberr(e: &tokio_postgres::Error) -> String {
    e.as_db_error().map(|d| d.message().to_string()).unwrap_or_else(|| e.to_string())
}
fn key(d: NaiveDate) -> String { format!("nba:refs:{d}") }

/// Ridge: y_g = mean of the crew's b_r; b shrunk to 0 with `lambda` games of prior.
pub(crate) fn ridge(games: &[(f64, Vec<usize>, f64)], n: usize, lambda: f64) -> Result<Vec<f64>, String> {
    let mut a = vec![0.0; n * n];
    let mut v = vec![0.0; n];
    for (y, crew, w) in games {
        let k = crew.len() as f64;
        for &i in crew {
            v[i] += w * y / k;
            for &j in crew { a[i * n + j] += w / (k * k); }
        }
    }
    for i in 0..n { a[i * n + i] += lambda / 9.0; }
    crate::nba_impact::cholesky_solve(&mut a, &mut v, n)?;
    Ok(v)
}

/// Fit as of a date (games strictly before it) and store.
pub(crate) async fn fit(db: &tokio_postgres::Client, asof: NaiveDate) -> Result<Value, String> {
    let from = asof - chrono::Duration::days(365 * YEARS);
    let rows = db.query("SELECT g.espn_id, g.season, g.game_date, sum(b.fta)::float8, \
          (SELECT array_agg(o.name ORDER BY o.ord) FROM nba_officials_espn o WHERE o.espn_id = g.espn_id) \
        FROM nba_games_espn g JOIN nba_box_espn b USING (espn_id) \
        WHERE g.completed AND g.season_type IN (2,3) AND g.game_date >= $1 AND g.game_date < $2 \
        GROUP BY g.espn_id, g.season, g.game_date HAVING sum(b.min) < 490", &[&from, &asof]).await.map_err(|e| dberr(&e))?;
    let mut by_season: HashMap<i32, (f64, f64)> = HashMap::new();
    let mut obs: Vec<(i32, NaiveDate, f64, Vec<String>)> = Vec::new();
    for r in &rows {
        let Some(crew) = r.get::<_, Option<Vec<String>>>(4) else { continue };
        if crew.len() < 3 { continue; }
        let (season, fta): (i32, f64) = (r.get(1), r.get(3));
        let e = by_season.entry(season).or_default();
        e.0 += fta; e.1 += 1.0;
        obs.push((season, r.get(2), fta, crew.into_iter().take(3).collect()));
    }
    if obs.len() < 500 { return Err(format!("only {} games with crews before {asof}", obs.len())); }
    let mut idx: HashMap<String, usize> = HashMap::new();
    let games: Vec<(f64, Vec<usize>, f64)> = obs.iter().map(|(s, d, fta, crew)| {
        let (sum, n) = by_season[s];
        let crew = crew.iter().map(|c| { let k = idx.len(); *idx.entry(c.clone()).or_insert(k) }).collect();
        (fta - sum / n, crew, 0.5f64.powf((asof - *d).num_days() as f64 / HALF_LIFE_DAYS))
    }).collect();
    let n = idx.len();
    let b = web::block(move || ridge(&games, n, LAMBDA)).await.map_err(|e| e.to_string())??;
    let latest = obs.iter().map(|o| o.0).max().unwrap_or(0);
    let league_fta = by_season.get(&latest).map(|(s, n)| s / n).unwrap_or(44.0);
    let ratings: serde_json::Map<String, Value> = idx.iter().map(|(r, &i)| (r.clone(), json!((b[i] * 1000.0).round() / 1000.0))).collect();
    let v = json!({ "asof": asof, "method": METHOD, "lambda": LAMBDA, "games": obs.len(), "referees": n, "league_fta": league_fta,
        "unit": "free-throw attempts per game (both teams) above the season average", "ratings": ratings });
    db.execute("INSERT INTO sim_model_params (key, params, source) VALUES ($1, $2, $3) ON CONFLICT (key) DO UPDATE SET params = EXCLUDED.params, source = EXCLUDED.source, fitted_at = now()",
        &[&key(asof), &Json(&v), &format!("referee crew FTA ridge, {} games before {asof}", obs.len())]).await.map_err(|e| dberr(&e))?;
    Ok(json!({ "asof": asof, "games": obs.len(), "referees": n }))
}

pub(crate) struct Ratings { pub fta: HashMap<String, f64>, pub league_fta: f64, pub asof: Option<NaiveDate> }

/// The fit in force on a date (the latest on or before it).
pub(crate) async fn load(db: &tokio_postgres::Client, date: NaiveDate) -> Result<Ratings, String> {
    let Some(r) = db.query_opt("SELECT params FROM sim_model_params WHERE key LIKE 'nba:refs:%' AND key <= $1 ORDER BY key DESC LIMIT 1", &[&key(date)])
        .await.map_err(|e| dberr(&e))? else { return Ok(Ratings { fta: HashMap::new(), league_fta: 44.0, asof: None }) };
    let v: Json<Value> = r.get(0);
    Ok(Ratings {
        fta: v.0["ratings"].as_object().map(|o| o.iter().filter_map(|(k, x)| x.as_f64().map(|f| (k.clone(), f))).collect()).unwrap_or_default(),
        league_fta: v.0["league_fta"].as_f64().unwrap_or(44.0),
        asof: v.0["asof"].as_str().and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()),
    })
}

/// A game's crew (ESPN or official.nba.com), crew chief first; empty when not posted.
pub(crate) async fn crew(db: &tokio_postgres::Client, espn_id: &str) -> Result<Vec<String>, String> {
    Ok(db.query("SELECT name FROM nba_officials_espn WHERE espn_id = $1 ORDER BY ord LIMIT 3", &[&espn_id]).await.map_err(|e| dberr(&e))?
        .iter().map(|r| r.get(0)).collect())
}

/// The crew's FTA effect: the mean of its referees (unknown referees count 0).
pub(crate) fn crew_effect(r: &Ratings, crew: &[String]) -> f64 {
    if crew.is_empty() { return 0.0; }
    crew.iter().map(|c| r.fta.get(&norm(c)).or_else(|| r.fta.get(c)).copied().unwrap_or(0.0)).sum::<f64>() / crew.len() as f64
}

fn norm(s: &str) -> String { s.split(" (#").next().unwrap_or(s).trim().to_string() }

fn months(from: NaiveDate, to: NaiveDate) -> Vec<NaiveDate> {
    let mut d = NaiveDate::from_ymd_opt(from.year(), from.month(), 1).unwrap_or(from);
    let mut out = Vec::new();
    while d <= to {
        if !(7..=9).contains(&d.month()) { out.push(d); }
        d = if d.month() == 12 { NaiveDate::from_ymd_opt(d.year() + 1, 1, 1) } else { NaiveDate::from_ymd_opt(d.year(), d.month() + 1, 1) }.unwrap_or(to + chrono::Duration::days(1));
    }
    out
}

/// Fit every missing month in the range.
pub(crate) async fn ensure(db: &tokio_postgres::Client, from: NaiveDate, to: NaiveDate) -> Result<Vec<Value>, String> {
    let have: std::collections::HashSet<String> = db.query("SELECT key FROM sim_model_params WHERE key LIKE 'nba:refs:%' AND params->>'method' = $1", &[&METHOD])
        .await.map_err(|e| dberr(&e))?.iter().map(|r| r.get(0)).collect();
    let mut out = Vec::new();
    for m in months(from, to) {
        if have.contains(&key(m)) { continue; }
        out.push(fit(db, m).await.unwrap_or_else(|e| json!({ "asof": m, "error": e })));
    }
    Ok(out)
}

/// official.nba.com team names -> ESPN abbreviations.
fn team_abbr(name: &str) -> Option<&'static str> {
    let n = name.to_lowercase().replace('.', "");
    if n.contains("clippers") { return Some("LAC"); }
    if n.contains("lakers") { return Some("LAL"); }
    Some(match n.trim() {
        "atlanta" => "ATL", "boston" => "BOS", "brooklyn" => "BKN", "charlotte" => "CHA", "chicago" => "CHI", "cleveland" => "CLE",
        "dallas" => "DAL", "denver" => "DEN", "detroit" => "DET", "golden state" => "GS", "houston" => "HOU", "indiana" => "IND",
        "memphis" => "MEM", "miami" => "MIA", "milwaukee" => "MIL", "minnesota" => "MIN", "new orleans" => "NO", "new york" => "NY",
        "oklahoma city" => "OKC", "orlando" => "ORL", "philadelphia" => "PHI", "phoenix" => "PHX", "portland" => "POR",
        "sacramento" => "SAC", "san antonio" => "SA", "toronto" => "TOR", "utah" => "UTAH", "washington" => "WSH",
        _ => return None,
    })
}

fn strip_tags(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c { '<' => in_tag = true, '>' => { in_tag = false; out.push(' '); } _ if !in_tag => out.push(c), _ => {} }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// (date, [(away, home, [crew chief, referee, umpire])]) from the assignments page.
pub(crate) fn parse_assignments(html: &str) -> Option<(NaiveDate, Vec<(String, String, Vec<String>)>)> {
    let meta = html.split("entry-meta\">").nth(1)?.split('<').next()?.trim();
    let date = NaiveDate::parse_from_str(meta, "%B %d, %Y").ok()?;
    let mut games = Vec::new();
    for row in html.split("<tr>").skip(1) {
        let row = row.split("</tr>").next().unwrap_or("");
        let cells: Vec<String> = row.split("<td").skip(1).map(|c| strip_tags(&format!("<td{c}"))).collect();
        let Some((away, home)) = cells.first().and_then(|g| g.split_once(" @ ")) else { continue };
        let (Some(a), Some(h)) = (team_abbr(away), team_abbr(home)) else { continue };
        let crew: Vec<String> = cells.iter().skip(1).take(3).map(|c| norm(c)).filter(|c| !c.is_empty()).collect();
        if crew.len() == 3 { games.push((a.to_string(), h.to_string(), crew)); }
    }
    Some((date, games))
}

/// Fetch today's assignments and store them against the matching ESPN games.
pub(crate) async fn ingest_assignments(state: &State) -> Result<Value, String> {
    let html = state.http.get(ASSIGNMENTS).header("User-Agent", "Mozilla/5.0").send().await.map_err(|e| e.to_string())?
        .text().await.map_err(|e| e.to_string())?;
    let Some((date, games)) = parse_assignments(&html) else { return Err("assignments page: no date found".into()) };
    let mut stored = 0;
    let mut unmatched = Vec::new();
    for (a, h, crew) in &games {
        let Some(r) = state.db.query_opt("SELECT espn_id FROM nba_games_espn WHERE game_date = $1 AND away = $2 AND home = $3", &[&date, a, h])
            .await.map_err(|e| dberr(&e))? else { unmatched.push(format!("{a} @ {h}")); continue };
        let id: String = r.get(0);
        for (i, name) in crew.iter().enumerate() {
            state.db.execute("INSERT INTO nba_officials_espn (espn_id, ord, name) VALUES ($1,$2,$3) ON CONFLICT (espn_id, ord) DO UPDATE SET name=EXCLUDED.name",
                &[&id, &(i as i32 + 1), name]).await.map_err(|e| dberr(&e))?;
        }
        stored += 1;
    }
    Ok(json!({ "date": date, "games": games.len(), "stored": stored, "unmatched": unmatched }))
}

#[derive(Deserialize)]
pub struct FitQ { from: Option<String>, to: Option<String> }

pub async fn fit_http(state: web::Data<State>, q: web::Query<FitQ>) -> impl Responder {
    let today = (chrono::Utc::now() - chrono::Duration::hours(5)).date_naive();
    let p = |s: &Option<String>| s.as_deref().and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok());
    match ensure(&state.db, p(&q.from).unwrap_or(today), p(&q.to).unwrap_or(today)).await {
        Ok(v) => HttpResponse::Ok().json(json!({ "fitted": v })),
        Err(e) => HttpResponse::InternalServerError().json(json!({ "error": e })),
    }
}

pub async fn assignments_http(state: web::Data<State>) -> impl Responder {
    match ingest_assignments(&state).await {
        Ok(v) => HttpResponse::Ok().json(v),
        Err(e) => HttpResponse::InternalServerError().json(json!({ "error": e })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ridge_finds_a_whistle_happy_referee() {
        // referee 0 adds 6 FTA a game; crews rotate through 10 referees
        let games: Vec<(f64, Vec<usize>, f64)> = (0..3000).filter_map(|g| {
            let a = g % 10;
            let b = (a + 1 + (g / 10) % 8) % 10;
            let c = (b + 1 + (g / 80) % 8) % 10;
            if c == a { return None; }
            let crew = vec![a, b, c];
            Some((if crew.contains(&0) { 2.0 } else { 0.0 }, crew, 1.0))
        }).collect();
        let b = ridge(&games, 10, 10.0).unwrap();
        let rest = b[1..].iter().sum::<f64>() / 9.0;
        assert!(b[0] > 4.0 * rest.abs().max(0.1), "{b:?}");
    }

    #[test]
    fn parses_the_assignments_page() {
        let html = r#"<div class="entry-meta">October 22, 2026</div><table><tbody>
            <tr><td>Cleveland @ Philadelphia</td><td><a href="x">Curtis Blair (#74) </a></td><td><a>Aaron Smith (#51)</a></td><td><a>CJ Washington (#12)</a></td><td></td></tr>
            <tr><td>LA Clippers @ L.A. Lakers</td><td>A B (#1)</td><td>C D (#2)</td><td>E F (#3)</td><td>G H (#4)</td></tr></tbody></table>"#;
        let (d, g) = parse_assignments(html).unwrap();
        assert_eq!(d, NaiveDate::from_ymd_opt(2026, 10, 22).unwrap());
        assert_eq!(g[0], ("CLE".into(), "PHI".into(), vec!["Curtis Blair".into(), "Aaron Smith".into(), "CJ Washington".into()]));
        assert_eq!((g[1].0.as_str(), g[1].1.as_str(), g[1].2.len()), ("LAC", "LAL", 3));
    }
}
