//! NFL play-by-play warehouse, simulator fitting, game simulation (ensemble +
//! interactive broadcast sessions) and the walk-forward backtest.
//!
//! Data: nflverse play-by-play (github.com/nflverse/nflverse-data releases,
//! CC-BY 4.0), one row per snap/kick, stored in `nfl_plays`.
//!
//! Leakage control: league parameters are fitted per season (`nfl:sim:{S}`);
//! a regular-season game in season S uses `nfl:sim:{S-1}`, a postseason game
//! `nfl:sim:{S}`; every team/player input uses plays strictly before the game
//! date.

use std::collections::HashMap;
use std::io::Read;
use std::time::Instant;

use actix_web::{web, HttpResponse, Responder};
use chrono::NaiveDate;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_postgres::types::{Json, ToSql};

use crate::nfl_sim::{self as sim, GameInput, League, Rate};
use crate::State;

pub const MODEL_VERSION: &str = "nfl-play-sim-v1";
const PBP_URL: &str = "https://github.com/nflverse/nflverse-data/releases/download/pbp/play_by_play_{season}.csv.gz";

fn internal(m: impl std::fmt::Display) -> HttpResponse {
    eprintln!("projection-service(nfl-sim): {m}");
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

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

/// One stored play.
#[derive(Clone, Debug, Default)]
pub struct P {
    pub game_id: String,
    pub play_id: i32,
    pub season: i32,
    pub week: i32,
    pub season_type: String,
    pub date: NaiveDate,
    pub home: String,
    pub away: String,
    pub posteam: Option<String>,
    pub defteam: Option<String>,
    pub qtr: i32,
    pub qsr: Option<i32>,
    pub yl: Option<i32>,
    pub down: Option<i32>,
    pub togo: Option<i32>,
    pub diff: Option<i32>,
    pub ptype: String,
    pub yards: i32,
    pub sack: bool,
    pub scramble: bool,
    pub int: bool,
    pub complete: bool,
    pub fumble_lost: bool,
    pub td: bool,
    pub td_team: Option<String>,
    pub safety: bool,
    pub oob: bool,
    pub penalty: bool,
    pub two_point: bool,
    pub fg_result: Option<String>,
    pub xp_result: Option<String>,
    pub two_pt_result: Option<String>,
    pub kick_distance: Option<i32>,
    pub air_yards: Option<i32>,
    pub location: Option<String>,
    pub passer: Option<(String, String)>,
    pub rusher: Option<(String, String)>,
    pub receiver: Option<(String, String)>,
    pub kicker: Option<(String, String)>,
    pub penalty_team: Option<String>,
    pub penalty_yards: Option<i32>,
    pub first_down_penalty: bool,
}

impl P {
    fn scrimmage(&self) -> bool { (self.ptype == "pass" || self.ptype == "run") && !self.two_point && self.down.is_some() }
    fn dropback(&self) -> bool { self.scrimmage() && (self.ptype == "pass" || self.scramble) }
    fn attempt(&self) -> bool { self.scrimmage() && self.ptype == "pass" && !self.sack }
    fn designed_run(&self) -> bool { self.scrimmage() && self.ptype == "run" && !self.scramble }
    fn snap(&self) -> bool { self.scrimmage() || self.ptype == "qb_kneel" || self.ptype == "qb_spike" }
}

const COLS: &str = "game_id, play_id, season, week, season_type, game_date, home_team, away_team, posteam, defteam, qtr, qsr, \
    yardline_100, down, ydstogo, score_diff, play_type, yards, sack, scramble, interception, complete, fumble_lost, touchdown, \
    td_team, safety, out_of_bounds, penalty, two_point, fg_result, xp_result, two_pt_result, kick_distance, air_yards, location, \
    passer_id, passer_name, rusher_id, rusher_name, receiver_id, receiver_name, kicker_id, kicker_name, \
    penalty_team, penalty_yards, first_down_penalty";
const NCOLS: usize = 46;

fn pair(r: &tokio_postgres::Row, a: usize) -> Option<(String, String)> {
    let id: Option<String> = r.get(a);
    id.map(|i| (i, r.get::<_, Option<String>>(a + 1).unwrap_or_default()))
}
fn from_row(r: &tokio_postgres::Row) -> P {
    P {
        game_id: r.get(0), play_id: r.get(1), season: r.get(2), week: r.get(3), season_type: r.get(4), date: r.get(5),
        home: r.get(6), away: r.get(7), posteam: r.get(8), defteam: r.get(9), qtr: r.get::<_, Option<i32>>(10).unwrap_or(0),
        qsr: r.get(11), yl: r.get(12), down: r.get(13), togo: r.get(14), diff: r.get(15),
        ptype: r.get::<_, Option<String>>(16).unwrap_or_default(), yards: r.get::<_, Option<i32>>(17).unwrap_or(0),
        sack: r.get(18), scramble: r.get(19), int: r.get(20), complete: r.get(21), fumble_lost: r.get(22), td: r.get(23),
        td_team: r.get(24), safety: r.get(25), oob: r.get(26), penalty: r.get(27), two_point: r.get(28),
        fg_result: r.get(29), xp_result: r.get(30), two_pt_result: r.get(31), kick_distance: r.get(32), air_yards: r.get(33),
        location: r.get(34), passer: pair(r, 35), rusher: pair(r, 37), receiver: pair(r, 39), kicker: pair(r, 41),
        penalty_team: r.get(43), penalty_yards: r.get(44), first_down_penalty: r.get(45),
    }
}

// ---------------------------------------------------------------------------
// Ingest
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct IngestQuery {
    /// comma-separated seasons, default 2024,2025,2026
    seasons: Option<String>,
}

fn opt_int(s: &str) -> Option<i32> {
    if s.is_empty() || s == "NA" { None } else { s.parse::<f64>().ok().map(|v| v.round() as i32) }
}
fn opt_str(s: &str) -> Option<String> {
    if s.is_empty() || s == "NA" { None } else { Some(s.to_string()) }
}
fn flag(s: &str) -> bool {
    matches!(s, "1" | "1.0" | "TRUE" | "true")
}

/// Parse one nflverse pbp CSV (already decompressed) into rows.
pub fn parse_pbp(csv_text: &[u8]) -> Result<Vec<P>, String> {
    let mut rdr = csv::ReaderBuilder::new().from_reader(csv_text);
    let headers = rdr.headers().map_err(|e| e.to_string())?.clone();
    let idx: HashMap<&str, usize> = headers.iter().enumerate().map(|(i, h)| (h, i)).collect();
    let col = |name: &str| idx.get(name).copied();
    let need = ["game_id", "play_id", "season", "week", "season_type", "game_date", "home_team", "away_team", "posteam",
        "defteam", "qtr", "quarter_seconds_remaining", "yardline_100", "down", "ydstogo", "score_differential", "play_type",
        "yards_gained", "sack", "qb_scramble", "interception", "complete_pass", "fumble_lost", "touchdown", "td_team",
        "safety", "out_of_bounds", "penalty", "two_point_attempt", "field_goal_result", "extra_point_result",
        "two_point_conv_result", "kick_distance", "air_yards", "pass_location", "run_location", "passer_player_id",
        "passer_player_name", "rusher_player_id", "rusher_player_name", "receiver_player_id", "receiver_player_name",
        "kicker_player_id", "kicker_player_name", "penalty_team", "penalty_yards", "first_down_penalty"];
    for n in need {
        if col(n).is_none() {
            return Err(format!("pbp csv missing column {n}"));
        }
    }
    let c: HashMap<&str, usize> = need.iter().map(|n| (*n, col(n).unwrap())).collect();
    let mut out = Vec::new();
    for rec in rdr.records() {
        let rec = rec.map_err(|e| e.to_string())?;
        let g = |n: &str| rec.get(c[n]).unwrap_or("");
        let ptype = g("play_type");
        if ptype.is_empty() || ptype == "NA" {
            continue;
        }
        let Ok(date) = NaiveDate::parse_from_str(g("game_date"), "%Y-%m-%d") else { continue };
        let pr = |id: &str, nm: &str| opt_str(g(id)).map(|i| (i, g(nm).to_string()));
        out.push(P {
            game_id: g("game_id").into(), play_id: opt_int(g("play_id")).unwrap_or(0), season: opt_int(g("season")).unwrap_or(0),
            week: opt_int(g("week")).unwrap_or(0), season_type: g("season_type").into(), date,
            home: g("home_team").into(), away: g("away_team").into(), posteam: opt_str(g("posteam")), defteam: opt_str(g("defteam")),
            qtr: opt_int(g("qtr")).unwrap_or(0), qsr: opt_int(g("quarter_seconds_remaining")), yl: opt_int(g("yardline_100")),
            down: opt_int(g("down")), togo: opt_int(g("ydstogo")), diff: opt_int(g("score_differential")), ptype: ptype.into(),
            yards: opt_int(g("yards_gained")).unwrap_or(0), sack: flag(g("sack")), scramble: flag(g("qb_scramble")),
            int: flag(g("interception")), complete: flag(g("complete_pass")), fumble_lost: flag(g("fumble_lost")),
            td: flag(g("touchdown")), td_team: opt_str(g("td_team")), safety: flag(g("safety")), oob: flag(g("out_of_bounds")),
            penalty: flag(g("penalty")), two_point: flag(g("two_point_attempt")), fg_result: opt_str(g("field_goal_result")),
            xp_result: opt_str(g("extra_point_result")), two_pt_result: opt_str(g("two_point_conv_result")),
            kick_distance: opt_int(g("kick_distance")), air_yards: opt_int(g("air_yards")),
            location: opt_str(g("pass_location")).or_else(|| opt_str(g("run_location"))),
            passer: pr("passer_player_id", "passer_player_name"), rusher: pr("rusher_player_id", "rusher_player_name"),
            receiver: pr("receiver_player_id", "receiver_player_name"), kicker: pr("kicker_player_id", "kicker_player_name"),
            penalty_team: opt_str(g("penalty_team")), penalty_yards: opt_int(g("penalty_yards")), first_down_penalty: flag(g("first_down_penalty")),
        });
    }
    Ok(out)
}

async fn store_plays(db: &tokio_postgres::Client, rows: &[P]) -> Result<u64, String> {
    let mut n = 0u64;
    for chunk in rows.chunks(250) {
        let mut sql = format!("INSERT INTO nfl_plays ({}) VALUES ", COLS);
        let mut params: Vec<Box<dyn ToSql + Sync + Send>> = Vec::new();
        for (i, p) in chunk.iter().enumerate() {
            if i > 0 { sql.push(','); }
            sql.push('(');
            for k in 0..NCOLS {
                if k > 0 { sql.push(','); }
                sql.push_str(&format!("${}", i * NCOLS + k + 1));
            }
            sql.push(')');
            let split = |x: &Option<(String, String)>| (x.as_ref().map(|v| v.0.clone()), x.as_ref().map(|v| v.1.clone()));
            let (pa, pn) = split(&p.passer);
            let (ra, rn) = split(&p.rusher);
            let (ca, cn) = split(&p.receiver);
            let (ka, kn) = split(&p.kicker);
            params.push(Box::new(p.game_id.clone()));
            params.push(Box::new(p.play_id));
            params.push(Box::new(p.season));
            params.push(Box::new(p.week));
            params.push(Box::new(p.season_type.clone()));
            params.push(Box::new(p.date));
            params.push(Box::new(p.home.clone()));
            params.push(Box::new(p.away.clone()));
            params.push(Box::new(p.posteam.clone()));
            params.push(Box::new(p.defteam.clone()));
            params.push(Box::new(Some(p.qtr)));
            params.push(Box::new(p.qsr));
            params.push(Box::new(p.yl));
            params.push(Box::new(p.down));
            params.push(Box::new(p.togo));
            params.push(Box::new(p.diff));
            params.push(Box::new(Some(p.ptype.clone())));
            params.push(Box::new(Some(p.yards)));
            params.push(Box::new(p.sack));
            params.push(Box::new(p.scramble));
            params.push(Box::new(p.int));
            params.push(Box::new(p.complete));
            params.push(Box::new(p.fumble_lost));
            params.push(Box::new(p.td));
            params.push(Box::new(p.td_team.clone()));
            params.push(Box::new(p.safety));
            params.push(Box::new(p.oob));
            params.push(Box::new(p.penalty));
            params.push(Box::new(p.two_point));
            params.push(Box::new(p.fg_result.clone()));
            params.push(Box::new(p.xp_result.clone()));
            params.push(Box::new(p.two_pt_result.clone()));
            params.push(Box::new(p.kick_distance));
            params.push(Box::new(p.air_yards));
            params.push(Box::new(p.location.clone()));
            params.push(Box::new(pa));
            params.push(Box::new(pn));
            params.push(Box::new(ra));
            params.push(Box::new(rn));
            params.push(Box::new(ca));
            params.push(Box::new(cn));
            params.push(Box::new(ka));
            params.push(Box::new(kn));
            params.push(Box::new(p.penalty_team.clone()));
            params.push(Box::new(p.penalty_yards));
            params.push(Box::new(p.first_down_penalty));
        }
        sql.push_str(" ON CONFLICT (game_id, play_id) DO NOTHING");
        let refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|b| b.as_ref() as &(dyn ToSql + Sync)).collect();
        n += db.execute(sql.as_str(), &refs).await.map_err(|e| format!("insert plays: {}", dberr(&e)))?;
    }
    Ok(n)
}

/// POST /ingest/nfl/pbp?seasons=2024,2025,2026 — replaces each season's plays.
pub async fn ingest(state: web::Data<State>, q: web::Query<IngestQuery>) -> impl Responder {
    let seasons: Vec<i32> = q.seasons.clone().unwrap_or_else(|| "2024,2025,2026".into())
        .split(',').filter_map(|s| s.trim().parse().ok()).collect();
    if seasons.is_empty() {
        return bad_request("no seasons");
    }
    let mut report = Vec::new();
    for season in seasons {
        let url = PBP_URL.replace("{season}", &season.to_string());
        let t = Instant::now();
        let bytes = match state.http.get(&url).send().await.and_then(|r| r.error_for_status()) {
            Ok(r) => match r.bytes().await { Ok(b) => b, Err(e) => return internal(format!("{url}: {e}")) },
            Err(e) => return internal(format!("{url}: {e}")),
        };
        let parsed = web::block(move || {
            let mut dec = flate2::read::GzDecoder::new(&bytes[..]);
            let mut text = Vec::new();
            dec.read_to_end(&mut text).map_err(|e| format!("gunzip: {e}"))?;
            parse_pbp(&text)
        }).await;
        let rows = match parsed {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return internal(e),
            Err(e) => return internal(e),
        };
        if let Err(e) = state.db.execute("DELETE FROM nfl_plays WHERE season = $1", &[&season]).await {
            return internal(dberr(&e));
        }
        let n = match store_plays(&state.db, &rows).await { Ok(n) => n, Err(e) => return internal(e) };
        let games: std::collections::HashSet<&str> = rows.iter().map(|r| r.game_id.as_str()).collect();
        let last = rows.iter().map(|r| r.date).max();
        report.push(json!({ "season": season, "plays": n, "games": games.len(), "last_game_date": last, "secs": t.elapsed().as_secs() }));
    }
    HttpResponse::Ok().json(json!({ "source": "nflverse play-by-play (CC-BY 4.0)", "seasons": report }))
}

const ROSTER_URL: &str = "https://github.com/nflverse/nflverse-data/releases/download/weekly_rosters/roster_weekly_{season}.csv";
const INJURY_URL: &str = "https://github.com/nflverse/nflverse-data/releases/download/injuries/injuries_{season}.csv";

fn csv_rows(text: &str, cols: &[&str]) -> Result<Vec<Vec<String>>, String> {
    let mut rdr = csv::ReaderBuilder::new().from_reader(text.as_bytes());
    let h = rdr.headers().map_err(|e| e.to_string())?.clone();
    let idx: Vec<usize> = cols.iter().map(|c| h.iter().position(|x| x == *c).ok_or(format!("csv missing column {c}"))).collect::<Result<_, _>>()?;
    let mut out = Vec::new();
    for r in rdr.records() {
        let r = r.map_err(|e| e.to_string())?;
        out.push(idx.iter().map(|i| r.get(*i).unwrap_or("").to_string()).collect());
    }
    Ok(out)
}

