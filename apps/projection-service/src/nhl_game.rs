//! NHL event simulator: league fit (incl. an expected-goals model), team /
//! skater / goalie inputs strictly before puck drop, lineups (projected /
//! confirmed / actual), ensemble, broadcast session and walk-forward backtest.
//! Data: NHL API tables from `nhl_api.rs`.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use actix_web::{web, HttpResponse, Responder};
use chrono::NaiveDate;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_postgres::types::Json;

use crate::nhl_sim::{self as sim, GameInput, League, Xg, EV, PP, SH};
use crate::State;

pub const MODEL_VERSION: &str = "nhl-event-sim-v1";

fn internal(m: impl std::fmt::Display) -> HttpResponse {
    eprintln!("projection-service(nhl-sim): {m}");
    HttpResponse::InternalServerError().json(json!({ "error": m.to_string() }))
}
fn unprocessable(m: impl Into<String>) -> HttpResponse { HttpResponse::UnprocessableEntity().json(json!({ "error": m.into() })) }
fn dberr(e: &tokio_postgres::Error) -> String {
    e.as_db_error().map(|d| format!("{} ({})", d.message(), d.detail().unwrap_or(""))).unwrap_or_else(|| e.to_string())
}
fn r3(x: f64) -> f64 { (x * 1000.0).round() / 1000.0 }
fn r4(x: f64) -> f64 { (x * 10000.0).round() / 10000.0 }
fn sidx(s: &str) -> usize { match s { "PP" => PP, "SH" => SH, _ => EV } }

#[derive(Clone, Debug)]
struct Shot { game: i64, team: String, shooter: Option<i64>, #[allow(dead_code)] goalie: Option<i64>, period: i32, ptype: String, sec: i32, kind: String,
              strength: String, skaters: String, x: Option<f64>, y: Option<f64>, ty: usize, a1: Option<i64>, a2: Option<i64>, en: bool }
const SHOT_COLS: &str = "s.game_id, s.team, s.shooter, s.goalie, s.period, COALESCE(s.period_type,'REG'), s.sec, s.kind, s.strength, COALESCE(s.skaters,''), s.x, s.y, COALESCE(s.shot_type,'other'), s.assist1, s.assist2, s.empty_net";
fn shot_row(r: &tokio_postgres::Row) -> Shot {
    Shot { game: r.get(0), team: r.get(1), shooter: r.get(2), goalie: r.get(3), period: r.get(4), ptype: r.get(5), sec: r.get(6), kind: r.get(7),
        strength: r.get(8), skaters: r.get(9), x: r.get(10), y: r.get(11), ty: sim::shot_type_idx(&r.get::<_, String>(12)), a1: r.get(13), a2: r.get(14), en: r.get(15) }
}

// ---------------------------------------------------------------------------
// League fit
// ---------------------------------------------------------------------------

/// Logistic regression by Newton's method (features already include the intercept).
fn logistic(x: &[Vec<f64>], y: &[f64], ridge: f64) -> Vec<f64> {
    let k = x.first().map(|v| v.len()).unwrap_or(0);
    let mut b = vec![0.0; k];
    for _ in 0..30 {
        let mut g = vec![0.0; k];
        let mut h = vec![vec![0.0; k]; k];
        for (xi, yi) in x.iter().zip(y) {
            let z: f64 = xi.iter().zip(&b).map(|(a, c)| a * c).sum();
            let p = 1.0 / (1.0 + (-z).exp());
            let w = p * (1.0 - p);
            for a in 0..k { g[a] += (yi - p) * xi[a]; for c in 0..k { h[a][c] += w * xi[a] * xi[c]; } }
        }
        for a in 1..k { g[a] -= ridge * b[a]; h[a][a] += ridge; }
        // solve h * d = g (Gaussian elimination)
        let mut m = h.clone();
        let mut v = g.clone();
        for i in 0..k {
            let piv = (i..k).max_by(|&a, &c| m[a][i].abs().total_cmp(&m[c][i].abs())).unwrap();
            m.swap(i, piv); v.swap(i, piv);
            if m[i][i].abs() < 1e-12 { continue; }
            for r in 0..k {
                if r == i { continue; }
                let f = m[r][i] / m[i][i];
                for c in i..k { m[r][c] -= f * m[i][c]; }
                v[r] -= f * v[i];
            }
        }
        let d: Vec<f64> = (0..k).map(|i| if m[i][i].abs() < 1e-12 { 0.0 } else { v[i] / m[i][i] }).collect();
        for i in 0..k { b[i] += d[i]; }
        if d.iter().map(|x| x.abs()).sum::<f64>() < 1e-8 { break; }
    }
    b
}

fn xg_features(x: f64, y: f64, ty: usize, pp: bool) -> Vec<f64> {
    let dx = (89.0 - x).max(0.5);
    let dist = (dx * dx + y * y).sqrt();
    let angle = (y.abs() / dx).atan();
    let mut f = vec![1.0, dist, angle, pp as i32 as f64];
    for t in 1..8 { f.push((ty == t) as i32 as f64); } // wrist = baseline
    f
}

