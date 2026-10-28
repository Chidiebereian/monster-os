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
use crate::learning::{freq_over, Forecast, GameEval, Outcomes, Prediction, Scorecard, SlateGame};
use crate::State;

pub const MODEL_VERSION: &str = "nba-poss-sim-v4";

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
    pub tune: Tuning,
}

crate::learning::tuning!(Tuning {
    team_halflife: 15.0, 3.0, 60.0, "team aggregate recency half-life, in team games";
    player_halflife: 20.0, 3.0, 80.0, "player rate recency half-life, in games played";
    minutes_halflife: 5.0, 1.0, 20.0, "half-life of the minutes projection over the last 10 games";
    rate_prior_min: 60.0, 5.0, 600.0, "minutes of league-average prior on per-minute rebound / assist / steal / block rates";
    usage_prior_min: 60.0, 5.0, 600.0, "minutes of league-average prior on usage";
    def_prior: 800.0, 50.0, 5000.0, "attempts of league-average prior on team defence (2P%, 3P%, turnovers)";
    reb_prior: 400.0, 50.0, 4000.0, "rebound chances of league-average prior on team rebounding";
    pace_prior_games: 8.0, 1.0, 60.0, "games of league-average prior on team pace";
    rotation_depth: 12.0, 7.0, 15.0, "players (by projected minutes) treated as the rotation; deeper players share deep_bench_minutes";
    starter_protect: 0.85, 0.0, 1.0, "1 = projected starters keep their minutes when the rotation is scaled to 240; 0 = scaled like everyone else";
    deep_bench_minutes: 12.0, 0.0, 48.0, "garbage-time minutes shared by all players outside the rotation (in proportion to their own projections)";
    rotation_scale: 1.0, 0.0, 1.0, "1 = scale the rotation's projected minutes to the 240 available; 0 = leave the cut to the simulator (heaviest first)";
    preseason_only_cap: 18.0, 4.0, 30.0, "projected-minutes cap for players whose only history is preseason (rookies, camp invites)";
    b2b_pts: 2.4, 0.0, 6.0, "points of margin a team loses on the second night of a back-to-back";
    tz_east_pts: 0.7, 0.0, 2.0, "points of margin lost per time zone crossed eastward since the previous game";
    tz_west_pts: 0.3, 0.0, 2.0, "points of margin lost per time zone crossed westward since the previous game";
    rest_total_pts: 0.9, -2.0, 3.0, "points added to the game total per team with three or more days off";
    schedule_scale: 0.0, 0.0, 1.0, "1 = apply the schedule effects above; 0 = ignore the schedule";
    impact_weight: 0.0, 0.0, 1.5, "share of the lineup gap (player impact ratings, tonight vs the team's last ten games) added to the margin; 0 = off";
    blowout: 0.0, 0.0, 1.0, "1 = garbage time: core players sit in the 4th once the margin reaches blowout_base + blowout_per_min x minutes left; 0 = off";
    blowout_base: 8.0, 0.0, 30.0, "garbage-time margin threshold with no time left";
    blowout_per_min: 1.5, 0.0, 4.0, "points added to the garbage-time threshold per minute left in the 4th";
    blowout_core: 6.0, 5.0, 9.0, "players (by drawn minutes) who sit in garbage time";
    ref_weight: 0.0, 0.0, 1.5, "share of the referee crew's free-throw tendency applied to shooting-foul trips (nba_refs.rs; crews posted ~9:00 ET); 0 = off";
    blowout_boost: 1.6, 0.0, 4.0, "minutes added to the core players' draws so their average over all game scripts stays their real average";
});

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

/// Arena time zone (standard-time UTC offset), by home team. A game's venue is
/// its home team's arena (neutral-site games are rare enough to ignore).
fn arena_tz(team: &str) -> Option<i32> {
    Some(match team {
        "ATL" | "BOS" | "BKN" | "CHA" | "CLE" | "DET" | "IND" | "MIA" | "NY" | "ORL" | "PHI" | "TOR" | "WSH" => -5,
        "CHI" | "DAL" | "HOU" | "MEM" | "MIL" | "MIN" | "NO" | "OKC" | "SA" => -6,
        "DEN" | "PHX" | "UTAH" => -7,
        "GS" | "LAC" | "LAL" | "POR" | "SAC" => -8,
        _ => return None,
    })
}

/// A team's schedule going into a game: days since its previous game (same
/// season, regular season or playoffs; 5 when it is the first) and the time
/// zones crossed to get here (positive = travelled east).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Schedule { pub rest_days: i64, pub tz_shift: i32 }

async fn schedule(db: &tokio_postgres::Client, team: &str, g: &GameRef) -> Result<Schedule, String> {
    let prev = db.query_opt("SELECT game_date, home FROM nba_games_espn WHERE (home=$1 OR away=$1) AND season=$2 AND season_type IN (2,3) AND game_date < $3 \
        ORDER BY game_date DESC LIMIT 1", &[&team, &g.season, &g.date]).await.map_err(|e| dberr(&e))?;
    Ok(match prev {
        Some(r) => {
            let (d, venue): (NaiveDate, String) = (r.get(0), r.get(1));
            let tz = match (arena_tz(&g.home), arena_tz(&venue)) { (Some(a), Some(b)) => a - b, _ => 0 };
            Schedule { rest_days: (g.date - d).num_days(), tz_shift: tz }
        }
        None => Schedule { rest_days: 5, tz_shift: 0 },
    })
}