async fn insert_rows(db: &tokio_postgres::Client, table: &str, cols: &str, ncol: usize, rows: &[Vec<Option<String>>], ints: &[usize]) -> Result<u64, String> {
    let mut n = 0;
    for chunk in rows.chunks(500) {
        let mut sql = format!("INSERT INTO {table} ({cols}) VALUES ");
        let mut params: Vec<Box<dyn ToSql + Sync + Send>> = Vec::new();
        for (i, r) in chunk.iter().enumerate() {
            if i > 0 { sql.push(','); }
            sql.push('(');
            for k in 0..ncol {
                if k > 0 { sql.push(','); }
                sql.push_str(&format!("${}", i * ncol + k + 1));
                if ints.contains(&k) {
                    params.push(Box::new(r[k].as_deref().and_then(|x| x.parse::<f64>().ok()).map(|x| x as i32)));
                } else {
                    params.push(Box::new(r[k].clone()));
                }
            }
            sql.push(')');
        }
        sql.push_str(" ON CONFLICT DO NOTHING");
        let refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|b| b.as_ref() as &(dyn ToSql + Sync)).collect();
        n += db.execute(sql.as_str(), &refs).await.map_err(|e| format!("insert {table}: {}", dberr(&e)))?;
    }
    Ok(n)
}

/// POST /ingest/nfl/availability?seasons=2024,2025,2026 — weekly roster status + injury reports.
pub async fn ingest_availability(state: web::Data<State>, q: web::Query<IngestQuery>) -> impl Responder {
    let seasons: Vec<i32> = q.seasons.clone().unwrap_or_else(|| "2024,2025,2026".into())
        .split(',').filter_map(|s| s.trim().parse().ok()).collect();
    let mut report = Vec::new();
    for season in seasons {
        let fetch = |u: String| { let http = state.http.clone(); async move {
            http.get(&u).send().await.and_then(|r| r.error_for_status()).map_err(|e| format!("{u}: {e}"))?
                .text().await.map_err(|e| format!("{u}: {e}"))
        } };
        let roster = match fetch(ROSTER_URL.replace("{season}", &season.to_string())).await { Ok(t) => t, Err(e) => return internal(e) };
        let rr = match csv_rows(&roster, &["season", "week", "team", "gsis_id", "full_name", "position", "depth_chart_position", "status", "status_description_abbr"]) {
            Ok(r) => r, Err(e) => return internal(e) };
        let rr: Vec<Vec<Option<String>>> = rr.into_iter().filter(|r| !r[3].is_empty() && r[3] != "NA")
            .map(|r| r.into_iter().map(|x| if x.is_empty() || x == "NA" { None } else { Some(x) }).collect()).collect();
        let _ = state.db.execute("DELETE FROM nfl_roster_weekly WHERE season = $1", &[&season]).await;
        let n_roster = match insert_rows(&state.db, "nfl_roster_weekly", "season, week, team, gsis_id, full_name, position, depth_chart_position, status, status_detail", 9, &rr, &[0, 1]).await {
            Ok(n) => n, Err(e) => return internal(e) };
        let inj = match fetch(INJURY_URL.replace("{season}", &season.to_string())).await { Ok(t) => t, Err(e) => return internal(e) };
        let ir = match csv_rows(&inj, &["season", "week", "team", "gsis_id", "full_name", "position", "report_status", "practice_status", "report_primary_injury"]) {
            Ok(r) => r, Err(e) => return internal(e) };
        let ir: Vec<Vec<Option<String>>> = ir.into_iter().filter(|r| !r[3].is_empty() && r[3] != "NA")
            .map(|r| r.into_iter().map(|x| if x.is_empty() || x == "NA" { None } else { Some(x) }).collect()).collect();
        let _ = state.db.execute("DELETE FROM nfl_injuries WHERE season = $1", &[&season]).await;
        let n_inj = match insert_rows(&state.db, "nfl_injuries", "season, week, team, gsis_id, full_name, position, report_status, practice_status, injury", 9, &ir, &[0, 1]).await {
            Ok(n) => n, Err(e) => return internal(e) };
        let last_week: Option<i32> = state.db.query_one("SELECT max(week) FROM nfl_injuries WHERE season = $1", &[&season]).await.ok().and_then(|r| r.get(0));
        report.push(json!({ "season": season, "roster_rows": n_roster, "injury_rows": n_inj, "injury_report_through_week": last_week }));
    }
    HttpResponse::Ok().json(json!({ "source": "nflverse weekly_rosters + injuries", "seasons": report }))
}

/// P(plays | injury designation) for skill positions, from seasons before
/// `season`: "played" = recorded a dropback, carry or target that week. Divided
/// by the same rate for players listed without a game designation, so players
/// who were active but never touched the ball do not bias it.
async fn fit_availability(db: &tokio_postgres::Client, season: i32) -> Result<Option<Value>, String> {
    let rows = db.query(
        "WITH inj AS (SELECT season, week, team, gsis_id, COALESCE(NULLIF(report_status, ''), 'None') rs FROM nfl_injuries \
                      WHERE season < $1 AND position IN ('QB','RB','WR','TE','FB')), \
              gm AS (SELECT DISTINCT season, week, posteam team FROM nfl_plays WHERE season < $1 AND posteam IS NOT NULL), \
              pl AS (SELECT DISTINCT season, week, posteam team, pid FROM ( \
                       SELECT season, week, posteam, passer_id pid FROM nfl_plays WHERE season < $1 UNION ALL \
                       SELECT season, week, posteam, rusher_id FROM nfl_plays WHERE season < $1 UNION ALL \
                       SELECT season, week, posteam, receiver_id FROM nfl_plays WHERE season < $1) x WHERE pid IS NOT NULL) \
         SELECT inj.rs, count(*)::float8, avg((pl.pid IS NOT NULL)::int)::float8 FROM inj JOIN gm USING (season, week, team) \
         LEFT JOIN pl ON pl.season = inj.season AND pl.week = inj.week AND pl.team = inj.team AND pl.pid = inj.gsis_id GROUP BY 1",
        &[&season]).await.map_err(|e| dberr(&e))?;
    let get = |k: &str| rows.iter().find(|r| r.get::<_, String>(0) == k).map(|r| (r.get::<_, f64>(1), r.get::<_, f64>(2)));
    let (Some((nn, base)), Some((nq, q)), Some((nd, d))) = (get("None"), get("Questionable"), get("Doubtful")) else { return Ok(None) };
    if base <= 0.0 { return Ok(None); }
    // add-one smoothing: 0 of 105 Doubtful players touching the ball is not "never"
    let sm = |rate: f64, n: f64| (rate * n + 1.0) / (n + 2.0);
    let rates = AvailRates { questionable: (sm(q, nq) / sm(base, nn)).clamp(0.0, 1.0), doubtful: (sm(d, nd) / sm(base, nn)).clamp(0.0, 1.0) };
    Ok(Some(json!({
        "rates": rates, "source": format!("injury reports before {season}"),
        "raw": { "no_designation": { "n": nn, "touched_ball": r3(base) }, "questionable": { "n": nq, "touched_ball": r3(q) }, "doubtful": { "n": nd, "touched_ball": r3(d) } },
    })))
}

