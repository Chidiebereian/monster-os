//! NBA possession simulator: league fit, game inputs (strictly before tip-off),
//! lineups (projected / confirmed / actual), ensemble, broadcast session and
//! walk-forward backtest. Data: ESPN tables from `nba_espn.rs`.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use actix_web::{web, HttpResponse, Responder};
use chrono::NaiveDate;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_postgres::types::Json;

use crate::nba_sim::{self as sim, GameInput, League, Rate};
use crate::State;

pub const MODEL_VERSION: &str = "nba-poss-sim-v1";

fn internal(m: impl std::fmt::Display) -> HttpResponse {
    eprintln!("projection-service(nba-sim): {m}");
    HttpResponse::InternalServerError().json(json!({ "error": m.to_string() }))
}
fn unprocessable(m: impl Into<String>) -> HttpResponse {
    HttpResponse::UnprocessableEntity().json(json!({ "error": m.into() }))
}
fn dberr(e: &tokio_postgres::Error) -> String {
    e.as_db_error().map(|d| format!("{} ({})", d.message(), d.detail().unwrap_or(""))).unwrap_or_else(|| e.to_string())
}
fn r3(x: f64) -> f64 { (x * 1000.0).round() / 1000.0 }
fn r4(x: f64) -> f64 { (x * 10000.0).round() / 10000.0 }

/// Box totals: min pts fgm fga fg3m fg3a ftm fta oreb dreb ast tov stl blk
#[derive(Clone, Copy, Default, Debug)]
struct Tot { min: f64, pts: f64, fgm: f64, fga: f64, f3m: f64, f3a: f64, ftm: f64, fta: f64, oreb: f64, dreb: f64, ast: f64, tov: f64, stl: f64, blk: f64 }
impl Tot {
    fn add(&mut self, o: &Tot, w: f64) {
        self.min += w * o.min; self.pts += w * o.pts; self.fgm += w * o.fgm; self.fga += w * o.fga; self.f3m += w * o.f3m; self.f3a += w * o.f3a;
        self.ftm += w * o.ftm; self.fta += w * o.fta; self.oreb += w * o.oreb; self.dreb += w * o.dreb; self.ast += w * o.ast; self.tov += w * o.tov;
        self.stl += w * o.stl; self.blk += w * o.blk;
    }
    fn usage(&self) -> f64 { self.fga + self.tov + self.fta / 2.0 }
    fn poss(&self) -> f64 { self.fga - self.oreb + self.tov + 0.44 * self.fta }
}
const BOX_COLS: &str = "b.min, b.pts, b.fgm, b.fga, b.fg3m, b.fg3a, b.ftm, b.fta, b.oreb, b.dreb, b.ast, b.tov, b.stl, b.blk";
fn tot_from(r: &tokio_postgres::Row, at: usize) -> Tot {
    let f = |i: usize| r.get::<_, Option<f64>>(i).unwrap_or(0.0);
    let n = |i: usize| r.get::<_, Option<i32>>(i).unwrap_or(0) as f64;
    Tot { min: f(at), pts: n(at + 1), fgm: n(at + 2), fga: n(at + 3), f3m: n(at + 4), f3a: n(at + 5), ftm: n(at + 6), fta: n(at + 7),
          oreb: n(at + 8), dreb: n(at + 9), ast: n(at + 10), tov: n(at + 11), stl: n(at + 12), blk: n(at + 13) }
}

// ---------------------------------------------------------------------------
// League fit
// ---------------------------------------------------------------------------

fn kappa(units: &[(f64, f64)], min_n: f64, default: f64) -> f64 {
    crate::mlb_sim::mom_kappa(units, min_n).unwrap_or(default).clamp(5.0, 5000.0)
}

