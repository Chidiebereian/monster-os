//! Hit rates + cheatsheets for player props AND game lines.
//!
//! A hit rate is how often a result cleared a line over a window of past games:
//! last 5/10/20, the latest season, career, home/away, and head-to-head vs an
//! opponent, plus the current streak. Player props read `player_game_logs`;
//! game lines read final scores in `sports_games`.
//!
//! "vs line" rates apply TODAY's line to past games (would it have cleared?).
//! That is descriptive, not predictive — the model P(over) next to it is the
//! forward-looking number. NFL also stores true historical closing lines, so its
//! team endpoints add the honest ATS / O-U record against the line of the day.
//!
//! `rate` = overs / (overs + unders); pushes are counted but excluded.
//!
//!   GET /hitrates/{sport}/player/{player_id}?stat=&line=&opponent=&games=
//!   GET /hitrates/{sport}/team/{team}?spread=&total=&team_total=&opponent=&games=
//!   GET /cheatsheet/{sport}/props?stat=&source=market|projection&sort=&side=&min_games=&limit=
//!   GET /cheatsheet/{sport}/games

use std::collections::HashMap;

use actix_web::{web, HttpResponse, Responder};
use chrono::NaiveDate;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_postgres::types::Json;

use crate::market::{self, PropLine};
use crate::player_backtest::stat_def;
use crate::{Family, State, StatDef};

fn internal(m: impl std::fmt::Display) -> HttpResponse {
    eprintln!("projection-service(hitrates): {m}");
    HttpResponse::InternalServerError().json(json!({ "error": m.to_string() }))
}
fn bad_request(m: impl Into<String>) -> HttpResponse { HttpResponse::BadRequest().json(json!({ "error": m.into() })) }
fn r3(x: f64) -> f64 { (x * 1000.0).round() / 1000.0 }
fn r1(x: f64) -> f64 { (x * 10.0).round() / 10.0 }

fn registry(sport: &str) -> &'static [&'static [StatDef]] {
    match sport {
        "nfl" => &[crate::NFL_STATS],
        "nhl" => &[crate::nhl::NHL_STATS, crate::nhl::GOALIE_STATS],
        "nba" => &[crate::nba::NBA_STATS],
        "mlb" => &[crate::mlb::MLB_BATTER_STATS, crate::mlb::MLB_PITCHER_STATS],
        _ => &[],
    }
}

/// Map the team codes different sources use (SportsGameOdds, MoneyPuck,
/// nflverse, stats.nba.com) onto the codes stored in our warehouse.
pub(crate) fn canon(sport: &str, code: &str) -> String {
    let c = code.trim().to_ascii_uppercase();
    let m = match (sport, c.as_str()) {
        ("nhl", "T.B" | "TB") => "TBL", ("nhl", "N.J" | "NJ") => "NJD", ("nhl", "S.J" | "SJ") => "SJS",
        ("nhl", "L.A" | "LA") => "LAK", ("nhl", "VEG" | "VGS") => "VGK", ("nhl", "WAS") => "WSH",
        ("nhl", "MON") => "MTL", ("nhl", "CLB") => "CBJ", ("nhl", "NAS") => "NSH", ("nhl", "CAL") => "CGY",
        ("nba", "GS") => "GSW", ("nba", "NO") => "NOP", ("nba", "NY") => "NYK", ("nba", "SA") => "SAS",
        ("nba", "PHO") => "PHX", ("nba", "UTAH") => "UTA", ("nba", "WSH") => "WAS", ("nba", "BRK" | "BKLYN") => "BKN",
        ("nba", "CHO") => "CHA",
        ("nfl", "LAR") => "LA", ("nfl", "JAC") => "JAX", ("nfl", "WSH") => "WAS", ("nfl", "LVR") => "LV",
        ("nfl", "ARZ") => "ARI", ("nfl", "BLT") => "BAL", ("nfl", "CLV") => "CLE", ("nfl", "HST") => "HOU",
        _ => return c,
    };
    m.to_string()
}

// --------------------------------------------------------------------------
// Hit-rate math (pure)
// --------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
enum Outcome { Over, Under, Push }

fn outcome(value: f64, line: f64) -> Outcome {
    if (value - line).abs() < 1e-9 { Outcome::Push } else if value > line { Outcome::Over } else { Outcome::Under }
}

/// One past result, newest first in every slice handed around below.
struct Mark { out: Outcome, season: i32, home: Option<bool>, opp: String }

fn summary<'a>(outs: impl Iterator<Item = &'a Outcome>) -> Value {
    let (mut over, mut under, mut push) = (0u32, 0u32, 0u32);
    for o in outs { match o { Outcome::Over => over += 1, Outcome::Under => under += 1, Outcome::Push => push += 1 } }
    let decided = over + under;
    json!({
        "games": over + under + push, "over": over, "under": under, "push": push,
        "rate": if decided > 0 { Some(r3(over as f64 / decided as f64)) } else { None },
    })
}

/// Current run of the same side, newest game first; pushes neither extend nor break it.
fn streak(marks: &[Mark]) -> Value {
    let mut side = None;
    let mut len = 0u32;
    for m in marks {
        if m.out == Outcome::Push { continue; }
        match side {
            None => { side = Some(m.out); len = 1; }
            Some(s) if s == m.out => len += 1,
            _ => break,
        }
    }
    let name = match side { Some(Outcome::Over) => Some("over"), Some(Outcome::Under) => Some("under"), _ => None };
    json!({ "side": name, "games": len })
}

