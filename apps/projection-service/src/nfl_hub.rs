//! NFL game hub analytics: the season-to-date team profiles behind the Games
//! page's matchup tabs (rankings, offense-vs-defense, situations, coaching,
//! team stats, player matchups, ATS/O-U logs).
//!
//! Data (all free, refreshed by autopilot job `hub:nfl`):
//!   - nflverse play-by-play efficiency columns (EPA, success, PROE, CPOE, run gap)
//!     -> `nfl_play_metrics`, joined to the plays we already store (`nfl_plays`)
//!   - FTN charting (play action, screens, motion, blitzers, pass rushers, box)
//!     -> `nfl_ftn_charting`
//!   - nflverse schedule (kickoff, stadium, roof/surface, coaches, referee, lines,
//!     ESPN id) -> `nfl_game_info`
//! Coverage shells (man/zone) and pressure are not in any free 2026 feed, so the
//! "through the air" and "QB comfort" sections use the charted equivalents.
//!
//! Every number is computed from plays of the season BEFORE the game's week, so a
//! matchup page shows what each team brought into the game.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use actix_web::{web, HttpResponse, Responder};
use chrono::NaiveDate;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_postgres::types::ToSql;

use crate::State;

const PBP_URL: &str = "https://github.com/nflverse/nflverse-data/releases/download/pbp/play_by_play_{season}.csv.gz";
const FTN_URL: &str = "https://github.com/nflverse/nflverse-data/releases/download/ftn_charting/ftn_charting_{season}.csv";
const GAMES_URL: &str = "https://github.com/nflverse/nfldata/raw/master/data/games.csv";

fn dberr(e: &tokio_postgres::Error) -> String {
    e.as_db_error().map(|d| d.message().to_string()).unwrap_or_else(|| e.to_string())
}
fn r1(x: f64) -> f64 { (x * 10.0).round() / 10.0 }
fn r3(x: f64) -> f64 { (x * 1000.0).round() / 1000.0 }

pub fn season_of(d: NaiveDate) -> i32 {
    use chrono::Datelike;
    if d.month() >= 3 { d.year() } else { d.year() - 1 }
}

// ---------------------------------------------------------------------------
// Ingest
// ---------------------------------------------------------------------------

fn csv_index(h: &csv::StringRecord) -> HashMap<String, usize> {
    h.iter().enumerate().map(|(i, n)| (n.to_string(), i)).collect()
}
fn cell<'a>(rec: &'a csv::StringRecord, idx: &HashMap<String, usize>, name: &str) -> &'a str {
    idx.get(name).and_then(|&i| rec.get(i)).map(|s| if s == "NA" { "" } else { s }).unwrap_or("")
}
fn opt_i(s: &str) -> Option<i32> { s.parse::<f64>().ok().map(|v| v.round() as i32) }
fn opt_f(s: &str) -> Option<f32> { s.parse::<f32>().ok().filter(|v| v.is_finite()) }
fn opt_s(s: &str) -> Option<String> { if s.is_empty() { None } else { Some(s.to_string()) } }
fn flag(s: &str) -> bool { matches!(s, "1" | "1.0" | "TRUE" | "True" | "true") }
fn opt_flag(s: &str) -> Option<bool> { if s.is_empty() { None } else { Some(flag(s)) } }

type Row = Vec<Box<dyn ToSql + Sync + Send>>;

async fn bulk_insert(db: &tokio_postgres::Client, table: &str, cols: &[&str], rows: Vec<Row>, conflict: &str) -> Result<u64, String> {
    let n = cols.len();
    let per = (60_000 / n).max(1).min(500);
    let mut total = 0;
    let mut rows = rows;
    while !rows.is_empty() {
        let take = rows.len().min(per);
        let chunk: Vec<Row> = rows.drain(..take).collect();
        let mut sql = format!("INSERT INTO {table} ({}) VALUES ", cols.join(","));
        for i in 0..chunk.len() {
            if i > 0 { sql.push(','); }
            sql.push('(');
            for k in 0..n {
                if k > 0 { sql.push(','); }
                sql.push_str(&format!("${}", i * n + k + 1));
            }
            sql.push(')');
        }
        sql.push(' ');
        sql.push_str(conflict);
        let refs: Vec<&(dyn ToSql + Sync)> = chunk.iter().flat_map(|r| r.iter().map(|b| b.as_ref() as &(dyn ToSql + Sync))).collect();
        total += db.execute(sql.as_str(), &refs).await.map_err(|e| format!("insert {table}: {}", dberr(&e)))?;
    }
    Ok(total)
}

async fn fetch(state: &State, url: &str) -> Result<Vec<u8>, String> {
    let r = state.http.get(url).timeout(Duration::from_secs(180)).send().await
        .and_then(|r| r.error_for_status()).map_err(|e| format!("{url}: {e}"))?;
    Ok(r.bytes().await.map_err(|e| format!("{url}: {e}"))?.to_vec())
}

fn parse_metrics(text: &[u8]) -> Result<Vec<Row>, String> {
    let mut rdr = csv::ReaderBuilder::new().from_reader(text);
    let idx = csv_index(rdr.headers().map_err(|e| e.to_string())?);
    for c in ["game_id", "play_id", "epa", "success", "pass", "rush"] {
        if !idx.contains_key(c) { return Err(format!("pbp csv missing {c}")); }
    }
    let mut out = Vec::new();
    for rec in rdr.records() {
        let rec = rec.map_err(|e| e.to_string())?;
        let g = |n: &str| cell(&rec, &idx, n);
        let pt = g("play_type");
        if pt.is_empty() { continue; }
        let (Some(play_id), Some(season), Some(week)) = (opt_i(g("play_id")), opt_i(g("season")), opt_i(g("week"))) else { continue };
        let row: Row = vec![
            Box::new(g("game_id").to_string()), Box::new(play_id), Box::new(season), Box::new(week),
            Box::new(g("season_type").to_string()), Box::new(opt_s(g("posteam"))), Box::new(opt_s(g("defteam"))),
            Box::new(opt_f(g("epa"))), Box::new(opt_flag(g("success"))), Box::new(opt_f(g("wp"))),
            Box::new(opt_f(g("xpass"))), Box::new(opt_f(g("pass_oe"))), Box::new(opt_f(g("cpoe"))),
            Box::new(flag(g("qb_dropback"))), Box::new(flag(g("pass"))), Box::new(flag(g("rush"))),
            Box::new(flag(g("shotgun"))), Box::new(flag(g("no_huddle"))), Box::new(flag(g("first_down"))),
            Box::new(opt_s(g("run_gap"))), Box::new(opt_s(g("run_location"))), Box::new(opt_i(g("yards_after_catch"))),
        ];
        out.push(row);
    }
    Ok(out)
}
const METRIC_COLS: &[&str] = &["game_id", "play_id", "season", "week", "season_type", "posteam", "defteam", "epa", "success", "wp",
    "xpass", "pass_oe", "cpoe", "qb_dropback", "is_pass", "is_rush", "shotgun", "no_huddle", "first_down", "run_gap", "run_location", "yac"];

fn parse_ftn(text: &[u8]) -> Result<Vec<Row>, String> {
    let mut rdr = csv::ReaderBuilder::new().from_reader(text);
    let idx = csv_index(rdr.headers().map_err(|e| e.to_string())?);
    if !idx.contains_key("nflverse_game_id") { return Err("ftn csv missing nflverse_game_id".into()); }
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for rec in rdr.records() {
        let rec = rec.map_err(|e| e.to_string())?;
        let g = |n: &str| cell(&rec, &idx, n);
        let (Some(play_id), Some(season), Some(week)) = (opt_i(g("nflverse_play_id")), opt_i(g("season")), opt_i(g("week"))) else { continue };
        let gid = g("nflverse_game_id").to_string();
        if gid.is_empty() || !seen.insert((gid.clone(), play_id)) { continue; }
        out.push(vec![
            Box::new(gid) as Box<dyn ToSql + Sync + Send>, Box::new(play_id), Box::new(season), Box::new(week),
            Box::new(opt_flag(g("is_play_action"))), Box::new(opt_flag(g("is_rpo"))), Box::new(opt_flag(g("is_screen_pass"))),
            Box::new(opt_flag(g("is_motion"))), Box::new(opt_flag(g("is_no_huddle"))), Box::new(opt_flag(g("is_qb_out_of_pocket"))),
            Box::new(opt_flag(g("is_throw_away"))), Box::new(opt_flag(g("is_drop"))), Box::new(opt_flag(g("is_catchable_ball"))),
            Box::new(opt_flag(g("is_interception_worthy"))), Box::new(opt_i(g("n_blitzers"))), Box::new(opt_i(g("n_pass_rushers"))),
            Box::new(opt_i(g("n_defense_box"))), Box::new(opt_s(g("qb_location"))),
        ]);
    }
    Ok(out)
}
const FTN_COLS: &[&str] = &["game_id", "play_id", "season", "week", "is_play_action", "is_rpo", "is_screen_pass", "is_motion", "is_no_huddle",
    "is_qb_out_of_pocket", "is_throw_away", "is_drop", "is_catchable_ball", "is_int_worthy", "n_blitzers", "n_pass_rushers", "n_defense_box", "qb_location"];