async fn load_plays(db: &tokio_postgres::Client, where_sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<Vec<P>, String> {
    let sql = format!("SELECT {COLS} FROM nfl_plays WHERE {where_sql} ORDER BY game_date, game_id, play_id");
    db.query(sql.as_str(), params).await.map(|rows| rows.iter().map(from_row).collect()).map_err(|e| dberr(&e))
}

// ---------------------------------------------------------------------------
// League fit (pure)
// ---------------------------------------------------------------------------

fn cap(mut v: Vec<i32>, n: usize) -> Vec<i32> {
    if v.len() > n {
        let step = v.len() as f64 / n as f64;
        v = (0..n).map(|i| v[(i as f64 * step) as usize]).collect();
    }
    v
}

/// Index of the next scrimmage snap after each play in the same game.
fn next_snaps(rows: &[P]) -> Vec<Option<usize>> {
    let mut out = vec![None; rows.len()];
    let mut nxt: Option<usize> = None;
    for i in (0..rows.len()).rev() {
        if i + 1 < rows.len() && rows[i + 1].game_id != rows[i].game_id {
            nxt = None;
        }
        out[i] = nxt.filter(|j| rows[*j].game_id == rows[i].game_id);
        if rows[i].snap() {
            nxt = Some(i);
        }
    }
    out
}

/// Method-of-moments beta-binomial strength over units (successes, trials).
fn kappa(units: &[(f64, f64)], min_trials: f64, default: f64) -> f64 {
    crate::mlb_sim::mom_kappa(units, min_trials).unwrap_or(default).clamp(5.0, 5000.0)
}

/// Empirical-Bayes mixing weight for means: sigma^2 / tau^2 (prior sample size).
fn mean_kappa(groups: &[Vec<i32>], min_n: usize, default: f64) -> f64 {
    let g: Vec<&Vec<i32>> = groups.iter().filter(|v| v.len() >= min_n).collect();
    if g.len() < 8 {
        return default;
    }
    let means: Vec<f64> = g.iter().map(|v| v.iter().map(|x| *x as f64).sum::<f64>() / v.len() as f64).collect();
    let within: f64 = g.iter().zip(&means).map(|(v, m)| v.iter().map(|x| (*x as f64 - m).powi(2)).sum::<f64>()).sum::<f64>()
        / g.iter().map(|v| v.len() as f64 - 1.0).sum::<f64>();
    let gm = means.iter().sum::<f64>() / means.len() as f64;
    let var_means = means.iter().map(|m| (m - gm).powi(2)).sum::<f64>() / (means.len() as f64 - 1.0);
    let samp = g.iter().map(|v| within / v.len() as f64).sum::<f64>() / g.len() as f64;
    let tau2 = var_means - samp;
    if tau2 <= 1e-9 { 5000.0 } else { (within / tau2).clamp(5.0, 5000.0) }
}

pub fn fit_league(rows: &[P], season: i32) -> League {
    let next = next_snaps(rows);
    let mut pass_cells = vec![[0.0; 2]; sim::PASS_CELLS];
    let mut pass_marg = vec![[0.0; 2]; 15];
    let mut fourth = vec![[0.0; 3]; sim::FOURTH_CELLS];
    let (mut db, mut sacks, mut scr, mut att, mut ints, mut cmps) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
    let mut cmp_yards = vec![Vec::new(); sim::GAIN_CELLS];
    let mut run_yards = vec![Vec::new(); sim::GAIN_CELLS];
    // by down-and-distance group: [dropbacks, sacks, scrambles, attempts, ints, completions]
    let mut ddc = [[0.0f64; 6]; sim::DD];
    let (mut scramble_yards, mut sack_yards) = (Vec::new(), Vec::new());
    let (mut cmp_sum, mut run_sum, mut run_n) = (0.0, 0.0, 0.0);
    let (mut carries, mut fum_run, mut fum_rec) = (0.0, 0.0, 0.0);
    let mut oob_run = [[0.0; 2]; 3];
    let mut oob_cmp = [[0.0; 2]; 3];
    let mut elapsed = vec![Vec::new(); sim::CLOCK_CLASSES];
    let mut kickoff_start = Vec::new();
    let mut punt_start = vec![Vec::new(); 10];
    let (mut int_ret, mut fum_ret) = (Vec::new(), Vec::new());
    let (mut int_n, mut int_tds, mut fum_n, mut fum_tds) = (0.0, 0.0, 0.0, 0.0);
    let mut fg: Vec<(f64, f64)> = Vec::new();
    let (mut xp_n, mut xp_ok) = (0.0, 0.0);
    let mut try_cells = vec![[0.0; 2]; 7]; // [two-point tries, all tries]
    let (mut two_n, mut two_ok) = (0.0, 0.0);
    let mut loc_pass = [1.0; 3];
    let mut loc_run = [1.0; 3];
    // per-unit tallies for pooling strengths
    let mut qb: HashMap<&str, [f64; 6]> = HashMap::new(); // dropbacks, sacks, scrambles, att, int, cmp
    let mut rec: HashMap<&str, (f64, f64, Vec<i32>)> = HashMap::new(); // targets, catches, yards
    let mut rush: HashMap<&str, Vec<i32>> = HashMap::new();
    let mut def: HashMap<&str, [f64; 6]> = HashMap::new(); // dropbacks, sacks, att, int, cmp, _
    let mut def_ypc: HashMap<&str, Vec<i32>> = HashMap::new();
    let mut team_pr: HashMap<(&str, &str), [f64; 3]> = HashMap::new(); // (team, game) -> actual, expected, var

    // home vs away offence: [completions, completion yards, carries, carry yards, dropbacks, sacks, attempts, ints]
    let mut hfa = [[0.0f64; 8]; 2];
    let (mut n_scrim, mut pen_null, mut pen_live) = (0.0, Vec::new(), Vec::new());
    let (mut sack_n, mut sack_fum) = (0.0, 0.0);
    let loc_i = |l: &Option<String>| match l.as_deref() { Some("left") => Some(0), Some("middle") => Some(1), Some("right") => Some(2), _ => None };

    // first pass: situational pass cells (needed for PROE expectations)
    for p in rows {
        if !p.scrimmage() { continue; }
        let (Some(d), Some(t), Some(q)) = (p.down, p.togo, p.qsr) else { continue };
        if d <= 3 {
            let c = sim::pass_cell(d, t, p.diff.unwrap_or(0), p.qtr, q);
            pass_cells[c][0] += p.dropback() as i32 as f64;
            pass_cells[c][1] += 1.0;
            let m = sim::pass_marg(d, t);
            pass_marg[m][0] += p.dropback() as i32 as f64;
            pass_marg[m][1] += 1.0;
        }
    }
    let base_rate = |d: i32, t: i32, diff: i32, qtr: i32, q: i32| {
        let [a, n] = pass_cells[sim::pass_cell(d, t, diff, qtr, q)];
        let [am, nm] = pass_marg[sim::pass_marg(d, t)];
        let prior = if nm > 0.0 { am / nm } else { 0.58 };
        (a + 25.0 * prior) / (n + 25.0)
    };

    for (i, p) in rows.iter().enumerate() {
        let yl = p.yl.unwrap_or(0);
        let qsr = p.qsr.unwrap_or(0);
        let diff = p.diff.unwrap_or(0);
        let sit = sim::situation(p.qtr, qsr, diff);
        let nxt = next[i].map(|j| &rows[j]);
        // ---- clock
        if (p.snap() || matches!(p.ptype.as_str(), "kickoff" | "punt" | "field_goal")) && !p.penalty {
            // time to the next snap or kick (penalty / timeout rows in between are
            // part of the real clock the simulator does not model separately)
            let nxt_live = rows[i + 1..].iter().take_while(|n| n.game_id == p.game_id)
                .find(|n| n.snap() || matches!(n.ptype.as_str(), "kickoff" | "punt" | "field_goal" | "extra_point"));
            if let Some(n) = nxt_live.filter(|n| n.qtr == p.qtr) {
                if let (Some(a), Some(b)) = (p.qsr, n.qsr) {
                    let e = a - b;
                    if (0..=60).contains(&e) {
                        let class = if p.snap() {
                            let outcome = if p.td || p.int || p.fumble_lost || p.ptype == "qb_spike" {
                                sim::OTHER_STOP
                            } else if p.ptype == "pass" && !p.complete && !p.sack {
                                sim::INCOMPLETE
                            } else if p.oob {
                                sim::OOB
                            } else {
                                sim::RUN_IN
                            };
                            sim::clock_class(sit, outcome)
                        } else { sim::KICK_CLASS };
                        elapsed[class].push(e);
                    }
                }
            }
        }
        // ---- 4th down decisions
        if p.down == Some(4) && (p.scrimmage() || p.ptype == "punt" || p.ptype == "field_goal") && !p.penalty {
            let c = sim::fourth_cell(yl, p.togo.unwrap_or(1), diff, p.qtr);
            let k = match p.ptype.as_str() { "punt" => 1, "field_goal" => 2, _ => 0 };
            fourth[c][k] += 1.0;
        }
        // ---- penalties: nullified snaps (no_play) and live-ball fouls after a play
        let pen_sample = |p: &P| p.penalty_yards.map(|y| [(p.penalty_team.is_some() && p.penalty_team == p.posteam) as i32, y.abs(), p.first_down_penalty as i32]);
        if p.ptype == "no_play" && p.penalty && p.posteam.is_some() && p.down.is_some() && p.penalty_team.is_some() {
            if let Some(x) = pen_sample(p) { pen_null.push(x); }
        }
        if p.scrimmage() {
            n_scrim += 1.0;
            if p.penalty && !p.td && !p.int && !p.fumble_lost && p.penalty_team.is_some() {
                if let Some(x) = pen_sample(p) { pen_live.push(x); }
            }
            if p.sack { sack_n += 1.0; sack_fum += p.fumble_lost as i32 as f64; }
        }
        // ---- scrimmage outcomes
        if p.scrimmage() {
            let off = p.posteam.as_deref().unwrap_or("");
            let hx = (p.posteam.as_deref() == Some(p.home.as_str())) as usize;
            let g = sim::dd_group(p.down.unwrap_or(1), p.togo.unwrap_or(10));
            if p.dropback() {
                ddc[g][0] += 1.0;
                ddc[g][1] += p.sack as i32 as f64;
                ddc[g][2] += (p.scramble && !p.sack) as i32 as f64;
            }
            if p.attempt() {
                ddc[g][3] += 1.0;
                ddc[g][4] += p.int as i32 as f64;
                ddc[g][5] += (p.complete && !p.int) as i32 as f64;
            }
            if p.dropback() { hfa[hx][4] += 1.0; hfa[hx][5] += p.sack as i32 as f64; }
            if p.attempt() {
                hfa[hx][6] += 1.0;
                hfa[hx][7] += p.int as i32 as f64;
                if p.complete && !p.int { hfa[hx][0] += 1.0; hfa[hx][1] += p.yards as f64; }
            }
            if p.designed_run() { hfa[hx][2] += 1.0; hfa[hx][3] += p.yards as f64; }
            let dteam = p.defteam.as_deref().unwrap_or("");
            if p.down.unwrap_or(4) <= 3 {
                if let (Some(d), Some(t)) = (p.down, p.togo) {
                    let e = base_rate(d, t, diff, p.qtr, qsr);
                    let tp = team_pr.entry((off, p.game_id.as_str())).or_default();
                    tp[0] += p.dropback() as i32 as f64;
                    tp[1] += e;
                    tp[2] += e * (1.0 - e);
                }
            }
            if p.dropback() {
                db += 1.0;
                let pid = p.passer.as_ref().or(p.rusher.as_ref()).map(|x| x.0.as_str()).unwrap_or("");
                let u = qb.entry(pid).or_default();
                u[0] += 1.0;
                let dd = def.entry(dteam).or_default();
                dd[0] += 1.0;
                if p.sack {
                    sacks += 1.0;
                    u[1] += 1.0;
                    dd[1] += 1.0;
                    sack_yards.push((-p.yards).max(0));
                } else if p.scramble {
                    scr += 1.0;
                    u[2] += 1.0;
                    scramble_yards.push(p.yards);
                }
            }
            if p.attempt() {
                att += 1.0;
                let pid = p.passer.as_ref().map(|x| x.0.as_str()).unwrap_or("");
                let u = qb.entry(pid).or_default();
                u[3] += 1.0;
                let dd = def.entry(dteam).or_default();
                dd[2] += 1.0;
                if let Some(l) = loc_i(&p.location) { loc_pass[l] += 1.0; }
                if p.int {
                    ints += 1.0;
                    u[4] += 1.0;
                    dd[3] += 1.0;
                } else if p.complete {
                    cmps += 1.0;
                    u[5] += 1.0;
                    dd[4] += 1.0;
                    cmp_yards[sim::gain_cell(yl, p.down.unwrap_or(1), p.togo.unwrap_or(10))].push(p.yards);
                    cmp_sum += p.yards as f64;
                    fum_rec += p.fumble_lost as i32 as f64;
                    let s = sim::situation(p.qtr, qsr, diff);
                    oob_cmp[s][0] += p.oob as i32 as f64;
                    oob_cmp[s][1] += 1.0;
                }
                if let Some((rid, _)) = &p.receiver {
                    let r = rec.entry(rid.as_str()).or_insert((0.0, 0.0, Vec::new()));
                    r.0 += 1.0;
                    if p.complete && !p.int {
                        r.1 += 1.0;
                        r.2.push(p.yards);
                    }
                }
            }
            if p.ptype == "run" {
                carries += 1.0;
                fum_run += p.fumble_lost as i32 as f64;
                oob_run[sit][0] += p.oob as i32 as f64;
                oob_run[sit][1] += 1.0;
            }
            if p.designed_run() {
                run_yards[sim::gain_cell(yl, p.down.unwrap_or(1), p.togo.unwrap_or(10))].push(p.yards);
                run_sum += p.yards as f64;
                run_n += 1.0;
                if let Some(l) = loc_i(&p.location) { loc_run[l] += 1.0; }
                if let Some((rid, _)) = &p.rusher {
                    rush.entry(rid.as_str()).or_default().push(p.yards);
                }
                def_ypc.entry(dteam).or_default().push(p.yards);
            }
            // turnovers: next snap by the other team
            if (p.int || p.fumble_lost) && !p.penalty {
                let other = p.defteam.clone();
                let is_td = p.td && p.td_team == other;
                if p.int { int_n += 1.0; int_tds += is_td as i32 as f64; } else { fum_n += 1.0; fum_tds += is_td as i32 as f64; }
                if !is_td {
                    if let Some(n) = nxt.filter(|n| n.posteam == other) {
                        let spot = if p.int { yl } else { yl - p.yards };
                        let delta = n.yl.unwrap_or(0) - (100 - spot);
                        if p.int { int_ret.push(delta) } else { fum_ret.push(delta) }
                    }
                }
            }
        }
        // ---- kicks
        match p.ptype.as_str() {
            "kickoff" => {
                let recv = p.posteam.clone();
                if p.td && p.td_team.is_some() && p.td_team == recv {
                    kickoff_start.push(0);
                } else if let Some(n) = nxt.filter(|n| n.posteam == recv && !p.td) {
                    kickoff_start.push(n.yl.unwrap_or(70));
                }
            }
            "punt" if !p.penalty => {
                let recv = p.defteam.clone();
                let b = ((yl.clamp(1, 99) - 1) / 10) as usize;
                if p.td && p.td_team.is_some() && p.td_team == recv {
                    punt_start[b].push(0);
                } else if let Some(n) = nxt.filter(|n| n.posteam == recv && !p.td) {
                    punt_start[b].push(n.yl.unwrap_or(80));
                }
            }
            "field_goal" => {
                if let (Some(d), Some(r)) = (p.kick_distance, &p.fg_result) {
                    fg.push((d as f64, (r == "made") as i32 as f64));
                }
            }
            "extra_point" => {
                xp_n += 1.0;
                xp_ok += (p.xp_result.as_deref() == Some("good")) as i32 as f64;
                try_cells[sim::diff_b(diff)][1] += 1.0;
            }
            _ => {}
        }
        if p.two_point && (p.ptype == "pass" || p.ptype == "run") {
            two_n += 1.0;
            two_ok += (p.two_pt_result.as_deref() == Some("success")) as i32 as f64;
            try_cells[sim::diff_b(diff)][0] += 1.0;
            try_cells[sim::diff_b(diff)][1] += 1.0;
        }
    }

    // FG logistic by Newton's method on (a, b)
    let (mut a, mut b) = (5.0, -0.1);
    for _ in 0..50 {
        let (mut g0, mut g1, mut h00, mut h01, mut h11) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for (x, y) in &fg {
            let p = 1.0 / (1.0 + (-(a + b * x)).exp());
            g0 += y - p;
            g1 += (y - p) * x;
            let w = p * (1.0 - p);
            h00 += w;
            h01 += w * x;
            h11 += w * x * x;
        }
        let det = h00 * h11 - h01 * h01;
        if det.abs() < 1e-12 { break; }
        let da = (h11 * g0 - h01 * g1) / det;
        let dbb = (h00 * g1 - h01 * g0) / det;
        a += da;
        b += dbb;
        if da.abs() + dbb.abs() < 1e-9 { break; }
    }

    let qb_units = |k: usize, nk: usize| qb.values().map(|u| (u[k], u[nk])).collect::<Vec<_>>();
    let def_units = |k: usize, nk: usize| def.values().map(|u| (u[k], u[nk])).collect::<Vec<_>>();
    let rec_units: Vec<(f64, f64)> = rec.values().map(|r| (r.1, r.0)).collect();
    // pass rate over expected, team-game units: tau^2 between teams vs binomial noise
    let mut by_team: HashMap<&str, [f64; 3]> = HashMap::new();
    for ((t, _), v) in &team_pr {
        let e = by_team.entry(t).or_default();
        for k in 0..3 { e[k] += v[k]; }
    }
    let proe_kappa = {
        let v: Vec<(f64, f64, f64)> = by_team.values().filter(|v| v[2] > 50.0).map(|v| (v[0], v[1], v[2])).collect();
        if v.len() >= 10 {
            let n_mean = v.iter().map(|x| x.2).sum::<f64>() / v.len() as f64;
            let d: Vec<f64> = v.iter().map(|x| (x.0 - x.1) / x.2).collect();
            let dm = d.iter().sum::<f64>() / d.len() as f64;
            let var = d.iter().map(|x| (x - dm).powi(2)).sum::<f64>() / (d.len() as f64 - 1.0);
            let noise = v.iter().map(|x| 1.0 / x.2).sum::<f64>() / v.len() as f64;
            let tau2 = var - noise;
            let _ = n_mean;
            if tau2 > 1e-9 { (1.0 / tau2).clamp(20.0, 5000.0) } else { 5000.0 }
        } else { 300.0 }
    };
    // thin gain cells (e.g. 1-2 yards out on 2nd-and-long) borrow the field band's pooled gains
    let fill = |cells: Vec<Vec<i32>>| -> Vec<Vec<i32>> {
        let pooled: Vec<Vec<i32>> = (0..sim::ZONES).map(|z| (0..sim::DD).flat_map(|g| cells[z * sim::DD + g].clone()).collect()).collect();
        cells.into_iter().enumerate().map(|(c, v)| if v.len() >= 40 { cap(v, 1500) } else { cap(pooled[c / sim::DD].clone(), 1500) }).collect()
    };
    let cmp_yards = fill(cmp_yards);
    let run_yards = fill(run_yards);
    let rec_groups: Vec<Vec<i32>> = rec.values().map(|r| r.2.clone()).collect();
    let rush_groups: Vec<Vec<i32>> = rush.values().cloned().collect();
    let def_groups: Vec<Vec<i32>> = def_ypc.values().cloned().collect();
    let kap = sim::Kappas {
        sack: kappa(&qb_units(1, 0), 100.0, 200.0),
        scramble: kappa(&qb_units(2, 0), 100.0, 200.0),
        int: kappa(&qb_units(4, 3), 100.0, 400.0),
        cmp_qb: kappa(&qb_units(5, 3), 100.0, 150.0),
        catch: kappa(&rec_units, 20.0, 60.0),
        def_sack: kappa(&def_units(1, 0), 100.0, 400.0),
        def_int: kappa(&def_units(3, 2), 100.0, 400.0),
        def_cmp: kappa(&def_units(4, 2), 100.0, 400.0),
        ypc: mean_kappa(&rush_groups, 30, 80.0),
        ypr: mean_kappa(&rec_groups, 15, 40.0),
        def_yards: mean_kappa(&def_groups, 100, 500.0),
        proe: proe_kappa,
    };
    let rate = |a: f64, n: f64, d: f64| if n > 0.0 { a / n } else { d };
    let ratio = |h: f64, a: f64| if h > 0.0 && a > 0.0 { (h / a).sqrt() } else { 1.0 };
    let odds_r = |hx: f64, hn: f64, ax: f64, an: f64| {
        let (ph, pa) = (rate(hx, hn, 0.0), rate(ax, an, 0.0));
        if ph > 0.0 && pa > 0.0 && ph < 1.0 && pa < 1.0 { ((ph / (1.0 - ph)) / (pa / (1.0 - pa))).sqrt() } else { 1.0 }
    };
    let [aw, hm] = hfa;
    let home = sim::HomeField {
        ypr: ratio(rate(hm[1], hm[0], 0.0), rate(aw[1], aw[0], 0.0)),
        ypc: ratio(rate(hm[3], hm[2], 0.0), rate(aw[3], aw[2], 0.0)),
        sack: odds_r(hm[5], hm[4], aw[5], aw[4]),
        int: odds_r(hm[7], hm[6], aw[7], aw[6]),
        cmp: odds_r(hm[0], hm[6], aw[0], aw[6]),
    };
    let norm3 = |v: [f64; 3]| { let s: f64 = v.iter().sum(); [v[0] / s, v[1] / s, v[2] / s] };
    League {
        season,
        pass_cells,
        pass_marg,
        fourth_cells: fourth,
        sack: rate(sacks, db, 0.07),
        scramble: rate(scr, db, 0.04),
        int: rate(ints, att, 0.023),
        cmp: rate(cmps, att, 0.65),
        kappa: kap,
        cmp_yards,
        run_yards,
        scramble_yards: cap(scramble_yards, 2000),
        sack_yards: cap(sack_yards, 2000),
        fumble_run: rate(fum_run, carries, 0.008),
        fumble_rec: rate(fum_rec, cmps, 0.006),
        oob_run: [0, 1, 2].map(|s| rate(oob_run[s][0], oob_run[s][1], 0.05)),
        oob_cmp: [0, 1, 2].map(|s| rate(oob_cmp[s][0], oob_cmp[s][1], 0.15)),
        elapsed: elapsed.into_iter().map(|v| cap(v, 4000)).collect(),
        kickoff_start: cap(kickoff_start, 3000),
        punt_start: punt_start.into_iter().map(|v| cap(v, 1500)).collect(),
        int_return: cap(int_ret, 1500),
        int_td: rate(int_tds, int_n, 0.05),
        fum_return: cap(fum_ret, 1500),
        fum_td: rate(fum_tds, fum_n, 0.04),
        fg_a: a,
        fg_b: b,
        xp: rate(xp_ok, xp_n, 0.95),
        two_pt_go: try_cells.iter().map(|c| rate(c[0], c[1], 0.1)).collect(),
        two_pt_success: rate(two_ok, two_n, 0.48),
        loc_pass: norm3(loc_pass),
        loc_run: norm3(loc_run),
        home,
        pen_rate: rate(pen_null.len() as f64, n_scrim + pen_null.len() as f64, 0.0),
        pen_live_rate: rate(pen_live.len() as f64, n_scrim, 0.0),
        pen: pen_null,
        pen_live,
        fumble_sack: rate(sack_fum, sack_n, 0.0),
        cmp_mean: rate(cmp_sum, cmps, 11.0),
        run_mean: rate(run_sum, run_n, 4.3),
        dd_sack: dd_factor(&ddc, 1, 0),
        dd_scramble: dd_factor(&ddc, 2, 0),
        dd_int: dd_factor(&ddc, 4, 3),
        dd_cmp: dd_factor(&ddc, 5, 3),
    }
}

/// POST /fit/nfl/sim — fit league parameters for every stored season.
pub async fn fit(state: web::Data<State>) -> impl Responder {
    let seasons: Vec<i32> = match state.db.query("SELECT DISTINCT season FROM nfl_plays ORDER BY 1", &[]).await {
        Ok(r) => r.iter().map(|x| x.get(0)).collect(),
        Err(e) => return internal(dberr(&e)),
    };
    let mut out = Vec::new();
    for season in seasons {
        let rows = match load_plays(&state.db, "season = $1", &[&season]).await { Ok(r) => r, Err(e) => return internal(e) };
        let n_plays = rows.len();
        let n_games = rows.iter().map(|r| r.game_id.as_str()).collect::<std::collections::HashSet<_>>().len();
        let lg = match web::block(move || fit_league(&rows, season)).await { Ok(l) => l, Err(e) => return internal(e) };
        let summary = json!({
            "season": season, "plays": n_plays, "games": n_games,
            "sack_rate": r4(lg.sack), "scramble_rate": r4(lg.scramble), "int_rate": r4(lg.int), "cmp_rate": r4(lg.cmp),
            "xp": r4(lg.xp), "two_pt_success": r4(lg.two_pt_success), "fg_50yd": r3(1.0 / (1.0 + (-(lg.fg_a + lg.fg_b * 50.0)).exp())),
            "fg_40yd": r3(1.0 / (1.0 + (-(lg.fg_a + lg.fg_b * 40.0)).exp())),
            "kickoff_mean_start": r3(lg.kickoff_start.iter().map(|x| *x as f64).sum::<f64>() / lg.kickoff_start.len().max(1) as f64),
            "kappa": serde_json::to_value(&lg.kappa).unwrap_or(Value::Null),
            "home_field": serde_json::to_value(&lg.home).unwrap_or(Value::Null),
            "dd_sack": lg.dd_sack.map(r3), "dd_cmp": lg.dd_cmp.map(r3), "dd_int": lg.dd_int.map(r3), "dd_scramble": lg.dd_scramble.map(r3),
            "nullifying_penalty_rate": r4(lg.pen_rate), "live_penalty_rate": r4(lg.pen_live_rate), "sack_fumble_rate": r4(lg.fumble_sack),
            "elapsed_mean_by_class": lg.elapsed.iter().map(|v| r3(v.iter().map(|x| *x as f64).sum::<f64>() / v.len().max(1) as f64)).collect::<Vec<_>>(),
        });
        let key = format!("nfl:sim:{season}");
        if let Err(e) = state.db.execute(
            "INSERT INTO sim_model_params (key, params, source) VALUES ($1, $2, $3) \
             ON CONFLICT (key) DO UPDATE SET params = EXCLUDED.params, source = EXCLUDED.source, fitted_at = now()",
            &[&key, &Json(serde_json::to_value(&lg).unwrap_or(Value::Null)), &format!("nflverse pbp {season}: {n_plays} plays, {n_games} games")],
        ).await {
            return internal(dberr(&e));
        }
        out.push(summary);
    }
    // availability rates for every season that has an injury report
    let mut avail = Vec::new();
    let seasons_inj: Vec<i32> = state.db.query("SELECT DISTINCT season FROM nfl_injuries ORDER BY 1", &[]).await
        .map(|r| r.iter().map(|x| x.get(0)).collect()).unwrap_or_default();
    for season in seasons_inj.iter().map(|s| s + 0).chain(seasons_inj.last().map(|s| s + 1)) {
        match fit_availability(&state.db, season).await {
            Ok(Some(v)) => {
                let key = format!("nfl:availability:{season}");
                let _ = state.db.execute(
                    "INSERT INTO sim_model_params (key, params, source) VALUES ($1, $2, 'injury report history') \
                     ON CONFLICT (key) DO UPDATE SET params = EXCLUDED.params, source = EXCLUDED.source, fitted_at = now()",
                    &[&key, &Json(v.clone())]).await;
                avail.push(json!({ "season": season, "fit": v }));
            }
            Ok(None) => {}
            Err(e) => return internal(e),
        }
    }
    HttpResponse::Ok().json(json!({ "model_version": MODEL_VERSION, "fitted": out, "availability": avail }))
}

async fn load_league(db: &tokio_postgres::Client, season: i32) -> Result<(League, String), String> {
    let key = format!("nfl:sim:{season}");
    let row = db.query_opt("SELECT params FROM sim_model_params WHERE key = $1", &[&key]).await.map_err(|e| dberr(&e))?
        .ok_or_else(|| format!("no fitted NFL simulator parameters for {season}; run POST /fit/nfl/sim"))?;
    let v: Json<Value> = row.get(0);
    serde_json::from_value(v.0).map(|l| (l, key)).map_err(|e| format!("params {season}: {e}"))
}

// ---------------------------------------------------------------------------
// Game inputs (team usage + efficiency before the game date)
// ---------------------------------------------------------------------------

pub struct GameRef {
    pub game_id: String,
    pub season: i32,
    pub week: i32,
    pub postseason: bool,
    pub date: NaiveDate,
    pub away: String,
    pub home: String,
    pub spread_line: Option<f64>,
    pub total_line: Option<f64>,
    pub away_score: Option<i32>,
    pub home_score: Option<i32>,
}

async fn game_ref(db: &tokio_postgres::Client, game_id: &str) -> Result<GameRef, String> {
    let r = db.query_opt(
        "SELECT game_id, season, game_type, game_date, away_team, home_team, spread_line, total_line, away_score, home_score, week \
         FROM sports_games WHERE sport = 'nfl' AND game_id = $1", &[&game_id]).await.map_err(|e| dberr(&e))?
        .ok_or_else(|| format!("unknown NFL game {game_id}"))?;
    let gt: Option<String> = r.get(2);
    Ok(GameRef {
        game_id: r.get(0), season: r.get::<_, Option<i32>>(1).unwrap_or(0),
        week: r.get::<_, Option<String>>(10).and_then(|w| w.parse().ok()).unwrap_or(0),
        postseason: !matches!(gt.as_deref(), None | Some("REG") | Some("")),
        date: r.get::<_, Option<NaiveDate>>(3).ok_or("game has no date")?,
        away: r.get::<_, Option<String>>(4).unwrap_or_default(), home: r.get::<_, Option<String>>(5).unwrap_or_default(),
        spread_line: r.get(6), total_line: r.get(7), away_score: r.get(8), home_score: r.get(9),
    })
}

pub struct Opts {
    pub share_halflife: f64,  // team games
    pub team_halflife: f64,   // team games, for PROE and defence
    pub away_qb: Option<String>,
    pub home_qb: Option<String>,
    /// alternate reality: players forced out / forced active (gsis ids)
    pub out: std::collections::HashSet<String>,
    pub force_in: std::collections::HashSet<String>,
    pub lineup_mode: LineupMode,
}

/// Which lineup the engine plays.
/// * Projected — usage only (players used in the team's last 3 games), no roster or injury report.
/// * Confirmed — this week's active roster + official injury report (default).
/// * Actual — who really played, from the game's own play-by-play (completed games only; hindsight).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum LineupMode { Projected, Confirmed, Actual }
impl LineupMode {
    pub fn parse(s: Option<&str>) -> LineupMode {
        match s { Some("projected") => LineupMode::Projected, Some("actual") => LineupMode::Actual, _ => LineupMode::Confirmed }
    }
    pub fn as_str(&self) -> &'static str {
        match self { LineupMode::Projected => "projected", LineupMode::Confirmed => "confirmed", LineupMode::Actual => "actual" }
    }
}