/// Points a schedule adds to the team's own scoring and to its opponent's
/// (measured on 6,153 regular-season games 2022-2026 with team-season strength
/// held fixed: a back-to-back costs 2.4 points of margin, each time zone
/// crossed eastward 0.7 and westward 0.3; three or more days off add 0.9 to
/// the total). Margin effects are split evenly between offence and defence.
pub(crate) fn schedule_points(s: &Schedule, t: &Tuning) -> (f64, f64) {
    let mut margin = 0.0;
    if s.rest_days == 1 { margin -= t.b2b_pts; }
    margin -= if s.tz_shift > 0 { t.tz_east_pts * s.tz_shift as f64 } else { t.tz_west_pts * (-s.tz_shift) as f64 };
    let total = if s.rest_days >= 3 { t.rest_total_pts } else { 0.0 };
    let (own, opp) = (margin / 2.0 + total / 2.0, -margin / 2.0 + total / 2.0);
    (own * t.schedule_scale, opp * t.schedule_scale)
}

/// Points per unit of log shooting odds for one team-game: turns a points
/// effect into the odds multiplier the simulator applies to made shots.
fn points_per_logodds(lg: &League, pace: f64) -> f64 {
    let shots = pace * (1.0 - lg.tov_share - lg.trip_share).max(0.4) * (1.0 + lg.oreb_pct * 0.5);
    let var = lg.three_share * lg.fg3 * (1.0 - lg.fg3) * 3.0 + (1.0 - lg.three_share) * lg.fg2 * (1.0 - lg.fg2) * 2.0;
    (shots * var).max(10.0)
}

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
    // player impact ratings in force before this game (nba_impact.rs)
    // only when availability is known (confirmed: the injury report; actual: hindsight). A
    // projected lineup is "whoever played lately", so its gap would be noise.
    // referee crew (nba_refs.rs): official.nba.com posts it ~9:00 ET on game day; ESPN keeps it
    let mut trip_mult = 1.0;
    let mut officials = Value::Null;
    if opts.tune.ref_weight > 0.0 {
        let crew = crate::nba_refs::crew(db, &g.id).await?;
        if crew.len() == 3 {
            let r = crate::nba_refs::load(db, g.date).await?;
            let effect = crate::nba_refs::crew_effect(&r, &crew);
            trip_mult = (1.0 + opts.tune.ref_weight * effect / r.league_fta.max(20.0)).clamp(0.8, 1.25);
            officials = json!({ "crew": crew, "fta_effect": (effect * 100.0).round() / 100.0, "trip_mult": (trip_mult * 1000.0).round() / 1000.0, "fit_asof": r.asof });
        } else {
            flags.push("referee crew not posted yet: league-average whistle".into());
        }
    }
    let (impact, impact_asof) = if opts.tune.impact_weight > 0.0 && opts.mode != LineupMode::Projected { crate::nba_impact::load(db, g.date).await? } else { (HashMap::new(), None) };
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
        let mut recent_starts: HashMap<String, usize> = HashMap::new(); // starts in the team's last 5 games
        for r in &rows {
            let gid: String = r.get(0);
            let i = gi[gid.as_str()];
            let w = 0.5f64.powf(i as f64 / opts.tune.team_halflife);
            let team: String = r.get(1);
            let t = tot_from(r, 7);
            if &team == abbr { own.add(&t, w) } else { opp.add(&t, w) }
            if &team == abbr && seen_g.insert(gid.clone()) { gw += w; }
            if &team == abbr {
                let e = pool.entry(r.get(2)).or_insert((r.get(3), r.get(4), Vec::new(), None));
                if !r.get::<_, bool>(6) && t.min > 0.0 { e.2.push(i); }
                if r.get::<_, bool>(5) && e.3.map(|x| i < x).unwrap_or(true) { e.3 = Some(i); }
                if r.get::<_, bool>(5) && i < 5 { *recent_starts.entry(r.get(2)).or_default() += 1; }
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
            for (i, x) in games.iter().enumerate() { let w = 0.5f64.powf(i as f64 / opts.tune.player_halflife); t.add(x, w); wsum += w; }
            let mins: Vec<f64> = games.iter().take(10).map(|x| x.min).collect();
            let (mm, msd) = if mins.len() >= 3 {
                let wts: Vec<f64> = (0..mins.len()).map(|i| 0.5f64.powf(i as f64 / opts.tune.minutes_halflife)).collect();
                let ws: f64 = wts.iter().sum();
                let m = mins.iter().zip(&wts).map(|(a, w)| a * w).sum::<f64>() / ws;
                let v = mins.iter().zip(&wts).map(|(a, w)| w * (a - m).powi(2)).sum::<f64>() / ws;
                (m, v.sqrt().clamp(2.0, 10.0))
            } else { (if c.starter { 28.0 } else { 12.0 }, 6.0) };
            // preseason-only history (rookies, new signings): preseason minutes overstate roles
            let mm = if by.get(&c.id).map(|v| v.len()).unwrap_or(0) < 3 { mm.min(opts.tune.preseason_only_cap) } else { mm };
            let u = t.usage();
            let per_min = |x: f64, lgv: f64| (x + opts.tune.rate_prior_min * lgv) / (t.min + opts.tune.rate_prior_min);
            let lg_per_min = |f: fn(&Tot) -> f64| f(&t) / t.min.max(1.0); // own, for meta
            let _ = lg_per_min;
            let shots = chart.remove(&c.id).unwrap_or_default();
            players.push(sim::Player {
                id: c.id.clone(), name: c.name.clone(), pos: c.pos.clone(), p_active: c.p, starter: c.starter,
                min_mean: mm, min_sd: msd,
                usage: (u + opts.tune.usage_prior_min * 0.42) / (t.min + opts.tune.usage_prior_min),
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
        // after a preseason game the projected starters are the five who started
        // most of the team's recent (preseason) games, then the heaviest minutes
        if last_was_preseason && opts.mode != LineupMode::Actual {
            let starts: Vec<usize> = cands.iter().map(|c| recent_starts.get(&c.id).copied().unwrap_or(0)).collect();
            let mut order: Vec<usize> = (0..players.len()).filter(|&i| players[i].p_active > 0.0).collect();
            order.sort_by(|a, b| starts[*b].cmp(&starts[*a]).then(players[*b].min_mean.total_cmp(&players[*a].min_mean)));
            for (k, i) in order.iter().enumerate() { players[*i].starter = k < 5; pmeta[*i]["starter"] = json!(k < 5); }
        }
        // fill starters: ruled-out starters replaced by the heaviest-minute active non-starters
        let n_start = players.iter().filter(|p| p.starter && p.p_active > 0.0).count();
        if n_start < 5 {
            let mut bench: Vec<usize> = (0..players.len()).filter(|&i| !players[i].starter && players[i].p_active > 0.0).collect();
            bench.sort_by(|a, b| players[*b].min_mean.total_cmp(&players[*a].min_mean));
            for i in bench.into_iter().take(5 - n_start) { players[i].starter = true; pmeta[i]["starter"] = json!(true); }
        }
        // Minutes budget. Projected minutes come from each player's own recent
        // games, so when many players are active (opening week: everyone who
        // played in the preseason) they add up to far more than 240 and the
        // simulator's heaviest-first cut starves whoever ranks ninth (a starting
        // center got 2 minutes). Starters keep their projection; players outside
        // the rotation share a garbage-time allowance; the rest of the rotation is
        // scaled to the minutes that remain.
        if opts.mode != LineupMode::Actual {
            let dem = |p: &sim::Player| p.min_mean * p.p_active;
            let mut order: Vec<usize> = (0..players.len()).filter(|&i| players[i].p_active > 0.0).collect();
            order.sort_by(|a, b| (players[*b].starter as u8).cmp(&(players[*a].starter as u8)).then(dem(&players[*b]).total_cmp(&dem(&players[*a]))));
            if order.iter().map(|&i| dem(&players[i])).sum::<f64>() > 240.0 {
                let depth = (opts.tune.rotation_depth.round().max(5.0) as usize).min(order.len());
                let (rot, deep) = order.split_at(depth);
                let d0: f64 = deep.iter().map(|&i| dem(&players[i])).sum();
                if d0 > opts.tune.deep_bench_minutes && d0 > 0.0 {
                    let k = opts.tune.deep_bench_minutes / d0;
                    for &i in deep { players[i].min_mean *= k; }
                }
                let d: f64 = deep.iter().map(|&i| dem(&players[i])).sum();
                let protect = opts.tune.starter_protect;
                let fixed: f64 = rot.iter().filter(|&&i| players[i].starter).map(|&i| protect * dem(&players[i])).sum();
                let flex: f64 = rot.iter().map(|&i| dem(&players[i])).sum::<f64>() - fixed;
                if opts.tune.rotation_scale > 0.0 && fixed + flex + d > 240.0 && flex > 0.0 {
                    let f = (((240.0 - d - fixed).max(0.4 * flex)) / flex).min(1.0).powf(opts.tune.rotation_scale);
                    for &i in rot {
                        // a protected starter scales only on the unprotected part of his projection
                        let share = if players[i].starter { 1.0 - protect } else { 1.0 };
                        players[i].min_mean *= 1.0 - share * (1.0 - f);
                    }
                }
                for &i in &order { pmeta[i]["min_mean"] = json!(r3(players[i].min_mean)); }
            }
        }
        if players.iter().filter(|p| p.p_active > 0.0).count() < 8 {
            flags.push(format!("{abbr}: fewer than 8 available players"));
        }
        let pace = {
            let gms = (own.min / 240.0).max(0.0);
            let poss = 0.5 * (own.poss() + opp.poss());
            let p = if gms > 0.0 { poss / gms } else { league.pace };
            (p * gms + league.pace * opts.tune.pace_prior_games) / (gms + opts.tune.pace_prior_games)
        };
        let o = |p: f64| p / (1.0 - p);
        let ratio = |made: f64, att: f64, lgp: f64, k: f64| o((made + k * lgp) / (att + k)) / o(lgp);
        let def_fg2 = ratio(opp.fgm - opp.f3m, opp.fga - opp.f3a, league.fg2, opts.tune.def_prior);
        let def_fg3 = ratio(opp.f3m, opp.f3a, league.fg3, opts.tune.def_prior);
        let def_tov = ratio(opp.tov, opp.usage(), league.tov_share, opts.tune.def_prior);
        let dreb = ratio(own.dreb, own.dreb + opp.oreb, 1.0 - league.oreb_pct, opts.tune.reb_prior);
        let oreb = ratio(own.oreb, own.oreb + opp.dreb, league.oreb_pct, opts.tune.reb_prior);
        let sched = schedule(db, abbr, g).await?;
        let (mut own_pts, mut opp_pts) = schedule_points(&sched, &opts.tune);
        // lineup impact: tonight's expected minutes against the team's normal
        // (its last ten games), both at the players' ratings; the minutes of
        // anyone missing go to whoever plays, so the gap is who, not how many
        let lineup_gap = if impact.is_empty() { None } else {
            let mut normal_min: HashMap<String, f64> = HashMap::new();
            let n10 = tg.len().min(10) as f64;
            for r in &rows {
                let (gid, team, pid): (String, String, String) = (r.get(0), r.get(1), r.get(2));
                if &team != abbr || gi[gid.as_str()] >= 10 { continue; }
                *normal_min.entry(pid).or_default() += tot_from(r, 7).min / n10;
            }
            let normal: f64 = normal_min.iter().map(|(p, m)| impact.get(p).copied().unwrap_or(0.0) * m / 48.0).sum();
            let exp: Vec<(f64, f64)> = players.iter().map(|p| (impact.get(&p.id).copied().unwrap_or(0.0), p.min_mean * p.p_active)).collect();
            let mins: f64 = exp.iter().map(|x| x.1).sum();
            let tonight = exp.iter().map(|(r, m)| r * m / 48.0).sum::<f64>() * 240.0 / mins.max(1.0);
            Some((tonight - normal, tonight, normal))
        };
        if let Some((gap, _, _)) = lineup_gap {
            own_pts += opts.tune.impact_weight * gap / 2.0;
            opp_pts -= opts.tune.impact_weight * gap / 2.0;
        }
        let k = points_per_logodds(&league, pace);
        let (shoot, allow) = ((own_pts / k).exp(), (opp_pts / k).exp());
        team_meta.push(json!({
            "abbr": abbr, "recent_games": tg.len(), "pace": r3(pace), "def_fg2": r3(def_fg2), "def_fg3": r3(def_fg3), "def_tov": r3(def_tov),
            "dreb": r3(dreb), "oreb": r3(oreb), "players": pmeta,
            "schedule": { "rest_days": sched.rest_days, "back_to_back": sched.rest_days == 1, "tz_shift": sched.tz_shift,
                "own_points": r3(own_pts), "opponent_points": r3(opp_pts), "shoot": r4(shoot), "allow": r4(allow) },
            "lineup_impact": lineup_gap.map(|(gap, tonight, normal)| json!({ "gap": r3(gap), "tonight": r3(tonight), "normal": r3(normal), "ratings_asof": impact_asof })),
            "starters": {
                "projected": if last_was_preseason { players.iter().filter(|p| p.starter).map(|p| p.id.clone()).collect::<Vec<_>>() } else { last_game_starters },
                "actual": actual.iter().filter(|(_, v)| v.0 && &v.2 == abbr).map(|(k, _)| k.clone()).collect::<Vec<_>>(),
                "used": players.iter().filter(|p| p.starter).map(|p| p.id.clone()).collect::<Vec<_>>(),
            },
        }));
        let _ = ti;
        team_inputs.push(sim::Team { abbr: abbr.clone(), players, pace, def_fg2, def_fg3, def_tov, dreb, oreb, shoot, allow });
    }
    if opts.mode == LineupMode::Confirmed && g.date < today {
        flags.push("past game: the injury report is only available for upcoming games, so 'confirmed' = projected availability".into());
    }
    let [a, h]: [sim::Team; 2] = team_inputs.try_into().map_err(|_| "teams")?;
    let meta = json!({
        "model_version": MODEL_VERSION, "params": params_key, "data_cutoff": g.date, "lineup_mode": opts.mode.as_str(),
        "actual_available": !actual.is_empty(), "teams": team_meta, "flags": flags, "officials": officials,
        "assumptions": [
            "possession model: usage x outcome (turnover / two-shot trip / 2 or 3 FGA) with player x defence odds ratios; no fouls, and-1s or timeouts",
            "rotation: minutes from each player's recent games, greedy 2-minute stints, starters open the 1st and 3rd quarters",
            "injury designations -> play probabilities are documented priors (Out 0, Doubtful 0.15, Questionable 0.5, Day-To-Day 0.6, Probable 0.9), not fitted",
            "schedule: back-to-backs, time zones crossed and 3+ days off shift each team's shooting by effects measured on 2022-2026 regular seasons (team strength held fixed)",
            "shot locations on the court are drawn from the shooter's real ESPN shot chart (display only)",
        ],
    });
    let t = &opts.tune;
    let blowout = sim::Blowout { on: t.blowout > 0.5, base: t.blowout_base, per_min: t.blowout_per_min, core: t.blowout_core.round() as usize, boost: t.blowout_boost };
    Ok(Built { game: GameInput { teams: [a, h], league, home_edge, blowout, trip_mult }, meta })
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
    Opts { mode: LineupMode::parse(lineup), out: out.iter().cloned().collect(), force_in: force_in.iter().cloned().collect(), tune: Tuning::default() }
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
    let o = Opts { mode: LineupMode::parse(q.lineup.as_deref()), out: ids(&q.out), force_in: ids(&q.force_in), tune: Tuning::default() };
    let Built { game, meta } = match build(&state, &gref, &o).await { Ok(b) => b, Err(e) => return unprocessable(e) };
    let seed = q.seed.unwrap_or_else(|| sim::derive_seed(0x6e6261, &[&id, &chrono::Utc::now().timestamp_millis().to_string()]));
    let (n, draws) = (q.n.clamp(100_000, 1_000_000), q.draws.clamp(50, 2000));
    let t = Instant::now();
    let g2 = game.clone();
    let e = match web::block(move || sim::simulate(&g2, n, draws, seed)).await { Ok(e) => e, Err(err) => return internal(err) };
    HttpResponse::Ok().json(payload(&gref, &game, &e, meta, seed, t.elapsed().as_millis() as u64))
}

fn payload(gref: &GameRef, game: &GameInput, e: &sim::Ensemble, meta: Value, seed: u64, ms: u64) -> Value {
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
    json!({
        "game_id": gref.id, "date": gref.date, "away": gref.away, "home": gref.home, "model_version": MODEL_VERSION, "seed": seed, "n": e.n, "sim_ms": ms,
        "win": { "home": r4(e.home_win as f64 / nn), "away": r4(1.0 - e.home_win as f64 / nn), "overtime": r4(e.overtime as f64 / nn) },
        "score": { "away": hist_json(&e.score[0], &[]), "home": hist_json(&e.score[1], &[]) },
        "total": hist_json(&e.total, &gref.over_under.into_iter().collect::<Vec<_>>()),
        "margin_home": { "mean": r3(e.margin_mean()), "pmf_offset": -sim::MARGIN_OFFSET,
            "pmf": e.margin.iter().map(|c| r4(*c as f64 / nn)).collect::<Vec<_>>() },
        "possessions": { "away": r3(e.possessions[0] / nn), "home": r3(e.possessions[1] / nn) },
        "h1": e.h1.json("nba"),
        "market": { "spread": spread_home, "over_under": gref.over_under,
            "p_home_cover": spread_home.map(|s| r4(e.margin_line(-s).0)), "p_over": gref.over_under.map(|l| r4(e.total.line(l).0)) },
        "players": { "away": players[0], "home": players[1] },
        "inputs": meta,
        "note": "player props are conditional on the player being active (p_active); mcse is Monte Carlo sampling error only",
    })
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
    let fin = json!({ "score": res.score, "overtime": res.overtime, "h1": res.seg });
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
    /// tuning overrides as JSON (e.g. {"rotation_depth": 10})
    cfg: Option<String>,
    to: Option<String>,
}
fn d_bt_season() -> i32 { 2026 }
fn d_bt_games() -> usize { 150 }
fn d_bt_n() -> usize { 2000 }
fn d_bt_draws() -> usize { 20 }
fn d_bt_seed() -> u64 { 777 }

fn erf(x: f64) -> f64 {
    let s = x.signum(); let x = x.abs(); let t = 1.0 / (1.0 + 0.3275911 * x);
    s * (1.0 - (((((1.061405429 * t - 1.453152027) * t) + 1.421413741) * t - 0.284496736) * t + 0.254829592) * t * (-x * x).exp())
}

/// GET /backtest/nba/sim?season=2026&games=150 — each game simulated with the
/// previous season's league parameters and data strictly before tip-off.
pub async fn backtest(state: web::Data<State>, q: web::Query<BacktestQuery>) -> impl Responder {
    let db = &state.db;
    let from = q.from.as_deref().and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok());
    let all: Vec<String> = if from.is_none() { match backtest_games(db, q.season).await { Ok(a) => a, Err(e) => return internal(e) } } else {
        let to = q.to.as_deref().and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok()).unwrap_or(NaiveDate::MAX);
        match db.query("SELECT espn_id FROM nba_games_espn WHERE season = $1 AND season_type = 2 AND completed AND game_date >= $2 AND game_date <= $3 ORDER BY game_date, espn_id", &[&q.season, &from, &to]).await {
            Ok(r) => r.iter().map(|x| x.get(0)).collect(), Err(e) => return internal(dberr(&e)),
        }
    };
    if all.is_empty() { return unprocessable("no games"); }
    let step = (all.len() as f64 / q.games.max(1) as f64).max(1.0);
    let picked: Vec<String> = (0..q.games.min(all.len())).map(|i| all[(i as f64 * step) as usize].clone()).collect();
    let t0 = Instant::now();
    let lineup = LineupMode::parse(q.lineup.as_deref()).as_str();
    let cfg: Value = q.cfg.as_deref().and_then(|c| serde_json::from_str(c).ok()).unwrap_or_else(|| json!({}));
    let mut card = Scorecard::default();
    let (mut tot_err, mut tot_bias, mut tot_base_err, mut nt) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    let (mut poss_sim, mut poss_act) = (0.0, 0.0);
    let (mut min_err, mut min_n, mut min_miss) = (0.0f64, 0.0f64, 0.0f64);
    let (mut st_bias, mut st_n) = (0.0f64, 0.0f64);
    let mut skipped = Vec::new();
    let mut sims = 0;
    for gid in &picked {
        let seed = sim::derive_seed(q.seed, &[gid]);
        let ev = match evaluate(&state, gid, lineup, &cfg, q.n, q.draws, seed).await {
            Ok(Some(e)) => e, Ok(None) => continue, Err(e) => { skipped.push(json!({ "game": gid, "why": e })); continue }
        };
        sims += 1;
        card.add_all(&ev.forecasts, &ev.outcomes);
        for (k, s, a) in &ev.diag {
            match *k {
                "total_points" => { tot_err += (s - a).abs(); tot_bias += s - a; nt += 1.0; }
                "total_line" => { tot_base_err += (s - a).abs(); }
                "possessions" => { poss_sim += s; poss_act += a; }
                "starter_minutes" => { st_bias += s - a; st_n += 1.0; }
                "player_minutes" => { min_err += (s - a).abs(); min_n += 1.0; if *a >= 15.0 && *s < 8.0 { min_miss += 1.0; } }
                _ => {}
            }
        }
    }
    let nt = nt.max(1.0);
    let report = json!({
        "model_version": MODEL_VERSION, "season": q.season, "games_simulated": sims, "sims_per_game": q.n, "lineup": lineup,
        "secs": t0.elapsed().as_secs(), "skipped": skipped.into_iter().take(10).collect::<Vec<_>>(),
        "possessions_per_game": { "simulated": r3(poss_sim / sims.max(1) as f64), "actual": r3(poss_act / sims.max(1) as f64) },
        "total_points": { "mae_model": r3(tot_err / nt), "mae_line": r3(tot_base_err / nt), "bias": r3(tot_bias / nt) },
        "player_minutes": { "mae": r3(min_err / min_n.max(1.0)), "players": min_n, "rotation_misses_per_game": r3(min_miss / sims.max(1) as f64),
            "starter_bias": r3(st_bias / st_n.max(1.0)), "starters": st_n },
        "binary": card.report(),
    });
    if sims >= 20 {
        let key = format!("nba:backtest:{}:{}", q.season, lineup);
        let _ = db.execute("INSERT INTO sim_model_params (key, params, source) VALUES ($1, $2, 'walk-forward backtest') \
                            ON CONFLICT (key) DO UPDATE SET params = EXCLUDED.params, fitted_at = now()", &[&key, &Json(report.clone())]).await;
    }
    HttpResponse::Ok().json(report)
}