fn parse_games(text: &[u8], from_season: i32) -> Result<Vec<Row>, String> {
    let mut rdr = csv::ReaderBuilder::new().from_reader(text);
    let idx = csv_index(rdr.headers().map_err(|e| e.to_string())?);
    if !idx.contains_key("espn") { return Err("games csv missing espn".into()); }
    let mut out = Vec::new();
    for rec in rdr.records() {
        let rec = rec.map_err(|e| e.to_string())?;
        let g = |n: &str| cell(&rec, &idx, n);
        let Some(season) = opt_i(g("season")) else { continue };
        if season < from_season { continue; }
        let day = NaiveDate::parse_from_str(g("gameday"), "%Y-%m-%d").ok();
        out.push(vec![
            Box::new(g("game_id").to_string()) as Box<dyn ToSql + Sync + Send>, Box::new(season), Box::new(opt_s(g("game_type"))),
            Box::new(opt_i(g("week"))), Box::new(day), Box::new(opt_s(g("gametime"))),
            Box::new(g("away_team").to_string()), Box::new(g("home_team").to_string()),
            Box::new(opt_i(g("away_score"))), Box::new(opt_i(g("home_score"))), Box::new(opt_s(g("espn"))),
            Box::new(opt_i(g("away_rest"))), Box::new(opt_i(g("home_rest"))), Box::new(opt_i(g("away_moneyline"))),
            Box::new(opt_i(g("home_moneyline"))), Box::new(g("spread_line").parse::<f64>().ok()), Box::new(g("total_line").parse::<f64>().ok()),
            Box::new(opt_flag(g("div_game"))), Box::new(opt_s(g("roof"))), Box::new(opt_s(g("surface"))),
            Box::new(opt_i(g("temp"))), Box::new(opt_i(g("wind"))), Box::new(opt_s(g("away_qb_name"))), Box::new(opt_s(g("home_qb_name"))),
            Box::new(opt_s(g("away_coach"))), Box::new(opt_s(g("home_coach"))), Box::new(opt_s(g("referee"))), Box::new(opt_s(g("stadium"))),
        ]);
    }
    Ok(out)
}
const GAME_COLS: &[&str] = &["game_id", "season", "game_type", "week", "gameday", "gametime", "away_team", "home_team", "away_score",
    "home_score", "espn_id", "away_rest", "home_rest", "away_moneyline", "home_moneyline", "spread_line", "total_line", "div_game",
    "roof", "surface", "temp", "wind", "away_qb_name", "home_qb_name", "away_coach", "home_coach", "referee", "stadium"];

/// Download and store the hub tables for the given seasons (schedule from 2015).
pub async fn ingest(state: &State, seasons: &[i32]) -> Result<Value, String> {
    let t = Instant::now();
    let mut report = json!({});
    // schedule
    let bytes = fetch(state, GAMES_URL).await?;
    let rows = web::block(move || parse_games(&bytes, 2015)).await.map_err(|e| e.to_string())??;
    let set: Vec<String> = GAME_COLS.iter().skip(1).map(|c| format!("{c}=EXCLUDED.{c}")).collect();
    let n = bulk_insert(&state.db, "nfl_game_info", GAME_COLS, rows,
        &format!("ON CONFLICT (game_id) DO UPDATE SET {}, updated_at=now()", set.join(","))).await?;
    report["games"] = json!(n);
    for &season in seasons {
        let url = PBP_URL.replace("{season}", &season.to_string());
        let gz = fetch(state, &url).await?;
        let rows = web::block(move || {
            let mut dec = flate2::read::GzDecoder::new(&gz[..]);
            let mut text = Vec::new();
            dec.read_to_end(&mut text).map_err(|e| format!("gunzip: {e}"))?;
            parse_metrics(&text)
        }).await.map_err(|e| e.to_string())??;
        state.db.execute("DELETE FROM nfl_play_metrics WHERE season=$1", &[&season]).await.map_err(|e| dberr(&e))?;
        let m = bulk_insert(&state.db, "nfl_play_metrics", METRIC_COLS, rows, "ON CONFLICT DO NOTHING").await?;
        let ftn = match fetch(state, &FTN_URL.replace("{season}", &season.to_string())).await {
            Ok(b) => {
                let rows = web::block(move || parse_ftn(&b)).await.map_err(|e| e.to_string())??;
                state.db.execute("DELETE FROM nfl_ftn_charting WHERE season=$1", &[&season]).await.map_err(|e| dberr(&e))?;
                json!(bulk_insert(&state.db, "nfl_ftn_charting", FTN_COLS, rows, "ON CONFLICT DO NOTHING").await?)
            }
            Err(e) => json!({ "error": e }),
        };
        report[season.to_string()] = json!({ "metrics": m, "ftn": ftn });
    }
    cache().lock().unwrap().clear();
    report["secs"] = json!(t.elapsed().as_secs());
    Ok(report)
}

#[derive(Deserialize)]
pub struct IngestQ { seasons: Option<String> }

/// POST /ingest/nfl/hub?seasons=2025,2026
pub async fn ingest_http(state: web::Data<State>, q: web::Query<IngestQ>) -> impl Responder {
    let now = chrono::Utc::now().date_naive();
    let seasons: Vec<i32> = q.seasons.clone().unwrap_or_else(|| format!("{},{}", season_of(now) - 1, season_of(now)))
        .split(',').filter_map(|s| s.trim().parse().ok()).collect();
    match ingest(&state, &seasons).await {
        Ok(v) => HttpResponse::Ok().json(v),
        Err(e) => HttpResponse::InternalServerError().json(json!({ "error": e })),
    }
}

// ---------------------------------------------------------------------------
// Season plays (cached)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct Play {
    game_id: String,
    week: i32,
    off: String,
    def: String,
    qtr: i32,
    qsr: Option<i32>,
    yl: Option<i32>,
    down: Option<i32>,
    togo: Option<i32>,
    diff: Option<i32>,
    ptype: String,
    yards: i32,
    sack: bool,
    scramble: bool,
    int: bool,
    complete: bool,
    fumble_lost: bool,
    td: bool,
    td_team: Option<String>,
    two_point: bool,
    air_yards: Option<i32>,
    passer: Option<(String, String)>,
    rusher: Option<(String, String)>,
    receiver: Option<(String, String)>,
    penalty: bool,
    penalty_team: Option<String>,
    epa: Option<f64>,
    success: Option<bool>,
    wp: Option<f64>,
    pass_oe: Option<f64>,
    cpoe: Option<f64>,
    dropback: bool,
    shotgun: bool,
    no_huddle: bool,
    first_down: bool,
    run_gap: Option<String>,
    run_loc: Option<String>,
    charted: bool,
    play_action: Option<bool>,
    screen: Option<bool>,
    motion: Option<bool>,
    blitzers: Option<i32>,
    rushers: Option<i32>,
    boxn: Option<i32>,
    oop: Option<bool>,
}

impl Play {
    fn scrim(&self) -> bool { (self.ptype == "pass" || self.ptype == "run") && !self.two_point && self.down.is_some() }
    fn is_dropback(&self) -> bool { self.scrim() && (self.dropback || self.ptype == "pass" || self.scramble) }
    fn attempt(&self) -> bool { self.scrim() && self.ptype == "pass" && !self.sack }
    fn designed_run(&self) -> bool { self.scrim() && self.ptype == "run" && !self.scramble }
}

pub struct Season {
    plays: Vec<Play>,
    pos: HashMap<String, String>,
    games: Vec<GameRow>,
}

#[derive(Clone, Debug)]
pub struct GameRow {
    pub game_id: String,
    pub season: i32,
    pub game_type: String,
    pub week: i32,
    pub day: Option<NaiveDate>,
    pub away: String,
    pub home: String,
    pub away_score: Option<i32>,
    pub home_score: Option<i32>,
    pub spread_line: Option<f64>,
    pub total_line: Option<f64>,
    pub away_coach: Option<String>,
    pub home_coach: Option<String>,
}

fn cache() -> &'static Mutex<HashMap<String, (Instant, Arc<Value>)>> {
    static C: OnceLock<Mutex<HashMap<String, (Instant, Arc<Value>)>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}
fn season_cache() -> &'static Mutex<HashMap<i32, (Instant, Arc<Season>)>> {
    static C: OnceLock<Mutex<HashMap<i32, (Instant, Arc<Season>)>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}
const TTL: Duration = Duration::from_secs(20 * 60);

pub async fn load_games(db: &tokio_postgres::Client, from_season: i32) -> Result<Vec<GameRow>, String> {
    let rows = db.query(
        "SELECT game_id, season, coalesce(game_type,'REG'), coalesce(week,0), gameday, away_team, home_team, away_score, home_score, \
                spread_line, total_line, away_coach, home_coach FROM nfl_game_info WHERE season >= $1 ORDER BY gameday, game_id",
        &[&from_season]).await.map_err(|e| dberr(&e))?;
    Ok(rows.iter().map(|r| GameRow {
        game_id: r.get(0), season: r.get(1), game_type: r.get(2), week: r.get(3), day: r.get(4), away: r.get(5), home: r.get(6),
        away_score: r.get(7), home_score: r.get(8), spread_line: r.get(9), total_line: r.get(10), away_coach: r.get(11), home_coach: r.get(12),
    }).collect())
}