/// Home/away splits look at the last this-many games at that venue, so they
/// stay as recent as the L20 window instead of spanning a career.
const SPLIT_GAMES: usize = 20;

/// Every standard window for one line. `venue` (Some(true)=home) picks the
/// split matching tonight's game; `opponent` adds head-to-head.
fn windows(marks: &[Mark], opponent: Option<&str>, venue: Option<bool>) -> Value {
    let latest = marks.first().map(|m| m.season);
    let head = |n: usize| summary(marks.iter().take(n).map(|m| &m.out));
    let filt = |f: &dyn Fn(&Mark) -> bool| summary(marks.iter().filter(|m| f(m)).map(|m| &m.out));
    let at = |v: bool| summary(marks.iter().filter(|m| m.home == Some(v)).take(SPLIT_GAMES).map(|m| &m.out));
    json!({
        "l5": head(5), "l10": head(10), "l20": head(20),
        "season": filt(&|m| Some(m.season) == latest), "season_label": latest,
        "career": head(usize::MAX),
        "home": at(true), "away": at(false),
        "venue": venue.map(at),
        "h2h": opponent.map(|o| summary(marks.iter().filter(|m| m.opp == o).take(10).map(|m| &m.out))),
        "streak": streak(marks),
    })
}

fn mean(v: impl Iterator<Item = f64>) -> Option<f64> {
    let (mut s, mut n) = (0.0, 0.0);
    for x in v { s += x; n += 1.0; }
    if n > 0.0 { Some(r1(s / n)) } else { None }
}

/// Nearest x.5 line to a projected mean (sportsbook-style hook, never a push).
fn half_line(m: f64) -> f64 { m.floor() + 0.5 }

// --------------------------------------------------------------------------
// Model probabilities (pure): count = Negative-Binomial(mean, size), continuous
// = Gamma matched to (mean, sd) — the same families simulation-engine draws.
// --------------------------------------------------------------------------

fn ln_gamma(x: f64) -> f64 {
    // Lanczos, g = 7
    const C: [f64; 9] = [0.999_999_999_999_809_9, 676.520_368_121_885_1, -1_259.139_216_722_402_8, 771.323_428_777_653_1,
        -176.615_029_162_140_6, 12.507_343_278_686_905, -0.138_571_095_265_720_12, 9.984_369_578_019_572e-6, 1.505_632_735_149_311_6e-7];
    if x < 0.5 { return (std::f64::consts::PI / (std::f64::consts::PI * x).sin()).ln() - ln_gamma(1.0 - x); }
    let x = x - 1.0;
    let mut a = C[0];
    let t = x + 7.5;
    for (i, &c) in C.iter().enumerate().skip(1) { a += c / (x + i as f64); }
    0.5 * (2.0 * std::f64::consts::PI).ln() + (x + 0.5) * t.ln() - t + a.ln()
}

/// Regularized lower incomplete gamma P(a, x).
fn gamma_p(a: f64, x: f64) -> f64 {
    if x <= 0.0 { return 0.0; }
    let lead = a * x.ln() - x - ln_gamma(a);
    if x < a + 1.0 {
        let (mut sum, mut term, mut n) = (1.0 / a, 1.0 / a, a);
        for _ in 0..500 { n += 1.0; term *= x / n; sum += term; if term.abs() < sum.abs() * 1e-14 { break; } }
        (sum.ln() + lead).exp().min(1.0)
    } else {
        // Lentz continued fraction for Q(a, x)
        let tiny = 1e-300;
        let mut b = x + 1.0 - a;
        let mut c = 1.0 / tiny;
        let mut d = 1.0 / b;
        let mut h = d;
        for i in 1..500 {
            let an = -(i as f64) * (i as f64 - a);
            b += 2.0;
            d = an * d + b; if d.abs() < tiny { d = tiny; }
            c = b + an / c; if c.abs() < tiny { c = tiny; }
            d = 1.0 / d;
            let del = d * c;
            h *= del;
            if (del - 1.0).abs() < 1e-14 { break; }
        }
        (1.0 - (lead.exp() * h)).max(0.0)
    }
}

/// P(X <= k) and P(X = k) for NB(mean, size); Poisson when size is huge.
fn nb_cdf_pmf(k: i64, mean: f64, size: f64) -> (f64, f64) {
    if k < 0 { return (0.0, 0.0); }
    let m = mean.max(1e-9);
    let (mut p, ratio): (f64, Box<dyn Fn(f64) -> f64>) = if size > 1e5 {
        ((-m).exp(), Box::new(move |j: f64| m / (j + 1.0)))
    } else {
        let q = m / (size + m);
        ((size * (size / (size + m)).ln()).exp(), Box::new(move |j: f64| (j + size) / (j + 1.0) * q))
    };
    let mut cdf = p;
    for j in 0..k { p *= ratio(j as f64); cdf += p; }
    (cdf.min(1.0), p)
}

/// (P(over), P(push)) of `line` under the stat's predictive distribution.
fn p_over(family: Family, mean: f64, sd: f64, dispersion: Option<f64>, line: f64) -> (f64, f64) {
    match family {
        Family::Count => {
            let size = dispersion.unwrap_or(1e6);
            let k = line.floor() as i64;
            let (cdf, pmf) = nb_cdf_pmf(k, mean, size);
            if (line - line.floor()).abs() < 1e-9 { (1.0 - cdf, pmf) } else { (1.0 - cdf, 0.0) }
        }
        Family::Continuous => {
            if mean <= 0.0 || sd <= 0.0 { return (if mean > line { 1.0 } else { 0.0 }, 0.0); }
            let shape = (mean / sd).powi(2);
            let scale = sd * sd / mean;
            (1.0 - gamma_p(shape, line / scale), 0.0)
        }
    }
}