// ---------------------------------------------------------------------------
// Learning-loop adapters (see learning.rs)
// ---------------------------------------------------------------------------

/// Regular-season games used for walk-forward tests (the first 200 of a season are skipped so players have history).
pub async fn backtest_games(db: &tokio_postgres::Client, season: i32) -> Result<Vec<String>, String> {
    Ok(db.query("SELECT espn_id FROM nba_games_espn WHERE season = $1 AND season_type = 2 AND completed ORDER BY game_date, espn_id", &[&season]).await
        .map_err(|e| dberr(&e))?.iter().map(|x| x.get::<_, String>(0)).skip(200).collect())
}

pub async fn slate(db: &tokio_postgres::Client, date: NaiveDate) -> Result<Vec<SlateGame>, String> {
    Ok(db.query("SELECT espn_id, game_date, start_time, away, home FROM nba_games_espn WHERE game_date = $1 AND NOT completed AND season_type IN (2,3) ORDER BY start_time, espn_id",
        &[&date]).await.map_err(|e| dberr(&e))?.iter().map(|r| SlateGame { id: r.get(0), date: r.get(1), start: r.get(2),
            label: format!("{} @ {}", r.get::<_, String>(3), r.get::<_, String>(4)) }).collect())
}

/// Prop forecasts at the player's own average line, one per simulator prop
/// (nba_sim::PROPS) the books post. Double- and triple-doubles are yes/no
/// (line 0.5). `min` is a diagnostic, not a market.
const PROP_MARKETS: [(&str, &str); 21] = [("pts_over_avg", "pts"), ("reb_over_avg", "reb"), ("ast_over_avg", "ast"), ("fg3m_over_avg", "fg3m"),
    ("stl_over_avg", "stl"), ("blk_over_avg", "blk"), ("tov_over_avg", "tov"), ("pra_over_avg", "pra"),
    ("pr_over_avg", "pr"), ("pa_over_avg", "pa"), ("ra_over_avg", "ra"), ("stocks_over_avg", "stocks"), ("fgm_over_avg", "fgm"), ("fga_over_avg", "fga"),
    ("fg3a_over_avg", "fg3a"), ("ftm_over_avg", "ftm"), ("fta_over_avg", "fta"), ("oreb_over_avg", "oreb"), ("dreb_over_avg", "dreb"),
    ("double_double_yes", "double_double"), ("triple_double_yes", "triple_double")];