/// POST /fit/nhl/sim — league parameters + xG model per NHL season.
pub async fn fit(state: web::Data<State>) -> impl Responder {
    let db = &state.db;
    let seasons: Vec<i32> = match db.query("SELECT DISTINCT season FROM nhl_games_api WHERE completed AND game_type IN (2,3) ORDER BY 1", &[]).await {
        Ok(r) => r.iter().map(|x| x.get(0)).collect(), Err(e) => return internal(dberr(&e)),
    };
    let mut out = Vec::new();
    for season in seasons {
        let games = match db.query("SELECT game_id, away, home, game_type FROM nhl_games_api WHERE season = $1 AND completed AND game_type IN (2,3)", &[&season]).await {
            Ok(r) => r, Err(e) => return internal(dberr(&e)),
        };
        let ginfo: HashMap<i64, (String, String, i32)> = games.iter().map(|r| (r.get(0), (r.get(1), r.get(2), r.get(3)))).collect();
        let ids: Vec<i64> = ginfo.keys().copied().collect();
        let shots: Vec<Shot> = match db.query(&format!("SELECT {SHOT_COLS} FROM nhl_shots_api s WHERE s.game_id = ANY($1) ORDER BY s.game_id, s.period, s.sec, s.event_id"), &[&ids]).await {
            Ok(r) => r.iter().map(shot_row).collect(), Err(e) => return internal(dberr(&e)),
        };
        let pens = match db.query("SELECT game_id, team, duration FROM nhl_penalties_api WHERE game_id = ANY($1)", &[&ids]).await {
            Ok(r) => r, Err(e) => return internal(dberr(&e)),
        };
        let n_games = ginfo.len() as f64;
        // penalties -> PP / SH minutes per team-game (minors 2', majors 5'; misconducts excluded; ~1' lost per PP goal)
        let mut pen_min: HashMap<(i64, String), f64> = HashMap::new();
        let mut durations = Vec::new();
        let mut minors = 0.0;
        for r in &pens {
            let d: i32 = r.get(2);
            if d >= 10 { continue; }
            durations.push(d);
            if d <= 2 { minors += 1.0; }
            *pen_min.entry((r.get(0), r.get(1))).or_default() += d as f64;
        }
        let mut att = [0.0f64; 3];
        let mut blk = [0.0f64; 3];
        let mut mis = [0.0f64; 3];
        let mut goals_s = [0.0f64; 3];
        let mut a1 = [0.0f64; 3];
        let mut a2 = [0.0f64; 3];
        let mut ppg: HashMap<(i64, String), f64> = HashMap::new();
        let (mut home_att, mut away_att) = (0.0f64, 0.0f64);
        let (mut ot_att, mut ot_secs) = (0.0, 0.0);
        let (mut so_att, mut so_goals) = (0.0, 0.0);
        let mut ot_max: HashMap<i64, i32> = HashMap::new();
        let mut xs: Vec<Vec<f64>> = Vec::new();
        let mut ys: Vec<f64> = Vec::new();
        for s in &shots {
            let (away, home, gt) = &ginfo[&s.game];
            if s.ptype == "SO" { so_att += 1.0; so_goals += (s.kind == "goal") as i32 as f64; continue; }
            if s.ptype == "OT" && *gt == 2 { ot_att += 1.0; let e = ot_max.entry(s.game).or_insert(0); *e = (*e).max(s.sec); continue; }
            if s.en || s.skaters.starts_with('6') { continue; }
            let k = sidx(&s.strength);
            att[k] += 1.0;
            if &s.team == home { if k == EV { home_att += 1.0; } } else if &s.team == away && k == EV { away_att += 1.0; }
            match s.kind.as_str() {
                "blocked" => blk[k] += 1.0,
                "missed" => mis[k] += 1.0,
                _ => {
                    if s.kind == "goal" {
                        goals_s[k] += 1.0;
                        if s.a1.is_some() { a1[k] += 1.0; }
                        if s.a2.is_some() { a2[k] += 1.0; }
                        if k == PP { *ppg.entry((s.game, s.team.clone())).or_default() += 1.0; }
                    }
                    if let (Some(x), Some(y)) = (s.x, s.y) {
                        if x > -5.0 { xs.push(xg_features(x, y, s.ty, k == PP)); ys.push((s.kind == "goal") as i32 as f64); }
                    }
                }
            }
        }
        for v in ot_max.values() { ot_secs += (*v).max(30) as f64; }
        // strength minutes
        let (mut ev_min, mut pp_min, mut sh_min) = (0.0, 0.0, 0.0);
        for (g, (away, home, _)) in &ginfo {
            for (t, o) in [(away, home), (home, away)] {
                let pp = (pen_min.get(&(*g, o.clone())).copied().unwrap_or(0.0) - ppg.get(&(*g, t.clone())).copied().unwrap_or(0.0)).max(0.0);
                let sh = (pen_min.get(&(*g, t.clone())).copied().unwrap_or(0.0) - ppg.get(&(*g, o.clone())).copied().unwrap_or(0.0)).max(0.0);
                pp_min += pp; sh_min += sh; ev_min += (60.0 - pp - sh).max(40.0);
            }
        }
        // goalie pulls, from the score at 17:00 of the 3rd and first 6-skater attempts
        let mut by_game: HashMap<i64, Vec<&Shot>> = HashMap::new();
        for s in &shots { by_game.entry(s.game).or_default().push(s); }
        let (mut eligible, mut pulled) = (0.0, 0.0);
        let mut pull_secs = Vec::new();
        let (mut six_att, mut en_att, mut en_unblocked, mut en_goals, mut pull_time) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for (g, ss) in &by_game {
            let (away, home, _) = &ginfo[g];
            let mut score: HashMap<&str, i32> = HashMap::new();
            for s in ss.iter().filter(|s| s.kind == "goal" && (s.period < 3 || (s.period == 3 && s.sec < 1020))) { *score.entry(s.team.as_str()).or_default() += 1; }
            for (t, o) in [(away, home), (home, away)] {
                let d = score.get(o.as_str()).copied().unwrap_or(0) - score.get(t.as_str()).copied().unwrap_or(0);
                if !(1..=2).contains(&d) { continue; }
                eligible += 1.0;
                let first6 = ss.iter().filter(|s| s.period == 3 && &s.team == t && s.skaters.starts_with('6')).map(|s| s.sec).min();
                if let Some(f) = first6 {
                    pulled += 1.0;
                    let at = (f - 10).max(900);
                    pull_secs.push(1200 - at);
                    let end = ss.iter().filter(|s| s.period == 3 && s.kind == "goal" && s.sec > at).map(|s| s.sec).min().unwrap_or(1200);
                    pull_time += (end - at).max(10) as f64;
                    for s in ss.iter().filter(|s| s.period == 3 && s.sec >= at && s.sec <= end) {
                        if &s.team == t && s.skaters.starts_with('6') { six_att += 1.0; }
                        if &s.team == o && s.en { en_att += 1.0; if s.kind != "blocked" { en_unblocked += 1.0; if s.kind == "goal" { en_goals += 1.0; } } }
                    }
                }
            }
        }
        let xgb = logistic(&xs, &ys, 1.0);
        let xg = Xg { b0: xgb[0], dist: xgb[1], angle: xgb[2], pp: xgb[3],
            types: [0.0, xgb[4], xgb[5], xgb[6], xgb[7], xgb[8], xgb[9], xgb[10]] };
        let rate = |a: f64, b: f64, d: f64| if b > 0.0 { a / b } else { d };
        let league = League {
            season,
            att60: [rate(att[EV], ev_min, 50.0) * 60.0, rate(att[PP], pp_min, 1.5) * 60.0, rate(att[SH], sh_min, 0.4) * 60.0],
            blocked: [0, 1, 2].map(|k| rate(blk[k], att[k], 0.25)),
            missed: [0, 1, 2].map(|k| rate(mis[k], att[k], 0.22)),
            pen60: minors / (2.0 * n_games),
            pen_durations: { let mut d = durations.clone(); d.truncate(3000); d },
            xg,
            p_assist1: [0, 1, 2].map(|k| rate(a1[k], goals_s[k], 0.85)),
            p_assist2: [0, 1, 2].map(|k| rate(a2[k], goals_s[k], 0.65)),
            ot3_att60: rate(ot_att, ot_secs / 60.0, 1.2) * 60.0 / 2.0,
            so_pct: rate(so_goals, so_att, 0.32),
            pull_secs_left: pull_secs.clone(),
            pull_rate: rate(pulled, eligible, 0.7),
            en_att60: rate(en_att, pull_time / 60.0, 0.6) * 60.0,
            en_goal_pct: rate(en_goals, en_unblocked, 0.3),
            sixv5_att60: rate(six_att, pull_time / 60.0, 1.8) * 60.0,
            home_edge: (home_att / away_att.max(1.0)).sqrt(),
        };
        let key = format!("nhl:sim:{season}");
        if let Err(e) = db.execute("INSERT INTO sim_model_params (key, params, source) VALUES ($1, $2, $3) ON CONFLICT (key) DO UPDATE SET params = EXCLUDED.params, source = EXCLUDED.source, fitted_at = now()",
            &[&key, &Json(&league), &format!("NHL API shots {season}: {} attempts, {} games", shots.len(), ginfo.len())]).await { return internal(dberr(&e)); }
        let mean_xg = xs.iter().map(|f| { let z: f64 = f.iter().zip(&xgb).map(|(a, b)| a * b).sum(); 1.0 / (1.0 + (-z).exp()) }).sum::<f64>() / xs.len().max(1) as f64;
        out.push(json!({ "season": season, "games": ginfo.len(), "attempts": shots.len(), "att60": league.att60.map(r3), "blocked": league.blocked.map(r3), "missed": league.missed.map(r3),
            "pen60": r3(league.pen60), "sog_goal_rate": r4(ys.iter().sum::<f64>() / ys.len().max(1) as f64), "mean_xg_on_goal": r4(mean_xg),
            "xg_coef": xgb.iter().map(|x| r4(*x)).collect::<Vec<_>>(), "so_pct": r3(league.so_pct), "ot3_att60": r3(league.ot3_att60),
            "pull_rate": r3(league.pull_rate), "pulls": pull_secs.len(), "en_goal_pct": r3(league.en_goal_pct), "sixv5_att60": r3(league.sixv5_att60), "en_att60": r3(league.en_att60),
            "home_edge": r4(league.home_edge), "strength_minutes_per_game": [r3(ev_min / (2.0 * n_games)), r3(pp_min / (2.0 * n_games)), r3(sh_min / (2.0 * n_games))] }));
    }
    HttpResponse::Ok().json(json!({ "model_version": MODEL_VERSION, "fitted": out }))
}