/// De-vigged implied probability of the over.
fn novig_over(over: Option<f64>, under: Option<f64>) -> Option<f64> {
    match (over, under) {
        (Some(o), Some(u)) => { let (io, iu) = (market::implied(o), market::implied(u)); Some(io / (io + iu)) }
        (Some(o), None) => Some(market::implied(o)),
        _ => None,
    }
}

// --------------------------------------------------------------------------
// Data loading
// --------------------------------------------------------------------------

/// Which team was home on a given day / week, so player logs (which only store
/// the opponent) get a home/away flag.
struct VenueIndex { by_date: HashMap<(NaiveDate, String), bool>, by_week: HashMap<(i32, String, String), bool> }

impl VenueIndex {
    async fn load(state: &State, sport: &str) -> Result<Self, String> {
        let rows = state.db.query(
            "SELECT season, week, game_date, home_team, away_team FROM sports_games WHERE sport=$1", &[&sport],
        ).await.map_err(|e| e.to_string())?;
        let (mut by_date, mut by_week) = (HashMap::new(), HashMap::new());
        for r in &rows {
            let (Some(h), Some(a)) = (r.get::<_, Option<String>>("home_team"), r.get::<_, Option<String>>("away_team")) else { continue };
            let (h, a) = (canon(sport, &h), canon(sport, &a));
            if let Some(d) = r.get::<_, Option<NaiveDate>>("game_date") {
                by_date.insert((d, h.clone()), true);
                by_date.insert((d, a.clone()), false);
            }
            if let (Some(s), Some(w)) = (r.get::<_, Option<i32>>("season"), r.get::<_, Option<String>>("week")) {
                by_week.insert((s, w.clone(), h), true);
                by_week.insert((s, w, a), false);
            }
        }
        Ok(Self { by_date, by_week })
    }
    /// Was the player home? We know the opponent, so: home iff the opponent was away.
    fn player_home(&self, date: Option<NaiveDate>, season: i32, week: &str, opp: &str) -> Option<bool> {
        let opp_home = match date {
            Some(d) => self.by_date.get(&(d, opp.to_string())),
            None => self.by_week.get(&(season, week.to_string(), opp.to_string())),
        };
        opp_home.map(|h| !h)
    }
}

struct LogRow { season: i32, week: String, date: Option<NaiveDate>, opp: String, home: Option<bool>, stats: Value }

/// Game logs for a set of players, newest first, at most `per_player` each.
async fn player_logs(state: &State, sport: &str, pids: &[String], per_player: i64, venues: &VenueIndex)
    -> Result<HashMap<String, Vec<LogRow>>, String> {
    let rows = state.db.query(
        "SELECT player_id, season, week, game_date, opponent, stats FROM ( \
           SELECT player_id, season, week, game_date, opponent, stats, ROW_NUMBER() OVER ( \
             PARTITION BY player_id ORDER BY season DESC, game_date DESC NULLS LAST, \
             CASE WHEN week ~ '^[0-9]+$' THEN week::bigint END DESC NULLS LAST) AS rn \
           FROM player_game_logs WHERE sport=$1 AND player_id = ANY($2)) t \
         WHERE rn <= $3 ORDER BY player_id, rn",
        &[&sport, &pids, &per_player],
    ).await.map_err(|e| e.to_string())?;
    let mut out: HashMap<String, Vec<LogRow>> = HashMap::new();
    for r in &rows {
        let season: i32 = r.get("season");
        let week: String = r.get("week");
        let date: Option<NaiveDate> = r.get("game_date");
        let opp = canon(sport, &r.get::<_, Option<String>>("opponent").unwrap_or_default());
        let home = venues.player_home(date, season, &week, &opp);
        let stats: Json<Value> = r.get("stats");
        out.entry(r.get("player_id")).or_default().push(LogRow { season, week, date, opp, home, stats: stats.0 });
    }
    Ok(out)
}

fn stat_value(stats: &Value, def: &StatDef) -> f64 { stats.get(def.column).and_then(|v| v.as_f64()).unwrap_or(0.0) }

fn marks_for(logs: &[LogRow], def: &StatDef, line: f64) -> Vec<Mark> {
    logs.iter().map(|g| Mark { out: outcome(stat_value(&g.stats, def), line), season: g.season, home: g.home, opp: g.opp.clone() }).collect()
}

struct Proj { mean: f64, sd: f64, dispersion: Option<f64> }

async fn projections(state: &State, sport: &str, pids: &[String], stats: &[String]) -> Result<HashMap<(String, String), Proj>, String> {
    let rows = state.db.query(
        "SELECT player_id, stat, proj_mean, proj_sd, dispersion FROM player_projections \
         WHERE sport=$1 AND game_id='' AND player_id = ANY($2) AND stat = ANY($3)",
        &[&sport, &pids, &stats],
    ).await.map_err(|e| e.to_string())?;
    Ok(rows.iter().map(|r| ((r.get("player_id"), r.get("stat")),
        Proj { mean: r.get("proj_mean"), sd: r.get("proj_sd"), dispersion: r.get("dispersion") })).collect())
}