/// A box-score row's value of every prop stat.
fn box_props(g: &dyn Fn(&str) -> f64) -> HashMap<&'static str, f64> {
    let reb = g("oreb") + g("dreb");
    let tens = [g("pts"), reb, g("ast"), g("stl"), g("blk")].iter().filter(|x| **x >= 10.0).count();
    [("pts", g("pts")), ("reb", reb), ("ast", g("ast")), ("fg3m", g("fg3m")), ("stl", g("stl")), ("blk", g("blk")), ("tov", g("tov")),
     ("pra", g("pts") + reb + g("ast")), ("pr", g("pts") + reb), ("pa", g("pts") + g("ast")), ("ra", reb + g("ast")), ("stocks", g("stl") + g("blk")),
     ("fgm", g("fgm")), ("fga", g("fga")), ("fg3a", g("fg3a")), ("ftm", g("ftm")), ("fta", g("fta")), ("oreb", g("oreb")), ("dreb", g("dreb")),
     ("double_double", (tens >= 2) as i32 as f64), ("triple_double", (tens >= 3) as i32 as f64)].into()
}

/// Scoreable forecasts. Market baselines use the line stored for the game; player baselines use only games before it.
async fn forecasts(db: &tokio_postgres::Client, gref: &GameRef, game: &GameInput, e: &sim::Ensemble) -> Result<Vec<Forecast>, String> {
    let nn = e.n.max(1) as f64;
    let mkt = gref.spread.map(|s| 0.5 * (1.0 + erf(-s / (12.0 * std::f64::consts::SQRT_2))));
    let mut out = vec![Forecast::game("home_win", "market spread (normal, sd 12)", "home_margin", 0.0, e.home_win as f64 / nn, mkt.unwrap_or(0.58))];
    if let Some(s) = gref.spread {
        let (o, u) = e.margin_line(-s);
        out.push(Forecast::game("home_cover", "0.5 at the line", "home_margin", -s, o / (o + u).max(1e-9), 0.5).pushes());
    }
    if let Some(l) = gref.over_under {
        let (o, u, _, _) = e.total.line(l);
        out.push(Forecast::game("total_over", "0.5 at the line", "total", l, o / (o + u).max(1e-9), 0.5).pushes());
    }
    // team totals and the 1st half (segments.rs)
    let full_over = |home: bool, l: f64| { let (o, u, _, _) = e.score[home as usize].line(l); o / (o + u).max(1e-9) };
    out.extend(crate::segments::forecasts("nba", gref.spread.map(|s| -s), gref.over_under, &e.h1, &full_over));
    let pids: Vec<String> = game.teams.iter().flat_map(|t| t.players.iter().map(|p| p.id.clone())).collect();
    let hist = db.query("SELECT b.player_id, b.pts, b.oreb, b.dreb, b.ast, b.fg3m, b.stl, b.blk, b.tov, b.fgm, b.fga, b.fg3a, b.ftm, b.fta FROM nba_box_espn b JOIN nba_games_espn g USING (espn_id) \
                         WHERE b.player_id = ANY($1) AND g.game_date < $2 AND g.game_date >= $2 - 450 AND NOT b.dnp AND COALESCE(b.min,0) > 0",
                        &[&pids, &gref.date]).await.map_err(|e| dberr(&e))?;
    const COLS: [&str; 13] = ["pts", "oreb", "dreb", "ast", "fg3m", "stl", "blk", "tov", "fgm", "fga", "fg3a", "ftm", "fta"];
    let mut past: HashMap<String, HashMap<&'static str, Vec<f64>>> = HashMap::new();
    for r in &hist {
        let g = |c: &str| COLS.iter().position(|x| *x == c).map(|i| r.get::<_, Option<i32>>(i + 1).unwrap_or(0) as f64).unwrap_or(0.0);
        let v = past.entry(r.get(0)).or_default();
        for (k, x) in box_props(&g) { v.entry(k).or_default().push(x); }
    }
    for t in 0..2 {
        for (i, p) in game.teams[t].players.iter().enumerate() {
            let Some(ph) = past.get(&p.id) else { continue };
            if ph.get("pts").map(|v| v.len()).unwrap_or(0) < 5 || e.active_n[t][i] == 0 { continue; }
            for (market, stat) in PROP_MARKETS.iter() {
                let (Some(k), Some(vals)) = (sim::PROPS.iter().position(|s| s == stat), ph.get(stat)) else { continue };
                let avg = vals.iter().sum::<f64>() / vals.len() as f64;
                // markets books rarely post for this player
                let thin = match *stat {
                    "fg3m" | "fg3a" | "ftm" | "fta" | "oreb" => avg < 0.8,
                    "stl" | "blk" | "tov" | "stocks" => avg < 0.3,
                    "double_double" => avg < 0.08,
                    "triple_double" => avg < 0.03,
                    _ => false,
                };
                if thin { continue; }
                let line = if stat.ends_with("_double") { 0.5 } else { avg.floor() + 0.5 };
                out.push(Forecast::player(market, "player's own prior frequency", &p.id, &p.name, stat, line, e.player[t][i][k].line(line).0, freq_over(vals, line)));
            }
        }
    }
    Ok(out)
}