/// Who actually played for each team, from a completed game's play-by-play.
struct ActualGame {
    played: HashMap<String, std::collections::HashSet<String>>,
    qb: HashMap<String, String>,
    kicker: HashMap<String, String>,
    /// team -> (id, short name, dropbacks, carries, targets)
    usage: HashMap<String, Vec<(String, String, f64, f64, f64)>>,
}

async fn load_actual(db: &tokio_postgres::Client, game_id: &str) -> Result<Option<ActualGame>, String> {
    let plays = load_plays(db, "game_id = $1", &[&game_id]).await?;
    if plays.iter().filter(|p| p.scrimmage()).count() < 40 {
        return Ok(None);
    }
    let mut a = ActualGame { played: HashMap::new(), qb: HashMap::new(), kicker: HashMap::new(), usage: HashMap::new() };
    let mut u: HashMap<(String, String), (String, f64, f64, f64)> = HashMap::new();
    let mut kick: HashMap<(String, String), f64> = HashMap::new();
    for p in &plays {
        let Some(team) = p.posteam.clone() else { continue };
        if p.scrimmage() {
            if p.dropback() {
                if let Some((id, nm)) = p.passer.as_ref().or(p.rusher.as_ref()) { u.entry((team.clone(), id.clone())).or_insert((nm.clone(), 0.0, 0.0, 0.0)).1 += 1.0; }
            }
            if p.designed_run() {
                if let Some((id, nm)) = &p.rusher { u.entry((team.clone(), id.clone())).or_insert((nm.clone(), 0.0, 0.0, 0.0)).2 += 1.0; }
            }
            if p.attempt() {
                if let Some((id, nm)) = &p.receiver { u.entry((team.clone(), id.clone())).or_insert((nm.clone(), 0.0, 0.0, 0.0)).3 += 1.0; }
            }
        }
        if matches!(p.ptype.as_str(), "field_goal" | "extra_point") {
            if let Some((id, _)) = &p.kicker { *kick.entry((team.clone(), id.clone())).or_insert(0.0) += 1.0; }
        }
    }
    for ((team, id), (nm, db_, ca, tg)) in u {
        a.played.entry(team.clone()).or_default().insert(id.clone());
        a.usage.entry(team).or_default().push((id, nm, db_, ca, tg));
    }
    for (team, v) in a.usage.iter_mut() {
        v.sort_by(|x, y| (y.2 + y.3 + y.4).total_cmp(&(x.2 + x.3 + x.4)));
        if let Some(q) = v.iter().max_by(|x, y| x.2.total_cmp(&y.2)).filter(|q| q.2 > 0.0) { a.qb.insert(team.clone(), q.0.clone()); }
    }
    for ((team, id), n) in kick {
        let cur = a.kicker.get(&team).cloned();
        if cur.is_none() { a.kicker.insert(team, id); } else { let _ = n; }
    }
    Ok(Some(a))
}

/// Probability a player listed on the injury report plays, fitted from
/// earlier seasons (see `fit_availability`).
#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct AvailRates {
    pub questionable: f64,
    pub doubtful: f64,
}

/// Availability sources for one game: weekly roster status and injury report.
struct Avail {
    roster: HashMap<String, (String, String, String)>, // id -> (name, position, status) for both teams
    roster_team: HashMap<String, String>,              // id -> team on this week's roster
    teams_with_roster: std::collections::HashSet<String>,
    injury: HashMap<String, (String, String, String)>, // id -> (report_status, practice_status, injury)
    rates: AvailRates,
    rates_source: String,
}

impl Avail {
    /// Same play-probability rates, but no roster and no injury report.
    fn without_reports(&self) -> Avail {
        Avail { roster: HashMap::new(), roster_team: HashMap::new(), teams_with_roster: Default::default(), injury: HashMap::new(),
            rates: self.rates, rates_source: self.rates_source.clone() }
    }
}

async fn load_avail(db: &tokio_postgres::Client, gref: &GameRef) -> Result<Avail, String> {
    let teams = vec![gref.away.clone(), gref.home.clone()];
    let mut a = Avail { roster: HashMap::new(), roster_team: HashMap::new(), teams_with_roster: Default::default(),
        injury: HashMap::new(), rates: AvailRates { questionable: 0.8, doubtful: 0.25 }, rates_source: "default (not fitted)".into() };
    for r in db.query("SELECT gsis_id, full_name, COALESCE(position, ''), COALESCE(status, ''), team FROM nfl_roster_weekly \
                       WHERE season = $1 AND week = $2 AND team = ANY($3)", &[&gref.season, &gref.week, &teams]).await.map_err(|e| dberr(&e))? {
        let id: String = r.get(0);
        let team: String = r.get(4);
        a.teams_with_roster.insert(team.clone());
        a.roster_team.insert(id.clone(), team);
        a.roster.insert(id, (r.get(1), r.get(2), r.get(3)));
    }
    for r in db.query("SELECT gsis_id, COALESCE(report_status, ''), COALESCE(practice_status, ''), COALESCE(injury, '') FROM nfl_injuries \
                       WHERE season = $1 AND week = $2 AND team = ANY($3)", &[&gref.season, &gref.week, &teams]).await.map_err(|e| dberr(&e))? {
        a.injury.insert(r.get(0), (r.get(1), r.get(2), r.get(3)));
    }
    let key = format!("nfl:availability:{}", gref.season);
    if let Some(r) = db.query_opt("SELECT params FROM sim_model_params WHERE key = $1", &[&key]).await.map_err(|e| dberr(&e))? {
        let v: Json<Value> = r.get(0);
        if let Ok(rt) = serde_json::from_value::<AvailRates>(v.0["rates"].clone()) {
            a.rates = rt;
            a.rates_source = format!("{key} ({})", v.0["source"].as_str().unwrap_or("fitted"));
        }
    }
    Ok(a)
}

pub struct Built {
    pub game: GameInput,
    pub meta: Value,
    #[allow(dead_code)]
    pub params_key: String,
}

fn shrink(x: f64, n: f64, lg: f64, k: f64) -> Rate {
    Rate { mean: (x + k * lg) / (n + k), n: n + k }
}
fn loc3(v: [f64; 3], lg: [f64; 3]) -> [f64; 3] {
    let k = 10.0;
    let s: f64 = v.iter().sum::<f64>() + k;
    [(v[0] + k * lg[0]) / s, (v[1] + k * lg[1]) / s, (v[2] + k * lg[2]) / s]
}

/// Most recent `n` games (as offence) for a team, newest first.
fn team_games(plays: &[P], team: &str) -> Vec<String> {
    let mut seen: Vec<(NaiveDate, String)> = Vec::new();
    for p in plays.iter().rev() {
        if p.posteam.as_deref() == Some(team) && !seen.iter().any(|(_, g)| g == &p.game_id) {
            seen.push((p.date, p.game_id.clone()));
        }
    }
    seen.sort_by(|a, b| b.0.cmp(&a.0));
    seen.into_iter().map(|x| x.1).collect()
}

/// One skill-position candidate for a team's lineup.
#[derive(Clone)]
struct Cand {
    id: String,
    short: String,
    target_share: f64,
    carry_share: f64,
    dropbacks: f64,
    last_game_dropbacks: f64,
    p_active: f64,
    reason: String,
    roster_status: Option<String>,
    report: Option<String>,
    practice: Option<String>,
    injury: Option<String>,
}

struct Usage {
    qb: Option<(String, String)>,
    backup_qb: Option<(String, String)>,
    qb_p: f64,
    kicker: Option<(String, String)>,
    /// (id, short name, share, p_active)
    targets: Vec<(String, String, f64, f64)>,
    carries: Vec<(String, String, f64, f64)>,
    target_n: f64,
    carry_n: f64,
    proe: (f64, f64, f64, f64), // actual, expected, variance, weight
    games: usize,
    lineup: Vec<Cand>,
    lineup_source: String,
}