/// Share of the last 20 logged games in which the player had the stat's
/// opportunity (a carry, a target, a minute...). Projections are fit only on
/// those games, but a prop settles over every game, where no opportunity = 0.
fn participation(logs: &[LogRow], def: &StatDef) -> f64 {
    let recent: Vec<&LogRow> = logs.iter().take(20).collect();
    if recent.is_empty() { return 1.0; }
    let n = recent.iter().filter(|g| g.stats.get(def.opportunity).and_then(|v| v.as_f64()).unwrap_or(0.0) >= 1.0).count();
    n as f64 / recent.len() as f64
}

/// Model probabilities for the line: a zero-inflated mixture of "no
/// opportunity" (value 0) and the fitted conditional distribution.
fn model_json(def: &StatDef, p: Option<&Proj>, play: f64, line: f64, implied_over: Option<f64>) -> Value {
    let Some(p) = p else { return Value::Null };
    let (c_over, c_push) = p_over(def.family, p.mean, p.sd, p.dispersion, line);
    let zero = 1.0 - play;
    let over = play * c_over + if line < 0.0 { zero } else { 0.0 };
    let push = play * c_push + if line.abs() < 1e-9 { zero } else { 0.0 };
    let under = (1.0 - over - push).max(0.0);
    json!({
        "mean": r1(play * p.mean), "conditional_mean": r1(p.mean), "participation": r3(play),
        "p_over": r3(over), "p_under": r3(under), "p_push": r3(push),
        "edge_over": implied_over.map(|i| r3(over - i)), "edge_under": implied_over.map(|i| r3(under - (1.0 - i))),
    })
}

// --------------------------------------------------------------------------
// GET /hitrates/{sport}/player/{player_id}
// --------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct PlayerQ {
    stat: String,
    line: Option<f64>,
    opponent: Option<String>,
    #[serde(default = "d_games")] games: usize,
}
fn d_games() -> usize { 25 }

pub async fn player(state: web::Data<State>, path: web::Path<(String, String)>, q: web::Query<PlayerQ>) -> impl Responder {
    let (sport, pid) = path.into_inner();
    let Some(def) = stat_def(&sport, &q.stat) else { return bad_request("unknown sport/stat"); };
    let venues = match VenueIndex::load(&state, &sport).await { Ok(v) => v, Err(e) => return internal(e) };
    let ids = vec![pid.clone()];
    let mut logs = match player_logs(&state, &sport, &ids, i64::MAX, &venues).await { Ok(l) => l, Err(e) => return internal(e) };
    let Some(logs) = logs.remove(&pid) else { return HttpResponse::NotFound().json(json!({ "error": "no game logs for player" })) };
    let info = state.db.query_opt(
        "SELECT display_name, team, position FROM sports_players WHERE sport=$1 AND player_id=$2", &[&sport, &pid],
    ).await.ok().flatten();
    let proj = match projections(&state, &sport, &ids, &[def.key.to_string()]).await { Ok(p) => p, Err(e) => return internal(e) };
    let proj = proj.get(&(pid.clone(), def.key.to_string()));

    let values: Vec<f64> = logs.iter().map(|g| stat_value(&g.stats, def)).collect();
    let (line, source) = match (q.line, proj) {
        (Some(l), _) => (l, "query"),
        (None, Some(p)) => (half_line(p.mean), "projection"),
        (None, None) => (half_line(mean(values.iter().take(20).copied()).unwrap_or(0.0)), "l20_average"),
    };
    let marks = marks_for(&logs, def, line);
    let opponent = q.opponent.as_deref().map(|o| canon(&sport, o));
    let latest = logs.first().map(|g| g.season);
    let log: Vec<Value> = logs.iter().zip(&values).take(q.games).map(|(g, &v)| json!({
        "season": g.season, "week": g.week, "date": g.date.map(|d| d.to_string()), "opponent": g.opp, "home": g.home,
        "value": v, "result": match outcome(v, line) { Outcome::Over => "over", Outcome::Under => "under", Outcome::Push => "push" },
    })).collect();

    HttpResponse::Ok().json(json!({
        "sport": sport, "player_id": pid,
        "player": info.as_ref().map(|r| r.get::<_, String>("display_name")),
        "team": info.as_ref().and_then(|r| r.get::<_, Option<String>>("team")),
        "position": info.as_ref().and_then(|r| r.get::<_, Option<String>>("position")),
        "stat": def.key, "label": def.label, "unit": def.unit, "family": def.family.as_str(),
        "line": line, "line_source": source, "opponent": opponent,
        "hit_rates": windows(&marks, opponent.as_deref(), None),
        "averages": {
            "l5": mean(values.iter().take(5).copied()), "l10": mean(values.iter().take(10).copied()),
            "l20": mean(values.iter().take(20).copied()),
            "season": mean(logs.iter().zip(&values).filter(|(g, _)| Some(g.season) == latest).map(|(_, &v)| v)),
            "career": mean(values.iter().copied()),
        },
        "model": model_json(def, proj, participation(&logs, def), line, None),
        "log": log,
    }))
}

// --------------------------------------------------------------------------
// GET /hitrates/{sport}/team/{team}
// --------------------------------------------------------------------------

struct TeamGame { season: i32, date: Option<NaiveDate>, opp: String, home: bool, pf: f64, pa: f64,
                  hist_spread: Option<f64>, hist_total: Option<f64> }