/// Final score, possessions, and the box score of players who played; None until final.
pub async fn outcomes(db: &tokio_postgres::Client, id: &str) -> Result<Option<Outcomes>, String> {
    let Some(g) = db.query_opt("SELECT home_score, away_score FROM nba_games_espn WHERE espn_id = $1 AND completed", &[&id]).await.map_err(|e| dberr(&e))? else { return Ok(None) };
    let (Some(hs), Some(as_)) = (g.get::<_, Option<i32>>(0), g.get::<_, Option<i32>>(1)) else { return Ok(None) };
    let act = db.query("SELECT team, player_id, pts, oreb, dreb, ast, fg3m, fga, fta, tov, min, stl, blk, fgm, fg3a, ftm FROM nba_box_espn WHERE espn_id = $1 AND NOT dnp", &[&id]).await.map_err(|e| dberr(&e))?;
    if act.is_empty() { return Ok(None); }
    let mut o = Outcomes::default();
    o.game.insert("home_margin", (hs - as_) as f64);
    o.game.insert("total", (hs + as_) as f64);
    let mut pa = 0.0;
    for r in &act {
        let g = |i: usize| r.get::<_, Option<i32>>(i).unwrap_or(0) as f64;
        pa += g(7) - g(3) + g(9) + 0.44 * g(8);
        if r.get::<_, Option<f64>>(10).unwrap_or(0.0) <= 0.0 { continue; }
        let m = o.players.entry(r.get(1)).or_default();
        m.insert("pts", g(2)); m.insert("reb", g(3) + g(4)); m.insert("ast", g(5)); m.insert("fg3m", g(6));
        m.insert("stl", g(11)); m.insert("blk", g(12)); m.insert("tov", g(9)); m.insert("pra", g(2) + g(3) + g(4) + g(5));
        let reb = g(3) + g(4);
        m.insert("pr", g(2) + reb); m.insert("pa", g(2) + g(5)); m.insert("ra", reb + g(5)); m.insert("stocks", g(11) + g(12));
        m.insert("oreb", g(3)); m.insert("dreb", g(4)); m.insert("fga", g(7)); m.insert("fta", g(8));
        m.insert("fgm", g(13)); m.insert("fg3a", g(14)); m.insert("ftm", g(15));
        let tens = [g(2), reb, g(5), g(11), g(12)].iter().filter(|x| **x >= 10.0).count();
        m.insert("double_double", (tens >= 2) as i32 as f64); m.insert("triple_double", (tens >= 3) as i32 as f64);
        m.insert("min", r.get::<_, Option<f64>>(10).unwrap_or(0.0));
    }
    o.game.insert("possessions", pa);
    crate::segments::augment(db, "nba", id, &mut o).await;
    Ok(Some(o))
}