/// Lineup + usage for one team.
///
/// Candidates are players the team used in its last 16 games. A player's
/// share is his recency-weighted share of the team's targets (carries) in the
/// games HE played, so a player returning from injury keeps his normal role
/// and a missing player's work is spread over whoever is active.
/// Availability: scenario overrides > injury report Out > this week's roster
/// status (must be ACT) > fallback "used in the last 3 games"; Questionable /
/// Doubtful players play with the probability fitted from earlier seasons.
fn usage(plays: &[P], team: &str, league: &League, opts: &Opts, qb_override: Option<&String>, av: &Avail,
         actual: Option<&std::collections::HashSet<String>>) -> Usage {
    let games: Vec<String> = team_games(plays, team).into_iter().take(16).collect();
    let gi: HashMap<&str, usize> = games.iter().enumerate().map(|(i, g)| (g.as_str(), i)).collect();
    let w_share = |i: usize| 0.5f64.powf(i as f64 / opts.share_halflife);
    let w_team = |i: usize| 0.5f64.powf(i as f64 / opts.team_halflife);
    let ng = games.len();
    let mut tot_t = vec![0.0; ng];
    let mut tot_c = vec![0.0; ng];
    // id -> (short, targets per game, carries per game, dropbacks per game)
    let mut per: HashMap<String, (String, Vec<f64>, Vec<f64>, Vec<f64>)> = HashMap::new();
    let mut kick: HashMap<String, (String, f64)> = HashMap::new();
    let (mut a, mut e, mut v, mut wsum) = (0.0, 0.0, 0.0, 0.0);
    let entry = |per: &mut HashMap<String, (String, Vec<f64>, Vec<f64>, Vec<f64>)>, id: &str, nm: &str| {
        if !per.contains_key(id) { per.insert(id.to_string(), (nm.to_string(), vec![0.0; ng], vec![0.0; ng], vec![0.0; ng])); }
    };
    for p in plays {
        if p.posteam.as_deref() != Some(team) { continue; }
        let Some(&i) = gi.get(p.game_id.as_str()) else { continue };
        if p.attempt() {
            tot_t[i] += 1.0;
            if let Some((id, nm)) = &p.receiver { entry(&mut per, id, nm); per.get_mut(id).unwrap().1[i] += 1.0; }
        }
        if p.designed_run() {
            tot_c[i] += 1.0;
            if let Some((id, nm)) = &p.rusher { entry(&mut per, id, nm); per.get_mut(id).unwrap().2[i] += 1.0; }
        }
        if p.dropback() {
            if let Some((id, nm)) = p.passer.as_ref().or(p.rusher.as_ref()) { entry(&mut per, id, nm); per.get_mut(id).unwrap().3[i] += 1.0; }
        }
        if matches!(p.ptype.as_str(), "field_goal" | "extra_point") && i < 3 {
            if let Some((id, nm)) = &p.kicker {
                let t = kick.entry(id.clone()).or_insert((nm.clone(), 0.0));
                t.1 += w_share(i);
            }
        }
        if p.scrimmage() && p.down.unwrap_or(4) <= 3 {
            let (d, t, q) = (p.down.unwrap(), p.togo.unwrap_or(10), p.qsr.unwrap_or(0));
            let [x, n] = league.pass_cells[sim::pass_cell(d, t, p.diff.unwrap_or(0), p.qtr, q)];
            let [xm, nm] = league.pass_marg[sim::pass_marg(d, t)];
            let prior = if nm > 0.0 { xm / nm } else { 0.58 };
            let base = (x + 25.0 * prior) / (n + 25.0);
            let wt = w_team(i);
            a += wt * p.dropback() as i32 as f64;
            e += wt * base;
            v += wt * base * (1.0 - base);
            wsum += wt;
        }
    }
    let has_roster = av.teams_with_roster.contains(team);
    let mut cands: Vec<Cand> = Vec::new();
    for (id, (short, tg, cr, db)) in &per {
        let played: Vec<usize> = (0..ng).filter(|&g| tg[g] + cr[g] + db[g] > 0.0).collect();
        if played.is_empty() { continue; }
        // share of the team's work in the games he played, shrunk toward 0 by a
        // prior worth two average team games (one-game cameos stay small)
        let share = |x: &Vec<f64>, tot: &Vec<f64>| {
            let num: f64 = played.iter().map(|&g| w_share(g) * x[g]).sum();
            let den: f64 = played.iter().map(|&g| w_share(g) * tot[g]).sum();
            let per_game = tot.iter().sum::<f64>() / ng.max(1) as f64;
            if den > 0.0 { num / (den + 2.0 * per_game * w_share(0)) } else { 0.0 }
        };
        let roster = av.roster.get(id);
        let on_team = av.roster_team.get(id).map(|t| t == team).unwrap_or(false);
        let inj = av.injury.get(id);
        let report = inj.map(|x| x.0.clone()).filter(|x| !x.is_empty());
        let elsewhere = has_roster && !on_team;
        let (p_active, reason) = if let Some(act) = actual {
            if opts.out.contains(id) { (0.0, "scenario: ruled out".to_string()) }
            else if opts.force_in.contains(id) { (1.0, "scenario: plays".to_string()) }
            else if act.contains(id) { (1.0, "actual: played in this game".to_string()) }
            else { (0.0, "actual: no touch in this game".to_string()) }
        } else if elsewhere {
            (0.0, match av.roster_team.get(id) { Some(t) => format!("now on {t}"), None => "not on this week's active roster".into() })
        } else if opts.out.contains(id) {
            (0.0, "scenario: ruled out".to_string())
        } else if opts.force_in.contains(id) {
            (1.0, "scenario: plays".to_string())
        } else if report.as_deref() == Some("Out") {
            (0.0, "injury report: Out".to_string())
        } else if has_roster && !(on_team && roster.map(|r| r.2 == "ACT").unwrap_or(false)) {
            (0.0, match roster { Some(r) if on_team => format!("roster status {}", r.2), _ => "not on this week's active roster".into() })
        } else if !has_roster && !played.iter().any(|&g| g < 3) {
            (0.0, "not used in the team's last 3 games (no roster published)".into())
        } else {
            match report.as_deref() {
                Some("Questionable") => (av.rates.questionable, "injury report: Questionable".into()),
                Some("Doubtful") => (av.rates.doubtful, "injury report: Doubtful".into()),
                _ => (1.0, if has_roster { "active roster".into() } else { "used in last 3 games".into() }),
            }
        };
        cands.push(Cand {
            id: id.clone(), short: short.clone(), target_share: share(tg, &tot_t), carry_share: share(cr, &tot_c),
            dropbacks: db.iter().sum(), last_game_dropbacks: db.first().copied().unwrap_or(0.0), p_active, reason,
            roster_status: roster.map(|r| r.2.clone()), report, practice: inj.map(|x| x.1.clone()).filter(|x| !x.is_empty()),
            injury: inj.map(|x| x.2.clone()).filter(|x| !x.is_empty()),
        });
    }
    cands.sort_by(|a, b| (b.target_share + b.carry_share + b.dropbacks / 40.0).total_cmp(&(a.target_share + a.carry_share + a.dropbacks / 40.0)));
    // QB: override, else most dropbacks in the last game among available, then window dropbacks
    let mut qbs: Vec<&Cand> = cands.iter().filter(|c| c.dropbacks >= 5.0).collect();
    qbs.sort_by(|a, b| (b.last_game_dropbacks, b.dropbacks).partial_cmp(&(a.last_game_dropbacks, a.dropbacks)).unwrap_or(std::cmp::Ordering::Equal));
    let (qb, qb_p) = match qb_override {
        Some(id) => (Some((id.clone(), cands.iter().find(|c| &c.id == id).map(|c| c.short.clone()).unwrap_or_default())), 1.0),
        None => match qbs.iter().find(|c| c.p_active > 0.0) {
            Some(c) => (Some((c.id.clone(), c.short.clone())), c.p_active),
            None => (None, 1.0),
        },
    };
    // active roster QBs with no snaps for this team in the window (league-average rates)
    let roster_qbs: Vec<(String, String)> = if has_roster {
        let mut v: Vec<(String, String)> = av.roster.iter()
            .filter(|(id, r)| r.1 == "QB" && r.2 == "ACT" && av.roster_team.get(*id).map(|t| t == team).unwrap_or(false)
                && !opts.out.contains(*id) && av.injury.get(*id).map(|x| x.0 != "Out").unwrap_or(true))
            .map(|(id, r)| (id.clone(), r.0.clone())).collect();
        v.sort();
        v
    } else { Vec::new() };
    let (qb, qb_p) = if qb.is_none() { (roster_qbs.first().cloned(), 1.0) } else { (qb, qb_p) };
    let qb_id = qb.as_ref().map(|q| q.0.clone()).unwrap_or_default();
    let mut backup_qb = qbs.iter().filter(|c| c.id != qb_id && c.p_active >= 1.0).map(|c| (c.id.clone(), c.short.clone())).next();
    if backup_qb.is_none() {
        backup_qb = roster_qbs.iter().find(|q| q.0 != qb_id).cloned();
    }
    // targets / carries among available non-QB-only players; tiny roles dropped
    let pick = |f: &dyn Fn(&Cand) -> f64| -> Vec<(String, String, f64, f64)> {
        let mut l: Vec<(String, String, f64, f64)> = cands.iter().filter(|c| c.p_active > 0.0 && f(c) > 0.0)
            .map(|c| (c.id.clone(), c.short.clone(), f(c), c.p_active)).collect();
        let tot: f64 = l.iter().map(|x| x.2).sum();
        l.retain(|x| tot > 0.0 && x.2 / tot >= 0.02);
        let tot: f64 = l.iter().map(|x| x.2).sum();
        for x in l.iter_mut() { x.2 /= tot.max(1e-9); }
        l.sort_by(|a, b| b.2.total_cmp(&a.2));
        l
    };
    let targets = pick(&|c: &Cand| c.target_share);
    let carries = pick(&|c: &Cand| c.carry_share);
    let target_n: f64 = (0..ng).map(|g| w_share(g) * tot_t[g]).sum();
    let carry_n: f64 = (0..ng).map(|g| w_share(g) * tot_c[g]).sum();
    let kicker = kick.into_iter().filter(|(id, _)| !opts.out.contains(id) && av.injury.get(id).map(|x| x.0 != "Out").unwrap_or(true))
        .max_by(|a, b| a.1 .1.total_cmp(&b.1 .1)).map(|(id, (nm, _))| (id, nm));
    let lineup: Vec<Cand> = cands.into_iter()
        .filter(|c| !c.reason.starts_with("now on "))
        .filter(|c| c.target_share >= 0.02 || c.carry_share >= 0.02 || c.dropbacks >= 5.0).collect();
    let lineup_source = if actual.is_some() {
        "actual: players who recorded a dropback, carry or target in this game (hindsight)".to_string()
    } else if has_roster {
        "nflverse weekly roster (status ACT) + official injury report".to_string()
    } else {
        "no roster published for this week: players used in the last 3 games + injury report".to_string()
    };
    Usage { qb, backup_qb, qb_p, kicker, targets, carries, target_n, carry_n, proe: (a, e, v, wsum), games: ng, lineup, lineup_source }
}

fn defense(plays: &[P], team: &str, league: &League, opts: &Opts) -> (sim::Defense, Value) {
    let mut games: Vec<(NaiveDate, String)> = Vec::new();
    for p in plays.iter().rev() {
        if p.defteam.as_deref() == Some(team) && !games.iter().any(|(_, g)| g == &p.game_id) {
            games.push((p.date, p.game_id.clone()));
        }
    }
    games.sort_by(|a, b| b.0.cmp(&a.0));
    let gi: HashMap<&str, usize> = games.iter().enumerate().map(|(i, g)| (g.1.as_str(), i)).collect();
    let (mut db, mut sk, mut at, mut it, mut cm, mut cy, mut ca, mut ry) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
    for p in plays {
        if p.defteam.as_deref() != Some(team) || !p.scrimmage() { continue; }
        let Some(&i) = gi.get(p.game_id.as_str()) else { continue };
        let w = 0.5f64.powf(i as f64 / opts.team_halflife);
        if p.dropback() { db += w; if p.sack { sk += w; } }
        if p.attempt() {
            at += w;
            if p.int { it += w; } else if p.complete { cm += w; cy += w * p.yards as f64; }
        }
        if p.designed_run() { ca += w; ry += w * p.yards as f64; }
    }
    let k = &league.kappa;
    let lg_ypr = if league.cmp_mean > 0.0 { league.cmp_mean } else { mean_of(&league.cmp_yards) };
    let lg_ypc = if league.run_mean > 0.0 { league.run_mean } else { mean_of(&league.run_yards) };
    let ypr = ((cy + k.def_yards * lg_ypr) / (cm + k.def_yards)) / lg_ypr;
    let ypc = ((ry + k.def_yards * lg_ypc) / (ca + k.def_yards)) / lg_ypc;
    let d = sim::Defense {
        sack: shrink(sk, db, league.sack, k.def_sack),
        int: shrink(it, at, league.int, k.def_int),
        cmp: shrink(cm, at, league.cmp, k.def_cmp),
        ypc_ratio: ypc,
        ypr_ratio: ypr,
    };
    let meta = json!({
        "games": games.len(), "sack_rate": r4(d.sack.mean), "int_rate": r4(d.int.mean), "cmp_allowed": r4(d.cmp.mean),
        "yards_per_completion_vs_league": r3(ypr), "yards_per_carry_vs_league": r3(ypc),
    });
    (d, meta)
}

/// Odds multiplier of a rate in each down-and-distance group vs overall.
fn dd_factor(c: &[[f64; 6]; sim::DD], x: usize, n: usize) -> [f64; sim::DD] {
    let tx: f64 = c.iter().map(|r| r[x]).sum();
    let tn: f64 = c.iter().map(|r| r[n]).sum();
    let o = |a: f64, b: f64| { let p = (a / b).clamp(1e-4, 1.0 - 1e-4); p / (1.0 - p) };
    let base = if tn > 0.0 { o(tx, tn) } else { 1.0 };
    let mut out = [1.0; sim::DD];
    for g in 0..sim::DD {
        if c[g][n] >= 50.0 && tn > 0.0 { out[g] = o(c[g][x], c[g][n]) / base; }
    }
    out
}

/// Shrunk ratio of a player's mean gain to the league mean (EB prior size k).
fn mean_ratio(own: &[i32], lg_mean: f64, k: f64) -> f64 {
    if lg_mean <= 0.0 { return 1.0; }
    let s: f64 = own.iter().map(|x| *x as f64).sum();
    ((s + k * lg_mean) / (own.len() as f64 + k)) / lg_mean
}

fn mean_of(v: &[Vec<i32>]) -> f64 {
    let (s, n) = v.iter().flatten().fold((0.0, 0.0), |(s, n), x| (s + *x as f64, n + 1.0));
    if n > 0.0 { s / n } else { 5.0 }
}