/// Completed games for a team, newest first. `hist_spread` is the team's own
/// closing spread in bettor convention (negative = favored).
async fn team_games(state: &State, sport: &str, team: &str, limit: i64) -> Result<Vec<TeamGame>, String> {
    // NHL holds some games twice (NHL-API schedule id + MoneyPuck id), so keep
    // one row per (date, matchup), preferring the one carrying market lines.
    let rows = state.db.query(
        "SELECT * FROM ( \
           SELECT DISTINCT ON (game_date, home_team, away_team) \
             season, game_date, home_team, away_team, home_score, away_score, spread_line, total_line FROM sports_games \
           WHERE sport=$1 AND home_score IS NOT NULL AND away_score IS NOT NULL AND (home_team=$2 OR away_team=$2) \
             AND COALESCE(game_type, 'REG') NOT IN ('PRE', 'ASG') \
           ORDER BY game_date, home_team, away_team, spread_line NULLS LAST) t \
         ORDER BY game_date DESC NULLS LAST, season DESC LIMIT $3",
        &[&sport, &team, &limit],
    ).await.map_err(|e| e.to_string())?;
    Ok(rows.iter().map(|r| {
        let home = r.get::<_, Option<String>>("home_team").as_deref() == Some(team);
        let (hs, as_) = (r.get::<_, i32>("home_score") as f64, r.get::<_, i32>("away_score") as f64);
        let opp = if home { r.get::<_, Option<String>>("away_team") } else { r.get::<_, Option<String>>("home_team") };
        // stored spread_line (nflverse) = expected HOME margin; positive = home favored
        let sl: Option<f64> = r.get("spread_line");
        TeamGame {
            season: r.get::<_, Option<i32>>("season").unwrap_or(0), date: r.get("game_date"),
            opp: canon(sport, &opp.unwrap_or_default()), home,
            pf: if home { hs } else { as_ }, pa: if home { as_ } else { hs },
            hist_spread: sl.map(|l| if home { -l } else { l }), hist_total: r.get("total_line"),
        }
    }).collect())
}

fn team_marks(games: &[TeamGame], f: impl Fn(&TeamGame) -> Option<Outcome>) -> Vec<Mark> {
    games.iter().filter_map(|g| f(g).map(|out| Mark { out, season: g.season, home: Some(g.home), opp: g.opp.clone() })).collect()
}

/// All market hit rates for one team. `spread` is the team's spread (bettor
/// convention), `total` the game total, `team_total` the team's own points line.
fn team_markets(games: &[TeamGame], spread: Option<f64>, total: Option<f64>, team_total: Option<f64>,
                opponent: Option<&str>, venue: Option<bool>) -> Value {
    let w = |m: Vec<Mark>| if m.is_empty() { Value::Null } else { windows(&m, opponent, venue) };
    json!({
        "win": w(team_marks(games, |g| Some(outcome(g.pf - g.pa, 0.0)))),
        "cover": spread.map(|s| w(team_marks(games, |g| Some(outcome(g.pf - g.pa + s, 0.0))))),
        "over": total.map(|t| w(team_marks(games, |g| Some(outcome(g.pf + g.pa, t))))),
        "team_total_over": team_total.map(|t| w(team_marks(games, |g| Some(outcome(g.pf, t))))),
        "ats_closing": w(team_marks(games, |g| g.hist_spread.map(|s| outcome(g.pf - g.pa + s, 0.0)))),
        "ou_closing": w(team_marks(games, |g| g.hist_total.map(|t| outcome(g.pf + g.pa, t)))),
    })
}

fn team_averages(games: &[TeamGame]) -> Value {
    let latest = games.first().map(|g| g.season);
    let avg = |n: usize, f: fn(&TeamGame) -> f64| mean(games.iter().take(n).map(f));
    let season = |f: fn(&TeamGame) -> f64| mean(games.iter().filter(|g| Some(g.season) == latest).map(f));
    json!({
        "points_for": { "l5": avg(5, |g| g.pf), "l10": avg(10, |g| g.pf), "season": season(|g| g.pf) },
        "points_against": { "l5": avg(5, |g| g.pa), "l10": avg(10, |g| g.pa), "season": season(|g| g.pa) },
        "margin": { "l5": avg(5, |g| g.pf - g.pa), "l10": avg(10, |g| g.pf - g.pa), "season": season(|g| g.pf - g.pa) },
        "total": { "l5": avg(5, |g| g.pf + g.pa), "l10": avg(10, |g| g.pf + g.pa), "season": season(|g| g.pf + g.pa) },
    })
}

#[derive(Deserialize)]
pub struct TeamQ {
    spread: Option<f64>,
    total: Option<f64>,
    team_total: Option<f64>,
    opponent: Option<String>,
    #[serde(default = "d_team_games")] games: usize,
}
fn d_team_games() -> usize { 20 }