async fn load_league(db: &tokio_postgres::Client, season: i32) -> Result<(League, String), String> {
    let key = format!("nhl:sim:{season}");
    let row = db.query_opt("SELECT params FROM sim_model_params WHERE key = $1", &[&key]).await.map_err(|e| dberr(&e))?
        .ok_or_else(|| format!("no NHL simulator parameters for season {season}; run POST /fit/nhl/sim"))?;
    let v: Json<Value> = row.get(0);
    serde_json::from_value(v.0).map(|l| (l, key)).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------

pub struct GameRef { pub id: i64, pub season: i32, pub game_type: i32, pub date: NaiveDate, pub away: String, pub home: String,
                     pub away_score: Option<i32>, pub home_score: Option<i32>, pub completed: bool, pub last_period: Option<String> }

async fn game_ref(db: &tokio_postgres::Client, id: i64) -> Result<GameRef, String> {
    let r = db.query_opt("SELECT game_id, season, game_type, game_date, away, home, away_score, home_score, completed, last_period FROM nhl_games_api WHERE game_id = $1", &[&id])
        .await.map_err(|e| dberr(&e))?.ok_or_else(|| format!("unknown NHL game {id} (ingest with POST /ingest/nhl/api)"))?;
    Ok(GameRef { id: r.get(0), season: r.get(1), game_type: r.get(2), date: r.get(3), away: r.get(4), home: r.get(5), away_score: r.get(6), home_score: r.get(7),
        completed: r.get(8), last_period: r.get(9) })
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum LineupMode { Projected, Confirmed, Actual }
impl LineupMode {
    pub fn parse(s: Option<&str>) -> Self { match s { Some("projected") => Self::Projected, Some("actual") => Self::Actual, _ => Self::Confirmed } }
    pub fn as_str(&self) -> &'static str { match self { Self::Projected => "projected", Self::Confirmed => "confirmed", Self::Actual => "actual" } }
}
pub struct Opts { pub mode: LineupMode, pub out: HashSet<i64>, pub force_in: HashSet<i64>, pub away_goalie: Option<i64>, pub home_goalie: Option<i64> }

fn norm_name(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase()
}
/// ESPN NHL injury report: normalised "first-initial + last name" -> status. ESPN ids differ from NHL ids, so match by name.
async fn espn_injuries(http: &reqwest::Client) -> HashMap<String, String> {
    let mut m = HashMap::new();
    if let Ok(v) = crate::nba_espn::get_json(http, "https://site.api.espn.com/apis/site/v2/sports/hockey/nhl/injuries").await {
        for t in v["injuries"].as_array().into_iter().flatten() {
            for i in t["injuries"].as_array().into_iter().flatten() {
                let name = i["athlete"]["displayName"].as_str().unwrap_or("");
                let mut parts = name.split_whitespace();
                if let (Some(f), Some(l)) = (parts.next(), parts.last()) {
                    m.insert(format!("{}{}", &norm_name(f)[..1.min(norm_name(f).len())], norm_name(l)), i["status"].as_str().unwrap_or("").to_string());
                }
            }
        }
    }
    m
}
fn box_key(name: &str) -> String {
    // NHL box names look like "B. Gallagher"
    let mut parts = name.split_whitespace();
    let f = parts.next().unwrap_or("");
    let l = parts.last().unwrap_or("");
    format!("{}{}", &norm_name(f)[..1.min(norm_name(f).len())], norm_name(l))
}
fn injury_p(status: &str) -> f64 {
    match status.to_ascii_lowercase().as_str() { "out" | "injured reserve" | "ir" => 0.0, "day-to-day" => 0.6, "questionable" => 0.5, "doubtful" => 0.15, _ => 0.75 }
}

pub struct Built { pub game: GameInput, pub meta: Value }

pub async fn build(state: &State, g: &GameRef, o: &Opts) -> Result<Built, String> {
    let db = &state.db;
    let params_season = if g.game_type == 3 { g.season } else { g.season - 10001 };
    let (league, params_key) = load_league(db, params_season).await?;
    let today = chrono::Local::now().date_naive();
    let injuries = if o.mode == LineupMode::Confirmed && g.date >= today { espn_injuries(&state.http).await } else { HashMap::new() };
    let actual: HashMap<i64, (bool, i32)> = if o.mode == LineupMode::Actual || g.completed {
        db.query("SELECT player_id, starter, COALESCE(toi_sec,0) FROM nhl_box_api WHERE game_id = $1", &[&g.id]).await.map_err(|e| dberr(&e))?
            .iter().map(|r| (r.get(0), (r.get(1), r.get(2)))).collect()
    } else { HashMap::new() };
    if o.mode == LineupMode::Actual && actual.is_empty() { return Err("actual lineup is only available after the game (no box score yet)".into()); }
    let teams = [g.away.clone(), g.home.clone()];
    let mut team_inputs = Vec::new();
    let mut team_meta = Vec::new();
    let mut flags = Vec::new();
    // league per-60 baselines for shrinkage
    for (ti, abbr) in teams.iter().enumerate() {
        let tg: Vec<(i64, NaiveDate)> = db.query("SELECT game_id, game_date FROM nhl_games_api WHERE completed AND game_type IN (2,3) AND (home = $1 OR away = $1) AND game_date < $2 \
                                                ORDER BY game_date DESC LIMIT 40", &[abbr, &g.date]).await.map_err(|e| dberr(&e))?
            .iter().map(|r| (r.get(0), r.get(1))).collect();
        if tg.len() < 3 { return Err(format!("{abbr}: fewer than 3 completed games before {}", g.date)); }
        let ids: Vec<i64> = tg.iter().map(|x| x.0).collect();
        let gi: HashMap<i64, usize> = ids.iter().enumerate().map(|(i, x)| (*x, i)).collect();
        let w = |i: usize| 0.5f64.powf(i as f64 / 20.0);
        let shots: Vec<Shot> = db.query(&format!("SELECT {SHOT_COLS} FROM nhl_shots_api s WHERE s.game_id = ANY($1)"), &[&ids]).await.map_err(|e| dberr(&e))?.iter().map(shot_row).collect();
        let pens = db.query("SELECT game_id, team, duration FROM nhl_penalties_api WHERE game_id = ANY($1) AND duration < 10", &[&ids]).await.map_err(|e| dberr(&e))?;
        let boxes = db.query("SELECT game_id, player_id, name, pos, COALESCE(toi_sec,0), COALESCE(blocks,0), starter, COALESCE(shots_against,0), COALESCE(goals_against,0) \
                              FROM nhl_box_api WHERE game_id = ANY($1) AND team = $2", &[&ids, abbr]).await.map_err(|e| dberr(&e))?;
        // team rates
        let (mut att_f, mut att_a, mut mins) = ([0.0f64; 3], [0.0f64; 3], [0.0f64; 3]);
        let (mut blk_f, mut opp_att, mut pen_taken, mut wsum) = (0.0, 0.0, 0.0, 0.0);
        let mut pen_by: HashMap<(i64, bool), f64> = HashMap::new();
        for r in &pens { let gid: i64 = r.get(0); let t: String = r.get(1); *pen_by.entry((gid, &t == abbr)).or_default() += r.get::<_, i32>(2) as f64; }
        for (gid, i) in &gi {
            let wt = w(*i);
            wsum += wt;
            let own_pm = pen_by.get(&(*gid, true)).copied().unwrap_or(0.0);
            let opp_pm = pen_by.get(&(*gid, false)).copied().unwrap_or(0.0);
            pen_taken += wt * own_pm / 2.0;
            mins[PP] += wt * opp_pm * 0.85; mins[SH] += wt * own_pm * 0.85;
            mins[EV] += wt * (60.0 - 0.85 * (own_pm + opp_pm)).max(40.0);
        }
        for s in &shots {
            if s.en || s.skaters.starts_with('6') || s.ptype != "REG" { continue; }
            let wt = w(gi[&s.game]);
            let k = sidx(&s.strength);
            if &s.team == abbr { att_f[k] += wt; } else {
                let kk = match k { PP => SH, SH => PP, _ => EV };
                att_a[kk] += wt;
                opp_att += wt;
                if s.kind == "blocked" { blk_f += wt; }
            }
        }
        let shrink_ratio = |x: f64, m: f64, lg60: f64, km: f64| ((x + lg60 * km / 60.0) / (m + km)) * 60.0 / lg60;
        let km = [300.0, 60.0, 60.0];
        let off = [0, 1, 2].map(|k| shrink_ratio(att_f[k], mins[k], league.att60[k], km[k]));
        // attempts allowed while this team is at strength k, vs the league rate of the opponent's strength (PP <-> SH)
        let opp = |k: usize| match k { PP => SH, SH => PP, _ => EV };
        let def = [0, 1, 2].map(|k| shrink_ratio(att_a[k], mins[k], league.att60[opp(k)], km[k]));
        // the team's blocked share of all opponent attempts vs the league's even-strength share (most attempts are at EV)
        let lg_blk = league.blocked[EV];
        let block = ((blk_f + 200.0 * lg_blk) / (opp_att + 200.0)) / lg_blk;
        let pen = ((pen_taken + 10.0 * league.pen60) / (wsum + 10.0)) / league.pen60;
        // skaters
        struct P { id: i64, name: String, pos: String, played: Vec<usize>, blocks: f64, toi: f64, last: bool }
        let mut ps: HashMap<i64, P> = HashMap::new();
        let mut goalies: HashMap<i64, (String, Vec<usize>, Vec<usize>)> = HashMap::new(); // name, games, starts
        for r in &boxes {
            let gid: i64 = r.get(0);
            let i = gi[&gid];
            let pid: i64 = r.get(1);
            let pos: String = r.get(3);
            let toi: i32 = r.get(4);
            if pos == "G" {
                let e = goalies.entry(pid).or_insert((r.get(2), vec![], vec![]));
                if toi > 0 { e.1.push(i); }
                if r.get::<_, bool>(6) { e.2.push(i); }
                continue;
            }
            if toi <= 0 { continue; }
            let e = ps.entry(pid).or_insert(P { id: pid, name: r.get(2), pos: pos.clone(), played: vec![], blocks: 0.0, toi: 0.0, last: false });
            e.played.push(i);
            e.blocks += r.get::<_, i32>(5) as f64;
            e.toi += toi as f64;
            if i == 0 { e.last = true; }
        }
        // per-game team attempts by strength (for shares)
        let mut team_att: HashMap<(i64, usize), f64> = HashMap::new();
        let mut team_goal: HashMap<(i64, usize), f64> = HashMap::new();
        let mut p_att: HashMap<(i64, usize), f64> = HashMap::new();
        let mut p_ast: HashMap<(i64, usize), f64> = HashMap::new();
        let mut p_g: HashMap<i64, (f64, f64)> = HashMap::new(); // goals, xG on goal
        let mut shot_loc: HashMap<i64, Vec<(f32, f32, u8)>> = HashMap::new();
        for s in &shots {
            if &s.team != abbr || s.en || s.ptype == "SO" { continue; }
            let k = sidx(&s.strength);
            *team_att.entry((s.game, k)).or_default() += 1.0;
            if let Some(sh) = s.shooter {
                *p_att.entry((sh, k)).or_default() += w(gi[&s.game]);
                if let (Some(x), Some(y)) = (s.x, s.y) { let v = shot_loc.entry(sh).or_default(); if v.len() < 300 { v.push((x as f32, y as f32, s.ty as u8)); } }
                if s.kind == "goal" || s.kind == "sog" {
                    if let (Some(x), Some(y)) = (s.x, s.y) {
                        let e = p_g.entry(sh).or_default();
                        e.1 += league.xg.p(x, y, s.ty, k == PP);
                        if s.kind == "goal" { e.0 += 1.0; }
                    }
                }
            }
            if s.kind == "goal" {
                *team_goal.entry((s.game, k)).or_default() += 1.0;
                for a in [s.a1, s.a2].into_iter().flatten() { *p_ast.entry((a, k)).or_default() += w(gi[&s.game]); }
            }
        }
        let mut skaters = Vec::new();
        let mut meta_p = Vec::new();
        let mut cand: Vec<&P> = ps.values().filter(|p| p.played.iter().any(|&i| i < 5) || o.force_in.contains(&p.id) || actual.contains_key(&p.id)).collect();
        cand.sort_by(|a, b| (b.toi / b.played.len() as f64).total_cmp(&(a.toi / a.played.len() as f64)));
        for p in cand {
            let (mut pa, mut reason): (f64, String) = match o.mode {
                LineupMode::Actual => match actual.get(&p.id) { Some((_, t)) if *t > 0 => (1.0, "actual: played".to_string()), _ => (0.0, "actual: did not play".to_string()) },
                _ => if p.played.iter().any(|&i| i < 5) { (1.0, "played in the team's last 5 games".to_string()) } else { (0.0, "not in the last 5 games".to_string()) },
            };
            if let Some(st) = injuries.get(&box_key(&p.name)) { pa = pa.min(injury_p(st)); reason = format!("injury report (ESPN, matched by name): {st}"); }
            if o.out.contains(&p.id) { pa = 0.0; reason = "scenario: ruled out".into(); }
            if o.force_in.contains(&p.id) { pa = 1.0; reason = "scenario: plays".into(); }
            let share = |m: &HashMap<(i64, usize), f64>, tot: &HashMap<(i64, usize), f64>, k: usize| {
                let num = m.get(&(p.id, k)).copied().unwrap_or(0.0);
                let den: f64 = p.played.iter().map(|&i| w(i) * tot.get(&(ids[i], k)).copied().unwrap_or(0.0)).sum();
                let per = tot.iter().filter(|((_, kk), _)| *kk == k).map(|(_, v)| v).sum::<f64>() / ids.len() as f64;
                (num + 2.0 * per / 18.0) / (den + 2.0 * per)
            };
            let (gl, xg) = p_g.get(&p.id).copied().unwrap_or((0.0, 0.0));
            let gp = p.played.len() as f64;
            skaters.push(sim::Skater {
                id: p.id, name: p.name.clone(), pos: if p.pos == "D" { "D".into() } else { "F".into() }, p_active: pa,
                att_share: [EV, PP, SH].map(|k| share(&p_att, &team_att, k)), ast_share: [EV, PP, SH].map(|k| share(&p_ast, &team_goal, k)),
                finish: (gl + 12.0) / (xg + 12.0), block_share: (p.blocks + 1.0) / (gp + 2.0),
                shots: shot_loc.remove(&p.id).unwrap_or_default(),
            });
            meta_p.push(json!({ "id": p.id, "name": p.name, "pos": p.pos, "p_active": r3(pa), "status": reason, "games": p.played.len(),
                "toi_mean": r3(p.toi / gp / 60.0), "finish": r3((gl + 12.0) / (xg + 12.0)), "scenario": if o.out.contains(&p.id) { "out" } else if o.force_in.contains(&p.id) { "in" } else { "auto" } }));
        }
        // goalie: scenario > actual starter > projected (most recent starter)
        let override_g = if ti == 0 { o.away_goalie } else { o.home_goalie };
        let projected_g = goalies.iter().filter(|(_, v)| !v.2.is_empty()).min_by_key(|(_, v)| v.2.iter().min().copied().unwrap_or(999)).map(|(k, _)| *k);
        let actual_g: Option<i64> = if actual.is_empty() { None } else {
            db.query_opt("SELECT player_id FROM nhl_box_api WHERE game_id = $1 AND team = $2 AND pos = 'G' AND starter", &[&g.id, abbr]).await
                .map_err(|e| dberr(&e))?.map(|r| r.get(0))
        };
        let gid = override_g.or(if o.mode == LineupMode::Actual { actual_g } else { None }).or(projected_g)
            .ok_or_else(|| format!("{abbr}: no goalie found"))?;
        let gname = goalies.get(&gid).map(|v| v.0.clone()).unwrap_or_else(|| gid.to_string());
        // goalie factor: goals vs xG on shots he faced (window: up to 450 days)
        let faced = db.query("SELECT s.x, s.y, COALESCE(s.shot_type,'other'), s.strength, s.kind FROM nhl_shots_api s JOIN nhl_games_api g USING (game_id) \
                              WHERE s.goalie = $1 AND g.game_date < $2 AND g.game_date >= $2 - 450 AND s.kind IN ('goal','sog') AND NOT s.empty_net AND s.x IS NOT NULL",
                              &[&gid, &g.date]).await.map_err(|e| dberr(&e))?;
        let (mut ga, mut xga) = (0.0, 0.0);
        for r in &faced {
            let (x, y): (f64, f64) = (r.get(0), r.get(1));
            xga += league.xg.p(x, y, sim::shot_type_idx(&r.get::<_, String>(2)), r.get::<_, String>(3) == "PP");
            if r.get::<_, String>(4) == "goal" { ga += 1.0; }
        }
        let gfactor = (ga + 25.0) / (xga + 25.0);
        if skaters.iter().filter(|s| s.p_active > 0.0).count() < 16 { flags.push(format!("{abbr}: fewer than 16 available skaters")); }
        team_meta.push(json!({ "abbr": abbr, "recent_games": ids.len(), "off": off.map(r3), "def": def.map(r3), "pen": r3(pen), "block": r3(block),
            "goalie": { "id": gid, "name": gname, "factor": r3(gfactor), "shots_faced": faced.len(),
                "source": if override_g.is_some() { "scenario" } else if o.mode == LineupMode::Actual { "actual starter" } else { "projected: started the team's most recent game" },
                "projected": projected_g, "actual": actual_g },
            "players": meta_p }));
        team_inputs.push(sim::Team { abbr: abbr.clone(), skaters, goalie: sim::Goalie { id: gid, name: gname, factor: gfactor }, off, def, pen, block });
    }
    if o.mode == LineupMode::Confirmed && g.date < today { flags.push("past game: injury feed is current-only, so 'confirmed' = projected availability".into()); }
    let [a, h]: [sim::Team; 2] = team_inputs.try_into().map_err(|_| "teams")?;
    let meta = json!({ "model_version": MODEL_VERSION, "params": params_key, "data_cutoff": g.date, "lineup_mode": o.mode.as_str(), "actual_available": !actual.is_empty(),
        "teams": team_meta, "flags": flags,
        "assumptions": [
            "event model: shot attempts and penalties as competing rates by strength; shooters/assisters by their real shares (no explicit lines or shifts)",
            "goal | on goal = fitted expected goals at a location drawn from the shooter's real attempts x shooter finishing x goalie (goals vs xG, shrunk)",
            "PP/SH time per game is estimated from penalty minutes (power-play goals end minors early); goalie pulls fitted from real games",
            "injury designations (ESPN, matched to NHL players by name) map to documented priors, not fitted",
        ] });
    Ok(Built { game: GameInput { teams: [a, h], league, playoff: g.game_type == 3 }, meta })
}

// ---------------------------------------------------------------------------
// Endpoints
// ---------------------------------------------------------------------------

pub async fn games(state: web::Data<State>) -> impl Responder {
    let today = chrono::Local::now().date_naive();
    match state.db.query("SELECT game_id, game_date, away, home, away_score, home_score, completed, game_type FROM nhl_games_api \
                          WHERE game_date BETWEEN $1 AND $2 ORDER BY start_time, game_id", &[&(today - chrono::Duration::days(4)), &(today + chrono::Duration::days(8))]).await {
        Ok(rows) => HttpResponse::Ok().json(json!({ "today": today, "games": rows.iter().map(|r| json!({
            "game_id": r.get::<_, i64>(0).to_string(), "date": r.get::<_, NaiveDate>(1), "away": r.get::<_, String>(2), "home": r.get::<_, String>(3),
            "away_score": r.get::<_, Option<i32>>(4), "home_score": r.get::<_, Option<i32>>(5), "completed": r.get::<_, bool>(6), "game_type": r.get::<_, i32>(7),
            "today": r.get::<_, NaiveDate>(1) == today })).collect::<Vec<_>>() })),
        Err(e) => internal(dberr(&e)),
    }
}

#[derive(Deserialize)]
pub struct SimQuery {
    #[serde(default = "d_n")] n: usize, #[serde(default = "d_draws")] draws: usize, seed: Option<u64>, lineup: Option<String>,
    out: Option<String>, #[serde(rename = "in")] force_in: Option<String>, away_goalie: Option<i64>, home_goalie: Option<i64>,
}
fn d_n() -> usize { 100_000 }
fn d_draws() -> usize { 200 }
fn ids(s: &Option<String>) -> HashSet<i64> { s.as_deref().unwrap_or("").split(',').filter_map(|x| x.trim().parse().ok()).collect() }
pub fn opts(lineup: Option<&str>, out: &[String], force_in: &[String], away_g: Option<String>, home_g: Option<String>) -> Opts {
    Opts { mode: LineupMode::parse(lineup), out: out.iter().filter_map(|x| x.parse().ok()).collect(), force_in: force_in.iter().filter_map(|x| x.parse().ok()).collect(),
        away_goalie: away_g.and_then(|x| x.parse().ok()), home_goalie: home_g.and_then(|x| x.parse().ok()) }
}

fn hist_json(h: &sim::Hist, lines: &[f64]) -> Value {
    json!({ "mean": r3(h.mean()), "q10": h.quantile(0.1), "q50": h.quantile(0.5), "q90": h.quantile(0.9),
        "lines": lines.iter().map(|l| { let (o, u, p, se) = h.line(*l); json!({ "line": l, "over": r4(o), "under": r4(u), "push": r4(p), "mcse": r4(se) }) }).collect::<Vec<_>>(),
        "pmf": h.pmf().iter().map(|p| r4(*p)).collect::<Vec<_>>() })
}

pub async fn simulate(state: web::Data<State>, path: web::Path<i64>, q: web::Query<SimQuery>) -> impl Responder {
    let id = path.into_inner();
    let gref = match game_ref(&state.db, id).await { Ok(g) => g, Err(e) => return unprocessable(e) };
    let o = Opts { mode: LineupMode::parse(q.lineup.as_deref()), out: ids(&q.out), force_in: ids(&q.force_in), away_goalie: q.away_goalie, home_goalie: q.home_goalie };
    let Built { game, meta } = match build(&state, &gref, &o).await { Ok(b) => b, Err(e) => return unprocessable(e) };
    let seed = q.seed.unwrap_or_else(|| sim::derive_seed(0x6e686c, &[&id.to_string(), &chrono::Utc::now().timestamp_millis().to_string()]));
    let (n, draws) = (q.n.clamp(100_000, 1_000_000), q.draws.clamp(50, 2000));
    let t = Instant::now();
    let g2 = game.clone();
    let e = match web::block(move || sim::simulate(&g2, n, draws, seed)).await { Ok(e) => e, Err(err) => return internal(err) };
    let ms = t.elapsed().as_millis() as u64;
    let nn = e.n.max(1) as f64;
    let players: Vec<Vec<Value>> = (0..2).map(|t| game.teams[t].skaters.iter().enumerate().filter(|(i, _)| e.active_n[t][*i] > 0).map(|(i, p)| {
        let stats: serde_json::Map<String, Value> = sim::PROPS.iter().enumerate().map(|(k, s)| (s.to_string(), hist_json(&e.player[t][i][k], &[e.player[t][i][k].mean().floor() + 0.5]))).collect();
        json!({ "idx": i, "id": p.id, "name": p.name, "pos": p.pos, "p_active": r4(e.active_n[t][i] as f64 / nn), "stats": stats })
    }).collect()).collect();
    HttpResponse::Ok().json(json!({
        "game_id": id.to_string(), "date": gref.date, "away": gref.away, "home": gref.home, "model_version": MODEL_VERSION, "seed": seed, "n": e.n, "sim_ms": ms,
        "win": { "home": r4(e.home_win as f64 / nn), "away": r4(1.0 - e.home_win as f64 / nn), "overtime": r4(e.regulation_tie as f64 / nn) },
        "puckline": { "home_minus_1_5": r4(e.home_puckline as f64 / nn), "away_minus_1_5": r4(e.away_puckline as f64 / nn) },
        "score": { "away": hist_json(&e.score[0], &[]), "home": hist_json(&e.score[1], &[]) },
        "total": hist_json(&e.total, &[5.5, 6.5]),
        "goalies": ([0, 1].map(|t| json!({ "team": game.teams[t].abbr, "name": game.teams[t].goalie.name, "saves": hist_json(&e.saves[t], &[e.saves[t].mean().floor() + 0.5]),
            "shots_against": hist_json(&e.shots[1 - t], &[]) }))),
        "players": { "away": players[0], "home": players[1] },
        "inputs": meta,
        "note": "skater props conditional on playing; mcse is Monte Carlo sampling error only",
    }))
}

pub async fn new_session(state: &State, id: &str, seed: u64, o: Opts) -> Result<(Value, Vec<Value>, Value, NaiveDate), String> {
    let gid: i64 = id.parse().map_err(|_| "NHL game id must be numeric")?;
    let gref = game_ref(&state.db, gid).await?;
    let Built { game, meta } = build(state, &gref, &o).await?;
    let g2 = game.clone();
    let (res, events) = web::block(move || sim::single_game(&g2, seed)).await.map_err(|e| e.to_string())?;
    let name = |t: usize, i: usize| game.teams[t].skaters.get(i).map(|p| p.name.clone()).unwrap_or_default();
    let events: Vec<Value> = events.into_iter().map(|e| {
        let t = e.team as usize;
        let who = e.player.map(|i| name(t, i)).unwrap_or_default();
        let text = match (e.kind, e.result) {
            ("goal", _) => format!("GOAL {} — {who}{}{}", game.teams[t].abbr,
                e.assist1.map(|a| format!(" (assists: {}", name(t, a))).unwrap_or_default() + &e.assist2.map(|a| format!(", {}", name(t, a))).unwrap_or_default() + if e.assist1.is_some() { ")" } else { " (unassisted)" },
                format!(" · {}{}", e.strength, if e.empty_net { ", empty net" } else { "" })),
            ("attempt", "sog") => format!("{who} {} shot on goal, saved by {}", e.shot_type.unwrap_or("a"), game.teams[1 - t].goalie.name),
            ("attempt", "missed") => format!("{who} misses the net"),
            ("attempt", "blocked") => format!("{who}'s shot blocked by {}", e.blocker.map(|b| name(1 - t, b)).unwrap_or_default()),
            ("penalty", _) => format!("PENALTY {}: {who}, {} minutes", game.teams[t].abbr, e.minutes),
            ("penalty_end", _) => format!("{} back to full strength", game.teams[t].abbr),
            ("goalie_pulled", _) => format!("{} pulls the goalie for an extra attacker", game.teams[t].abbr),
            ("end_period", _) => format!("End of {}", match e.period { 1 => "1st", 2 => "2nd", 3 => "3rd", 4 => "overtime", _ => "period" }),
            ("shootout", _) => format!("{} win the shootout ({} goals)", game.teams[t].abbr, e.minutes),
            _ => format!("FINAL{}: {} {} — {} {}", if res.shootout { " (SO)" } else if res.overtime { " (OT)" } else { "" }, game.teams[0].abbr, res.score[0], game.teams[1].abbr, res.score[1]),
        };
        let mut v = serde_json::to_value(&e).unwrap_or(Value::Null);
        v["text"] = json!(text);
        v
    }).collect();
    let roster: Vec<Value> = (0..2).map(|t| json!(game.teams[t].skaters.iter().enumerate().map(|(i, p)| json!({ "idx": i, "id": p.id, "name": p.name, "pos": p.pos,
        "p_active": r3(p.p_active), "active_in_this_game": res.active[t][i] })).collect::<Vec<_>>())).collect();
    let meta = json!({ "sport": "nhl", "game_id": id, "date": gref.date, "away": gref.away, "home": gref.home, "roster": roster,
        "goalies": [game.teams[0].goalie.name.clone(), game.teams[1].goalie.name.clone()],
        "actual": { "away_score": gref.away_score, "home_score": gref.home_score, "last_period": gref.last_period }, "inputs": meta });
    Ok((meta, events, json!({ "score": res.score, "overtime": res.overtime, "shootout": res.shootout }), gref.date))
}

// ---------------------------------------------------------------------------
// Walk-forward backtest
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct BacktestQuery { #[serde(default = "d_bt_season")] season: i32, #[serde(default = "d_bt_games")] games: usize, #[serde(default = "d_bt_n")] n: usize,
                          #[serde(default = "d_bt_draws")] draws: usize, #[serde(default = "d_bt_seed")] seed: u64, lineup: Option<String> }
fn d_bt_season() -> i32 { 20252026 }
fn d_bt_games() -> usize { 150 }
fn d_bt_n() -> usize { 2000 }
fn d_bt_draws() -> usize { 20 }
fn d_bt_seed() -> u64 { 4242 }

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

pub async fn backtest(state: web::Data<State>, q: web::Query<BacktestQuery>) -> impl Responder {
    let db = &state.db;
    let all: Vec<i64> = match db.query("SELECT game_id FROM nhl_games_api WHERE season = $1 AND game_type = 2 AND completed ORDER BY game_date, game_id", &[&q.season]).await {
        Ok(r) => r.iter().map(|x| x.get(0)).skip(150).collect(), Err(e) => return internal(dberr(&e)),
    };
    if all.is_empty() { return unprocessable("no games"); }
    let step = (all.len() as f64 / q.games.max(1) as f64).max(1.0);
    let picked: Vec<i64> = (0..q.games.min(all.len())).map(|i| all[(i as f64 * step) as usize]).collect();
    let t0 = Instant::now();
    let (mut hw, mut over55, mut sog, mut pts, mut sav) = (Binary::default(), Binary::default(), Binary::default(), Binary::default(), Binary::default());
    let (mut tot_err, mut tot_bias, mut tot_base, mut ng) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    let (mut sog_sim, mut sog_act) = (0.0, 0.0);
    let mut sims = 0;
    let mut skipped = Vec::new();
    for gid in &picked {
        let gref = match game_ref(db, *gid).await { Ok(g) => g, Err(e) => { skipped.push(e); continue } };
        let (Some(hs), Some(as_)) = (gref.home_score, gref.away_score) else { continue };
        let o = Opts { mode: LineupMode::parse(q.lineup.as_deref()), out: HashSet::new(), force_in: HashSet::new(), away_goalie: None, home_goalie: None };
        let Built { game, .. } = match build(&state, &gref, &o).await { Ok(b) => b, Err(e) => { skipped.push(e); continue } };
        let seed = sim::derive_seed(q.seed, &[&gid.to_string()]);
        let (n, draws) = (q.n, q.draws);
        let g2 = game.clone();
        let e = match web::block(move || sim::simulate(&g2, n, draws, seed)).await { Ok(e) => e, Err(err) => return internal(err) };
        sims += 1;
        let nn = e.n as f64;
        hw.add(e.home_win as f64 / nn, 0.535, (hs > as_) as i32 as f64);
        let tot = (hs + as_) as f64;
        let (o55, _, _, _) = e.total.line(5.5);
        over55.add(o55, 0.53, (tot > 5.5) as i32 as f64);
        tot_err += (e.total.mean() - tot).abs(); tot_bias += e.total.mean() - tot; tot_base += (6.1 - tot).abs(); ng += 1.0;
        let act = match db.query("SELECT player_id, pos, COALESCE(sog,0), COALESCE(goals,0)+COALESCE(assists,0), COALESCE(saves,0), team, COALESCE(toi_sec,0) FROM nhl_box_api WHERE game_id = $1", &[gid]).await {
            Ok(r) => r, Err(err) => return internal(dberr(&err)) };
        for r in &act { if r.get::<_, String>(1) != "G" { sog_act += r.get::<_, i32>(2) as f64; } }
        sog_sim += e.shots[0].mean() + e.shots[1].mean();
        let pids: Vec<i64> = game.teams.iter().flat_map(|t| t.skaters.iter().map(|p| p.id)).chain(game.teams.iter().map(|t| t.goalie.id)).collect();
        let hist = match db.query("SELECT b.player_id, COALESCE(b.sog,0), COALESCE(b.goals,0)+COALESCE(b.assists,0), COALESCE(b.saves,0) FROM nhl_box_api b JOIN nhl_games_api g USING (game_id) \
                                   WHERE b.player_id = ANY($1) AND g.game_date < $2 AND g.game_date >= $2 - 450 AND COALESCE(b.toi_sec,0) > 0", &[&pids, &gref.date]).await {
            Ok(r) => r, Err(err) => return internal(dberr(&err)) };
        let mut past: HashMap<i64, Vec<[f64; 3]>> = HashMap::new();
        for r in &hist { past.entry(r.get(0)).or_default().push([r.get::<_, i32>(1) as f64, r.get::<_, i32>(2) as f64, r.get::<_, i32>(3) as f64]); }
        let actual: HashMap<i64, [f64; 3]> = act.iter().filter(|r| r.get::<_, i32>(6) > 0).map(|r| (r.get(0), [r.get::<_, i32>(2) as f64, r.get::<_, i32>(3) as f64, r.get::<_, i32>(4) as f64])).collect();
        let freq = |ph: &Vec<[f64; 3]>, k: usize, line: f64| (ph.iter().filter(|x| x[k] > line).count() as f64 + 1.0) / (ph.len() as f64 + 2.0);
        for t in 0..2 {
            for (i, p) in game.teams[t].skaters.iter().enumerate() {
                let (Some(a), Some(ph)) = (actual.get(&p.id), past.get(&p.id)) else { continue };
                if ph.len() < 5 || e.active_n[t][i] == 0 { continue; }
                let avg = ph.iter().map(|x| x[0]).sum::<f64>() / ph.len() as f64;
                let line = (avg.floor() + 0.5).max(0.5);
                sog.add(e.player[t][i][0].line(line).0, freq(ph, 0, line), (a[0] > line) as i32 as f64);
                pts.add(e.player[t][i][3].line(0.5).0, freq(ph, 1, 0.5), (a[1] > 0.5) as i32 as f64);
            }
            let gk = game.teams[t].goalie.id;
            if let (Some(a), Some(ph)) = (actual.get(&gk), past.get(&gk)) {
                if ph.len() >= 5 {
                    let avg = ph.iter().map(|x| x[2]).sum::<f64>() / ph.len() as f64;
                    let line = avg.floor() + 0.5;
                    sav.add(e.saves[t].line(line).0, freq(ph, 2, line), (a[2] > line) as i32 as f64);
                }
            }
        }
    }
    let ng = ng.max(1.0);
    let report = json!({ "model_version": MODEL_VERSION, "season": q.season, "lineup": LineupMode::parse(q.lineup.as_deref()).as_str(), "games_simulated": sims, "sims_per_game": q.n,
        "secs": t0.elapsed().as_secs(), "skipped": skipped.into_iter().take(10).collect::<Vec<_>>(),
        "shots_on_goal_per_game": { "simulated": r3(sog_sim / sims.max(1) as f64), "actual": r3(sog_act / sims.max(1) as f64) },
        "total_goals": { "mae_model": r3(tot_err / ng), "mae_league_avg_6_1": r3(tot_base / ng), "bias": r3(tot_bias / ng) },
        "binary": { "home_win": hw.report("league home-win rate 0.535"), "total_over_5_5": over55.report("league rate 0.53"),
            "sog_over_avg": sog.report("player's own prior frequency"), "points_1plus": pts.report("player's own prior frequency"), "goalie_saves_over_avg": sav.report("goalie's own prior frequency") } });
    if sims >= 20 {
        let key = format!("nhl:backtest:{}:{}", q.season, LineupMode::parse(q.lineup.as_deref()).as_str());
        let _ = db.execute("INSERT INTO sim_model_params (key, params, source) VALUES ($1, $2, 'walk-forward backtest') ON CONFLICT (key) DO UPDATE SET params = EXCLUDED.params, fitted_at = now()",
            &[&key, &Json(report.clone())]).await;
    }
    HttpResponse::Ok().json(report)
}