/// POST /fit/nba/sim — league parameters per ESPN season (completed regular + postseason games).
pub async fn fit(state: web::Data<State>) -> impl Responder {
    let seasons: Vec<i32> = match state.db.query("SELECT DISTINCT season FROM nba_games_espn WHERE completed AND season_type IN (2,3) ORDER BY 1", &[]).await {
        Ok(r) => r.iter().map(|x| x.get(0)).collect(), Err(e) => return internal(dberr(&e)),
    };
    let mut out = Vec::new();
    for season in seasons {
        let rows = match state.db.query(&format!(
            "SELECT b.player_id, b.team, g.home, {BOX_COLS} FROM nba_box_espn b JOIN nba_games_espn g USING (espn_id) \
             WHERE g.season = $1 AND g.completed AND g.season_type IN (2,3) AND NOT b.dnp"), &[&season]).await {
            Ok(r) => r, Err(e) => return internal(dberr(&e)),
        };
        let mut lg = Tot::default();
        let mut home = Tot::default();
        let mut away = Tot::default();
        let mut per: HashMap<String, Tot> = HashMap::new();
        for r in &rows {
            let t = tot_from(r, 3);
            lg.add(&t, 1.0);
            let pid: String = r.get(0);
            per.entry(pid).or_default().add(&t, 1.0);
            let team: String = r.get(1);
            let h: String = r.get(2);
            if team == h { home.add(&t, 1.0) } else { away.add(&t, 1.0) }
        }
        let team_games = rows.iter().map(|r| r.get::<_, String>(1)).count().max(1);
        let _ = team_games;
        let n_team_games: i64 = state.db.query_one("SELECT 2 * count(*) FROM nba_games_espn WHERE season = $1 AND completed AND season_type IN (2,3)", &[&season])
            .await.map(|r| r.get(0)).unwrap_or(1);
        let fg2 = (lg.fgm - lg.f3m) / (lg.fga - lg.f3a).max(1.0);
        let units = |f: &dyn Fn(&Tot) -> (f64, f64)| per.values().map(f).collect::<Vec<_>>();
        let k = sim::Kappas {
            fg2: kappa(&units(&|t| (t.fgm - t.f3m, t.fga - t.f3a)), 50.0, 200.0),
            fg3: kappa(&units(&|t| (t.f3m, t.f3a)), 50.0, 200.0),
            ft: kappa(&units(&|t| (t.ftm, t.fta)), 30.0, 50.0),
            tov: kappa(&units(&|t| (t.tov, t.usage())), 100.0, 150.0),
            trip: kappa(&units(&|t| (t.fta / 2.0, t.usage())), 100.0, 150.0),
            three: kappa(&units(&|t| (t.f3a, t.fga)), 50.0, 50.0),
        };
        // home shooting edge: sqrt of the home/away eFG odds ratio
        let efg = |t: &Tot| (t.fgm + 0.5 * t.f3m) / t.fga.max(1.0);
        let o = |p: f64| p / (1.0 - p);
        let home_edge = (o(efg(&home)) / o(efg(&away))).sqrt();
        let league = League {
            season,
            fg2, fg3: lg.f3m / lg.f3a.max(1.0), ft: lg.ftm / lg.fta.max(1.0),
            tov_share: lg.tov / lg.usage(), trip_share: (lg.fta / 2.0) / lg.usage(), three_share: lg.f3a / lg.fga.max(1.0),
            ast_per_fgm: lg.ast / lg.fgm.max(1.0), stl_per_tov: lg.stl / lg.tov.max(1.0), blk_per_miss: lg.blk / (lg.fga - lg.fgm).max(1.0),
            oreb_pct: lg.oreb / (lg.oreb + lg.dreb).max(1.0),
            pace: lg.poss() / n_team_games as f64 * 240.0 / (lg.min / n_team_games as f64).max(1.0),
            kappa: k,
        };
        let v = json!({ "league": league, "home_edge": home_edge });
        let key = format!("nba:sim:{season}");
        if let Err(e) = state.db.execute(
            "INSERT INTO sim_model_params (key, params, source) VALUES ($1, $2, $3) ON CONFLICT (key) DO UPDATE SET params = EXCLUDED.params, source = EXCLUDED.source, fitted_at = now()",
            &[&key, &Json(&v), &format!("ESPN box scores, season {season}: {} player-games", rows.len())]).await {
            return internal(dberr(&e));
        }
        out.push(json!({ "season": season, "player_games": rows.len(), "fg2": r4(league.fg2), "fg3": r4(league.fg3), "ft": r4(league.ft),
            "tov_share": r4(league.tov_share), "trip_share": r4(league.trip_share), "three_share": r4(league.three_share), "ast_per_fgm": r4(league.ast_per_fgm),
            "oreb_pct": r4(league.oreb_pct), "pace": r3(league.pace), "home_edge": r4(home_edge), "kappa": league.kappa }));
    }
    HttpResponse::Ok().json(json!({ "model_version": MODEL_VERSION, "fitted": out }))
}

async fn load_league(db: &tokio_postgres::Client, season: i32) -> Result<(League, f64, String), String> {
    let key = format!("nba:sim:{season}");
    let row = db.query_opt("SELECT params FROM sim_model_params WHERE key = $1", &[&key]).await.map_err(|e| dberr(&e))?
        .ok_or_else(|| format!("no NBA simulator parameters for season {season}; run POST /fit/nba/sim"))?;
    let v: Json<Value> = row.get(0);
    let lg: League = serde_json::from_value(v.0["league"].clone()).map_err(|e| e.to_string())?;
    Ok((lg, v.0["home_edge"].as_f64().unwrap_or(1.0), key))
}

// ---------------------------------------------------------------------------
// Game inputs
// ---------------------------------------------------------------------------

pub struct GameRef {
    pub id: String,
    pub season: i32,
    pub season_type: i32,
    pub date: NaiveDate,
    pub away: String,
    pub home: String,
    pub away_score: Option<i32>,
    pub home_score: Option<i32>,
    pub completed: bool,
    pub spread: Option<f64>,
    pub over_under: Option<f64>,
}