pub async fn team(state: web::Data<State>, path: web::Path<(String, String)>, q: web::Query<TeamQ>) -> impl Responder {
    let (sport, team) = path.into_inner();
    if registry(&sport).is_empty() { return bad_request("unknown sport"); }
    let team = canon(&sport, &team);
    let games = match team_games(&state, &sport, &team, 400).await { Ok(g) => g, Err(e) => return internal(e) };
    if games.is_empty() { return HttpResponse::NotFound().json(json!({ "error": format!("no completed games for {team}") })); }
    let opponent = q.opponent.as_deref().map(|o| canon(&sport, o));
    let log: Vec<Value> = games.iter().take(q.games).map(|g| json!({
        "season": g.season, "date": g.date.map(|d| d.to_string()), "opponent": g.opp, "home": g.home,
        "points_for": g.pf, "points_against": g.pa, "margin": g.pf - g.pa, "total": g.pf + g.pa,
        "closing_spread": g.hist_spread, "closing_total": g.hist_total,
        "covered_line": q.spread.map(|s| (g.pf - g.pa + s) > 0.0), "over_line": q.total.map(|t| g.pf + g.pa > t),
    })).collect();
    HttpResponse::Ok().json(json!({
        "sport": sport, "team": team, "opponent": opponent,
        "lines": { "spread": q.spread, "total": q.total, "team_total": q.team_total },
        "markets": team_markets(&games, q.spread, q.total, q.team_total, opponent.as_deref(), None),
        "averages": team_averages(&games),
        "log": log,
        "note": "cover/over apply the given line to past games; ats_closing/ou_closing use each game's own closing line (stored for NFL only).",
    }))
}

// --------------------------------------------------------------------------
// GET /cheatsheet/{sport}/props
// --------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct PropsQ {
    stat: Option<String>,
    #[serde(default = "d_source")] source: String,
    #[serde(default = "d_sort")] sort: String,
    #[serde(default = "d_side")] side: String,
    #[serde(default = "d_min_games")] min_games: usize,
    #[serde(default = "d_limit")] limit: usize,
    #[serde(default)] include_lopsided: bool,
}
fn d_source() -> String { "market".into() }
fn d_sort() -> String { "l10".into() }
fn d_side() -> String { "best".into() }
fn d_min_games() -> usize { 10 }
fn d_limit() -> usize { 300 }

/// Lines whose de-vigged over price is below this (or above 1 - this) are
/// near-certain and skipped unless `include_lopsided=true`.
const LOPSIDED: f64 = 0.15;

fn window_rate(hr: &Value, key: &str) -> Option<f64> { hr.get(key)?.get("rate")?.as_f64() }

pub async fn cheatsheet_props(state: web::Data<State>, path: web::Path<String>, q: web::Query<PropsQ>) -> impl Responder {
    let sport = path.into_inner();
    let regs = registry(&sport);
    if regs.is_empty() { return bad_request("unknown sport"); }
    let defs: Vec<&'static StatDef> = match &q.stat {
        Some(k) => match stat_def(&sport, k) { Some(d) => vec![d], None => return bad_request("unknown stat") },
        None => regs.iter().flat_map(|r| r.iter()).collect(),
    };

    // 1) the lines to grade
    let lines: Vec<PropLine> = match q.source.as_str() {
        "market" => {
            let keys: Vec<&'static str> = defs.iter().map(|d| d.key).filter(|k| market::sgo_stat(&sport, k).is_some()).collect();
            if keys.is_empty() { return bad_request("no market mapped for this stat; use source=projection"); }
            match market::fetch_props(&state, &sport, &keys).await { Ok(l) => l, Err(e) => return internal(e) }
        }
        "projection" => {
            let Some(def) = (if defs.len() == 1 { Some(defs[0]) } else { None }) else {
                return bad_request("source=projection needs a single stat");
            };
            let rows = match state.db.query(
                "SELECT p.player_id, pl.display_name, pl.team, p.proj_mean FROM player_projections p \
                 JOIN sports_players pl ON pl.sport=p.sport AND pl.player_id=p.player_id \
                 WHERE p.sport=$1 AND p.stat=$2 AND p.game_id='' AND p.sample_games >= $3 \
                   AND pl.last_season >= (SELECT max(last_season) - 1 FROM sports_players WHERE sport=$1) \
                 ORDER BY p.proj_mean DESC LIMIT $4",
                &[&sport, &def.key, &(q.min_games as i32), &(q.limit as i64)],
            ).await { Ok(r) => r, Err(e) => return internal(e) };
            rows.iter().map(|r| PropLine {
                stat: def.key, player: r.get("display_name"), player_id: Some(r.get("player_id")),
                team: r.get("team"), opponent: None, home: None, starts_at: Value::Null,
                line: half_line(r.get("proj_mean")), over_odds: None, under_odds: None,
            }).collect()
        }
        _ => return bad_request("source must be market or projection"),
    };

    // 2) one pass of logs + projections for every matched player
    let mut seen = std::collections::HashSet::new();
    let lines: Vec<PropLine> = lines.into_iter()
        .filter(|l| l.player_id.is_some() && seen.insert((l.player_id.clone(), l.stat))).collect();
    let unmatched_note = "props whose player name did not match our warehouse are dropped";
    let pids: Vec<String> = lines.iter().filter_map(|l| l.player_id.clone()).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    let stat_keys: Vec<String> = defs.iter().map(|d| d.key.to_string()).collect();
    let venues = match VenueIndex::load(&state, &sport).await { Ok(v) => v, Err(e) => return internal(e) };
    let logs = match player_logs(&state, &sport, &pids, 300, &venues).await { Ok(l) => l, Err(e) => return internal(e) };
    let projs = match projections(&state, &sport, &pids, &stat_keys).await { Ok(p) => p, Err(e) => return internal(e) };

    // 3) grade each line
    let mut rows: Vec<([f64; 3], Value)> = Vec::new();
    for l in &lines {
        let pid = l.player_id.clone().unwrap_or_default();
        let Some(def) = defs.iter().find(|d| d.key == l.stat) else { continue };
        let Some(pl) = logs.get(&pid) else { continue };
        if pl.len() < q.min_games { continue; }
        let opponent = l.opponent.as_deref().map(|o| canon(&sport, o));
        let marks = marks_for(pl, def, l.line);
        let hr = windows(&marks, opponent.as_deref(), l.home);
        let implied = novig_over(l.over_odds, l.under_odds);
        // near-certain lines (e.g. a depth defenseman's 1.5 points) crowd out real decisions
        if !q.include_lopsided {
            if let Some(i) = implied { if !(LOPSIDED..=1.0 - LOPSIDED).contains(&i) { continue; } }
        }
        let model = model_json(def, projs.get(&(pid.clone(), def.key.to_string())), participation(pl, def), l.line, implied);

        // side: the one the chosen window favours (or forced by ?side=)
        let window = if q.sort == "edge" { "l10" } else { q.sort.as_str() };
        let rate = window_rate(&hr, window);
        let side = match q.side.as_str() {
            "over" | "under" => q.side.clone(),
            _ => if rate.unwrap_or(0.5) >= 0.5 { "over".into() } else { "under".into() },
        };
        let side_rate = rate.map(|r| if side == "over" { r } else { 1.0 - r });
        let edge = model.get(if side == "over" { "edge_over" } else { "edge_under" }).and_then(|v| v.as_f64());
        // rank by the chosen window, ties broken by the season then L20 rate on the same side
        let on_side = |w: &str| window_rate(&hr, w).map(|r| if side == "over" { r } else { 1.0 - r }).unwrap_or(-1.0);
        let score = if q.sort == "edge" { [edge.unwrap_or(f64::MIN), side_rate.unwrap_or(-1.0), 0.0] }
                    else { [side_rate.unwrap_or(-1.0), on_side("season"), on_side("l20")] };
        let values = pl.iter().map(|g| stat_value(&g.stats, def));
        rows.push((score, json!({
            "player": l.player, "player_id": pid, "team": l.team, "opponent": opponent, "home": l.home,
            "starts_at": l.starts_at, "stat": def.key, "label": def.label, "unit": def.unit,
            "line": l.line, "over_odds": l.over_odds, "under_odds": l.under_odds,
            "implied_over": implied.map(r3), "side": side, "side_rate": side_rate.map(r3),
            "avg_l10": mean(values.take(10)), "hit_rates": hr, "model": model,
        })));
    }
    rows.sort_by(|a, b| b.0.iter().zip(&a.0).map(|(x, y)| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal))
        .find(|o| o.is_ne()).unwrap_or(std::cmp::Ordering::Equal));
    let out: Vec<Value> = rows.into_iter().take(q.limit).map(|(_, v)| v).collect();
    HttpResponse::Ok().json(json!({
        "sport": sport, "source": q.source, "sort": q.sort, "side": q.side,
        "stats": stat_keys, "count": out.len(), "props": out, "note": unmatched_note,
    }))
}