pub async fn build(db: &tokio_postgres::Client, gref: &GameRef, opts: &Opts) -> Result<Built, String> {
    let params_season = if gref.postseason { gref.season } else { gref.season - 1 };
    let (league, params_key) = load_league(db, params_season).await?;
    let teams = vec![gref.away.clone(), gref.home.clone()];
    let from_season = gref.season - 1;
    let team_plays = load_plays(db, "(posteam = ANY($1) OR defteam = ANY($1)) AND game_date < $2 AND season >= $3",
        &[&teams, &gref.date, &from_season]).await?;
    if team_plays.is_empty() {
        return Err(format!("no plays before {} for {} / {}", gref.date, gref.away, gref.home));
    }
    let av = load_avail(db, gref).await?;
    let av_proj = av.without_reports();
    let actual = load_actual(db, &gref.game_id).await?;
    if opts.lineup_mode == LineupMode::Actual && actual.is_none() {
        return Err("actual lineup is only available after the game has been played (no play-by-play yet)".into());
    }
    let act_qb: Vec<Option<String>> = teams.iter().map(|t| actual.as_ref().and_then(|a| a.qb.get(t).cloned())).collect();
    let mode = opts.lineup_mode;
    let overrides: [Option<&String>; 2] = [0, 1].map(|i| {
        let user = if i == 0 { opts.away_qb.as_ref() } else { opts.home_qb.as_ref() };
        if mode == LineupMode::Actual { user.or(act_qb[i].as_ref()) } else { user }
    });
    let used_av = if mode == LineupMode::Projected { &av_proj } else { &av };
    let usages: Vec<Usage> = teams.iter().zip(overrides).map(|(t, o)| {
        let act = if mode == LineupMode::Actual { actual.as_ref().and_then(|a| a.played.get(t)) } else { None };
        usage(&team_plays, t, &league, opts, o, used_av, act)
    }).collect();
    // the other views, for the starters comparison (no scenario overrides)
    let plain = opts_from(None, None);
    let view_proj: Vec<Usage> = teams.iter().map(|t| usage(&team_plays, t, &league, &plain, None, &av_proj, None)).collect();
    let view_conf: Vec<Usage> = teams.iter().map(|t| usage(&team_plays, t, &league, &plain, None, &av, None)).collect();
    // every chosen player's own plays (any team) for efficiency + yards
    let mut ids: Vec<String> = Vec::new();
    for u in &usages {
        if let Some(q) = &u.qb { ids.push(q.0.clone()); }
        if let Some(q) = &u.backup_qb { ids.push(q.0.clone()); }
        ids.extend(u.targets.iter().map(|x| x.0.clone()));
        ids.extend(u.carries.iter().map(|x| x.0.clone()));
    }
    ids.sort();
    ids.dedup();
    let pplays = load_plays(db, "game_date < $2 AND season >= $3 AND (passer_id = ANY($1) OR rusher_id = ANY($1) OR receiver_id = ANY($1))",
        &[&ids, &gref.date, &from_season]).await?;
    let mut all_ids = ids.clone();
    for u in usages.iter().chain(&view_proj).chain(&view_conf) {
        if let Some(k) = &u.kicker { all_ids.push(k.0.clone()); }
        if let Some(q) = &u.qb { all_ids.push(q.0.clone()); }
        all_ids.extend(u.lineup.iter().map(|c| c.id.clone()));
    }
    if let Some(a) = &actual {
        for v in a.usage.values() { all_ids.extend(v.iter().map(|x| x.0.clone())); }
        all_ids.extend(a.kicker.values().cloned());
    }
    let names: HashMap<String, (String, String)> = db.query(
        "SELECT player_id, display_name, COALESCE(position, '') FROM sports_players WHERE sport = 'nfl' AND player_id = ANY($1)", &[&all_ids])
        .await.map_err(|e| dberr(&e))?.iter().map(|r| (r.get::<_, String>(0), (r.get::<_, String>(1), r.get::<_, String>(2)))).collect();

    struct Own { db: f64, sk: f64, sc: f64, at: f64, it: f64, cm: f64, tg: f64, ct: f64, rec_y: Vec<i32>, run_y: Vec<i32>, scr_y: Vec<i32>,
                 ploc: [f64; 3], rloc: [f64; 3], sloc: [f64; 3] }
    let mut own: HashMap<&str, Own> = HashMap::new();
    let li = |l: &Option<String>| match l.as_deref() { Some("left") => Some(0), Some("middle") => Some(1), Some("right") => Some(2), _ => None };
    let new_own = || Own { db: 0.0, sk: 0.0, sc: 0.0, at: 0.0, it: 0.0, cm: 0.0, tg: 0.0, ct: 0.0, rec_y: vec![], run_y: vec![], scr_y: vec![],
        ploc: [0.0; 3], rloc: [0.0; 3], sloc: [0.0; 3] };
    for p in &pplays {
        if !p.scrimmage() { continue; }
        if p.dropback() {
            if let Some((id, _)) = p.passer.as_ref().or(p.rusher.as_ref()) {
                let o = own.entry(id.as_str()).or_insert_with(new_own);
                o.db += 1.0;
                if p.sack { o.sk += 1.0 } else if p.scramble {
                    o.sc += 1.0;
                    o.scr_y.push(p.yards);
                    if let Some(l) = li(&p.location) { o.sloc[l] += 1.0; }
                }
            }
        }
        if p.attempt() {
            if let Some((id, _)) = &p.passer {
                let o = own.entry(id.as_str()).or_insert_with(new_own);
                o.at += 1.0;
                if p.int { o.it += 1.0 } else if p.complete { o.cm += 1.0 }
            }
            if let Some((id, _)) = &p.receiver {
                let o = own.entry(id.as_str()).or_insert_with(new_own);
                o.tg += 1.0;
                if let Some(l) = li(&p.location) { o.ploc[l] += 1.0; }
                if p.complete && !p.int { o.ct += 1.0; o.rec_y.push(p.yards); }
            }
        }
        if p.designed_run() {
            if let Some((id, _)) = &p.rusher {
                let o = own.entry(id.as_str()).or_insert_with(new_own);
                o.run_y.push(p.yards);
                if let Some(l) = li(&p.location) { o.rloc[l] += 1.0; }
            }
        }
    }
    let empty = new_own();

    let mut players: Vec<sim::Player> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut pmeta: Vec<Value> = Vec::new();
    let display = |id: &str, short: &str| -> (String, String) {
        names.get(id).cloned()
            .or_else(|| av.roster.get(id).map(|r| (r.0.clone(), r.1.clone())))
            .unwrap_or_else(|| (short.to_string(), String::new()))
    };
    let mut add = |id: &str, short: &str, team: usize, p_active: f64, players: &mut Vec<sim::Player>, pmeta: &mut Vec<Value>| -> usize {
        if let Some(i) = index.get(id) { return *i; }
        let (name, pos) = display(id, short);
        let i = players.len();
        players.push(sim::Player { id: id.to_string(), name: name.clone(), pos: pos.clone(), p_active });
        pmeta.push(json!({ "idx": i, "id": id, "name": name, "short": short, "pos": pos, "team": team, "p_active": r3(p_active) }));
        index.insert(id.to_string(), i);
        i
    };
    let k = league.kappa.clone();
    let mut team_inputs: Vec<sim::TeamInput> = Vec::new();
    let mut team_meta: Vec<Value> = Vec::new();
    let mut flags: Vec<String> = Vec::new();
    for (t, (abbr, u)) in teams.iter().zip(&usages).enumerate() {
        let Some((qb_id, qb_short)) = u.qb.clone() else { return Err(format!("{abbr}: no available quarterback (all ruled out?)")) };
        let qi = add(&qb_id, &qb_short, t, u.qb_p, &mut players, &mut pmeta);
        let q = own.get(qb_id.as_str()).unwrap_or(&empty);
        let mut receivers = Vec::new();
        let tot_t: f64 = u.targets.iter().map(|x| x.2).sum::<f64>().max(1e-9);
        for (id, short, w, pa) in &u.targets {
            let p = add(id, short, t, *pa, &mut players, &mut pmeta);
            let o = own.get(id.as_str()).unwrap_or(&empty);
            receivers.push(sim::Receiver { p, share: w / tot_t, catch: shrink(o.ct, o.tg, league.cmp, k.catch),
                yards: o.rec_y.clone(), ratio: mean_ratio(&o.rec_y, league.cmp_mean, k.ypr), loc: loc3(o.ploc, league.loc_pass) });
        }
        let mut rushers = Vec::new();
        let tot_c: f64 = u.carries.iter().map(|x| x.2).sum::<f64>().max(1e-9);
        for (id, short, w, pa) in &u.carries {
            let p = add(id, short, t, *pa, &mut players, &mut pmeta);
            let o = own.get(id.as_str()).unwrap_or(&empty);
            rushers.push(sim::Rusher { p, share: w / tot_c, yards: o.run_y.clone(), ratio: mean_ratio(&o.run_y, league.run_mean, k.ypc),
                loc: loc3(o.rloc, league.loc_run) });
        }
        let kicker = u.kicker.as_ref().map(|(id, short)| add(id, short, t, 1.0, &mut players, &mut pmeta));
        let backup_qb = u.backup_qb.as_ref().map(|(id, short)| {
            let bi = add(id, short, t, 1.0, &mut players, &mut pmeta);
            let b = own.get(id.as_str()).unwrap_or(&empty);
            sim::BackupQb { p: bi, sack: shrink(b.sk, b.db, league.sack, k.sack), scramble: shrink(b.sc, b.db, league.scramble, k.scramble),
                int: shrink(b.it, b.at, league.int, k.int), cmp: shrink(b.cm, b.at, league.cmp, k.cmp_qb), loc: loc3(b.sloc, league.loc_run) }
        });
        if u.qb_p < 1.0 && backup_qb.is_none() { flags.push(format!("{abbr}: starting QB is uncertain and no backup QB was found")); }
        if kicker.is_none() { flags.push(format!("{abbr}: no kicker found in recent games (kicks still simulated, not credited)")); }
        if q.db < 100.0 { flags.push(format!("{abbr}: QB has only {:.0} dropbacks in the data window — heavily pooled to league", q.db)); }
        let (pa, pe, pv, pw) = u.proe;
        let proe = if pv > 0.0 { (pa - pe) / pv * (pw / (pw + k.proe)) } else { 0.0 };
        let off = sim::Offense {
            qb: qi, kicker, proe,
            sack: shrink(q.sk, q.db, league.sack, k.sack),
            scramble: shrink(q.sc, q.db, league.scramble, k.scramble),
            int: shrink(q.it, q.at, league.int, k.int),
            cmp: shrink(q.cm, q.at, league.cmp, k.cmp_qb),
            qb_scramble_yards: q.scr_y.clone(), qb_loc: loc3(q.sloc, league.loc_run),
            catch_ref: 0.0, backup_qb,
            receivers, rushers, share_n: [u.target_n, u.carry_n],
        };
        let mut off = off;
        off.catch_ref = sim::catch_ref(&off.receivers);
        let (def, def_meta) = defense(&team_plays, abbr, &league, opts);
        team_meta.push(json!({
            "abbr": abbr, "games_in_window": u.games,
            "offense": {
                "qb": pmeta[qi]["name"], "qb_id": qb_id, "qb_p_active": r3(u.qb_p),
                "qb_source": if overrides[t].is_some() { "scenario: chosen QB" } else { "most dropbacks in the team's last game among available QBs" },
                "backup_qb": u.backup_qb.as_ref().map(|(id, sh)| display(id, sh).0),
                "qb_dropbacks": q.db, "sack_rate": r4(off.sack.mean), "scramble_rate": r4(off.scramble.mean), "int_rate": r4(off.int.mean),
                "cmp_rate": r4(off.cmp.mean), "pass_rate_over_expected_logit": r3(proe),
                "receivers": off.receivers.iter().map(|r| json!({ "name": pmeta[r.p]["name"], "pos": pmeta[r.p]["pos"], "target_share": r3(r.share),
                    "catch_rate": r3(r.catch.mean), "own_catches": r.yards.len(), "yards_per_catch_vs_league": r3(r.ratio) })).collect::<Vec<_>>(),
                "rushers": off.rushers.iter().map(|r| json!({ "name": pmeta[r.p]["name"], "pos": pmeta[r.p]["pos"], "carry_share": r3(r.share),
                    "own_carries": r.yards.len(), "yards_per_carry_vs_league": r3(r.ratio) })).collect::<Vec<_>>(),
                "kicker": kicker.map(|i| pmeta[i]["name"].clone()),
            },
            "defense": def_meta,
            "lineup_source": u.lineup_source,
            "lineup": u.lineup.iter().map(|c| {
                let (name, pos) = display(&c.id, &c.short);
                let role = if c.id == qb_id { "starting QB" } else if u.backup_qb.as_ref().map(|b| b.0 == c.id).unwrap_or(false) { "backup QB" }
                    else if pos == "QB" || (c.dropbacks >= 5.0 && c.target_share < 0.02 && c.carry_share < 0.02) { "QB" } else { "skill" };
                json!({ "id": c.id, "name": name, "pos": pos, "role": role, "target_share": r3(c.target_share), "carry_share": r3(c.carry_share),
                    "dropbacks": c.dropbacks, "p_active": r3(c.p_active), "status": c.reason, "roster_status": c.roster_status,
                    "report": c.report, "practice": c.practice, "injury": c.injury,
                    "scenario": if opts.out.contains(&c.id) { "out" } else if opts.force_in.contains(&c.id) { "in" } else { "auto" } })
            }).collect::<Vec<_>>(),
        }));
        team_inputs.push(sim::TeamInput { abbr: abbr.clone(), off, def });
    }
    // starters by role under each lineup source
    let role_rows = |qb: Option<String>, kicker: Option<String>, carries: Vec<(String, f64, f64)>, targets: Vec<(String, f64, f64)>| -> Value {
        // (id, share-or-count, p_active)
        let person = |id: &str, p: f64| { let (n, pos) = display(id, id); json!({ "id": id, "name": n, "pos": pos, "p_active": r3(p) }) };
        let pos_of = |id: &str| display(id, id).1;
        let rbs: Vec<Value> = carries.iter().filter(|x| matches!(pos_of(&x.0).as_str(), "RB" | "FB" | "HB")).take(2).map(|x| person(&x.0, x.2)).collect();
        let wrs: Vec<Value> = targets.iter().filter(|x| pos_of(&x.0) == "WR").take(3).map(|x| person(&x.0, x.2)).collect();
        let tes: Vec<Value> = targets.iter().filter(|x| pos_of(&x.0) == "TE").take(1).map(|x| person(&x.0, x.2)).collect();
        let slot = |v: &Vec<Value>, i: usize| v.get(i).cloned().unwrap_or(Value::Null);
        json!([
            { "role": "QB", "player": qb.as_ref().map(|q| person(q, 1.0)) },
            { "role": "RB1", "player": slot(&rbs, 0) }, { "role": "RB2", "player": slot(&rbs, 1) },
            { "role": "WR1", "player": slot(&wrs, 0) }, { "role": "WR2", "player": slot(&wrs, 1) }, { "role": "WR3", "player": slot(&wrs, 2) },
            { "role": "TE1", "player": slot(&tes, 0) },
            { "role": "K", "player": kicker.as_ref().map(|k| person(k, 1.0)) },
        ])
    };
    let view = |u: &Usage| role_rows(u.qb.as_ref().map(|q| q.0.clone()), u.kicker.as_ref().map(|k| k.0.clone()),
        u.carries.iter().map(|x| (x.0.clone(), x.2, x.3)).collect(), u.targets.iter().map(|x| (x.0.clone(), x.2, x.3)).collect());
    let starters_json: Vec<Value> = teams.iter().enumerate().map(|(t, abbr)| {
        let actual_view = actual.as_ref().map(|a| {
            let us = a.usage.get(abbr).cloned().unwrap_or_default();
            let mut car: Vec<(String, f64, f64)> = us.iter().filter(|x| x.3 > 0.0).map(|x| (x.0.clone(), x.3, 1.0)).collect();
            car.sort_by(|x, y| y.1.total_cmp(&x.1));
            let mut tgt: Vec<(String, f64, f64)> = us.iter().filter(|x| x.4 > 0.0).map(|x| (x.0.clone(), x.4, 1.0)).collect();
            tgt.sort_by(|x, y| y.1.total_cmp(&x.1));
            role_rows(a.qb.get(abbr).cloned(), a.kicker.get(abbr).cloned(), car, tgt)
        });
        json!({ "team": abbr, "projected": view(&view_proj[t]), "confirmed": view(&view_conf[t]), "actual": actual_view })
    }).collect();
    let [a, h]: [sim::TeamInput; 2] = team_inputs.try_into().map_err(|_| "teams".to_string())?;
    let meta = json!({
        "model_version": MODEL_VERSION, "params": params_key, "data_cutoff": gref.date,
        "data_window": format!("plays from season {} up to the day before the game", from_season),
        "players": pmeta, "teams": team_meta, "flags": flags,
        "lineup_mode": mode.as_str(), "actual_available": actual.is_some(), "starters": starters_json,
        "availability": { "week": gref.week, "rates": { "questionable": r3(av.rates.questionable), "doubtful": r3(av.rates.doubtful) },
            "rates_source": av.rates_source, "scenario_out": opts.out.iter().collect::<Vec<_>>(), "scenario_in": opts.force_in.iter().collect::<Vec<_>>() },
        "assumptions": [
            "lineup = this week's active roster + official injury report (final inactives 90 min before kickoff may differ)",
            "Questionable / Doubtful players play with the probability fitted from earlier seasons; props are conditional on playing",
            "an inactive player's targets and carries go to the active players in proportion to their shares",
            "one quarterback per simulated game (backup only when the starter is inactive); no in-game injuries",
            "defensive injuries are not modelled; timeouts and onside kicks are not simulated; no weather term",
        ],
    });
    Ok(Built { game: GameInput { players, teams: [a, h], league, postseason: gref.postseason }, meta, params_key })
}

// ---------------------------------------------------------------------------
// Endpoints
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct GamesQuery {
    #[serde(default = "d_days")] days: i64,
    #[serde(default = "d_back")] back: i64,
}
fn d_days() -> i64 { 10 }
fn d_back() -> i64 { 7 }

/// GET /nfl/sim/games — recent and upcoming games that can be simulated.
pub async fn games(state: web::Data<State>, q: web::Query<GamesQuery>) -> impl Responder {
    let today = chrono::Local::now().date_naive();
    let from = today - chrono::Duration::days(q.back);
    let to = today + chrono::Duration::days(q.days);
    let rows = match state.db.query(
        "SELECT game_id, season, week, game_date, away_team, home_team, away_score, home_score, spread_line, total_line, game_type \
         FROM sports_games WHERE sport = 'nfl' AND game_date BETWEEN $1 AND $2 ORDER BY game_date, game_id", &[&from, &to]).await {
        Ok(r) => r,
        Err(e) => return internal(dberr(&e)),
    };
    let last_play: Option<NaiveDate> = state.db.query_one("SELECT max(game_date) FROM nfl_plays", &[]).await.ok().and_then(|r| r.get(0));
    let games: Vec<Value> = rows.iter().map(|r| {
        let d: Option<NaiveDate> = r.get(3);
        json!({
            "game_id": r.get::<_, String>(0), "season": r.get::<_, Option<i32>>(1), "week": r.get::<_, Option<String>>(2),
            "date": d, "away": r.get::<_, Option<String>>(4), "home": r.get::<_, Option<String>>(5),
            "away_score": r.get::<_, Option<i32>>(6), "home_score": r.get::<_, Option<i32>>(7),
            "spread_line": r.get::<_, Option<f64>>(8), "total_line": r.get::<_, Option<f64>>(9),
            "game_type": r.get::<_, Option<String>>(10), "today": d == Some(today),
        })
    }).collect();
    HttpResponse::Ok().json(json!({ "today": today, "plays_through": last_play, "games": games }))
}

#[derive(Deserialize)]
pub struct SimQuery {
    #[serde(default = "d_n")] n: usize,
    #[serde(default = "d_draws")] draws: usize,
    seed: Option<u64>,
    away_qb: Option<String>,
    home_qb: Option<String>,
    /// alternate reality: comma-separated player ids ruled out / forced to play
    out: Option<String>,
    #[serde(rename = "in")]
    force_in: Option<String>,
    /// projected | confirmed (default) | actual
    lineup: Option<String>,
}
fn d_n() -> usize { 100_000 }
fn d_draws() -> usize { 200 }

fn id_set(v: &Option<String>) -> std::collections::HashSet<String> {
    v.as_deref().unwrap_or("").split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()
}
pub fn opts_from(away_qb: Option<String>, home_qb: Option<String>) -> Opts {
    Opts { share_halflife: 3.0, team_halflife: 8.0, away_qb, home_qb, out: Default::default(), force_in: Default::default(),
        lineup_mode: LineupMode::Confirmed }
}
pub fn opts_scenario(away_qb: Option<String>, home_qb: Option<String>, out: &[String], force_in: &[String]) -> Opts {
    let mut o = opts_from(away_qb, home_qb);
    o.out = out.iter().cloned().collect();
    o.force_in = force_in.iter().cloned().collect();
    o
}