/// Walk-forward evaluation of one completed game (data strictly before tip-off).
pub async fn evaluate(state: &State, id: &str, lineup: &str, cfg: &Value, n: usize, draws: usize, seed: u64) -> Result<Option<GameEval>, String> {
    let gref = game_ref(&state.db, id).await?;
    if gref.home_score.is_none() || gref.away_score.is_none() { return Ok(None); }
    let Some(outcomes) = outcomes(&state.db, id).await? else { return Ok(None) };
    let o = Opts { mode: LineupMode::parse(Some(lineup)), out: HashSet::new(), force_in: HashSet::new(), tune: Tuning::from_cfg(cfg) };
    let Built { game, .. } = build(state, &gref, &o).await?;
    let g2 = game.clone();
    let e = web::block(move || sim::simulate(&g2, n, draws, seed)).await.map_err(|e| e.to_string())?;
    let forecasts = forecasts(&state.db, &gref, &game, &e).await?;
    let nn = e.n.max(1) as f64;
    let mut diag = vec![("total_points", e.total.mean(), outcomes.game["total"]), ("possessions", (e.possessions[0] + e.possessions[1]) / nn, outcomes.game["possessions"])];
    if let Some(l) = gref.over_under { diag.push(("total_line", l, outcomes.game["total"])); }
    // minutes: expected (mean if active x P(active)) vs actual, over players who played or were expected to
    let min_k = sim::PROPS.iter().position(|s| *s == "min").unwrap_or(8);
    for t in 0..2 {
        for (i, p) in game.teams[t].players.iter().enumerate() {
            let act = outcomes.players.get(&p.id).and_then(|m| m.get("min")).copied().unwrap_or(0.0);
            let pa = e.active_n[t][i] as f64 / nn;
            let exp = if pa > 0.0 { e.player[t][i][min_k].mean() * pa } else { 0.0 };
            if act > 0.0 || exp > 2.0 { diag.push(("player_minutes", exp, act)); }
            // projected starters (chosen before the game) who played: bias of the minutes props depend on
            if p.starter && act > 0.0 && pa > 0.0 { diag.push(("starter_minutes", e.player[t][i][min_k].mean(), act)); }
        }
    }
    Ok(Some(GameEval { forecasts, outcomes, diag }))
}