// --------------------------------------------------------------------------
// GET /cheatsheet/{sport}/games
// --------------------------------------------------------------------------

struct GameLine { home: String, away: String, starts_at: Value, spread: Option<f64>, total: Option<f64>, market: Value }

/// Upcoming games with stored lines — the fallback when the live market is
/// unavailable. Stored spread is home margin, flipped to bettor convention.
async fn stored_upcoming(state: &State, sport: &str) -> Result<Vec<GameLine>, String> {
    let rows = state.db.query(
        "SELECT DISTINCT ON (game_date, home_team, away_team) game_date, home_team, away_team, spread_line, total_line \
         FROM sports_games WHERE sport=$1 AND home_score IS NULL AND game_date >= CURRENT_DATE \
         ORDER BY game_date, home_team, away_team, spread_line NULLS LAST LIMIT 30",
        &[&sport],
    ).await.map_err(|e| e.to_string())?;
    Ok(rows.iter().filter_map(|r| Some(GameLine {
        home: r.get::<_, Option<String>>("home_team")?, away: r.get::<_, Option<String>>("away_team")?,
        starts_at: json!(r.get::<_, Option<NaiveDate>>("game_date").map(|d| d.to_string())),
        spread: r.get::<_, Option<f64>>("spread_line").map(|l| -l), total: r.get("total_line"), market: Value::Null,
    })).collect())
}