fn hist_json(h: &sim::Hist, lines: &[f64]) -> Value {
    let (off, pmf) = h.pmf();
    // trim negligible tails for the payload
    let lo = pmf.iter().position(|p| *p > 1e-5).unwrap_or(0);
    let hi = pmf.iter().rposition(|p| *p > 1e-5).unwrap_or(0);
    json!({
        "mean": r3(h.mean()), "q10": h.quantile(0.10), "q50": h.quantile(0.5), "q90": h.quantile(0.9),
        "lines": lines.iter().map(|l| { let (o, u, p, se) = h.line(*l); json!({ "line": l, "over": r4(o), "under": r4(u), "push": r4(p), "mcse": r4(se) }) }).collect::<Vec<_>>(),
        "pmf_offset": off + lo as i32,
        "pmf": pmf[lo..=hi.max(lo)].iter().map(|p| r4(*p)).collect::<Vec<_>>(),
    })
}

fn fair_line(h: &sim::Hist) -> f64 {
    h.quantile(0.5) as f64 - 0.5 + if h.line(h.quantile(0.5) as f64 - 0.5).0 < 0.5 { 0.0 } else { 1.0 }
}

/// GET /nfl/simulate/{game_id}?n=100000 — the prediction ensemble.
pub async fn simulate(state: web::Data<State>, path: web::Path<String>, q: web::Query<SimQuery>) -> impl Responder {
    let gid = path.into_inner();
    let gref = match game_ref(&state.db, &gid).await { Ok(g) => g, Err(e) => return unprocessable(e) };
    let out: Vec<String> = id_set(&q.out).into_iter().collect();
    let fin: Vec<String> = id_set(&q.force_in).into_iter().collect();
    let mut o = opts_scenario(q.away_qb.clone(), q.home_qb.clone(), &out, &fin);
    o.lineup_mode = LineupMode::parse(q.lineup.as_deref());
    let built = match build(&state.db, &gref, &o).await {
        Ok(b) => b,
        Err(e) => return unprocessable(e),
    };
    let Built { game, meta, .. } = built;
    let seed = q.seed.unwrap_or_else(|| sim::derive_seed(0x6e666c, &[&gid, &chrono::Utc::now().timestamp_millis().to_string()]));
    // production predictions always use at least 100,000 games
    let (n, draws) = (q.n.clamp(100_000, 1_000_000), q.draws.clamp(50, 2000));
    let t = Instant::now();
    let g2 = game.clone();
    let e = match web::block(move || sim::simulate(&g2, n, draws, seed)).await {
        Ok(e) => e,
        Err(err) => return internal(format!("simulation thread: {err}")),
    };
    let ms = t.elapsed().as_millis() as u64;
    HttpResponse::Ok().json(ensemble_json(&game, &e, &gref, &meta, seed, n, draws, ms))
}

#[allow(clippy::too_many_arguments)]
fn ensemble_json(game: &GameInput, e: &sim::Ensemble, gref: &GameRef, meta: &Value, seed: u64, n: usize, draws: usize, ms: u64) -> Value {
    let nn = e.n.max(1) as f64;
    let mut team_players: Vec<Vec<Value>> = vec![Vec::new(), Vec::new()];
    let pm = meta["players"].as_array().cloned().unwrap_or_default();
    for (i, pl) in game.players.iter().enumerate() {
        let team = pm.get(i).and_then(|m| m["team"].as_u64()).unwrap_or(0) as usize;
        let h = &e.player[i];
        let mut stats = serde_json::Map::new();
        let is_qb = game.teams.iter().any(|t| t.off.qb == i || t.off.backup_qb.as_ref().map(|b| b.p == i).unwrap_or(false));
        let is_k = game.teams.iter().any(|t| t.off.kicker == Some(i));
        let keys: Vec<usize> = if is_qb {
            vec![sim::PASS_YDS, sim::PASS_ATT, sim::CMP, sim::PASS_TD, sim::INT, sim::RUSH_YDS, sim::RUSH_ATT]
        } else if is_k {
            vec![sim::FGM, sim::XPM]
        } else {
            vec![sim::REC_YDS, sim::REC, sim::TGT, sim::RUSH_YDS, sim::RUSH_ATT, sim::REC_TD, sim::RUSH_TD]
        };
        if e.active_n[i] == 0 { continue; }
        for k in keys {
            if h[k].mean().abs() < 0.05 && !matches!(k, sim::REC | sim::REC_YDS) { continue; }
            stats.insert(sim::STAT_KEYS[k].into(), hist_json(&h[k], &[fair_line(&h[k])]));
        }
        let act = e.active_n[i] as f64;
        if act <= 0.0 { continue; }
        team_players[team].push(json!({
            "idx": i, "id": pl.id, "name": pl.name, "pos": pl.pos, "role": if is_qb { "QB" } else if is_k { "K" } else { "skill" },
            "p_active": r4(act / nn), "anytime_td": r4(e.anytime_td[i] as f64 / act), "stats": stats,
        }));
    }
    let market = json!({
        "spread_line": gref.spread_line, "total_line": gref.total_line,
        "p_home_cover": gref.spread_line.map(|s| { let (o, _, p, _) = e.margin.line(s); r4(o + 0.0 * p) }),
        "p_over_total": gref.total_line.map(|t| r4(e.total.line(t).0)),
    });
    json!({
        "game_id": gref.game_id, "date": gref.date, "away": game.teams[0].abbr, "home": game.teams[1].abbr,
        "model_version": MODEL_VERSION, "seed": seed, "n": e.n, "requested": n, "param_draws": draws, "sim_ms": ms,
        "win": { "home": r4(e.home_win as f64 / nn), "away": r4(e.away_win as f64 / nn), "tie": r4(e.ties as f64 / nn), "overtime": r4(e.overtime as f64 / nn) },
        "score": { "away": hist_json(&e.score[0], &[]), "home": hist_json(&e.score[1], &[]) },
        "total": hist_json(&e.total, &gref.total_line.into_iter().collect::<Vec<_>>()),
        "margin_home": hist_json(&e.margin, &gref.spread_line.into_iter().collect::<Vec<_>>()),
        "plays": { "away": r3(e.plays[0].mean()), "home": r3(e.plays[1].mean()) },
        "market": market,
        "players": { "away": team_players[0], "home": team_players[1] },
        "inputs": meta,
        "note": "Probabilities are model outputs from real play-by-play inputs; player props are conditional on the player being active (p_active); Monte Carlo error (mcse) is sampling error only, not model error.",
    })
}

// ---------------------------------------------------------------------------
// Broadcast sessions (interactive mode)
// ---------------------------------------------------------------------------

/// Run one seeded game and return (meta, events, final).
pub async fn new_session(db: &tokio_postgres::Client, game_id: &str, seed: u64, opts: Opts)
    -> Result<(Value, Vec<Value>, Value, NaiveDate), String> {
    let gref = game_ref(db, game_id).await?;
    let Built { game, meta, .. } = build(db, &gref, &opts).await?;
    let g2 = game.clone();
    let (res, events) = web::block(move || sim::single_game(&g2, seed)).await.map_err(|e| e.to_string())?;
    let events: Vec<Value> = events.into_iter().map(|e| serde_json::to_value(e).unwrap_or(Value::Null)).collect();
    let sat_out: Vec<Value> = game.players.iter().enumerate()
        .filter(|(i, p)| p.p_active < 1.0 && !res.active.get(*i).copied().unwrap_or(true))
        .map(|(_, p)| json!({ "id": p.id, "name": p.name, "p_active": r3(p.p_active) })).collect();
    let fin = json!({
        "score": res.score, "overtime": res.overtime, "plays": res.plays, "uncertain_players_inactive": sat_out.clone(),
        "box_score": res.stats.iter().enumerate().filter(|(_, s)| s.iter().any(|v| *v != 0))
            .map(|(i, s)| json!([i, s])).collect::<Vec<_>>(),
    });
    let meta = json!({
        "sport": "nfl", "game_id": gref.game_id, "date": gref.date, "season": gref.season, "postseason": gref.postseason,
        "away": game.teams[0].abbr, "home": game.teams[1].abbr,
        "stat_keys": sim::STAT_KEYS, "field": { "x0": "away goal line", "x100": "home goal line", "away_attacks": "x = 100" },
        "actual": { "away_score": gref.away_score, "home_score": gref.home_score },
        "market": { "spread_line": gref.spread_line, "total_line": gref.total_line },
        // in this one simulated reality, which uncertain (Questionable/Doubtful) players sat out
        "this_reality": { "inactive": sat_out },
        "inputs": meta,
    });
    Ok((meta, events, fin, gref.date))
}

// ---------------------------------------------------------------------------
// Walk-forward backtest
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct BacktestQuery {
    #[serde(default = "d_bt_season")] season: i32,
    #[serde(default = "d_bt_games")] games: usize,
    #[serde(default = "d_bt_n")] n: usize,
    #[serde(default = "d_bt_draws")] draws: usize,
    #[serde(default = "d_bt_seed")] seed: u64,
    from_week: Option<i32>,
    /// interactive games per backtest game for the team-level diagnostics
    #[serde(default = "d_diag")] diag_games: usize,
}
fn d_diag() -> usize { 20 }
fn d_bt_season() -> i32 { 2025 }
fn d_bt_games() -> usize { 120 }
fn d_bt_n() -> usize { 2000 }
fn d_bt_draws() -> usize { 20 }
fn d_bt_seed() -> u64 { 20260101 }

#[derive(Default)]
struct Binary {
    n: f64,
    model: f64,
    base: f64,
    diffs: Vec<f64>,
    ll_model: f64,
    ll_base: f64,
    bins: [[f64; 3]; 10], // predicted sum, actual sum, n
}
impl Binary {
    fn add(&mut self, pm: f64, pb: f64, y: f64) {
        let pm = pm.clamp(1e-4, 1.0 - 1e-4);
        let pb = pb.clamp(1e-4, 1.0 - 1e-4);
        let bm = (pm - y).powi(2);
        let bb = (pb - y).powi(2);
        self.n += 1.0;
        self.model += bm;
        self.base += bb;
        self.diffs.push(bm - bb);
        self.ll_model += -(y * pm.ln() + (1.0 - y) * (1.0 - pm).ln());
        self.ll_base += -(y * pb.ln() + (1.0 - y) * (1.0 - pb).ln());
        let b = ((pm * 10.0) as usize).min(9);
        self.bins[b][0] += pm;
        self.bins[b][1] += y;
        self.bins[b][2] += 1.0;
    }
    fn report(&self, baseline: &str) -> Value {
        let n = self.n.max(1.0);
        let md = self.diffs.iter().sum::<f64>() / n;
        let sd = (self.diffs.iter().map(|d| (d - md).powi(2)).sum::<f64>() / (n - 1.0).max(1.0)).sqrt();
        let ece = self.bins.iter().map(|b| if b[2] > 0.0 { b[2] / n * (b[0] / b[2] - b[1] / b[2]).abs() } else { 0.0 }).sum::<f64>();
        json!({
            "n": self.n, "baseline": baseline, "brier_model": r4(self.model / n), "brier_baseline": r4(self.base / n),
            "brier_diff": r4(md), "brier_diff_se": r4(sd / n.sqrt()), "logloss_model": r4(self.ll_model / n), "logloss_baseline": r4(self.ll_base / n),
            "ece_model": r4(ece),
            "calibration": self.bins.iter().filter(|b| b[2] > 0.0).map(|b| json!({ "pred": r3(b[0] / b[2]), "actual": r3(b[1] / b[2]), "n": b[2] })).collect::<Vec<_>>(),
        })
    }
}

#[derive(Default)]
struct Numeric {
    n: f64,
    abs_err: f64,
    abs_err_base: f64,
    bias: f64,
    cov80: f64,
}
impl Numeric {
    fn add(&mut self, pred: f64, base: f64, q10: f64, q90: f64, y: f64) {
        self.n += 1.0;
        self.abs_err += (pred - y).abs();
        self.abs_err_base += (base - y).abs();
        self.bias += pred - y;
        self.cov80 += (y >= q10 && y <= q90) as i32 as f64;
    }
    fn report(&self, baseline: &str) -> Value {
        let n = self.n.max(1.0);
        json!({ "n": self.n, "baseline": baseline, "mae_model": r3(self.abs_err / n), "mae_baseline": r3(self.abs_err_base / n),
                "bias": r3(self.bias / n), "coverage_80": r3(self.cov80 / n) })
    }
}

const ND: usize = 23;
/// Per-game team-level tallies, defined identically for simulated events and
/// real play-by-play so the two can be compared like for like.
pub const DIAG: [&str; ND] = ["scrimmage_plays", "dropbacks", "pass_att", "completions", "pass_yds", "sacks", "scrambles",
    "designed_runs", "rush_yds", "interceptions", "fumbles_lost", "offensive_td", "fg_att", "fg_made", "punts",
    "third_down_att", "third_down_conv", "fourth_down_go", "return_td", "points", "possessions", "plays_q4_last5", "pass_rate_q1_q3"];

fn diag_events(ev: &[sim::Event]) -> [f64; ND] {
    let mut d = [0.0; ND];
    let mut last_off: Option<u8> = None;
    let (mut q13, mut q13p) = (0.0, 0.0);
    for e in ev {
        let scrim = matches!(e.kind, "pass_complete" | "pass_incomplete" | "interception" | "sack" | "scramble" | "rush")
            || (e.kind == "fumble_lost" && e.down >= 1);
        if scrim {
            d[0] += 1.0;
            if last_off != Some(e.offense) { d[20] += 1.0; last_off = Some(e.offense); }
            if e.qtr == 4 && e.clock_before <= 300 { d[21] += 1.0; }
            let dropback = matches!(e.kind, "pass_complete" | "pass_incomplete" | "interception" | "sack" | "scramble") || (e.kind == "fumble_lost" && e.passer.is_some());
            if e.qtr <= 3 && e.down <= 3 { q13 += 1.0; q13p += dropback as i32 as f64; }
            if matches!(e.kind, "pass_complete" | "pass_incomplete" | "interception" | "sack" | "scramble") || (e.kind == "fumble_lost" && e.passer.is_some()) { d[1] += 1.0; }
            if matches!(e.kind, "pass_complete" | "pass_incomplete" | "interception") { d[2] += 1.0; }
            if e.kind == "pass_complete" { d[3] += 1.0; d[4] += e.yards as f64; }
            if e.kind == "sack" { d[5] += 1.0; }
            if e.kind == "scramble" { d[6] += 1.0; }
            if e.kind == "rush" { d[7] += 1.0; d[8] += e.yards as f64; }
            if e.kind == "interception" { d[9] += 1.0; }
            if e.kind == "fumble_lost" { d[10] += 1.0; }
            if e.touchdown && !e.turnover { d[11] += 1.0; }
            if e.down == 3 {
                d[15] += 1.0;
                if !e.turnover && (e.first_down || e.touchdown) { d[16] += 1.0; }
            }
            if e.down == 4 { d[17] += 1.0; }
        }
        if e.kind == "field_goal" { d[12] += 1.0; if e.points == 3 { d[13] += 1.0; } }
        if e.kind == "punt" { d[14] += 1.0; }
        if e.touchdown && (e.turnover || e.kind == "kickoff") { d[18] += 1.0; }
        d[19] += e.points as f64;
    }
    d[22] = if q13 > 0.0 { q13p / q13 } else { 0.0 };
    d
}

fn diag_plays(plays: &[P]) -> [f64; ND] {
    let mut d = [0.0; ND];
    let mut last_off: Option<&str> = None;
    let (mut q13, mut q13p) = (0.0, 0.0);
    for p in plays {
        if p.scrimmage() {
            if last_off != p.posteam.as_deref() { d[20] += 1.0; last_off = p.posteam.as_deref(); }
            if p.qtr == 4 && p.qsr.unwrap_or(900) <= 300 { d[21] += 1.0; }
            if p.qtr <= 3 && p.down.unwrap_or(4) <= 3 { q13 += 1.0; q13p += p.dropback() as i32 as f64; }
        }
        if p.scrimmage() && !p.penalty {
            d[0] += 1.0;
            if p.dropback() { d[1] += 1.0; }
            if p.attempt() { d[2] += 1.0; }
            if p.attempt() && p.complete && !p.int { d[3] += 1.0; d[4] += p.yards as f64; }
            if p.sack { d[5] += 1.0; }
            if p.scramble { d[6] += 1.0; }
            if p.designed_run() { d[7] += 1.0; d[8] += p.yards as f64; }
            if p.int { d[9] += 1.0; }
            if p.fumble_lost { d[10] += 1.0; }
            let off_td = p.td && p.td_team == p.posteam;
            if off_td { d[11] += 1.0; }
            if p.down == Some(3) {
                d[15] += 1.0;
                if !p.int && !p.fumble_lost && (off_td || p.yards >= p.togo.unwrap_or(99)) { d[16] += 1.0; }
            }
            if p.down == Some(4) { d[17] += 1.0; }
        }
        if p.ptype == "field_goal" { d[12] += 1.0; if p.fg_result.as_deref() == Some("made") { d[13] += 1.0; } }
        if p.ptype == "punt" { d[14] += 1.0; }
        if p.td && p.td_team.is_some() && p.td_team != p.posteam { d[18] += 1.0; }
        if p.ptype == "kickoff" && p.td && p.td_team == p.posteam { d[18] += 1.0; }
    }
    d[22] = if q13 > 0.0 { q13p / q13 } else { 0.0 };
    d
}