async fn game_ref(db: &tokio_postgres::Client, id: &str) -> Result<GameRef, String> {
    let r = db.query_opt("SELECT espn_id, season, season_type, game_date, away, home, away_score, home_score, completed, spread, over_under \
                          FROM nba_games_espn WHERE espn_id = $1", &[&id]).await.map_err(|e| dberr(&e))?
        .ok_or_else(|| format!("unknown NBA game {id} (ingest it with POST /ingest/nba/espn)"))?;
    Ok(GameRef { id: r.get(0), season: r.get(1), season_type: r.get(2), date: r.get(3), away: r.get(4), home: r.get(5),
        away_score: r.get(6), home_score: r.get(7), completed: r.get(8), spread: r.get(9), over_under: r.get(10) })
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum LineupMode { Projected, Confirmed, Actual }
impl LineupMode {
    pub fn parse(s: Option<&str>) -> Self { match s { Some("projected") => Self::Projected, Some("actual") => Self::Actual, _ => Self::Confirmed } }
    pub fn as_str(&self) -> &'static str { match self { Self::Projected => "projected", Self::Confirmed => "confirmed", Self::Actual => "actual" } }
}

pub struct Opts {
    pub mode: LineupMode,
    pub out: HashSet<String>,
    pub force_in: HashSet<String>,
}

/// ESPN injury designations -> probability of playing. Not fitted (no stored
/// history of NBA injury reports yet): documented priors.
fn injury_p(status: &str) -> f64 {
    match status.to_ascii_lowercase().as_str() {
        "out" => 0.0, "doubtful" => 0.15, "questionable" => 0.5, "day-to-day" => 0.6, "probable" => 0.9, _ => 0.75,
    }
}

/// Current ESPN injury report: player id -> (status, detail).
async fn espn_injuries(http: &reqwest::Client) -> HashMap<String, (String, String)> {
    let mut m = HashMap::new();
    if let Ok(v) = crate::nba_espn::get_json(http, "https://site.api.espn.com/apis/site/v2/sports/basketball/nba/injuries").await {
        for t in v["injuries"].as_array().into_iter().flatten() {
            for i in t["injuries"].as_array().into_iter().flatten() {
                if let Some(id) = i["athlete"]["id"].as_str().or_else(|| i["athlete"]["links"][0]["href"].as_str().and_then(|h| h.split("/id/").nth(1)).and_then(|x| x.split('/').next())) {
                    m.insert(id.to_string(), (i["status"].as_str().unwrap_or("").to_string(), i["shortComment"].as_str().unwrap_or("").to_string()));
                }
            }
        }
    }
    m
}

pub struct Built {
    pub game: GameInput,
    pub meta: Value,
}

fn shrink(x: f64, n: f64, lg: f64, k: f64) -> Rate { Rate { mean: (x + k * lg) / (n + k), n: n + k } }

pub async fn build(state: &State, g: &GameRef, opts: &Opts) -> Result<Built, String> {
    let db = &state.db;
    let params_season = if g.season_type == 3 { g.season } else { g.season - 1 };
    let (league, home_edge, params_key) = load_league(db, params_season).await?;
    let teams = [g.away.clone(), g.home.clone()];
    let today = chrono::Local::now().date_naive();
    let injuries = if opts.mode == LineupMode::Confirmed && g.date >= today { espn_injuries(&state.http).await } else { HashMap::new() };
    let actual: HashMap<String, (bool, bool, String)> = if opts.mode == LineupMode::Actual || g.completed {
        db.query("SELECT player_id, starter, (NOT dnp AND COALESCE(min, 0) > 0), team FROM nba_box_espn WHERE espn_id = $1", &[&g.id]).await
            .map_err(|e| dberr(&e))?.iter().map(|r| (r.get::<_, String>(0), (r.get::<_, bool>(1), r.get::<_, bool>(2), r.get::<_, String>(3)))).collect()
    } else { HashMap::new() };
    if opts.mode == LineupMode::Actual && actual.is_empty() {
        return Err("actual lineup is only available after the game (no box score yet)".into());
    }
    let mut team_inputs = Vec::new();
    let mut team_meta = Vec::new();
    let mut flags: Vec<String> = Vec::new();
    for (ti, abbr) in teams.iter().enumerate() {
        // team's recent completed games
        let tg: Vec<(String, NaiveDate, i32)> = db.query(
            "SELECT espn_id, game_date, season_type FROM nba_games_espn WHERE completed AND season_type IN (1,2,3) AND (home = $1 OR away = $1) AND game_date < $2 \
             ORDER BY game_date DESC LIMIT 30", &[abbr, &g.date]).await.map_err(|e| dberr(&e))?
            .iter().map(|r| (r.get(0), r.get(1), r.get(2))).collect();
        let last_was_preseason = tg.first().map(|x| x.2 == 1).unwrap_or(false);
        if tg.is_empty() { return Err(format!("{abbr}: no completed games before {}", g.date)); }
        let tg_ids: Vec<String> = tg.iter().map(|x| x.0.clone()).collect();
        let rows = db.query(&format!(
            "SELECT b.espn_id, b.team, b.player_id, b.name, COALESCE(b.pos, ''), b.starter, b.dnp, {BOX_COLS} FROM nba_box_espn b WHERE b.espn_id = ANY($1)"),
            &[&tg_ids]).await.map_err(|e| dberr(&e))?;
        let gi: HashMap<&str, usize> = tg_ids.iter().enumerate().map(|(i, x)| (x.as_str(), i)).collect();
        // team / opponent aggregates (recency weighted, half-life 15 games)
        let mut own = Tot::default();
        let mut opp = Tot::default();
        let mut gw = 0.0;
        let mut pool: HashMap<String, (String, String, Vec<usize>, Option<usize>)> = HashMap::new(); // id -> name, pos, games played idx, last start idx
        let mut seen_g: HashSet<String> = HashSet::new();
        for r in &rows {
            let gid: String = r.get(0);
            let i = gi[gid.as_str()];
            let w = 0.5f64.powf(i as f64 / 15.0);
            let team: String = r.get(1);
            let t = tot_from(r, 7);
            if &team == abbr { own.add(&t, w) } else { opp.add(&t, w) }
            if &team == abbr && seen_g.insert(gid.clone()) { gw += w; }
            if &team == abbr {
                let e = pool.entry(r.get(2)).or_insert((r.get(3), r.get(4), Vec::new(), None));
                if !r.get::<_, bool>(6) && t.min > 0.0 { e.2.push(i); }
                if r.get::<_, bool>(5) && e.3.map(|x| i < x).unwrap_or(true) { e.3 = Some(i); }
            }
        }
        let _ = gw;
        // players who left: latest team before the game must be this team
        let ids: Vec<String> = pool.keys().cloned().collect();
        let latest: HashMap<String, String> = db.query(
            "SELECT DISTINCT ON (b.player_id) b.player_id, b.team FROM nba_box_espn b JOIN nba_games_espn g USING (espn_id) \
             WHERE b.player_id = ANY($1) AND g.game_date < $2 AND NOT b.dnp ORDER BY b.player_id, g.game_date DESC", &[&ids, &g.date]).await
            .map_err(|e| dberr(&e))?.iter().map(|r| (r.get(0), r.get(1))).collect();
        let last_game_starters: Vec<String> = pool.iter().filter(|(_, v)| v.3 == Some(0)).map(|(k, _)| k.clone()).collect();
        // candidates and availability
        struct C { id: String, name: String, pos: String, p: f64, starter: bool, reason: String, games: usize }
        let mut cands: Vec<C> = Vec::new();
        for (id, (name, pos, played, last_start)) in &pool {
            if latest.get(id).map(|t| t != abbr).unwrap_or(false) { continue; }
            if played.is_empty() { continue; }
            let recent = played.iter().any(|&i| i < 5);
            let (mut p, mut reason, mut starter): (f64, String, bool) = match opts.mode {
                LineupMode::Actual => match actual.get(id) {
                    Some((st, pl, _)) => (if *pl { 1.0 } else { 0.0 }, if *pl { "actual: played".into() } else { "actual: did not play".into() }, *st),
                    None => (0.0, "actual: not on the game roster".to_string(), false),
                },
                _ => (if recent { 1.0 } else { 0.0 }, if recent { "played in the team's last 5 games".into() } else { "not used in the last 5 games".to_string() },
                      *last_start == Some(0)),
            };
            if opts.mode == LineupMode::Confirmed {
                if let Some((st, detail)) = injuries.get(id) {
                    p = p.min(injury_p(st));
                    reason = format!("injury report: {st}{}", if detail.is_empty() { String::new() } else { format!(" ({})", detail.chars().take(60).collect::<String>()) });
                }
            }
            if opts.out.contains(id) { p = 0.0; reason = "scenario: ruled out".into(); }
            if opts.force_in.contains(id) { p = 1.0; reason = "scenario: plays".into(); }
            if p == 0.0 { starter = false; }
            cands.push(C { id: id.clone(), name: name.clone(), pos: pos.clone(), p, starter, reason, games: played.len() });
        }
        // a starter ruled out is replaced by the heaviest-minute non-starter (computed below)
        let ids: Vec<String> = cands.iter().map(|c| c.id.clone()).collect();
        let hist = db.query(&format!(
            "SELECT b.player_id, g.season_type, {BOX_COLS} FROM nba_box_espn b JOIN nba_games_espn g USING (espn_id) \
             WHERE b.player_id = ANY($1) AND g.game_date < $2 AND g.game_date >= $2 - 450 AND g.completed AND NOT b.dnp AND COALESCE(b.min, 0) > 0 \
             ORDER BY g.game_date DESC"), &[&ids, &g.date]).await.map_err(|e| dberr(&e))?;
        // regular season + playoffs drive minutes and rates; preseason only for players with no other history
        let mut by: HashMap<String, Vec<Tot>> = HashMap::new();
        let mut pre: HashMap<String, Vec<Tot>> = HashMap::new();
        for r in &hist {
            let st: i32 = r.get(1);
            if st == 1 { pre.entry(r.get(0)).or_default().push(tot_from(r, 2)) } else { by.entry(r.get(0)).or_default().push(tot_from(r, 2)) }
        }
        let shots = db.query(
            "SELECT s.player_id, s.x, s.y, s.made, s.value FROM nba_shots_espn s JOIN nba_games_espn g USING (espn_id) \
             WHERE s.player_id = ANY($1) AND g.game_date < $2 AND g.game_date >= $2 - 450 AND s.y <= 47", &[&ids, &g.date]).await.map_err(|e| dberr(&e))?;
        let mut chart: HashMap<String, (Vec<(f32, f32, bool)>, Vec<(f32, f32, bool)>)> = HashMap::new();
        for r in &shots {
            let e = chart.entry(r.get(0)).or_default();
            let v = (r.get::<_, f64>(1) as f32, r.get::<_, f64>(2) as f32, r.get::<_, bool>(3));
            if r.get::<_, i32>(4) == 3 { if e.1.len() < 400 { e.1.push(v) } } else if e.0.len() < 400 { e.0.push(v) }
        }
        let lgk = &league.kappa;
        let lg_min = 0.0; let _ = lg_min;
        let mut players = Vec::new();
        let mut pmeta = Vec::new();
        for c in &cands {
            let reg = by.get(&c.id).cloned().unwrap_or_default();
            let games = if reg.len() < 3 { [reg, pre.get(&c.id).cloned().unwrap_or_default()].concat() } else { reg };
            let mut t = Tot::default();
            let mut wsum = 0.0;
            for (i, x) in games.iter().enumerate() { let w = 0.5f64.powf(i as f64 / 20.0); t.add(x, w); wsum += w; }
            let mins: Vec<f64> = games.iter().take(10).map(|x| x.min).collect();
            let (mm, msd) = if mins.len() >= 3 {
                let wts: Vec<f64> = (0..mins.len()).map(|i| 0.5f64.powf(i as f64 / 5.0)).collect();
                let ws: f64 = wts.iter().sum();
                let m = mins.iter().zip(&wts).map(|(a, w)| a * w).sum::<f64>() / ws;
                let v = mins.iter().zip(&wts).map(|(a, w)| w * (a - m).powi(2)).sum::<f64>() / ws;
                (m, v.sqrt().clamp(2.0, 10.0))
            } else { (if c.starter { 28.0 } else { 12.0 }, 6.0) };
            // preseason-only history (rookies, new signings): preseason minutes overstate roles
            let mm = if by.get(&c.id).map(|v| v.len()).unwrap_or(0) < 3 { mm.min(18.0) } else { mm };
            let u = t.usage();
            let per_min = |x: f64, lgv: f64| (x + 60.0 * lgv) / (t.min + 60.0);
            let lg_per_min = |f: fn(&Tot) -> f64| f(&t) / t.min.max(1.0); // own, for meta
            let _ = lg_per_min;
            let shots = chart.remove(&c.id).unwrap_or_default();
            players.push(sim::Player {
                id: c.id.clone(), name: c.name.clone(), pos: c.pos.clone(), p_active: c.p, starter: c.starter,
                min_mean: mm, min_sd: msd,
                usage: (u + 60.0 * 0.42) / (t.min + 60.0),
                tov: shrink(t.tov, u, league.tov_share, lgk.tov),
                trip: shrink(t.fta / 2.0, u, league.trip_share, lgk.trip),
                three: shrink(t.f3a, t.fga, league.three_share, lgk.three),
                fg2: shrink(t.fgm - t.f3m, t.fga - t.f3a, league.fg2, lgk.fg2),
                fg3: shrink(t.f3m, t.f3a, league.fg3, lgk.fg3),
                ft: shrink(t.ftm, t.fta, league.ft, lgk.ft),
                ast_w: per_min(t.ast, 0.10), oreb_w: per_min(t.oreb, 0.04), dreb_w: per_min(t.dreb, 0.13),
                stl_w: per_min(t.stl, 0.03), blk_w: per_min(t.blk, 0.02),
                shots2: shots.0, shots3: shots.1,
            });
            pmeta.push(json!({ "id": c.id, "name": c.name, "pos": c.pos, "p_active": r3(c.p), "starter": c.starter, "status": c.reason,
                "games": c.games, "min_mean": r3(mm), "scenario": if opts.out.contains(&c.id) { "out" } else if opts.force_in.contains(&c.id) { "in" } else { "auto" } }));
            let _ = wsum;
        }
        // after a preseason game the projected starters are the top five by projected minutes
        if last_was_preseason && opts.mode != LineupMode::Actual {
            let mut order: Vec<usize> = (0..players.len()).filter(|&i| players[i].p_active > 0.0).collect();
            order.sort_by(|a, b| players[*b].min_mean.total_cmp(&players[*a].min_mean));
            for (k, i) in order.iter().enumerate() { players[*i].starter = k < 5; pmeta[*i]["starter"] = json!(k < 5); }
        }
        // fill starters: ruled-out starters replaced by the heaviest-minute active non-starters
        let n_start = players.iter().filter(|p| p.starter && p.p_active > 0.0).count();
        if n_start < 5 {
            let mut bench: Vec<usize> = (0..players.len()).filter(|&i| !players[i].starter && players[i].p_active > 0.0).collect();
            bench.sort_by(|a, b| players[*b].min_mean.total_cmp(&players[*a].min_mean));
            for i in bench.into_iter().take(5 - n_start) { players[i].starter = true; pmeta[i]["starter"] = json!(true); }
        }
        if players.iter().filter(|p| p.p_active > 0.0).count() < 8 {
            flags.push(format!("{abbr}: fewer than 8 available players"));
        }
        let pace = {
            let gms = (own.min / 240.0).max(0.0);
            let poss = 0.5 * (own.poss() + opp.poss());
            let p = if gms > 0.0 { poss / gms } else { league.pace };
            (p * gms + league.pace * 8.0) / (gms + 8.0)
        };
        let o = |p: f64| p / (1.0 - p);
        let ratio = |made: f64, att: f64, lgp: f64, k: f64| o((made + k * lgp) / (att + k)) / o(lgp);
        let def_fg2 = ratio(opp.fgm - opp.f3m, opp.fga - opp.f3a, league.fg2, 800.0);
        let def_fg3 = ratio(opp.f3m, opp.f3a, league.fg3, 800.0);
        let def_tov = ratio(opp.tov, opp.usage(), league.tov_share, 800.0);
        let dreb = ratio(own.dreb, own.dreb + opp.oreb, 1.0 - league.oreb_pct, 400.0);
        let oreb = ratio(own.oreb, own.oreb + opp.dreb, league.oreb_pct, 400.0);
        team_meta.push(json!({
            "abbr": abbr, "recent_games": tg.len(), "pace": r3(pace), "def_fg2": r3(def_fg2), "def_fg3": r3(def_fg3), "def_tov": r3(def_tov),
            "dreb": r3(dreb), "oreb": r3(oreb), "players": pmeta,
            "starters": {
                "projected": if last_was_preseason { players.iter().filter(|p| p.starter).map(|p| p.id.clone()).collect::<Vec<_>>() } else { last_game_starters },
                "actual": actual.iter().filter(|(_, v)| v.0 && &v.2 == abbr).map(|(k, _)| k.clone()).collect::<Vec<_>>(),
                "used": players.iter().filter(|p| p.starter).map(|p| p.id.clone()).collect::<Vec<_>>(),
            },
        }));
        let _ = ti;
        team_inputs.push(sim::Team { abbr: abbr.clone(), players, pace, def_fg2, def_fg3, def_tov, dreb, oreb });
    }
    if opts.mode == LineupMode::Confirmed && g.date < today {
        flags.push("past game: the injury report is only available for upcoming games, so 'confirmed' = projected availability".into());
    }
    let [a, h]: [sim::Team; 2] = team_inputs.try_into().map_err(|_| "teams")?;
    let meta = json!({
        "model_version": MODEL_VERSION, "params": params_key, "data_cutoff": g.date, "lineup_mode": opts.mode.as_str(),
        "actual_available": !actual.is_empty(), "teams": team_meta, "flags": flags,
        "assumptions": [
            "possession model: usage x outcome (turnover / two-shot trip / 2 or 3 FGA) with player x defence odds ratios; no fouls, and-1s or timeouts",
            "rotation: minutes from each player's recent games, greedy 2-minute stints, starters open the 1st and 3rd quarters",
            "injury designations -> play probabilities are documented priors (Out 0, Doubtful 0.15, Questionable 0.5, Day-To-Day 0.6, Probable 0.9), not fitted",
            "shot locations on the court are drawn from the shooter's real ESPN shot chart (display only)",
        ],
    });
    Ok(Built { game: GameInput { teams: [a, h], league, home_edge }, meta })
}

// ---------------------------------------------------------------------------
// Endpoints
// ---------------------------------------------------------------------------

/// GET /nba/sim/games — recent and upcoming games.
pub async fn games(state: web::Data<State>) -> impl Responder {
    let today = chrono::Local::now().date_naive();
    let from = today - chrono::Duration::days(4);
    let to = today + chrono::Duration::days(8);
    match state.db.query("SELECT espn_id, game_date, away, home, away_score, home_score, completed, spread, over_under, season_type, start_time::text \
                          FROM nba_games_espn WHERE game_date BETWEEN $1 AND $2 ORDER BY start_time, espn_id", &[&from, &to]).await {
        Ok(rows) => HttpResponse::Ok().json(json!({ "today": today, "games": rows.iter().map(|r| json!({
            "game_id": r.get::<_, String>(0), "date": r.get::<_, NaiveDate>(1), "away": r.get::<_, String>(2), "home": r.get::<_, String>(3),
            "away_score": r.get::<_, Option<i32>>(4), "home_score": r.get::<_, Option<i32>>(5), "completed": r.get::<_, bool>(6),
            "spread": r.get::<_, Option<f64>>(7), "over_under": r.get::<_, Option<f64>>(8), "season_type": r.get::<_, i32>(9),
            "start": r.get::<_, Option<String>>(10), "today": r.get::<_, NaiveDate>(1) == today,
        })).collect::<Vec<_>>() })),
        Err(e) => internal(dberr(&e)),
    }
}

#[derive(Deserialize)]
pub struct SimQuery {
    #[serde(default = "d_n")] n: usize,
    #[serde(default = "d_draws")] draws: usize,
    seed: Option<u64>,
    lineup: Option<String>,
    out: Option<String>,
    #[serde(rename = "in")]
    force_in: Option<String>,
}
fn d_n() -> usize { 100_000 }
fn d_draws() -> usize { 200 }
fn ids(s: &Option<String>) -> HashSet<String> {
    s.as_deref().unwrap_or("").split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()
}
pub fn opts(lineup: Option<&str>, out: &[String], force_in: &[String]) -> Opts {
    Opts { mode: LineupMode::parse(lineup), out: out.iter().cloned().collect(), force_in: force_in.iter().cloned().collect() }
}

fn hist_json(h: &sim::Hist, lines: &[f64]) -> Value {
    json!({ "mean": r3(h.mean()), "q10": h.quantile(0.1), "q50": h.quantile(0.5), "q90": h.quantile(0.9),
        "lines": lines.iter().map(|l| { let (o, u, p, se) = h.line(*l); json!({ "line": l, "over": r4(o), "under": r4(u), "push": r4(p), "mcse": r4(se) }) }).collect::<Vec<_>>(),
        "pmf": h.pmf().iter().map(|p| r4(*p)).collect::<Vec<_>>() })
}

/// GET /nba/simulate/{id}?n=100000 — the prediction ensemble (n >= 100,000 enforced).
pub async fn simulate(state: web::Data<State>, path: web::Path<String>, q: web::Query<SimQuery>) -> impl Responder {
    let id = path.into_inner();
    let gref = match game_ref(&state.db, &id).await { Ok(g) => g, Err(e) => return unprocessable(e) };
    let o = Opts { mode: LineupMode::parse(q.lineup.as_deref()), out: ids(&q.out), force_in: ids(&q.force_in) };
    let Built { game, meta } = match build(&state, &gref, &o).await { Ok(b) => b, Err(e) => return unprocessable(e) };
    let seed = q.seed.unwrap_or_else(|| sim::derive_seed(0x6e6261, &[&id, &chrono::Utc::now().timestamp_millis().to_string()]));
    let (n, draws) = (q.n.clamp(100_000, 1_000_000), q.draws.clamp(50, 2000));
    let t = Instant::now();
    let g2 = game.clone();
    let e = match web::block(move || sim::simulate(&g2, n, draws, seed)).await { Ok(e) => e, Err(err) => return internal(err) };
    let ms = t.elapsed().as_millis() as u64;
    let nn = e.n.max(1) as f64;
    let players: Vec<Vec<Value>> = (0..2).map(|t| game.teams[t].players.iter().enumerate().filter(|(i, _)| e.active_n[t][*i] > 0).map(|(i, p)| {
        let h = &e.player[t][i];
        let stats: serde_json::Map<String, Value> = sim::PROPS.iter().enumerate().map(|(k, s)| {
            let m = h[k].mean();
            let line = (m.floor() + 0.5).max(0.5);
            (s.to_string(), hist_json(&h[k], &[line]))
        }).collect();
        json!({ "idx": i, "id": p.id, "name": p.name, "pos": p.pos, "starter": p.starter, "p_active": r4(e.active_n[t][i] as f64 / nn), "stats": stats })
    }).collect()).collect();
    let spread_home = gref.spread; // negative = home favoured
    HttpResponse::Ok().json(json!({
        "game_id": id, "date": gref.date, "away": gref.away, "home": gref.home, "model_version": MODEL_VERSION, "seed": seed, "n": e.n, "sim_ms": ms,
        "win": { "home": r4(e.home_win as f64 / nn), "away": r4(1.0 - e.home_win as f64 / nn), "overtime": r4(e.overtime as f64 / nn) },
        "score": { "away": hist_json(&e.score[0], &[]), "home": hist_json(&e.score[1], &[]) },
        "total": hist_json(&e.total, &gref.over_under.into_iter().collect::<Vec<_>>()),
        "margin_home": { "mean": r3(e.margin_mean()) },
        "possessions": { "away": r3(e.possessions[0] / nn), "home": r3(e.possessions[1] / nn) },
        "market": { "spread": spread_home, "over_under": gref.over_under,
            "p_home_cover": spread_home.map(|s| r4(e.margin_line(-s).0)), "p_over": gref.over_under.map(|l| r4(e.total.line(l).0)) },
        "players": { "away": players[0], "home": players[1] },
        "inputs": meta,
        "note": "player props are conditional on the player being active (p_active); mcse is Monte Carlo sampling error only",
    }))
}

/// Broadcast session for an NBA game.
pub async fn new_session(state: &State, id: &str, seed: u64, o: Opts) -> Result<(Value, Vec<Value>, Value, NaiveDate), String> {
    let gref = game_ref(&state.db, id).await?;
    let Built { game, meta } = build(state, &gref, &o).await?;
    let g2 = game.clone();
    let (res, events) = web::block(move || sim::single_game(&g2, seed)).await.map_err(|e| e.to_string())?;
    let name = |t: usize, i: usize| game.teams[t].players.get(i).map(|p| p.name.clone()).unwrap_or_default();
    let events: Vec<Value> = events.into_iter().map(|e| {
        let t = e.team as usize;
        let who = e.player.map(|i| name(t, i)).unwrap_or_default();
        let text = match e.kind {
            "shot" if e.made => format!("{who} makes a {}-pointer{}", e.value, e.assist.map(|a| format!(" (assist {})", name(t, a))).unwrap_or_default()),
            "shot" => format!("{who} misses a {}-pointer{}", e.value, e.block.map(|b| format!(", blocked by {}", name(1 - t, b))).unwrap_or_default()),
            "free_throws" => format!("{who} makes {} of {} free throws", e.ft_made, e.ft_att),
            "turnover" => format!("{who} turnover{}", e.steal.map(|s| format!(", stolen by {}", name(1 - t, s))).unwrap_or_default()),
            "rebound" => format!("{who} {} rebound", if e.offensive { "offensive" } else { "defensive" }),
            "substitution" => format!("Substitution: {} in, {} out",
                e.subs_in.iter().map(|i| name(t, *i)).collect::<Vec<_>>().join(", "), e.subs_out.iter().map(|i| name(t, *i)).collect::<Vec<_>>().join(", ")),
            "end_period" => format!("End of {}", if e.period <= 4 { format!("Q{}", e.period) } else { format!("OT{}", e.period - 4) }),
            _ => format!("FINAL{}: {} {} — {} {}", if res.overtime { " (OT)" } else { "" }, game.teams[0].abbr, res.score[0], game.teams[1].abbr, res.score[1]),
        };
        let mut v = serde_json::to_value(&e).unwrap_or(Value::Null);
        v["text"] = json!(text);
        v
    }).collect();
    let fin = json!({ "score": res.score, "overtime": res.overtime });
    let roster: Vec<Value> = (0..2).map(|t| json!(game.teams[t].players.iter().enumerate().map(|(i, p)| json!({
        "idx": i, "id": p.id, "name": p.name, "pos": p.pos, "starter": p.starter, "p_active": r3(p.p_active), "active_in_this_game": res.active[t][i] })).collect::<Vec<_>>())).collect();
    let meta = json!({
        "sport": "nba", "game_id": gref.id, "date": gref.date, "season": gref.season, "away": gref.away, "home": gref.home,
        "stat_keys": sim::STAT_KEYS, "roster": roster,
        "actual": { "away_score": gref.away_score, "home_score": gref.home_score },
        "market": { "spread": gref.spread, "over_under": gref.over_under },
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
    lineup: Option<String>,
    from: Option<String>,
}
fn d_bt_season() -> i32 { 2026 }
fn d_bt_games() -> usize { 150 }
fn d_bt_n() -> usize { 2000 }
fn d_bt_draws() -> usize { 20 }
fn d_bt_seed() -> u64 { 777 }

#[derive(Default)]
struct Binary { n: f64, m: f64, b: f64, d: Vec<f64> }
impl Binary {
    fn add(&mut self, pm: f64, pb: f64, y: f64) {
        let (pm, pb) = (pm.clamp(1e-4, 1.0 - 1e-4), pb.clamp(1e-4, 1.0 - 1e-4));
        let (bm, bb) = ((pm - y).powi(2), (pb - y).powi(2));
        self.n += 1.0; self.m += bm; self.b += bb; self.d.push(bm - bb);
    }
    fn report(&self, baseline: &str) -> Value {
        let n = self.n.max(1.0);
        let md = self.d.iter().sum::<f64>() / n;
        let sd = (self.d.iter().map(|x| (x - md).powi(2)).sum::<f64>() / (n - 1.0).max(1.0)).sqrt();
        json!({ "n": self.n, "baseline": baseline, "brier_model": r4(self.m / n), "brier_baseline": r4(self.b / n), "brier_diff": r4(md), "brier_diff_se": r4(sd / n.sqrt()) })
    }
}

fn erf(x: f64) -> f64 {
    let s = x.signum(); let x = x.abs(); let t = 1.0 / (1.0 + 0.3275911 * x);
    s * (1.0 - (((((1.061405429 * t - 1.453152027) * t) + 1.421413741) * t - 0.284496736) * t + 0.254829592) * t * (-x * x).exp())
}

/// GET /backtest/nba/sim?season=2026&games=150 — each game simulated with the
/// previous season's league parameters and data strictly before tip-off.
pub async fn backtest(state: web::Data<State>, q: web::Query<BacktestQuery>) -> impl Responder {
    let db = &state.db;
    let from = q.from.as_deref().and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok());
    let all: Vec<String> = match db.query("SELECT espn_id FROM nba_games_espn WHERE season = $1 AND season_type = 2 AND completed \
                                           AND ($2::date IS NULL OR game_date >= $2) ORDER BY game_date, espn_id", &[&q.season, &from]).await {
        Ok(r) => r.iter().map(|x| x.get(0)).collect(), Err(e) => return internal(dberr(&e)),
    };
    // skip the first month: players need some history
    let all: Vec<String> = if from.is_none() { all.into_iter().skip(200).collect() } else { all };
    if all.is_empty() { return unprocessable("no games"); }
    let step = (all.len() as f64 / q.games.max(1) as f64).max(1.0);
    let picked: Vec<String> = (0..q.games.min(all.len())).map(|i| all[(i as f64 * step) as usize].clone()).collect();
    let t0 = Instant::now();
    let (mut hw, mut cover, mut over, mut pts, mut reb, mut ast, mut f3) = (Binary::default(), Binary::default(), Binary::default(), Binary::default(), Binary::default(), Binary::default(), Binary::default());
    let (mut tot_err, mut tot_bias, mut tot_base_err, mut nt) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    let (mut poss_sim, mut poss_act) = (0.0, 0.0);
    let mut skipped = Vec::new();
    let mut sims = 0;
    for gid in &picked {
        let gref = match game_ref(db, gid).await { Ok(g) => g, Err(e) => { skipped.push(json!({ "game": gid, "why": e })); continue } };
        let (Some(hs), Some(as_)) = (gref.home_score, gref.away_score) else { continue };
        let o = Opts { mode: LineupMode::parse(q.lineup.as_deref()), out: HashSet::new(), force_in: HashSet::new() };
        let Built { game, .. } = match build(&state, &gref, &o).await { Ok(b) => b, Err(e) => { skipped.push(json!({ "game": gid, "why": e })); continue } };
        let seed = sim::derive_seed(q.seed, &[gid]);
        let (n, draws) = (q.n, q.draws);
        let g2 = game.clone();
        let e = match web::block(move || sim::simulate(&g2, n, draws, seed)).await { Ok(e) => e, Err(err) => return internal(err) };
        sims += 1;
        let nn = e.n as f64;
        let y = (hs > as_) as i32 as f64;
        let mkt = gref.spread.map(|s| 0.5 * (1.0 + erf(-s / (12.0 * std::f64::consts::SQRT_2))));
        hw.add(e.home_win as f64 / nn, mkt.unwrap_or(0.58), y);
        if let Some(s) = gref.spread {
            let (o, u) = e.margin_line(-s);
            let m = (hs - as_) as f64;
            if (m + s).abs() > 1e-9 { cover.add(o / (o + u).max(1e-9), 0.5, (m > -s) as i32 as f64); }
        }
        let tot = (hs + as_) as f64;
        if let Some(l) = gref.over_under {
            let (o, u, _, _) = e.total.line(l);
            if (tot - l).abs() > 1e-9 { over.add(o / (o + u).max(1e-9), 0.5, (tot > l) as i32 as f64); }
            tot_base_err += (l - tot).abs();
        }
        tot_err += (e.total.mean() - tot).abs();
        tot_bias += e.total.mean() - tot;
        nt += 1.0;
        poss_sim += (e.possessions[0] + e.possessions[1]) / nn;
        // actual box + prior per-player frequency baseline
        let act = match db.query("SELECT team, player_id, pts, oreb, dreb, ast, fg3m, fga, fta, tov, min FROM nba_box_espn WHERE espn_id = $1 AND NOT dnp", &[gid]).await {
            Ok(r) => r, Err(err) => return internal(dberr(&err)),
        };
        let mut pa = 0.0;
        for r in &act {
            let g = |i: usize| r.get::<_, Option<i32>>(i).unwrap_or(0) as f64;
            pa += g(7) - g(3) + g(9) + 0.44 * g(8);
        }
        poss_act += pa;
        let mut actual: HashMap<String, [f64; 4]> = HashMap::new();
        for r in &act {
            let g = |i: usize| r.get::<_, Option<i32>>(i).unwrap_or(0) as f64;
            if r.get::<_, Option<f64>>(10).unwrap_or(0.0) <= 0.0 { continue; }
            actual.insert(r.get(1), [g(2), g(3) + g(4), g(5), g(6)]);
        }
        let pids: Vec<String> = game.teams.iter().flat_map(|t| t.players.iter().map(|p| p.id.clone())).collect();
        let hist = match db.query("SELECT b.player_id, b.pts, b.oreb, b.dreb, b.ast, b.fg3m FROM nba_box_espn b JOIN nba_games_espn g USING (espn_id) \
                                   WHERE b.player_id = ANY($1) AND g.game_date < $2 AND g.game_date >= $2 - 450 AND NOT b.dnp AND COALESCE(b.min,0) > 0",
                                  &[&pids, &gref.date]).await { Ok(r) => r, Err(err) => return internal(dberr(&err)) };
        let mut past: HashMap<String, Vec<[f64; 4]>> = HashMap::new();
        for r in &hist {
            let g = |i: usize| r.get::<_, Option<i32>>(i).unwrap_or(0) as f64;
            past.entry(r.get(0)).or_default().push([g(1), g(2) + g(3), g(4), g(5)]);
        }
        for t in 0..2 {
            for (i, p) in game.teams[t].players.iter().enumerate() {
                let (Some(a), Some(ph)) = (actual.get(&p.id), past.get(&p.id)) else { continue };
                if ph.len() < 5 || e.active_n[t][i] == 0 { continue; }
                for (k, b, stat) in [(0usize, &mut pts, 0usize), (1, &mut reb, 1), (2, &mut ast, 2), (3, &mut f3, 3)] {
                    let avg = ph.iter().map(|x| x[k]).sum::<f64>() / ph.len() as f64;
                    if k == 3 && avg < 0.8 { continue; }
                    let line = avg.floor() + 0.5;
                    let freq = (ph.iter().filter(|x| x[k] > line).count() as f64 + 1.0) / (ph.len() as f64 + 2.0);
                    let (o, _, _, _) = e.player[t][i][stat].line(line);
                    b.add(o, freq, (a[k] > line) as i32 as f64);
                }
            }
        }
    }
    let nt = nt.max(1.0);
    let report = json!({
        "model_version": MODEL_VERSION, "season": q.season, "games_simulated": sims, "sims_per_game": q.n, "lineup": LineupMode::parse(q.lineup.as_deref()).as_str(),
        "secs": t0.elapsed().as_secs(), "skipped": skipped.into_iter().take(10).collect::<Vec<_>>(),
        "possessions_per_game": { "simulated": r3(poss_sim / sims.max(1) as f64), "actual": r3(poss_act / sims.max(1) as f64) },
        "total_points": { "mae_model": r3(tot_err / nt), "mae_line": r3(tot_base_err / nt), "bias": r3(tot_bias / nt) },
        "binary": {
            "home_win": hw.report("closing spread (normal, sd 12)"), "home_cover": cover.report("0.5 at the line"), "total_over": over.report("0.5 at the line"),
            "pts_over_avg": pts.report("player's own prior frequency"), "reb_over_avg": reb.report("player's own prior frequency"),
            "ast_over_avg": ast.report("player's own prior frequency"), "fg3m_over_avg": f3.report("player's own prior frequency"),
        },
    });
    if sims >= 20 {
        let key = format!("nba:backtest:{}:{}", q.season, LineupMode::parse(q.lineup.as_deref()).as_str());
        let _ = db.execute("INSERT INTO sim_model_params (key, params, source) VALUES ($1, $2, 'walk-forward backtest') \
                            ON CONFLICT (key) DO UPDATE SET params = EXCLUDED.params, fitted_at = now()", &[&key, &Json(report.clone())]).await;
    }
    HttpResponse::Ok().json(report)
}