pub async fn cheatsheet_games(state: web::Data<State>, path: web::Path<String>) -> impl Responder {
    let sport = path.into_inner();
    if registry(&sport).is_empty() { return bad_request("unknown sport"); }
    let (games, source) = match market::fetch_games(&state, &sport).await {
        Ok(g) if !g.is_empty() => (g.into_iter().map(|v| GameLine {
            home: v["home"].as_str().unwrap_or("").to_string(), away: v["away"].as_str().unwrap_or("").to_string(),
            starts_at: v["starts_at"].clone(), spread: v["spread"].as_f64(), total: v["total"].as_f64(), market: v,
        }).collect::<Vec<_>>(), "market"),
        Ok(_) | Err(_) => match stored_upcoming(&state, &sport).await { Ok(g) => (g, "schedule"), Err(e) => return internal(e) },
    };

    let mut cache: HashMap<String, Vec<TeamGame>> = HashMap::new();
    let mut out = Vec::new();
    for g in &games {
        let (home, away) = (canon(&sport, &g.home), canon(&sport, &g.away));
        for t in [&home, &away] {
            if !cache.contains_key(t) {
                match team_games(&state, &sport, t, 300).await { Ok(v) => { cache.insert(t.clone(), v); } Err(e) => return internal(e) }
            }
        }
        let (hg, ag) = (&cache[&home], &cache[&away]);
        out.push(json!({
            "home": home, "away": away, "starts_at": g.starts_at,
            "spread_home": g.spread, "total": g.total, "market": g.market,
            "home_team": {
                "games": hg.len(),
                "markets": team_markets(hg, g.spread, g.total, None, Some(&away), Some(true)),
                "averages": team_averages(hg),
            },
            "away_team": {
                "games": ag.len(),
                "markets": team_markets(ag, g.spread.map(|s| -s), g.total, None, Some(&home), Some(false)),
                "averages": team_averages(ag),
            },
        }));
    }
    HttpResponse::Ok().json(json!({
        "sport": sport, "source": source, "count": out.len(), "games": out,
        "note": "cover/over apply tonight's line to each team's past games; venue = tonight's home/away split; h2h = last 10 meetings.",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(out: Outcome, season: i32, home: bool, opp: &str) -> Mark { Mark { out, season, home: Some(home), opp: opp.into() } }

    #[test]
    fn outcome_and_summary() {
        assert_eq!(outcome(3.0, 2.5), Outcome::Over);
        assert_eq!(outcome(2.0, 2.5), Outcome::Under);
        assert_eq!(outcome(25.0, 25.0), Outcome::Push);
        let s = summary([Outcome::Over, Outcome::Over, Outcome::Under, Outcome::Push].iter());
        assert_eq!(s["games"], 4);
        assert_eq!(s["rate"], json!(0.667)); // pushes excluded
    }

    #[test]
    fn streak_skips_pushes() {
        let marks = vec![m(Outcome::Over, 1, true, "A"), m(Outcome::Push, 1, true, "A"), m(Outcome::Over, 1, false, "B"), m(Outcome::Under, 1, true, "A")];
        assert_eq!(streak(&marks), json!({ "side": "over", "games": 2 }));
    }

    #[test]
    fn windows_split_season_venue_h2h() {
        let marks = vec![m(Outcome::Over, 2, true, "BOS"), m(Outcome::Under, 2, false, "NYK"), m(Outcome::Over, 1, false, "BOS")];
        let w = windows(&marks, Some("BOS"), Some(true));
        assert_eq!(w["season"]["games"], 2);
        assert_eq!(w["season_label"], 2);
        assert_eq!(w["h2h"]["over"], 2);
        assert_eq!(w["venue"]["games"], 1);
        assert_eq!(w["away"]["rate"], json!(0.5));
    }

    #[test]
    fn half_line_hooks() {
        assert_eq!(half_line(23.2), 23.5);
        assert_eq!(half_line(23.8), 23.5);
        assert_eq!(half_line(0.3), 0.5);
    }

    #[test]
    fn poisson_and_nb_probs() {
        // Poisson(2.5): P(X >= 3) = 1 - e^-2.5 (1 + 2.5 + 3.125) = 0.4562
        let (o, push) = p_over(Family::Count, 2.5, 1.58, None, 2.5);
        assert!((o - 0.4562).abs() < 1e-3, "{o}");
        assert_eq!(push, 0.0);
        // integer line has a push mass = P(X = 2)
        let (_, push) = p_over(Family::Count, 2.5, 1.58, None, 2.0);
        assert!((push - 0.2565).abs() < 1e-3, "{push}");
        // NB with the same mean is wider: more mass far above the line
        let (o_nb, _) = p_over(Family::Count, 2.5, 2.0, Some(2.0), 5.5);
        let (o_po, _) = p_over(Family::Count, 2.5, 1.58, None, 5.5);
        assert!(o_nb > o_po);
    }

    #[test]
    fn gamma_tail_matches_median() {
        // Gamma(shape 1 = exponential, mean 10): P(X > 10) = e^-1
        let (o, _) = p_over(Family::Continuous, 10.0, 10.0, None, 10.0);
        assert!((o - (-1.0f64).exp()).abs() < 1e-6, "{o}");
        // right skew: P(X > mean) = 0.5 - 1/(3 sqrt(2 pi k)) + O(1/k) for shape k
        let k: f64 = (250.0f64 / 60.0).powi(2);
        let (o, _) = p_over(Family::Continuous, 250.0, 60.0, None, 250.0);
        assert!((o - (0.5 - 1.0 / (3.0 * (2.0 * std::f64::consts::PI * k).sqrt()))).abs() < 0.005, "{o}");
    }

    #[test]
    fn participation_mixes_in_zeros() {
        let def = &crate::NFL_STATS.iter().find(|s| s.key == "rush_yds").unwrap();
        let p = Proj { mean: 30.0, sd: 15.0, dispersion: None };
        let full = model_json(def, Some(&p), 1.0, 9.5, None);
        let half = model_json(def, Some(&p), 0.5, 9.5, None);
        let (f, h) = (full["p_over"].as_f64().unwrap(), half["p_over"].as_f64().unwrap());
        assert!((h - f / 2.0).abs() < 0.002, "{f} {h}");
        assert_eq!(half["mean"], json!(15.0));
    }

    #[test]
    fn canon_aliases() {
        assert_eq!(canon("nhl", "T.B"), "TBL");
        assert_eq!(canon("nba", "gs"), "GSW");
        assert_eq!(canon("nfl", "LAR"), "LA");
        assert_eq!(canon("nba", "BOS"), "BOS");
    }

    #[test]
    fn novig_normalizes() {
        let p = novig_over(Some(-110.0), Some(-110.0)).unwrap();
        assert!((p - 0.5).abs() < 1e-9);
    }
}