/// A production prediction (≥100k games) for the ledger.
pub async fn predict(state: &State, id: &str, lineup: &str, cfg: &Value, seed: u64, n: usize) -> Result<Prediction, String> {
    let gref = game_ref(&state.db, id).await?;
    let o = Opts { mode: LineupMode::parse(Some(lineup)), out: HashSet::new(), force_in: HashSet::new(), tune: Tuning::from_cfg(cfg) };
    let Built { game, meta } = build(state, &gref, &o).await?;
    let t = Instant::now();
    let g2 = game.clone();
    let e = web::block(move || sim::simulate(&g2, n, 200, seed)).await.map_err(|e| e.to_string())?;
    let forecasts = forecasts(&state.db, &gref, &game, &e).await?;
    let start: Option<chrono::DateTime<chrono::Utc>> = state.db.query_opt("SELECT start_time FROM nba_games_espn WHERE espn_id = $1", &[&id]).await.map_err(|e| dberr(&e))?.and_then(|r| r.get(0));
    Ok(Prediction { payload: payload(&gref, &game, &e, meta, seed, t.elapsed().as_millis() as u64), forecasts, date: gref.date, start, engine: MODEL_VERSION, n: e.n as usize })
}


/// Parlay Builder: every simulated game of the champion engine, reduced to the
/// game score columns and the requested (player id, prop) columns.
pub async fn parlay_samples(state: &State, id: &str, lineup: &str, cfg: &Value, n: usize, seed: u64, want: &[(String, String)]) -> Result<crate::parlay::Samples, String> {
    let gref = game_ref(&state.db, id).await?;
    let o = Opts { mode: LineupMode::parse(Some(lineup)), out: HashSet::new(), force_in: HashSet::new(), tune: Tuning::from_cfg(cfg) };
    let Built { game, .. } = build(state, &gref, &o).await?;
    let (mut idx, mut found, mut missing) = (Vec::new(), Vec::new(), Vec::new());
    for w in want {
        let k = sim::PROPS.iter().position(|p| *p == w.1);
        let pos = (0..2).find_map(|t| game.teams[t].players.iter().position(|p| p.id == w.0).map(|i| (t, i)));
        match (k, pos) { (Some(k), Some((t, i))) => { idx.push((t, i, k)); found.push(w.clone()); } _ => missing.push(w.clone()) }
    }
    let t0 = Instant::now();
    let rows = web::block(move || sim::sample(&game, n, crate::parlay::DRAWS, seed, &|r: &sim::GameResult| {
        let mut v: Vec<i16> = [r.score[0], r.score[1], r.seg[0], r.seg[1]].iter().map(|x| *x as i16).collect();
        for &(t, i, k) in &idx { v.push(if r.active[t][i] { sim::prop_values(&r.stats[t][i])[k] as i16 } else { crate::parlay::VOID }); }
        v
    })).await.map_err(|e| e.to_string())?;
    Ok(crate::parlay::Samples::from_rows(rows, found, missing, t0.elapsed().as_millis() as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_effects_split_margin_and_total() {
        let t = Tuning { schedule_scale: 1.0, ..Tuning::default() };
        // back-to-back after a game one zone west: -2.4 - 0.7 margin, split
        let (own, opp) = schedule_points(&Schedule { rest_days: 1, tz_shift: 1 }, &t);
        assert!((own + 1.55).abs() < 1e-9 && (opp - 1.55).abs() < 1e-9, "{own} {opp}");
        // three days off, no travel: total only
        let (own, opp) = schedule_points(&Schedule { rest_days: 3, tz_shift: 0 }, &t);
        assert!((own - 0.45).abs() < 1e-9 && (opp - 0.45).abs() < 1e-9);
        let off = Tuning { schedule_scale: 0.0, ..Tuning::default() };
        assert_eq!(schedule_points(&Schedule { rest_days: 1, tz_shift: -3 }, &off), (0.0, 0.0));
        assert_eq!(arena_tz("UTAH"), Some(-7));
    }
}