async fn load_season(db: &tokio_postgres::Client, season: i32) -> Result<Arc<Season>, String> {
    if let Some((t, s)) = season_cache().lock().unwrap().get(&season) {
        if t.elapsed() < TTL { return Ok(s.clone()); }
    }
    let rows = db.query(
        "SELECT pl.game_id, pl.week, pl.posteam, pl.defteam, pl.qtr, pl.qsr, pl.yardline_100, pl.down, pl.ydstogo, pl.score_diff, \
                pl.play_type, pl.yards, pl.sack, pl.scramble, pl.interception, pl.complete, pl.fumble_lost, pl.touchdown, pl.td_team, \
                pl.two_point, pl.air_yards, pl.passer_id, pl.passer_name, pl.rusher_id, pl.rusher_name, pl.receiver_id, pl.receiver_name, \
                pl.penalty, pl.penalty_team, pl.penalty_yards, \
                m.epa, m.success, m.wp, m.pass_oe, m.cpoe, coalesce(m.qb_dropback,false), coalesce(m.is_rush,false), \
                coalesce(m.shotgun,false), coalesce(m.no_huddle,false), coalesce(m.first_down,false), m.run_gap, m.run_location, \
                f.game_id IS NOT NULL, f.is_play_action, f.is_screen_pass, f.is_motion, f.n_blitzers, f.n_pass_rushers, f.n_defense_box, \
                f.is_qb_out_of_pocket \
           FROM nfl_plays pl LEFT JOIN nfl_play_metrics m USING (game_id, play_id) LEFT JOIN nfl_ftn_charting f USING (game_id, play_id) \
          WHERE pl.season = $1 AND pl.season_type = 'REG' AND pl.posteam IS NOT NULL AND pl.defteam IS NOT NULL \
          ORDER BY pl.game_id, pl.play_id", &[&season]).await.map_err(|e| dberr(&e))?;
    let pair = |r: &tokio_postgres::Row, a: usize| -> Option<(String, String)> {
        let id: Option<String> = r.get(a);
        id.map(|i| (i, r.get::<_, Option<String>>(a + 1).unwrap_or_default()))
    };
    let plays: Vec<Play> = rows.iter().map(|r| Play {
        game_id: r.get(0), week: r.get(1), off: r.get(2), def: r.get(3), qtr: r.get::<_, Option<i32>>(4).unwrap_or(0), qsr: r.get(5),
        yl: r.get(6), down: r.get(7), togo: r.get(8), diff: r.get(9), ptype: r.get::<_, Option<String>>(10).unwrap_or_default(),
        yards: r.get::<_, Option<i32>>(11).unwrap_or(0), sack: r.get(12), scramble: r.get(13), int: r.get(14), complete: r.get(15),
        fumble_lost: r.get(16), td: r.get(17), td_team: r.get(18), two_point: r.get(19), air_yards: r.get(20),
        passer: pair(r, 21), rusher: pair(r, 23), receiver: pair(r, 25), penalty: r.get(27), penalty_team: r.get(28),
        epa: r.get::<_, Option<f32>>(30).map(|v| v as f64), success: r.get(31), wp: r.get::<_, Option<f32>>(32).map(|v| v as f64),
        pass_oe: r.get::<_, Option<f32>>(33).map(|v| v as f64), cpoe: r.get::<_, Option<f32>>(34).map(|v| v as f64),
        dropback: r.get(35), shotgun: r.get(37), no_huddle: r.get(38), first_down: r.get(39),
        run_gap: r.get(40), run_loc: r.get(41), charted: r.get(42), play_action: r.get(43), screen: r.get(44), motion: r.get(45),
        blitzers: r.get(46), rushers: r.get(47), boxn: r.get(48), oop: r.get(49),
    }).collect();
    let pos_rows = db.query(
        "SELECT DISTINCT ON (gsis_id) gsis_id, coalesce(position,'') FROM nfl_roster_weekly WHERE season BETWEEN $1 - 1 AND $1 \
          ORDER BY gsis_id, season DESC, week DESC", &[&season]).await.map_err(|e| dberr(&e))?;
    let pos = pos_rows.iter().map(|r| (r.get::<_, String>(0), r.get::<_, String>(1))).collect();
    let games = load_games(db, season - 1).await?;
    let s = Arc::new(Season { plays, pos, games });
    season_cache().lock().unwrap().insert(season, (Instant::now(), s.clone()));
    Ok(s)
}

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Dir { Up, Down, Usage }
#[derive(Clone, Copy, PartialEq)]
enum Fmt { Pct, D1, D2, Epa }