/// Actual per-player stats for a game from its plays: (pass_yds, rush_yds, rec_yds, receptions, any TD)
fn actual_stats(plays: &[P]) -> HashMap<String, [f64; 5]> {
    let mut m: HashMap<String, [f64; 5]> = HashMap::new();
    for p in plays {
        if !p.scrimmage() { continue; }
        if p.attempt() && p.complete && !p.int {
            if let Some((id, _)) = &p.passer { m.entry(id.clone()).or_default()[0] += p.yards as f64; }
            if let Some((id, _)) = &p.receiver {
                let e = m.entry(id.clone()).or_default();
                e[2] += p.yards as f64;
                e[3] += 1.0;
                if p.td && p.td_team == p.posteam { e[4] = 1.0; }
            }
        }
        if p.ptype == "run" {
            if let Some((id, _)) = &p.rusher {
                let e = m.entry(id.clone()).or_default();
                e[1] += p.yards as f64;
                if p.td && p.td_team == p.posteam { e[4] = 1.0; }
            }
        }
    }
    m
}

/// GET /backtest/nfl/sim?season=2025&games=120&n=2000 — walk-forward: each game
/// is simulated with parameters from the previous season and player/team inputs
/// strictly before its date, then scored against what happened.
pub async fn backtest(state: web::Data<State>, q: web::Query<BacktestQuery>) -> impl Responder {
    let db = &state.db;
    let from_week = q.from_week.unwrap_or(3);
    let rows = match db.query(
        "SELECT game_id FROM sports_games WHERE sport = 'nfl' AND season = $1 AND game_type = 'REG' AND home_score IS NOT NULL \
         AND week::int >= $2 AND game_id IN (SELECT DISTINCT game_id FROM nfl_plays WHERE season = $1) ORDER BY game_date, game_id",
        &[&q.season, &from_week]).await {
        Ok(r) => r,
        Err(e) => return internal(dberr(&e)),
    };
    let all: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
    if all.is_empty() {
        return unprocessable(format!("no completed {} regular-season games with play-by-play", q.season));
    }
    // spread evenly through the season
    let step = (all.len() as f64 / q.games.max(1) as f64).max(1.0);
    let picked: Vec<String> = (0..q.games.min(all.len())).map(|i| all[((i as f64) * step) as usize].clone()).collect();
    let t0 = Instant::now();
    let (mut home_win, mut cover, mut over) = (Binary::default(), Binary::default(), Binary::default());
    let (mut pass_y, mut rush_y, mut rec_y, mut recs, mut any_td) = (Binary::default(), Binary::default(), Binary::default(), Binary::default(), Binary::default());
    let (mut total_n, mut margin_n, mut pass_num, mut rush_num, mut rec_num) = (Numeric::default(), Numeric::default(), Numeric::default(), Numeric::default(), Numeric::default());
    let mut plays_sim = 0.0;
    let mut plays_act = 0.0;
    let mut skipped: Vec<Value> = Vec::new();
    let mut sims = 0usize;
    let (mut diag_sim, mut diag_act) = ([0.0f64; ND], [0.0f64; ND]);
    for gid in &picked {
        let gref = match game_ref(db, gid).await { Ok(g) => g, Err(e) => { skipped.push(json!({ "game": gid, "why": e })); continue } };
        let (Some(hs), Some(as_)) = (gref.home_score, gref.away_score) else { continue };
        let built = match build(db, &gref, &opts_from(None, None)).await {
            Ok(b) => b,
            Err(e) => { skipped.push(json!({ "game": gid, "why": e })); continue }
        };
        let game = built.game;
        let seed = sim::derive_seed(q.seed, &[gid]);
        let (n, draws) = (q.n, q.draws);
        let g2 = game.clone();
        let e = match web::block(move || sim::simulate(&g2, n, draws, seed)).await { Ok(e) => e, Err(err) => return internal(err) };
        sims += 1;
        let nn = e.n as f64;
        // team markets
        let ph = (e.home_win as f64 + 0.5 * e.ties as f64) / nn;
        let y = if hs > as_ { 1.0 } else if hs < as_ { 0.0 } else { 0.5 };
        // market-implied home win from the closing spread (normal, sd 13.45) when available
        let market_p = gref.spread_line.map(|s| 0.5 * (1.0 + erf(s / (13.45 * std::f64::consts::SQRT_2))));
        home_win.add(ph, market_p.unwrap_or(0.55), y);
        if let Some(s) = gref.spread_line {
            let (o, u, _, _) = e.margin.line(s);
            let margin = (hs - as_) as f64;
            if (margin - s).abs() > 1e-9 { cover.add(o / (o + u).max(1e-9), 0.5, (margin > s) as i32 as f64); }
        }
        if let Some(tl) = gref.total_line {
            let (o, u, _, _) = e.total.line(tl);
            let tot = (hs + as_) as f64;
            if (tot - tl).abs() > 1e-9 { over.add(o / (o + u).max(1e-9), 0.5, (tot > tl) as i32 as f64); }
            total_n.add(e.total.mean(), tl, e.total.quantile(0.1) as f64, e.total.quantile(0.9) as f64, tot);
        }
        if let Some(s) = gref.spread_line {
            margin_n.add(e.margin.mean(), s, e.margin.quantile(0.1) as f64, e.margin.quantile(0.9) as f64, (hs - as_) as f64);
        }
        plays_sim += e.plays[0].mean() + e.plays[1].mean();
        // actual plays + player stats from the game's own play-by-play
        let gplays = match load_plays(db, "game_id = $1", &[gid]).await { Ok(p) => p, Err(err) => return internal(err) };
        let mut da = diag_plays(&gplays);
        da[19] = (hs + as_) as f64;
        for k in 0..ND { diag_act[k] += da[k]; }
        let dg = game.clone();
        let dn = q.diag_games.max(1);
        let ds = match web::block(move || {
            let mut acc = [0.0f64; ND];
            for j in 0..dn {
                let (_, ev) = sim::single_game(&dg, sim::derive_seed(seed, &["diag", &j.to_string()]));
                let d = diag_events(&ev);
                for k in 0..ND { acc[k] += d[k] / dn as f64; }
            }
            acc
        }).await { Ok(d) => d, Err(err) => return internal(err) };
        for k in 0..ND { diag_sim[k] += ds[k]; }
        plays_act += gplays.iter().filter(|p| p.scrimmage() || p.ptype == "qb_kneel").count() as f64;
        let act = actual_stats(&gplays);
        // baseline: each player's own prior frequency over the line (add-one), from the build window
        let prior = match load_plays(db, "game_date < $1 AND season >= $2 AND (passer_id = ANY($3) OR rusher_id = ANY($3) OR receiver_id = ANY($3))",
            &[&gref.date, &(gref.season - 1), &game.players.iter().map(|p| p.id.clone()).collect::<Vec<_>>()]).await {
            Ok(p) => p, Err(err) => return internal(err),
        };
        let mut hist: HashMap<String, Vec<[f64; 5]>> = HashMap::new();
        {
            let mut by_game: HashMap<&str, Vec<P>> = HashMap::new();
            for p in &prior { by_game.entry(p.game_id.as_str()).or_default().push(p.clone()); }
            for (_, ps) in by_game {
                for (id, s) in actual_stats(&ps) { hist.entry(id).or_default().push(s); }
            }
        }
        for (i, pl) in game.players.iter().enumerate() {
            let Some(a) = act.get(&pl.id) else { continue }; // did not record a stat: likely inactive
            let h = &e.player[i];
            let past = hist.get(&pl.id).cloned().unwrap_or_default();
            let freq = |k: usize, line: f64| (past.iter().filter(|s| s[k] > line).count() as f64 + 1.0) / (past.len() as f64 + 2.0);
            let avg = |k: usize| if past.is_empty() { 0.0 } else { past.iter().map(|s| s[k]).sum::<f64>() / past.len() as f64 };
            let is_qb = game.teams.iter().any(|t| t.off.qb == i || t.off.backup_qb.as_ref().map(|b| b.p == i).unwrap_or(false));
            let score = |b: &mut Binary, num: Option<&mut Numeric>, k: usize, stat: usize| {
                if past.len() < 3 { return; }
                let line = (avg(k)).floor() + 0.5;
                let (o, _, _, _) = h[stat].line(line);
                b.add(o, freq(k, line), (a[k] > line) as i32 as f64);
                if let Some(nm) = num { nm.add(h[stat].mean(), avg(k), h[stat].quantile(0.1) as f64, h[stat].quantile(0.9) as f64, a[k]); }
            };
            if is_qb {
                score(&mut pass_y, Some(&mut pass_num), 0, sim::PASS_YDS);
            } else {
                if avg(1) >= 20.0 { score(&mut rush_y, Some(&mut rush_num), 1, sim::RUSH_YDS); }
                if avg(2) >= 15.0 {
                    score(&mut rec_y, Some(&mut rec_num), 2, sim::REC_YDS);
                    score(&mut recs, None, 3, sim::REC);
                }
                if past.len() >= 3 {
                    let td_freq = (past.iter().filter(|s| s[4] > 0.0).count() as f64 + 1.0) / (past.len() as f64 + 2.0);
                    any_td.add(e.anytime_td[i] as f64 / nn, td_freq, a[4]);
                }
            }
        }
    }
    let report = json!({
        "model_version": MODEL_VERSION, "season": q.season, "from_week": from_week, "games_simulated": sims, "games_requested": picked.len(),
        "skipped": skipped, "sims_per_game": q.n, "param_draws": q.draws, "seed": q.seed, "secs": t0.elapsed().as_secs(),
        "params": format!("nfl:sim:{}", q.season - 1),
        "plays_per_game": { "simulated": r3(plays_sim / sims.max(1) as f64), "actual": r3(plays_act / sims.max(1) as f64) },
        "per_game_both_teams": DIAG.iter().enumerate().map(|(k, name)| json!({
            "stat": name, "simulated": r3(diag_sim[k] / sims.max(1) as f64), "actual": r3(diag_act[k] / sims.max(1) as f64) })).collect::<Vec<_>>(),
        "binary": {
            "home_win": home_win.report("closing spread (normal, sd 13.45)"),
            "home_cover": cover.report("0.5 (closing spread is the line)"),
            "total_over": over.report("0.5 (closing total is the line)"),
            "qb_pass_yds_over_avg": pass_y.report("player's own prior frequency over the line"),
            "rush_yds_over_avg": rush_y.report("player's own prior frequency over the line"),
            "rec_yds_over_avg": rec_y.report("player's own prior frequency over the line"),
            "receptions_over_avg": recs.report("player's own prior frequency over the line"),
            "anytime_td": any_td.report("player's own prior TD-game frequency"),
        },
        "numeric": {
            "total_points": total_n.report("closing total"),
            "home_margin": margin_n.report("closing spread"),
            "qb_pass_yds": pass_num.report("player's prior average"),
            "rush_yds": rush_num.report("player's prior average"),
            "rec_yds": rec_num.report("player's prior average"),
        },
    });
    if sims >= 20 {
        let key = format!("nfl:backtest:{}", q.season);
        let _ = db.execute(
            "INSERT INTO sim_model_params (key, params, source) VALUES ($1, $2, 'walk-forward backtest') \
             ON CONFLICT (key) DO UPDATE SET params = EXCLUDED.params, source = EXCLUDED.source, fitted_at = now()",
            &[&key, &Json(report.clone())]).await;
    }
    HttpResponse::Ok().json(report)
}

/// GET /nfl/backtests — stored walk-forward reports.
pub async fn backtests(state: web::Data<State>) -> impl Responder {
    match state.db.query("SELECT params FROM sim_model_params WHERE key LIKE 'nfl:backtest:%' ORDER BY key DESC", &[]).await {
        Ok(rows) => HttpResponse::Ok().json(json!({ "backtests": rows.iter().map(|r| r.get::<_, Json<Value>>(0).0).collect::<Vec<_>>() })),
        Err(e) => internal(dberr(&e)),
    }
}

/// Abramowitz-Stegun 7.1.26 error function.
fn erf(x: f64) -> f64 {
    let s = x.signum();
    let x = x.abs();
    let t = 1.0 / (1.0 + 0.3275911 * x);
    let y = 1.0 - (((((1.061405429 * t - 1.453152027) * t) + 1.421413741) * t - 0.284496736) * t + 0.254829592) * t * (-x * x).exp();
    s * y
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "game_id,play_id,season,week,season_type,game_date,home_team,away_team,posteam,defteam,qtr,quarter_seconds_remaining,yardline_100,down,ydstogo,score_differential,play_type,yards_gained,sack,qb_scramble,interception,complete_pass,fumble_lost,touchdown,td_team,safety,out_of_bounds,penalty,two_point_attempt,field_goal_result,extra_point_result,two_point_conv_result,kick_distance,air_yards,pass_location,run_location,passer_player_id,passer_player_name,rusher_player_id,rusher_player_name,receiver_player_id,receiver_player_name,kicker_player_id,kicker_player_name,penalty_team,penalty_yards,first_down_penalty
2026_01_A_B,1,2026,1,REG,2026-09-10,B,A,NA,NA,1,900,NA,NA,0,NA,,NA,0,0,0,0,0,0,NA,0,0,0,0,NA,NA,NA,NA,NA,NA,NA,NA,NA,NA,NA,NA,NA,NA,NA,NA,NA,0
2026_01_A_B,39,2026,1,REG,2026-09-10,B,A,A,B,1,900,35,NA,0,0,kickoff,0,0,0,0,0,0,0,NA,0,0,0,0,NA,NA,NA,60,NA,NA,NA,NA,NA,NA,NA,NA,NA,K1,K.One,NA,NA,0
2026_01_A_B,62,2026,1,REG,2026-09-10,B,A,A,B,1,896,70,1,10,0,pass,27,0,0,0,1,0,0,NA,0,0,0,0,NA,NA,NA,NA,12,right,NA,Q1,Q.One,NA,NA,W1,W.One,NA,NA,NA,NA,0
2026_01_A_B,87,2026,1,REG,2026-09-10,B,A,A,B,1,858,43,1,10,0,run,2,0,0,0,0,0,0,NA,0,0,0,0,NA,NA,NA,NA,NA,NA,left,NA,NA,R1,R.One,NA,NA,NA,NA,NA,NA,0
2026_01_A_B,132,2026,1,REG,2026-09-10,B,A,A,B,1,810,41,2,8,0,run,11,0,1,0,0,0,0,NA,0,1,0,0,NA,NA,NA,NA,NA,NA,left,NA,NA,Q1,Q.One,NA,NA,NA,NA,NA,NA,0
";

    #[test]
    fn parses_pbp() {
        let rows = parse_pbp(SAMPLE.as_bytes()).unwrap();
        assert_eq!(rows.len(), 4); // empty play_type row skipped
        assert_eq!(rows[0].ptype, "kickoff");
        assert_eq!(rows[1].receiver.as_ref().unwrap().0, "W1");
        assert!(rows[1].complete && rows[1].dropback() && rows[1].attempt());
        assert!(rows[2].designed_run());
        assert!(rows[3].scramble && rows[3].dropback() && !rows[3].designed_run() && rows[3].oob);
        let next = next_snaps(&rows);
        assert_eq!(next[0], Some(1));
        assert_eq!(next[3], None);
        let lg = fit_league(&rows, 2026);
        assert_eq!(lg.kickoff_start, vec![70]);
        assert_eq!(lg.elapsed[sim::KICK_CLASS], vec![4]);
        // pass at 896 -> run at 858, run at 858 -> scramble at 810: running clock, normal situation
        assert_eq!(lg.elapsed[sim::clock_class(0, sim::RUN_IN)], vec![38, 48]);
    }

    #[test]
    fn erf_matches() {
        assert!((erf(0.5) - 0.5205).abs() < 1e-3);
        assert!((erf(-1.0) + 0.8427).abs() < 1e-3);
    }
}