struct M { key: &'static str, label: &'static str, help: &'static str, dir: Dir, fmt: Fmt }

const METRICS: &[M] = &[
    M { key: "points_pg", label: "Points / game", help: "Points scored per game", dir: Dir::Up, fmt: Fmt::D1 },
    M { key: "yards_play", label: "Yards / play", help: "Scrimmage yards per offensive play (sacks count as pass plays)", dir: Dir::Up, fmt: Fmt::D2 },
    M { key: "yds_pg", label: "Total yards / game", help: "Scrimmage yards per game", dir: Dir::Up, fmt: Fmt::D1 },
    M { key: "epa_play", label: "EPA / play", help: "Expected points added per offensive play: how much each play moves the expected score", dir: Dir::Up, fmt: Fmt::Epa },
    M { key: "success", label: "Success %", help: "Share of plays with positive EPA", dir: Dir::Up, fmt: Fmt::Pct },
    M { key: "db_epa", label: "Dropback EPA / play", help: "EPA per dropback (passes, sacks, scrambles) - passing efficiency", dir: Dir::Up, fmt: Fmt::Epa },
    M { key: "db_success", label: "Dropback success %", help: "Share of dropbacks with positive EPA", dir: Dir::Up, fmt: Fmt::Pct },
    M { key: "db_explosive", label: "Dropback explosive %", help: "Share of dropbacks gaining 20+ yards", dir: Dir::Up, fmt: Fmt::Pct },
    M { key: "ypa", label: "QB yards / attempt", help: "Passing yards per pass attempt", dir: Dir::Up, fmt: Fmt::D2 },
    M { key: "comp_pct", label: "QB completion %", help: "Completions per pass attempt", dir: Dir::Up, fmt: Fmt::Pct },
    M { key: "cpoe", label: "CPOE", help: "Completion % over expected, given throw depth and situation", dir: Dir::Up, fmt: Fmt::D1 },
    M { key: "pass_yds_pg", label: "QB pass yards / game", help: "Passing yards per game", dir: Dir::Up, fmt: Fmt::D1 },
    M { key: "pass_att_pg", label: "QB pass attempts / game", help: "Pass attempts per game", dir: Dir::Up, fmt: Fmt::D1 },
    M { key: "comp_pg", label: "QB completions / game", help: "Completions per game", dir: Dir::Up, fmt: Fmt::D1 },
    M { key: "pass_td_pg", label: "Pass TDs / game", help: "Passing touchdowns per game", dir: Dir::Up, fmt: Fmt::D2 },
    M { key: "rush_epa", label: "Rushing EPA / play", help: "EPA per designed run (scrambles excluded) - rushing efficiency", dir: Dir::Up, fmt: Fmt::Epa },
    M { key: "rush_success", label: "Rush success %", help: "Share of designed runs with positive EPA", dir: Dir::Up, fmt: Fmt::Pct },
    M { key: "rush_explosive", label: "Rush explosive %", help: "Share of designed runs gaining 10+ yards", dir: Dir::Up, fmt: Fmt::Pct },
    M { key: "ypc", label: "Yards / carry", help: "Rushing yards per designed run", dir: Dir::Up, fmt: Fmt::D2 },
    M { key: "rush_yds_pg", label: "Rush yards / game", help: "Designed-run yards per game", dir: Dir::Up, fmt: Fmt::D1 },
    M { key: "carries_pg", label: "Carries / game", help: "Designed runs per game", dir: Dir::Up, fmt: Fmt::D1 },
    M { key: "rush_td_pg", label: "Rush TDs / game", help: "Rushing touchdowns per game", dir: Dir::Up, fmt: Fmt::D2 },
    M { key: "rb_epa", label: "RB rushing EPA / play", help: "EPA per carry by running backs", dir: Dir::Up, fmt: Fmt::Epa },
    M { key: "rb_rush_yds_pg", label: "RB rush yards / game", help: "Running-back rushing yards per game", dir: Dir::Up, fmt: Fmt::D1 },
    M { key: "rb_carries_pg", label: "RB carries / game", help: "Running-back carries per game", dir: Dir::Up, fmt: Fmt::D1 },
    M { key: "rb_ypc", label: "RB yards / carry", help: "Running-back yards per carry", dir: Dir::Up, fmt: Fmt::D2 },
    M { key: "rec_yds_wr_pg", label: "Receiving yards / game - WR", help: "Receiving yards per game to wide receivers", dir: Dir::Up, fmt: Fmt::D1 },
    M { key: "rec_yds_te_pg", label: "Receiving yards / game - TE", help: "Receiving yards per game to tight ends", dir: Dir::Up, fmt: Fmt::D1 },
    M { key: "rec_yds_rb_pg", label: "Receiving yards / game - RB", help: "Receiving yards per game to running backs", dir: Dir::Up, fmt: Fmt::D1 },
    M { key: "tgt_share_wr", label: "Target share - WR", help: "Share of targets that go to wide receivers", dir: Dir::Up, fmt: Fmt::Pct },
    M { key: "tgt_share_te", label: "Target share - TE", help: "Share of targets that go to tight ends", dir: Dir::Up, fmt: Fmt::Pct },
    M { key: "tgt_share_rb", label: "Target share - RB", help: "Share of targets that go to running backs", dir: Dir::Up, fmt: Fmt::Pct },
    M { key: "third_conv", label: "3rd-down conversion %", help: "Third downs converted to a first down or touchdown", dir: Dir::Up, fmt: Fmt::Pct },
    M { key: "fourth_conv", label: "4th-down conversion %", help: "Fourth-down attempts converted", dir: Dir::Up, fmt: Fmt::Pct },
    M { key: "rz_td", label: "Red-zone TD %", help: "Trips inside the 20 that end in a touchdown", dir: Dir::Up, fmt: Fmt::Pct },
    M { key: "pa_epa", label: "Play-action efficiency", help: "EPA per play-action dropback (FTN charting)", dir: Dir::Up, fmt: Fmt::Epa },
    M { key: "inside_epa", label: "Inside-run efficiency", help: "EPA per designed run between the tackles (guard/middle)", dir: Dir::Up, fmt: Fmt::Epa },
    M { key: "outside_epa", label: "Outside-run efficiency", help: "EPA per designed run outside (tackle/end)", dir: Dir::Up, fmt: Fmt::Epa },
    M { key: "sack_rate", label: "Sack rate", help: "Sacks per dropback", dir: Dir::Down, fmt: Fmt::Pct },
    M { key: "int_rate", label: "Interception rate", help: "Interceptions per pass attempt", dir: Dir::Down, fmt: Fmt::Pct },
    M { key: "turnovers_pg", label: "Turnovers / game", help: "Interceptions plus lost fumbles per game (defense: takeaways)", dir: Dir::Down, fmt: Fmt::D2 },
    M { key: "pen_pg", label: "Penalties / game", help: "Accepted penalties against the team per game", dir: Dir::Down, fmt: Fmt::D1 },
    M { key: "proe", label: "Pass rate over expected", help: "Pass rate minus the rate expected from down, distance, field position, score and clock (PROE)", dir: Dir::Usage, fmt: Fmt::D1 },
    M { key: "neutral_pass", label: "Neutral early-down pass rate", help: "Dropback rate on 1st/2nd down with win probability 20-80%", dir: Dir::Usage, fmt: Fmt::Pct },
    M { key: "pa_rate", label: "Play-action rate", help: "Share of charted dropbacks with play action (FTN)", dir: Dir::Usage, fmt: Fmt::Pct },
    M { key: "screen_rate", label: "Screen rate", help: "Share of charted passes that are screens (FTN)", dir: Dir::Usage, fmt: Fmt::Pct },
    M { key: "motion_rate", label: "Pre-snap motion rate", help: "Share of charted plays with motion (FTN)", dir: Dir::Usage, fmt: Fmt::Pct },
    M { key: "blitz_rate", label: "Blitz rate", help: "Share of charted dropbacks with 1+ blitzer (FTN). Offense: blitzes faced; defense: blitzes sent", dir: Dir::Usage, fmt: Fmt::Pct },
    M { key: "rushers_avg", label: "Pass rushers / dropback", help: "Average pass rushers on charted dropbacks (FTN)", dir: Dir::Usage, fmt: Fmt::D2 },
    M { key: "box_avg", label: "Defenders in the box", help: "Average box count on charted designed runs (FTN)", dir: Dir::Usage, fmt: Fmt::D2 },
    M { key: "oop_rate", label: "QB out of pocket", help: "Share of charted dropbacks where the QB left the pocket (FTN)", dir: Dir::Usage, fmt: Fmt::Pct },
    M { key: "shotgun_rate", label: "Shotgun rate", help: "Share of plays from shotgun", dir: Dir::Usage, fmt: Fmt::Pct },
    M { key: "no_huddle_rate", label: "No-huddle rate", help: "Share of plays run without a huddle", dir: Dir::Usage, fmt: Fmt::Pct },
    M { key: "inside_share", label: "Inside-run share", help: "Share of designed runs between the tackles", dir: Dir::Usage, fmt: Fmt::Pct },
    M { key: "fourth_go", label: "4th-down go rate", help: "Goes for it on 4th-and-4-or-less beyond its own 40", dir: Dir::Usage, fmt: Fmt::Pct },
    M { key: "plays_pg", label: "Plays / game", help: "Offensive scrimmage plays per game", dir: Dir::Usage, fmt: Fmt::D1 },
    M { key: "sec_per_play", label: "Seconds / play (neutral)", help: "Seconds between snaps in one-score games, quarters 1-3 (lower = faster)", dir: Dir::Usage, fmt: Fmt::D1 },
];

fn metric(key: &str) -> Option<&'static M> { METRICS.iter().find(|m| m.key == key) }

/// What a defense "allows" on the Rankings tab (screenshot parity).
const DEF_ALLOWS: &[&str] = &["rush_explosive", "db_explosive", "db_success", "ypa", "tgt_share_wr", "rb_epa", "rec_yds_te_pg",
    "rec_yds_wr_pg", "pass_yds_pg", "rb_ypc", "db_epa", "tgt_share_te", "comp_pct", "rb_rush_yds_pg", "pass_att_pg", "comp_pg",
    "rb_carries_pg", "rush_success", "rec_yds_rb_pg", "tgt_share_rb", "points_pg", "sack_rate"];

#[derive(Default, Clone)]
struct Acc { v: HashMap<&'static str, (f64, f64)>, games: HashSet<String> }
impl Acc {
    fn add(&mut self, k: &'static str, num: f64, den: f64) { let e = self.v.entry(k).or_default(); e.0 += num; e.1 += den; }
    fn rate(&mut self, k: &'static str, hit: bool) { self.add(k, if hit { 1.0 } else { 0.0 }, 1.0); }
    fn mean(&mut self, k: &'static str, x: Option<f64>) { if let Some(x) = x { self.add(k, x, 1.0); } }
    fn pg(&mut self, k: &'static str, x: f64) { self.add(k, x, 0.0); }
    fn get(&self, k: &str) -> Option<f64> {
        let (n, d) = *self.v.get(k)?;
        if d > 0.0 { Some(n / d) } else { None }
    }
}

const PG_KEYS: &[&str] = &["points_pg", "yds_pg", "pass_yds_pg", "pass_att_pg", "comp_pg", "pass_td_pg", "rush_yds_pg", "carries_pg",
    "rush_td_pg", "rb_rush_yds_pg", "rb_carries_pg", "rec_yds_wr_pg", "rec_yds_te_pg", "rec_yds_rb_pg", "turnovers_pg", "pen_pg", "plays_pg"];

fn pos_group(pos: Option<&String>) -> Option<&'static str> {
    match pos.map(|s| s.as_str()) {
        Some("WR") => Some("wr"), Some("TE") => Some("te"), Some("RB" | "FB" | "HB") => Some("rb"), Some("QB") => Some("qb"), _ => None,
    }
}

/// Team side accumulators for plays with week < `before_week`.
fn accumulate(s: &Season, before_week: i32) -> HashMap<(String, bool), Acc> {
    let mut acc: HashMap<(String, bool), Acc> = HashMap::new();
    let mut prev: Option<&Play> = None;
    let mut trip: Option<(String, String)> = None; // (game, team) currently inside the 20
    for p in s.plays.iter().filter(|p| p.week < before_week) {
        let new_game = prev.map(|q| q.game_id != p.game_id).unwrap_or(true);
        if new_game { trip = None; }
        for side in [true, false] {
            let team = if side { &p.off } else { &p.def };
            let a = acc.entry((team.clone(), side)).or_default();
            a.games.insert(p.game_id.clone());
            if p.int || p.fumble_lost { a.pg("turnovers_pg", 1.0); }
            if !p.scrim() {
                if p.down == Some(4) && p.togo.map(|t| t <= 4).unwrap_or(false) && p.yl.map(|y| y <= 60).unwrap_or(false)
                    && (p.ptype == "punt" || p.ptype == "field_goal") { a.rate("fourth_go", false); }
                continue;
            }
            a.pg("plays_pg", 1.0);
            a.pg("yds_pg", p.yards as f64);
            a.mean("yards_play", Some(p.yards as f64));
            a.mean("epa_play", p.epa);
            if let Some(sx) = p.success { a.rate("success", sx); }
            if p.shotgun || p.charted { a.rate("shotgun_rate", p.shotgun); }
            a.rate("no_huddle_rate", p.no_huddle);
            if let Some(m) = p.motion { a.rate("motion_rate", m); }
            a.mean("proe", p.pass_oe);
            if p.down == Some(3) { a.rate("third_conv", p.first_down || p.td); }
            if p.down == Some(4) {
                a.rate("fourth_conv", p.first_down || p.td);
                if p.togo.map(|t| t <= 4).unwrap_or(false) && p.yl.map(|y| y <= 60).unwrap_or(false) { a.rate("fourth_go", true); }
            }
            if p.down.map(|d| d <= 2).unwrap_or(false) && p.wp.map(|w| (0.2..=0.8).contains(&w)).unwrap_or(false) {
                a.rate("neutral_pass", p.is_dropback());
            }
            if p.is_dropback() {
                a.mean("db_epa", p.epa);
                if let Some(sx) = p.success { a.rate("db_success", sx); }
                a.rate("db_explosive", !p.sack && p.yards >= 20);
                a.rate("sack_rate", p.sack);
                if let Some(pa) = p.play_action { a.rate("pa_rate", pa); if pa { a.mean("pa_epa", p.epa); } }
                if let Some(b) = p.blitzers { a.rate("blitz_rate", b > 0); }
                if let Some(r) = p.rushers { a.mean("rushers_avg", Some(r as f64)); }
                if let Some(o) = p.oop { a.rate("oop_rate", o); }
            }
            if p.attempt() {
                a.pg("pass_att_pg", 1.0);
                a.pg("pass_yds_pg", p.yards as f64);
                a.mean("ypa", Some(p.yards as f64));
                a.rate("comp_pct", p.complete);
                a.rate("int_rate", p.int);
                a.mean("cpoe", p.cpoe);
                if p.complete { a.pg("comp_pg", 1.0); }
                if p.td && p.td_team.as_deref() == Some(&p.off) && !p.int { a.pg("pass_td_pg", 1.0); }
                if let Some(sc) = p.screen { a.rate("screen_rate", sc); }
                if let Some((rid, _)) = &p.receiver {
                    let g = pos_group(s.pos.get(rid));
                    a.add("tgt_share_wr", if g == Some("wr") { 1.0 } else { 0.0 }, 1.0);
                    a.add("tgt_share_te", if g == Some("te") { 1.0 } else { 0.0 }, 1.0);
                    a.add("tgt_share_rb", if g == Some("rb") { 1.0 } else { 0.0 }, 1.0);
                    if p.complete {
                        match g { Some("wr") => a.pg("rec_yds_wr_pg", p.yards as f64), Some("te") => a.pg("rec_yds_te_pg", p.yards as f64),
                            Some("rb") => a.pg("rec_yds_rb_pg", p.yards as f64), _ => {} }
                    }
                }
            }
            if p.designed_run() {
                a.pg("carries_pg", 1.0);
                a.pg("rush_yds_pg", p.yards as f64);
                a.mean("ypc", Some(p.yards as f64));
                a.mean("rush_epa", p.epa);
                if let Some(sx) = p.success { a.rate("rush_success", sx); }
                a.rate("rush_explosive", p.yards >= 10);
                if p.td && p.td_team.as_deref() == Some(&p.off) { a.pg("rush_td_pg", 1.0); }
                if let Some(b) = p.boxn { a.mean("box_avg", Some(b as f64)); }
                let inside = p.run_gap.as_deref() == Some("guard") || p.run_loc.as_deref() == Some("middle");
                if p.run_gap.is_some() || p.run_loc.is_some() {
                    a.rate("inside_share", inside);
                    a.mean(if inside { "inside_epa" } else { "outside_epa" }, p.epa);
                }
                if pos_group(p.rusher.as_ref().and_then(|r| s.pos.get(&r.0))) == Some("rb") {
                    a.pg("rb_carries_pg", 1.0);
                    a.pg("rb_rush_yds_pg", p.yards as f64);
                    a.mean("rb_ypc", Some(p.yards as f64));
                    a.mean("rb_epa", p.epa);
                }
            }
        }
        // penalties against the flagged team (either side of the ball)
        if p.penalty {
            if let Some(t) = &p.penalty_team {
                let side = *t == p.off;
                acc.entry((t.clone(), side)).or_default().pg("pen_pg", 1.0);
            }
        }
        // red-zone trips and tempo (offense only)
        if p.scrim() {
            let in_rz = p.yl.map(|y| y <= 20).unwrap_or(false);
            let key = (p.game_id.clone(), p.off.clone());
            if in_rz && trip.as_ref() != Some(&key) {
                trip = Some(key.clone());
                acc.entry((p.off.clone(), true)).or_default().add("rz_td", 0.0, 1.0);
                acc.entry((p.def.clone(), false)).or_default().add("rz_td", 0.0, 1.0);
            }
            if trip.as_ref().map(|t| t.1 != p.off).unwrap_or(false) || !in_rz && trip.is_some() && p.yl.map(|y| y > 30).unwrap_or(false) { trip = None; }
            if p.td && p.td_team.as_deref() == Some(&p.off) && trip.as_ref() == Some(&key) {
                acc.entry((p.off.clone(), true)).or_default().add("rz_td", 1.0, 0.0);
                acc.entry((p.def.clone(), false)).or_default().add("rz_td", 1.0, 0.0);
                trip = None;
            }
            if let Some(q) = prev {
                if q.game_id == p.game_id && q.off == p.off && q.qtr == p.qtr && p.qtr <= 3 && q.scrim()
                    && p.diff.map(|d| d.abs() <= 8).unwrap_or(false) {
                    if let (Some(a), Some(b)) = (q.qsr, p.qsr) {
                        let dt = (a - b) as f64;
                        if dt > 0.0 && dt < 60.0 { acc.entry((p.off.clone(), true)).or_default().mean("sec_per_play", Some(dt)); }
                    }
                }
            }
            prev = Some(p);
        } else if new_game {
            prev = Some(p);
        }
    }
    // points from the schedule
    let season_games: HashSet<&str> = s.plays.iter().filter(|p| p.week < before_week).map(|p| p.game_id.as_str()).collect();
    for g in s.games.iter().filter(|g| season_games.contains(g.game_id.as_str())) {
        if let (Some(h), Some(a)) = (g.home_score, g.away_score) {
            acc.entry((g.home.clone(), true)).or_default().pg("points_pg", h as f64);
            acc.entry((g.home.clone(), false)).or_default().pg("points_pg", a as f64);
            acc.entry((g.away.clone(), true)).or_default().pg("points_pg", a as f64);
            acc.entry((g.away.clone(), false)).or_default().pg("points_pg", h as f64);
        }
    }
    for a in acc.values_mut() {
        let n = a.games.len() as f64;
        for k in PG_KEYS {
            if let Some(e) = a.v.get_mut(k) { e.1 = n; }
        }
    }
    acc
}

/// League table for one (season, before_week): value + rank per team/side/metric.
pub struct Profile {
    pub season: i32,
    teams: BTreeMap<String, TeamP>,
    league: HashMap<&'static str, f64>,
}
#[derive(Default)]
struct TeamP { games: usize, off: HashMap<&'static str, (f64, usize)>, def: HashMap<&'static str, (f64, usize)> }

fn rank_side(acc: &HashMap<(String, bool), Acc>, side: bool, m: &M) -> HashMap<String, (f64, usize)> {
    let mut v: Vec<(String, f64)> = acc.iter().filter(|((_, s), _)| *s == side)
        .filter_map(|((t, _), a)| a.get(m.key).map(|x| (t.clone(), x))).collect();
    // rank 1 = best offense / stingiest defense; usage metrics rank by frequency
    let desc = match (m.dir, side) {
        (Dir::Usage, _) => m.key != "sec_per_play",
        (Dir::Up, true) | (Dir::Down, false) => true,
        (Dir::Up, false) | (Dir::Down, true) => false,
    };
    v.sort_by(|a, b| if desc { b.1.total_cmp(&a.1) } else { a.1.total_cmp(&b.1) });
    v.into_iter().enumerate().map(|(i, (t, x))| (t, (x, i + 1))).collect()
}

pub async fn profile(db: &tokio_postgres::Client, season: i32, before_week: i32) -> Result<Arc<Profile>, String> {
    static P: OnceLock<Mutex<HashMap<(i32, i32), (Instant, Arc<Profile>)>>> = OnceLock::new();
    let pc = P.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some((t, p)) = pc.lock().unwrap().get(&(season, before_week)) {
        if t.elapsed() < TTL { return Ok(p.clone()); }
    }
    let s = load_season(db, season).await?;
    let acc = accumulate(&s, before_week);
    let mut teams: BTreeMap<String, TeamP> = BTreeMap::new();
    let mut league = HashMap::new();
    for m in METRICS {
        for side in [true, false] {
            for (t, vr) in rank_side(&acc, side, m) {
                let tp = teams.entry(t).or_default();
                if side { tp.off.insert(m.key, vr); } else { tp.def.insert(m.key, vr); }
            }
        }
        let vals: Vec<f64> = acc.iter().filter(|((_, s), _)| *s).filter_map(|(_, a)| a.get(m.key)).collect();
        if !vals.is_empty() { league.insert(m.key, vals.iter().sum::<f64>() / vals.len() as f64); }
    }
    for ((t, side), a) in &acc {
        if *side { if let Some(tp) = teams.get_mut(t) { tp.games = a.games.len(); } }
    }
    let p = Arc::new(Profile { season, teams, league });
    pc.lock().unwrap().insert((season, before_week), (Instant::now(), p.clone()));
    Ok(p)
}

impl Profile {
    fn n_teams(&self) -> usize { self.teams.len().max(1) }
    fn cell(&self, team: &str, off: bool, key: &str) -> Option<(f64, usize)> {
        let t = self.teams.get(team)?;
        (if off { &t.off } else { &t.def }).get(key).copied()
    }
    fn vs_league(&self, key: &str, v: f64) -> Option<f64> {
        let lg = *self.league.get(key)?;
        let m = metric(key)?;
        // EPA and the "over expected" metrics compare as differences, the rest as % of league average
        Some(if m.fmt == Fmt::Epa || m.key == "cpoe" || m.key == "proe" { r3(v - lg) }
             else if lg.abs() > 1e-9 { r1((v - lg) / lg.abs() * 100.0) } else { 0.0 })
    }
    fn show(&self, team: &str, off: bool, key: &str) -> Value {
        let m = metric(key).unwrap();
        match self.cell(team, off, key) {
            Some((v, rank)) => json!({ "key": key, "label": m.label, "help": m.help, "fmt": fmt_name(m.fmt), "value": round_for(m.fmt, v),
                "rank": rank, "of": self.n_teams(), "vs_league": self.vs_league(key, v), "usage": m.dir == Dir::Usage }),
            None => json!({ "key": key, "label": m.label, "help": m.help, "fmt": fmt_name(m.fmt), "value": null, "rank": null, "of": self.n_teams() }),
        }
    }
    fn games(&self, team: &str) -> usize { self.teams.get(team).map(|t| t.games).unwrap_or(0) }
}

fn fmt_name(f: Fmt) -> &'static str { match f { Fmt::Pct => "pct", Fmt::D1 => "d1", Fmt::D2 => "d2", Fmt::Epa => "epa" } }
fn round_for(f: Fmt, v: f64) -> f64 { match f { Fmt::Pct => (v * 10000.0).round() / 10000.0, Fmt::D1 => r1(v), Fmt::D2 => (v * 100.0).round() / 100.0, Fmt::Epa => r3(v) } }
fn pct(v: f64) -> String { format!("{:.1}%", v * 100.0) }
fn epa(v: f64) -> String { format!("{}{:.2}", if v >= 0.0 { "+" } else { "" }, v) }

// ---------------------------------------------------------------------------
// Matchup sections
// ---------------------------------------------------------------------------

fn side_table(p: &Profile, off: &str, def: &str, keys: &[&str]) -> Vec<Value> {
    keys.iter().map(|k| json!({ "key": k, "offense": p.show(off, true, k), "defense": p.show(def, false, k),
        "label": metric(k).map(|m| m.label), "help": metric(k).map(|m| m.help) })).collect()
}

fn v(p: &Profile, team: &str, off: bool, key: &str) -> Option<f64> { p.cell(team, off, key).map(|x| x.0) }
fn rk(p: &Profile, team: &str, off: bool, key: &str) -> String { p.cell(team, off, key).map(|x| format!("#{}", x.1)).unwrap_or_else(|| "-".into()) }

fn has_ball(p: &Profile, off: &str, def: &str) -> Value {
    let air_note = match (v(p, def, false, "blitz_rate"), v(p, off, true, "db_epa"), v(p, def, false, "db_epa")) {
        (Some(b), Some(o), Some(d)) => format!("{def} sends a blitz on {} of charted dropbacks ({}). {off} produces {} EPA per dropback ({}); {def} allows {} ({}).",
            pct(b), rk(p, def, false, "blitz_rate"), epa(o), rk(p, off, true, "db_epa"), epa(d), rk(p, def, false, "db_epa")),
        _ => String::new(),
    };
    let ground_note = match (v(p, off, true, "inside_share"), v(p, off, true, "rush_epa")) {
        (Some(i), Some(_)) => format!("{off} runs between the tackles on {} of designed runs. Its rushing efficiency ranks {} against {def}'s {} run defense; {def} puts {} defenders in the box on average.",
            pct(i), rk(p, off, true, "rush_epa"), rk(p, def, false, "rush_epa"), v(p, def, false, "box_avg").map(|b| format!("{b:.1}")).unwrap_or("-".into())),
        _ => String::new(),
    };
    let qb_note = match (v(p, off, true, "sack_rate"), v(p, def, false, "sack_rate")) {
        (Some(a), Some(g)) => format!("{off} has allowed a sack on {} of dropbacks ({}). {def} has generated one on {} ({}). {def} rushes {} on average.",
            pct(a), rk(p, off, true, "sack_rate"), pct(g), rk(p, def, false, "sack_rate"),
            v(p, def, false, "rushers_avg").map(|r| format!("{r:.1}")).unwrap_or("-".into())),
        _ => String::new(),
    };
    json!({
        "offense": off, "defense": def,
        "sections": [
            { "title": "Through the air", "note": air_note, "explore": "receiving",
              "rows": side_table(p, off, def, &["db_epa", "db_success", "db_explosive", "ypa", "cpoe", "pa_rate", "pa_epa", "screen_rate", "blitz_rate"]) },
            { "title": "On the ground", "note": ground_note, "explore": "rushing",
              "rows": side_table(p, off, def, &["rush_epa", "rush_success", "rush_explosive", "ypc", "inside_share", "inside_epa", "outside_epa", "box_avg"]) },
            { "title": "Can the quarterback stay comfortable?", "note": qb_note, "explore": "qb",
              "rows": side_table(p, off, def, &["sack_rate", "blitz_rate", "rushers_avg", "oop_rate", "int_rate", "turnovers_pg"]) },
        ]
    })
}

fn stands_out(p: &Profile, off: &str, def: &str) -> Value {
    let gap = |k: &str| -> Option<(i64, usize, usize)> {
        let (o, d) = (p.cell(off, true, k)?.1, p.cell(def, false, k)?.1);
        Some((d as i64 - o as i64, o, d))
    };
    let pass = gap("db_epa");
    let rush = gap("rush_epa");
    let pick = match (pass, rush) {
        (Some(a), Some(b)) => if a.0.abs() >= b.0.abs() { Some(("passing", a)) } else { Some(("rushing", b)) },
        (Some(a), None) => Some(("passing", a)),
        (None, Some(b)) => Some(("rushing", b)),
        _ => None,
    };
    match pick {
        Some((what, (g, o, d))) => json!({ "team": off, "text": format!(
            "{off} ranks #{o} in {what} efficiency; {def} ranks #{d} defending it. This is the larger efficiency-rank {} for {off}'s offense.",
            if g >= 0 { "advantage" } else { "challenge" }), "edge": g }),
        None => json!({ "team": off, "text": format!("Not enough {} plays yet to compare.", p.season), "edge": 0 }),
    }
}

fn rankings(p: &Profile, def: &str, faces: &str) -> Value {
    let mut rows: Vec<Value> = DEF_ALLOWS.iter().map(|k| p.show(def, false, k)).filter(|r| !r["rank"].is_null()).collect();
    rows.sort_by_key(|r| r["rank"].as_u64().unwrap_or(99));
    let run = p.cell(def, false, "rush_epa").map(|x| x.1);
    let pass = p.cell(def, false, "db_epa").map(|x| x.1);
    json!({ "team": def, "faces": faces, "games": p.games(def), "rows": rows,
        "proe": p.show(faces, true, "proe"), "run_defense_rank": run, "pass_defense_rank": pass })
}

const TEAM_STATS: &[&str] = &["points_pg", "yds_pg", "yards_play", "epa_play", "success", "pass_yds_pg", "rush_yds_pg", "third_conv",
    "rz_td", "turnovers_pg", "sack_rate", "pen_pg", "plays_pg"];
const SITUATIONS: &[&str] = &["third_conv", "fourth_conv", "rz_td", "neutral_pass", "proe", "fourth_go", "sec_per_play", "no_huddle_rate"];
const COACHING: &[&str] = &["proe", "neutral_pass", "fourth_go", "sec_per_play", "shotgun_rate", "no_huddle_rate", "motion_rate", "pa_rate", "screen_rate"];
const COACHING_DEF: &[&str] = &["blitz_rate", "rushers_avg", "box_avg"];

fn team_block(p: &Profile, team: &str, keys: &[&str], off: bool) -> Vec<Value> { keys.iter().map(|k| p.show(team, off, k)).collect() }

// ---------------------------------------------------------------------------
// Game logs, records
// ---------------------------------------------------------------------------

fn team_log(games: &[GameRow], team: &str, before: Option<NaiveDate>, n: usize) -> Value {
    let mut log: Vec<Value> = games.iter().rev()
        .filter(|g| (g.home == team || g.away == team) && g.home_score.is_some() && g.away_score.is_some())
        .filter(|g| before.map(|b| g.day.map(|d| d < b).unwrap_or(false)).unwrap_or(true))
        .take(n)
        .map(|g| {
            let home = g.home == team;
            let (us, them) = if home { (g.home_score.unwrap(), g.away_score.unwrap()) } else { (g.away_score.unwrap(), g.home_score.unwrap()) };
            let margin = (us - them) as f64;
            // nflverse spread_line = home team's expected margin
            let line = g.spread_line.map(|s| if home { -s } else { s });
            let ats = line.map(|l| { let c = margin + l; if c > 0.0 { "W" } else if c < 0.0 { "L" } else { "P" } });
            let ou = g.total_line.map(|t| { let tot = (us + them) as f64; if tot > t { "O" } else if tot < t { "U" } else { "P" } });
            json!({ "game_id": g.game_id, "season": g.season, "week": g.week, "game_type": g.game_type, "date": g.day,
                "home": home, "opponent": if home { &g.away } else { &g.home }, "score": format!("{us}-{them}"),
                "result": if us > them { "W" } else if us < them { "L" } else { "T" },
                "line": line, "ats": ats, "total_line": g.total_line, "ou": ou })
        }).collect();
    let tally = |key: &str, a: &str, b: &str, c: &str| -> String {
        let cnt = |x: &str| log.iter().filter(|r| r[key] == x).count();
        format!("{}-{}-{}", cnt(a), cnt(b), cnt(c))
    };
    let su = tally("result", "W", "L", "T");
    let ats = tally("ats", "W", "L", "P");
    let ou = tally("ou", "O", "U", "P");
    log.iter_mut().for_each(|_| {});
    json!({ "team": team, "games": log, "su": su, "ats": ats, "ou": ou })
}

fn coach_record(games: &[GameRow], season: i32, coach: &str, before: Option<NaiveDate>) -> String {
    let (mut w, mut l, mut t) = (0, 0, 0);
    for g in games.iter().filter(|g| g.season == season && g.home_score.is_some()) {
        if before.map(|b| g.day.map(|d| d >= b).unwrap_or(true)).unwrap_or(false) { continue; }
        let (us, them) = if g.home_coach.as_deref() == Some(coach) { (g.home_score, g.away_score) }
            else if g.away_coach.as_deref() == Some(coach) { (g.away_score, g.home_score) } else { continue };
        match us.unwrap_or(0).cmp(&them.unwrap_or(0)) { std::cmp::Ordering::Greater => w += 1, std::cmp::Ordering::Less => l += 1, _ => t += 1 }
    }
    if t > 0 { format!("{w}-{l}-{t}") } else { format!("{w}-{l}") }
}

// ---------------------------------------------------------------------------
// Player matchups
// ---------------------------------------------------------------------------

#[derive(Default)]
struct PStat { name: String, team: String, games: HashSet<String>, v: HashMap<&'static str, f64> }
impl PStat { fn add(&mut self, k: &'static str, x: f64) { *self.v.entry(k).or_default() += x; } fn g(&self, k: &str) -> f64 { self.v.get(k).copied().unwrap_or(0.0) } }

fn players(s: &Season, before_week: i32, teams: &[&str]) -> (HashMap<String, PStat>, HashMap<String, HashMap<&'static str, f64>>) {
    let mut ps: HashMap<String, PStat> = HashMap::new();
    let mut team_tot: HashMap<String, HashMap<&'static str, f64>> = HashMap::new();
    for p in s.plays.iter().filter(|p| p.week < before_week && p.scrim() && teams.contains(&p.off.as_str())) {
        let tt = team_tot.entry(p.off.clone()).or_default();
        let rz = p.yl.map(|y| y <= 20).unwrap_or(false);
        let mut touch = |id: &str, name: &str, f: &mut dyn FnMut(&mut PStat)| {
            let e = ps.entry(id.to_string()).or_default();
            if e.name.is_empty() { e.name = name.to_string(); }
            e.team = p.off.clone();
            e.games.insert(p.game_id.clone());
            f(e);
        };
        if p.is_dropback() {
            if let Some((id, nm)) = &p.passer {
                touch(id, nm, &mut |e| {
                    e.add("dropbacks", 1.0);
                    if let Some(x) = p.epa { e.add("db_epa", x); }
                    if p.sack { e.add("sacks", 1.0); }
                    if p.attempt() {
                        e.add("att", 1.0); e.add("pass_yds", p.yards as f64);
                        if p.complete { e.add("comp", 1.0); }
                        if p.int { e.add("int", 1.0); }
                        if p.td && !p.int && p.td_team.as_deref() == Some(&p.off) { e.add("pass_td", 1.0); }
                        if let Some(c) = p.cpoe { e.add("cpoe_sum", c); e.add("cpoe_n", 1.0); }
                    }
                });
            }
        }
        if p.attempt() {
            *tt.entry("targets").or_default() += 1.0;
            if rz { *tt.entry("rz_targets").or_default() += 1.0; }
            *tt.entry("air").or_default() += p.air_yards.unwrap_or(0) as f64;
            if let Some((id, nm)) = &p.receiver {
                touch(id, nm, &mut |e| {
                    e.add("targets", 1.0);
                    e.add("air", p.air_yards.unwrap_or(0) as f64);
                    if rz { e.add("rz_targets", 1.0); }
                    if p.complete { e.add("rec", 1.0); e.add("rec_yds", p.yards as f64); }
                    if p.td && p.complete && p.td_team.as_deref() == Some(&p.off) { e.add("rec_td", 1.0); }
                });
            }
        }
        if p.ptype == "run" {
            *tt.entry("carries").or_default() += 1.0;
            if rz { *tt.entry("rz_carries").or_default() += 1.0; }
            if let Some((id, nm)) = &p.rusher {
                touch(id, nm, &mut |e| {
                    e.add("carries", 1.0); e.add("rush_yds", p.yards as f64);
                    if let Some(x) = p.epa { e.add("rush_epa", x); }
                    if rz { e.add("rz_carries", 1.0); }
                    if p.td && p.td_team.as_deref() == Some(&p.off) { e.add("rush_td", 1.0); }
                });
            }
        }
    }
    (ps, team_tot)
}

fn player_matchups(s: &Season, p: &Profile, before_week: i32, home: &str, away: &str, proj: &HashMap<String, Value>) -> Value {
    let (ps, tt) = players(s, before_week, &[home, away]);
    let opp = |t: &str| if t == home { away.to_string() } else { home.to_string() };
    let share = |t: &str, k: &str, x: f64| -> f64 { let d = tt.get(t).and_then(|m| m.get(k)).copied().unwrap_or(0.0); if d > 0.0 { x / d } else { 0.0 } };
    let mut receiving = Vec::new();
    let mut rushing = Vec::new();
    let mut qbs = Vec::new();
    let mut tds = Vec::new();
    for (id, st) in &ps {
        let g = st.games.len().max(1) as f64;
        let pos = s.pos.get(id).cloned().unwrap_or_default();
        let grp = pos_group(s.pos.get(id));
        let o = opp(&st.team);
        let pj = proj.get(id).cloned().unwrap_or(Value::Null);
        let base = json!({ "id": id, "name": st.name, "team": st.team, "pos": pos, "games": st.games.len(), "opponent": o, "projection": pj });
        if st.g("targets") >= 3.0 && matches!(grp, Some("wr" | "te" | "rb")) {
            let k = match grp { Some("wr") => "rec_yds_wr_pg", Some("te") => "rec_yds_te_pg", _ => "rec_yds_rb_pg" };
            let mut b = base.clone();
            b["stats"] = json!({ "targets_pg": r1(st.g("targets") / g), "target_share": r3(share(&st.team, "targets", st.g("targets"))),
                "air_share": r3(share(&st.team, "air", st.g("air"))), "rec_pg": r1(st.g("rec") / g), "rec_yds_pg": r1(st.g("rec_yds") / g),
                "rec_td": st.g("rec_td"), "adot": if st.g("targets") > 0.0 { r1(st.g("air") / st.g("targets")) } else { 0.0 } });
            b["opp_allows"] = p.show(&o, false, k);
            b["opp_target_share"] = p.show(&o, false, match grp { Some("wr") => "tgt_share_wr", Some("te") => "tgt_share_te", _ => "tgt_share_rb" });
            receiving.push(b);
        }
        if st.g("carries") >= 5.0 && grp != Some("qb") || st.g("carries") >= 15.0 {
            let mut b = base.clone();
            b["stats"] = json!({ "carries_pg": r1(st.g("carries") / g), "rush_share": r3(share(&st.team, "carries", st.g("carries"))),
                "rush_yds_pg": r1(st.g("rush_yds") / g), "ypc": if st.g("carries") > 0.0 { (st.g("rush_yds") / st.g("carries") * 100.0).round() / 100.0 } else { 0.0 },
                "epa_carry": if st.g("carries") > 0.0 { r3(st.g("rush_epa") / st.g("carries")) } else { 0.0 }, "rush_td": st.g("rush_td") });
            b["opp_allows"] = p.show(&o, false, if grp == Some("rb") { "rb_rush_yds_pg" } else { "rush_yds_pg" });
            b["opp_efficiency"] = p.show(&o, false, if grp == Some("rb") { "rb_epa" } else { "rush_epa" });
            rushing.push(b);
        }
        if st.g("att") >= 20.0 {
            let mut b = base.clone();
            b["stats"] = json!({ "att_pg": r1(st.g("att") / g), "comp_pct": r3(st.g("comp") / st.g("att")), "pass_yds_pg": r1(st.g("pass_yds") / g),
                "ypa": (st.g("pass_yds") / st.g("att") * 100.0).round() / 100.0, "pass_td": st.g("pass_td"), "int": st.g("int"),
                "epa_db": r3(st.g("db_epa") / st.g("dropbacks").max(1.0)), "sack_rate": r3(st.g("sacks") / st.g("dropbacks").max(1.0)),
                "cpoe": if st.g("cpoe_n") > 0.0 { r1(st.g("cpoe_sum") / st.g("cpoe_n")) } else { 0.0 } });
            b["opp_allows"] = p.show(&o, false, "pass_yds_pg");
            b["opp_efficiency"] = p.show(&o, false, "db_epa");
            b["opp_sack_rate"] = p.show(&o, false, "sack_rate");
            qbs.push(b);
        }
        let td = st.g("rec_td") + st.g("rush_td");
        let rz = st.g("rz_targets") + st.g("rz_carries");
        if rz >= 2.0 || td >= 1.0 {
            let mut b = base.clone();
            let rz_den = tt.get(&st.team).map(|m| m.get("rz_targets").copied().unwrap_or(0.0) + m.get("rz_carries").copied().unwrap_or(0.0)).unwrap_or(0.0);
            b["stats"] = json!({ "tds": td, "td_pg": (td / g * 100.0).round() / 100.0, "rz_touches": rz, "rz_share": if rz_den > 0.0 { r3(rz / rz_den) } else { 0.0 },
                "rz_targets": st.g("rz_targets"), "rz_carries": st.g("rz_carries") });
            b["opp_allows"] = p.show(&o, false, "rz_td");
            tds.push(b);
        }
    }
    let sort = |v: &mut Vec<Value>, k: &str| v.sort_by(|a, b| b["stats"][k].as_f64().unwrap_or(0.0).total_cmp(&a["stats"][k].as_f64().unwrap_or(0.0)));
    sort(&mut receiving, "rec_yds_pg");
    sort(&mut rushing, "rush_yds_pg");
    sort(&mut qbs, "att_pg");
    sort(&mut tds, "rz_touches");
    json!({ "receiving": receiving, "rushing": rushing, "qb": qbs, "touchdowns": tds })
}

// ---------------------------------------------------------------------------
// Public: analysis payload for one game
// ---------------------------------------------------------------------------

pub struct GameKey { pub game_id: Option<String>, pub season: i32, pub week: i32, pub home: String, pub away: String, pub day: Option<NaiveDate> }

/// Resolve an ESPN event (or teams + date) to the nflverse game row.
pub async fn resolve(db: &tokio_postgres::Client, espn_id: Option<&str>, home: &str, away: &str, day: Option<NaiveDate>) -> Result<GameKey, String> {
    let row = if let Some(e) = espn_id {
        db.query_opt("SELECT game_id, season, coalesce(week,0), gameday, home_team, away_team FROM nfl_game_info WHERE espn_id=$1", &[&e]).await.map_err(|e| dberr(&e))?
    } else { None };
    let row = match row {
        Some(r) => Some(r),
        None => match day {
            Some(d) => db.query_opt("SELECT game_id, season, coalesce(week,0), gameday, home_team, away_team FROM nfl_game_info \
                                     WHERE home_team=$1 AND away_team=$2 AND gameday BETWEEN $3::date - 2 AND $3::date + 2 LIMIT 1",
                &[&home, &away, &d]).await.map_err(|e| dberr(&e))?,
            None => None,
        },
    };
    Ok(match row {
        Some(r) => GameKey { game_id: Some(r.get(0)), season: r.get(1), week: r.get(2), day: r.get(3), home: r.get(4), away: r.get(5) },
        None => {
            let d = day.unwrap_or_else(|| chrono::Utc::now().date_naive());
            GameKey { game_id: None, season: season_of(d), week: 99, home: home.into(), away: away.into(), day }
        }
    })
}

/// Everything the NFL matchup tabs need, computed from data before the game's week.
pub async fn analysis(db: &tokio_postgres::Client, k: &GameKey, proj: &HashMap<String, Value>) -> Result<Value, String> {
    let ck = format!("{}:{}:{}:{}:{}", k.season, k.week, k.home, k.away, proj.len());
    if let Some((t, v)) = cache().lock().unwrap().get(&ck) {
        if t.elapsed() < Duration::from_secs(300) { return Ok((**v).clone()); }
    }
    let before = if k.week >= 1 { k.week } else { 99 };
    let p = profile(db, k.season, before).await?;
    let s = load_season(db, k.season).await?;
    let (h, a) = (k.home.as_str(), k.away.as_str());
    let games_all = load_games(db, k.season - 2).await?;
    let info = match &k.game_id {
        Some(g) => db.query_opt("SELECT home_coach, away_coach, referee, roof, surface, temp, wind, stadium, home_rest, away_rest, div_game, gametime \
                                 FROM nfl_game_info WHERE game_id=$1", &[g]).await.map_err(|e| dberr(&e))?,
        None => None,
    };
    let coach = |i: usize| -> Option<String> { info.as_ref().and_then(|r| r.get(i)) };
    let (hc, ac) = (coach(0), coach(1));
    let coaching = |team: &str, c: &Option<String>| json!({
        "team": team, "coach": c, "record": c.as_ref().map(|c| coach_record(&games_all, k.season, c, k.day)),
        "offense": team_block(&p, team, COACHING, true), "defense": team_block(&p, team, COACHING_DEF, false) });
    let this_season: Vec<GameRow> = games_all.iter().filter(|g| g.season == k.season && g.game_type == "REG").cloned().collect();
    let season_records: Vec<Value> = [a, h].iter().map(|t| {
        let l = team_log(&this_season, t, k.day, 25);
        json!({ "team": l["team"], "su": l["su"], "ats": l["ats"], "ou": l["ou"] })
    }).collect();
    let out = json!({
        "season": k.season, "through_week": before - 1, "games_played": { h: p.games(h), a: p.games(a) },
        "data_note": "Season-to-date plays before this game's week (nflverse play-by-play + FTN charting). Man/zone coverage and pressure aren't in any free 2026 feed, so blitzes, pass rushers, box counts and sacks stand in.",
        "stands_out": [stands_out(&p, a, h), stands_out(&p, h, a)],
        "has_ball": [has_ball(&p, a, h), has_ball(&p, h, a)],
        "rankings": [rankings(&p, h, a), rankings(&p, a, h)],
        "team_stats": { "rows": TEAM_STATS.iter().map(|key| json!({ "key": key, "label": metric(key).map(|m| m.label), "help": metric(key).map(|m| m.help),
            "home_off": p.show(h, true, key), "home_def": p.show(h, false, key), "away_off": p.show(a, true, key), "away_def": p.show(a, false, key) })).collect::<Vec<_>>() },
        "situations": [ { "team": a, "offense": team_block(&p, a, SITUATIONS, true), "defense": team_block(&p, a, &["third_conv", "fourth_conv", "rz_td"], false) },
                        { "team": h, "offense": team_block(&p, h, SITUATIONS, true), "defense": team_block(&p, h, &["third_conv", "fourth_conv", "rz_td"], false) } ],
        "coaching": [coaching(a, &ac), coaching(h, &hc)],
        "logs": [team_log(&games_all, a, k.day, 10), team_log(&games_all, h, k.day, 10)],
        "season_records": season_records,
        "game_info": info.as_ref().map(|r| json!({ "referee": r.get::<_, Option<String>>(2), "roof": r.get::<_, Option<String>>(3),
            "surface": r.get::<_, Option<String>>(4), "temp": r.get::<_, Option<i32>>(5), "wind": r.get::<_, Option<i32>>(6),
            "stadium": r.get::<_, Option<String>>(7), "home_rest": r.get::<_, Option<i32>>(8), "away_rest": r.get::<_, Option<i32>>(9),
            "div_game": r.get::<_, Option<bool>>(10), "gametime": r.get::<_, Option<String>>(11) })),
        "matchups": player_matchups(&s, &p, before, h, a, proj),
    });
    cache().lock().unwrap().insert(ck, (Instant::now(), Arc::new(out.clone())));
    Ok(out)
}

/// Week list for the schedule strip.
pub async fn weeks(db: &tokio_postgres::Client, season: i32) -> Result<Vec<Value>, String> {
    let rows = db.query(
        "SELECT game_type, week, min(gameday), max(gameday), count(*) FROM nfl_game_info WHERE season=$1 GROUP BY 1,2 ORDER BY min(gameday)",
        &[&season]).await.map_err(|e| dberr(&e))?;
    Ok(rows.iter().map(|r| {
        let gt: Option<String> = r.get(0);
        let label = match gt.as_deref() { Some("REG") => format!("Week {}", r.get::<_, i32>(1)), Some("WC") => "Wild Card".into(),
            Some("DIV") => "Divisional".into(), Some("CON") => "Conference".into(), Some("SB") => "Super Bowl".into(), _ => format!("Week {}", r.get::<_, i32>(1)) };
        json!({ "game_type": gt, "week": r.get::<_, i32>(1), "label": label, "start": r.get::<_, Option<NaiveDate>>(2),
            "end": r.get::<_, Option<NaiveDate>>(3), "games": r.get::<_, i64>(4) })
    }).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn play(game: &str, week: i32, off: &str, def: &str, ptype: &str, yards: i32, epa: f64) -> Play {
        Play { game_id: game.into(), week, off: off.into(), def: def.into(), qtr: 1, down: Some(1), togo: Some(10), yl: Some(50),
            ptype: ptype.into(), yards, epa: Some(epa), success: Some(epa > 0.0), dropback: ptype == "pass", ..Default::default() }
    }

    #[test]
    fn ranks_offense_high_and_defense_low() {
        let plays = vec![
            play("g1", 1, "AAA", "BBB", "pass", 25, 1.0), play("g1", 1, "BBB", "AAA", "pass", 2, -0.5),
            play("g2", 2, "AAA", "BBB", "run", 3, 0.1), play("g9", 9, "AAA", "BBB", "pass", 50, 5.0),
        ];
        let s = Season { plays, pos: HashMap::new(), games: vec![] };
        let acc = accumulate(&s, 3);
        let m = metric("db_epa").unwrap();
        let off = rank_side(&acc, true, m);
        assert_eq!(off["AAA"], (1.0, 1)); // week-9 play excluded
        assert_eq!(off["BBB"].1, 2);
        let def = rank_side(&acc, false, m);
        assert_eq!(def["AAA"].1, 1); // AAA allowed -0.5: stingiest
        let ex = acc[&("AAA".to_string(), true)].get("db_explosive").unwrap();
        assert!((ex - 1.0).abs() < 1e-9);
        assert_eq!(acc[&("AAA".to_string(), true)].games.len(), 2);
    }

    #[test]
    fn team_log_ats_uses_home_spread() {
        let g = |id: &str, home: &str, away: &str, hs: i32, as_: i32, spread: f64| GameRow { game_id: id.into(), season: 2026, game_type: "REG".into(),
            week: 1, day: NaiveDate::from_ymd_opt(2026, 9, 10), away: away.into(), home: home.into(), away_score: Some(as_), home_score: Some(hs),
            spread_line: Some(spread), total_line: Some(40.0), away_coach: None, home_coach: None };
        // home favored by 7, wins by 10 -> covers; away side loses ATS
        let games = vec![g("x", "NE", "NYJ", 26, 16, 7.0)];
        let ne = team_log(&games, "NE", None, 5);
        assert_eq!(ne["games"][0]["ats"], "W");
        assert_eq!(ne["games"][0]["line"], -7.0);
        assert_eq!(ne["games"][0]["ou"], "O");
        let nyj = team_log(&games, "NYJ", None, 5);
        assert_eq!(nyj["games"][0]["ats"], "L");
        assert_eq!(nyj["su"], "0-1-0");
    }
}
