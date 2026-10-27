//! First-segment scores of finished games (`game_segments`, migration 0026):
//! the settlement data for the 1st-half markets — moneyline, spread, total and
//! team totals. The segment is the sport's standard first-half market:
//!
//! * NBA, NFL — the 1st half (Q1 + Q2), from ESPN scoreboard linescores
//! * NHL — the 1st period, from the goals in `nhl_shots_api`
//! * MLB — the first five innings, from MLB StatsAPI linescores
//!
//! game_id is the sports_games / simulator id, so every engine, the hit rates
//! and AI Picks settle against the same rows.
//!
//!   POST /ingest/segments?sport=nba&since=2019-01-01

use std::collections::HashMap;

use actix_web::{web, HttpResponse, Responder};
use chrono::{Datelike, Duration, NaiveDate, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::hitrates::canon;
use crate::State;

pub const SPORTS: [&str; 4] = ["nba", "nhl", "nfl", "mlb"];

/// The segment's name on the page and in write-ups.
pub fn label(sport: &str) -> &'static str {
    match sport { "nhl" => "1st period", "mlb" => "first 5 innings", _ => "1st half" }
}

/// Short tag for market labels ("1H ML", "1P total", "F5 spread").
pub fn tag(sport: &str) -> &'static str {
    match sport { "nhl" => "1P", "mlb" => "F5", _ => "1H" }
}

/// Joint distribution of the (home, away) segment score over simulated games.
/// Every 1st-half market prices from it: moneyline (ties push), spread, total
/// and either team's total.
#[derive(Clone, Debug)]
pub struct Joint { pub dim: usize, pub n: u64, pub c: Vec<u64> }

impl Joint {
    /// `dim` - 1 is the largest score kept per team (higher scores are clamped).
    pub fn new(dim: usize) -> Self { Joint { dim, n: 0, c: vec![0; dim * dim] } }
    pub fn for_sport(sport: &str) -> Self { Joint::new(match sport { "nba" => 121, "nfl" => 71, "mlb" => 31, _ => 16 }) }
    pub fn add(&mut self, home: i32, away: i32) {
        let k = |x: i32| (x.max(0) as usize).min(self.dim - 1);
        let i = k(home) * self.dim + k(away);
        self.c[i] += 1;
        self.n += 1;
    }
    pub fn merge(&mut self, o: &Joint) {
        if o.dim != self.dim { return; }
        for (a, b) in self.c.iter_mut().zip(&o.c) { *a += b; }
        self.n += o.n;
    }
    fn cells(&self) -> impl Iterator<Item = (i32, i32, f64)> + '_ {
        let n = self.n.max(1) as f64;
        self.c.iter().enumerate().filter(|(_, c)| **c > 0).map(move |(i, c)| ((i / self.dim) as i32, (i % self.dim) as i32, *c as f64 / n))
    }
    /// (P(home ahead), P(away ahead), P(tied))
    pub fn win(&self) -> (f64, f64, f64) {
        let (mut h, mut a, mut t) = (0.0, 0.0, 0.0);
        for (x, y, p) in self.cells() { if x > y { h += p } else if y > x { a += p } else { t += p } }
        (h, a, t)
    }
    /// A pmf of f(home, away) as {pmf, pmf_offset, mean, q10, q50, q90}.
    fn dist(&self, f: impl Fn(i32, i32) -> i32) -> Value {
        let vals: Vec<(i32, f64)> = self.cells().map(|(x, y, p)| (f(x, y), p)).collect();
        if vals.is_empty() { return Value::Null; }
        let lo = vals.iter().map(|v| v.0).min().unwrap();
        let hi = vals.iter().map(|v| v.0).max().unwrap();
        let mut pmf = vec![0.0; (hi - lo + 1) as usize];
        for (v, p) in &vals { pmf[(v - lo) as usize] += p; }
        let mean: f64 = vals.iter().map(|(v, p)| *v as f64 * p).sum();
        let q = |q: f64| { let mut c = 0.0; for (i, m) in pmf.iter().enumerate() { c += m; if c >= q { return lo + i as i32; } } hi };
        let r4 = |x: f64| (x * 10000.0).round() / 10000.0;
        json!({ "pmf": pmf.iter().map(|p| r4(*p)).collect::<Vec<_>>(), "pmf_offset": lo, "mean": (mean * 1000.0).round() / 1000.0,
                "q10": q(0.1), "q50": q(0.5), "q90": q(0.9) })
    }
    /// The payload block every engine serves (`h1` in simulator payloads).
    pub fn json(&self, sport: &str) -> Value {
        let (h, a, t) = self.win();
        let r4 = |x: f64| (x * 10000.0).round() / 10000.0;
        json!({ "label": label(sport), "tag": tag(sport), "n": self.n,
                "win": { "home": r4(h), "away": r4(a), "tie": r4(t) },
                "home": self.dist(|x, _| x), "away": self.dist(|_, y| y),
                "total": self.dist(|x, y| x + y), "margin_home": self.dist(|x, y| x - y) })
    }
}

// ---------------------------------------------------------------------------
// Markets
// ---------------------------------------------------------------------------

/// Every game market: the full-game three, both team totals, and the same five
/// for the first segment.
pub const GAME_MARKETS: [&str; 10] = ["moneyline", "spread", "total", "team_total_home", "team_total_away",
    "moneyline_1h", "spread_1h", "total_1h", "team_total_home_1h", "team_total_away_1h"];

pub fn is_game_market(m: &str) -> bool { GAME_MARKETS.contains(&m) }

/// The full-game market a market settles like: moneyline, spread or an over/under ("total").
pub fn base_market(m: &str) -> &'static str {
    match m { "moneyline" | "moneyline_1h" => "moneyline", "spread" | "spread_1h" => "spread", _ => "total" }
}

/// Sides are home/away (moneyline, spread) or over/under (totals).
pub fn home_away(m: &str) -> bool { base_market(m) != "total" }

/// The outcome a game market settles on (keys of `Outcomes::game`).
pub fn outcome_key(m: &str) -> &'static str {
    match m {
        "moneyline" | "spread" => "home_margin",
        "team_total_home" => "home_pts", "team_total_away" => "away_pts",
        "moneyline_1h" | "spread_1h" => "h1_margin", "total_1h" => "h1_total",
        "team_total_home_1h" => "h1_home", "team_total_away_1h" => "h1_away",
        _ => "total",
    }
}

/// Market name for labels: "Moneyline", "1H Spread", "Home Team Total", "1P Total"...
pub fn market_label(sport: &str, m: &str) -> String {
    let (base, seg) = match m.strip_suffix("_1h") { Some(b) => (b, true), None => (m, false) };
    let name = match base { "moneyline" => "Moneyline", "spread" => if sport == "nhl" { "Puck Line" } else if sport == "mlb" { "Run Line" } else { "Spread" },
        "total" => "Total", "team_total_home" => "Home Team Total", "team_total_away" => "Away Team Total", _ => return String::new() };
    if seg { format!("{} {name}", tag(sport)) } else { name.to_string() }
}

// ---------------------------------------------------------------------------
// Learning-loop forecasts for the new markets
// ---------------------------------------------------------------------------

/// First-half margin and total per unit of the full-game market numbers
/// (regressions over 2019-2026 games with closing lines: NBA margin slope 0.595,
/// total ratio 0.505; NFL 0.569 and 0.510).
fn h1_scale(sport: &str) -> (f64, f64, f64) {
    // (margin slope, total ratio, sd of the 1H margin around the scaled spread)
    match sport { "nfl" => (0.57, 0.51, 10.1), _ => (0.595, 0.505, 10.9) }
}

fn half_line(x: f64) -> f64 { (x * 2.0).round() / 2.0 }
fn hook(x: f64) -> f64 { x.floor() + 0.5 }

fn ncdf(z: f64) -> f64 {
    let t = 1.0 / (1.0 + 0.2316419 * z.abs());
    let d = 0.3989423 * (-z * z / 2.0).exp();
    let p = d * t * (0.3193815 + t * (-0.3565638 + t * (1.781478 + t * (-1.821256 + t * 1.330274))));
    if z > 0.0 { 1.0 - p } else { p }
}

impl Joint {
    /// P(f > line), P(f < line) over the simulated games.
    pub fn over_under(&self, f: impl Fn(i32, i32) -> i32, line: f64) -> (f64, f64) {
        let (mut o, mut u) = (0.0, 0.0);
        for (x, y, p) in self.cells() { let v = f(x, y) as f64; if v > line + 1e-9 { o += p } else if v < line - 1e-9 { u += p } }
        (o, u)
    }
    /// P(f > line | not a push)
    pub fn p_over(&self, f: impl Fn(i32, i32) -> i32, line: f64) -> f64 {
        let (o, u) = self.over_under(f, line);
        o / (o + u).max(1e-12)
    }
}

/// Forecasts for team totals and the first-segment markets, in the learning
/// loop's terms (scored on game outcomes `home_pts`, `away_pts`, `h1_*`).
///
/// `exp_margin` / `exp_total`: the market's expected home margin (positive =
/// home favoured) and total for the full game, when the game has lines. NBA and
/// NFL lines are then derived from them and scored against 0.5; NHL and MLB have
/// no stored lines, so they use the standard lines and league base rates.
/// `full_over(home, line)` = P(team points > line | no push) for the full game.
pub fn forecasts(sport: &str, exp_margin: Option<f64>, exp_total: Option<f64>, h1: &Joint, full_over: &dyn Fn(bool, f64) -> f64) -> Vec<crate::learning::Forecast> {
    use crate::learning::Forecast as F;
    let mut out = Vec::new();
    let has = h1.n > 0;
    match sport {
        "nba" | "nfl" => {
            let (ms, ts, sd) = h1_scale(sport);
            if has {
                let base = exp_margin.map(|m| ncdf(ms * m / sd)).unwrap_or(0.53);
                out.push(F::game("h1_home_win", "scaled market spread (normal)", "h1_margin", 0.0, h1.p_over(|x, y| x - y, 0.0), base).pushes());
            }
            if let (Some(m), Some(t)) = (exp_margin, exp_total) {
                let (l_m1, l_t1) = (half_line(ms * m), half_line(ts * t));
                if has {
                    out.push(F::game("h1_home_cover", "0.5 at the scaled spread", "h1_margin", l_m1, h1.p_over(|x, y| x - y, l_m1), 0.5).pushes());
                    out.push(F::game("h1_total_over", "0.5 at the scaled total", "h1_total", l_t1, h1.p_over(|x, y| x + y, l_t1), 0.5).pushes());
                    let (lh1, la1) = (hook((l_t1 + l_m1) / 2.0), hook((l_t1 - l_m1) / 2.0));
                    out.push(F::game("h1_home_total_over", "0.5 at the implied team total", "h1_home", lh1, h1.p_over(|x, _| x, lh1), 0.5));
                    out.push(F::game("h1_away_total_over", "0.5 at the implied team total", "h1_away", la1, h1.p_over(|_, y| y, la1), 0.5));
                }
                let (lh, la) = (hook((t + m) / 2.0), hook((t - m) / 2.0));
                out.push(F::game("home_total_over", "0.5 at the implied team total", "home_pts", lh, full_over(true, lh), 0.5));
                out.push(F::game("away_total_over", "0.5 at the implied team total", "away_pts", la, full_over(false, la), 0.5));
            }
        }
        // league rates of the 2021-22 to 2026-27 regular seasons (6,747 games; MLB 2021-2026, 14,820)
        "nhl" => {
            if has {
                out.push(F::game("h1_home_win", "league 1st-period home lead rate 0.521", "h1_margin", 0.0, h1.p_over(|x, y| x - y, 0.0), 0.521).pushes());
                out.push(F::game("h1_total_over_1_5", "league 1st-period over 1.5 rate 0.54", "h1_total", 1.5, h1.p_over(|x, y| x + y, 1.5), 0.54));
                out.push(F::game("h1_home_total_over_0_5", "league rate 0.605", "h1_home", 0.5, h1.p_over(|x, _| x, 0.5), 0.605));
                out.push(F::game("h1_away_total_over_0_5", "league rate 0.583", "h1_away", 0.5, h1.p_over(|_, y| y, 0.5), 0.583));
            }
            out.push(F::game("home_total_over_2_5", "league rate 0.631", "home_pts", 2.5, full_over(true, 2.5), 0.631));
            out.push(F::game("away_total_over_2_5", "league rate 0.587", "away_pts", 2.5, full_over(false, 2.5), 0.587));
        }
        "mlb" => {
            if has {
                out.push(F::game("h1_home_win", "league F5 home lead rate 0.532", "h1_margin", 0.0, h1.p_over(|x, y| x - y, 0.0), 0.532).pushes());
                out.push(F::game("h1_total_over_4_5", "league F5 over 4.5 rate 0.50", "h1_total", 4.5, h1.p_over(|x, y| x + y, 4.5), 0.50));
                out.push(F::game("h1_home_total_over_2_5", "league rate", "h1_home", 2.5, h1.p_over(|x, _| x, 2.5), 0.43));
                out.push(F::game("h1_away_total_over_2_5", "league rate", "h1_away", 2.5, h1.p_over(|_, y| y, 2.5), 0.39));
            }
            out.push(F::game("home_total_over_4_5", "league rate", "home_pts", 4.5, full_over(true, 4.5), 0.435));
            out.push(F::game("away_total_over_4_5", "league rate", "away_pts", 4.5, full_over(false, 4.5), 0.42));
        }
        _ => {}
    }
    out
}

/// Team-total block for the Projections page matchups: each team's simulated
/// points (from paired margin / total draws) and, at a line, over / under / push.
pub fn team_totals_json(margins: &[f64], totals: &[f64], home_line: Option<f64>, away_line: Option<f64>) -> Value {
    let side = |home: bool, line: Option<f64>| {
        let mut v: Vec<f64> = margins.iter().zip(totals).map(|(m, t)| if home { (t + m) / 2.0 } else { (t - m) / 2.0 }).collect();
        if v.is_empty() { return Value::Null; }
        v.sort_by(|a, b| a.total_cmp(b));
        let n = v.len() as f64;
        let q = |p: f64| v[((p * (v.len() - 1) as f64) as usize).min(v.len() - 1)];
        let r4 = |x: f64| (x * 10000.0).round() / 10000.0;
        let at = line.map(|l| {
            let o = v.iter().filter(|x| **x > l + 1e-9).count() as f64 / n;
            let u = v.iter().filter(|x| **x < l - 1e-9).count() as f64 / n;
            json!({ "line": l, "over": r4(o), "under": r4(u), "push": r4(1.0 - o - u) })
        });
        json!({ "mean": (v.iter().sum::<f64>() / n * 100.0).round() / 100.0, "q10": q(0.1), "q50": q(0.5), "q90": q(0.9), "at_line": at })
    };
    json!({ "home": side(true, home_line), "away": side(false, away_line) })
}

/// Team scores and the first-segment results for a final game's outcomes
/// (the simulators report the margin and total; this adds the rest).
pub(crate) async fn augment(db: &tokio_postgres::Client, sport: &str, id: &str, o: &mut crate::learning::Outcomes) {
    if let (Some(m), Some(t)) = (o.game.get("home_margin").copied(), o.game.get("total").copied()) {
        o.game.insert("home_pts", (t + m) / 2.0);
        o.game.insert("away_pts", (t - m) / 2.0);
    }
    if let Some((h, a)) = get(db, sport, id).await {
        o.game.insert("h1_margin", (h - a) as f64);
        o.game.insert("h1_total", (h + a) as f64);
        o.game.insert("h1_home", h as f64);
        o.game.insert("h1_away", a as f64);
    }
}

fn dberr(e: &tokio_postgres::Error) -> String {
    e.as_db_error().map(|d| d.message().to_string()).unwrap_or_else(|| e.to_string())
}

/// (home, away) first-segment score of one game.
pub(crate) async fn get(db: &tokio_postgres::Client, sport: &str, id: &str) -> Option<(i32, i32)> {
    db.query_opt("SELECT home_1h, away_1h FROM game_segments WHERE sport=$1 AND game_id=$2", &[&sport, &id]).await
        .ok().flatten().map(|r| (r.get(0), r.get(1)))
}

// ---------------------------------------------------------------------------
// Parsing (pure)
// ---------------------------------------------------------------------------

/// Sum of the first `n` periods of an ESPN competitor's linescores.
fn espn_first(c: &Value, n: usize) -> Option<i32> {
    let ls = c["linescores"].as_array()?;
    if ls.len() < n { return None; }
    let mut s = 0.0;
    for x in &ls[..n] { s += x["value"].as_f64().or_else(|| x["displayValue"].as_str().and_then(|v| v.parse().ok()))?; }
    Some(s.round() as i32)
}

/// One finished ESPN event: (event id, home code, away code, home 1H, away 1H, home final, away final).
pub(crate) fn espn_event(ev: &Value) -> Option<(String, String, String, i32, i32, i32, i32)> {
    let c = &ev["competitions"][0];
    if c["status"]["type"]["completed"].as_bool() != Some(true) { return None; }
    let comps = c["competitors"].as_array()?;
    let side = |ha: &str| comps.iter().find(|t| t["homeAway"] == ha);
    let (h, a) = (side("home")?, side("away")?);
    let score = |t: &Value| t["score"].as_str().and_then(|s| s.parse::<i32>().ok()).or_else(|| t["score"].as_i64().map(|x| x as i32));
    Some((ev["id"].as_str()?.to_string(), h["team"]["abbreviation"].as_str()?.to_string(), a["team"]["abbreviation"].as_str()?.to_string(),
        espn_first(h, 2)?, espn_first(a, 2)?, score(h)?, score(a)?))
}

/// One MLB StatsAPI schedule game: (gamePk, home F5, away F5). Both teams must
/// have batted through the 5th (a game called earlier has no F5 result).
pub(crate) fn mlb_f5(g: &Value) -> Option<(String, i32, i32)> {
    let inn = g["linescore"]["innings"].as_array()?;
    if inn.len() < 5 { return None; }
    let (mut h, mut a) = (0i64, 0i64);
    for x in &inn[..5] {
        a += x["away"]["runs"].as_i64()?;
        h += x["home"]["runs"].as_i64()?;
    }
    Some((g["gamePk"].as_i64()?.to_string(), h as i32, a as i32))
}

// ---------------------------------------------------------------------------
// Fetch
// ---------------------------------------------------------------------------

struct Missing { id: String, date: NaiveDate, home: String, away: String, hs: i32, as_: i32 }

async fn missing(db: &tokio_postgres::Client, sport: &str, since: NaiveDate) -> Result<Vec<Missing>, String> {
    let rows = db.query(
        "SELECT g.game_id, g.game_date, g.home_team, g.away_team, g.home_score, g.away_score FROM sports_games g \
         LEFT JOIN game_segments s ON s.sport = g.sport AND s.game_id = g.game_id \
         WHERE g.sport=$1 AND lower(g.status) IN ('final', 'completed early') AND g.game_date >= $2 AND g.game_date < CURRENT_DATE + 1 \
           AND g.home_score IS NOT NULL AND s.game_id IS NULL", &[&sport, &since]).await.map_err(|e| dberr(&e))?;
    Ok(rows.iter().map(|r| Missing { id: r.get(0), date: r.get(1), home: r.get(2), away: r.get(3), hs: r.get(4), as_: r.get(5) }).collect())
}

async fn put(db: &tokio_postgres::Client, sport: &str, id: &str, h: i32, a: i32, source: &str) -> Result<(), String> {
    db.execute("INSERT INTO game_segments (sport, game_id, home_1h, away_1h, source) VALUES ($1,$2,$3,$4,$5) \
                ON CONFLICT (sport, game_id) DO UPDATE SET home_1h=EXCLUDED.home_1h, away_1h=EXCLUDED.away_1h, source=EXCLUDED.source, updated_at=now()",
        &[&sport, &id, &h, &a, &source]).await.map(|_| ()).map_err(|e| dberr(&e))
}

/// NHL: 1st-period goals per team, keyed by the NHL api game id (simulator,
/// AI Picks) and, for past seasons, also by the MoneyPuck id sports_games
/// uses there (`mp_YYYYMMDD_AWAY_HOME`, hit rates and the team model).
async fn nhl(db: &tokio_postgres::Client, since: NaiveDate) -> Result<Value, String> {
    let n = db.execute(
        "INSERT INTO game_segments (sport, game_id, home_1h, away_1h, source) \
         SELECT 'nhl', g.game_id::text, \
                count(*) FILTER (WHERE s.kind='goal' AND s.period=1 AND s.team=g.home)::int, \
                count(*) FILTER (WHERE s.kind='goal' AND s.period=1 AND s.team=g.away)::int, 'nhl_api' \
         FROM nhl_games_api g JOIN nhl_shots_api s ON s.game_id = g.game_id \
         WHERE g.completed AND g.game_date >= $1 \
         GROUP BY g.game_id, g.home, g.away \
         ON CONFLICT (sport, game_id) DO UPDATE SET home_1h=EXCLUDED.home_1h, away_1h=EXCLUDED.away_1h, updated_at=now()",
        &[&since]).await.map_err(|e| dberr(&e))?;
    // the same games under their sports_games ids (same date and teams, final score agrees)
    let rows = db.query(
        "SELECT x.game_id, x.game_date, x.home_team, x.away_team, x.home_score, x.away_score FROM sports_games x \
         LEFT JOIN game_segments s ON s.sport='nhl' AND s.game_id = x.game_id \
         WHERE x.sport='nhl' AND x.status='final' AND x.game_date >= $1 AND s.game_id IS NULL", &[&since]).await.map_err(|e| dberr(&e))?;
    let api = db.query(
        "SELECT g.game_date, g.home, g.away, g.home_score, g.away_score, s.home_1h, s.away_1h FROM nhl_games_api g \
         JOIN game_segments s ON s.sport='nhl' AND s.game_id = g.game_id::text WHERE g.completed AND g.game_date >= $1", &[&since]).await.map_err(|e| dberr(&e))?;
    let by: HashMap<(NaiveDate, String, String), (i32, i32, i32, i32)> = api.iter().map(|r| ((r.get(0), canon("nhl", r.get(1)), canon("nhl", r.get(2))),
        (r.get::<_, Option<i32>>(3).unwrap_or(-1), r.get::<_, Option<i32>>(4).unwrap_or(-1), r.get(5), r.get(6)))).collect();
    let mut aliased = 0u64;
    for r in &rows {
        let key = (r.get::<_, NaiveDate>(1), canon("nhl", r.get(2)), canon("nhl", r.get(3)));
        let Some(&(hs, as_, h1, a1)) = by.get(&key) else { continue };
        if Some(hs) != r.get::<_, Option<i32>>(4) || Some(as_) != r.get::<_, Option<i32>>(5) { continue; }
        put(db, "nhl", r.get(0), h1, a1, "nhl_api").await?;
        aliased += 1;
    }
    Ok(json!({ "stored": n, "aliased": aliased }))
}

/// NBA and NFL: ESPN scoreboard per game date; matched on the event id (NBA ids
/// are ESPN ids), else on the date's team pair, and checked against the final.
async fn espn(state: &State, sport: &str, since: NaiveDate) -> Result<Value, String> {
    let (sp, lg) = match sport { "nba" => ("basketball", "nba"), "nfl" => ("football", "nfl"), _ => return Err("no ESPN league".into()) };
    let miss = missing(&state.db, sport, since).await?;
    let mut by_date: HashMap<NaiveDate, Vec<&Missing>> = HashMap::new();
    for m in &miss { by_date.entry(m.date).or_default().push(m); }
    let mut dates: Vec<NaiveDate> = by_date.keys().copied().collect();
    dates.sort();
    let (mut stored, mut mismatched, mut errors) = (0u64, 0u64, Vec::new());
    for chunk in dates.chunks(8) {
        let fetched = futures::future::join_all(chunk.iter().map(|d| {
            let url = format!("https://site.api.espn.com/apis/site/v2/sports/{sp}/{lg}/scoreboard?dates={}&limit=100", d.format("%Y%m%d"));
            async move {
                let r = state.http.get(&url).timeout(std::time::Duration::from_secs(25)).send().await.map_err(|e| e.without_url().to_string())?;
                if !r.status().is_success() { return Err(format!("{} for {}", r.status(), d)); }
                r.json::<Value>().await.map_err(|e| e.without_url().to_string()).map(|v| (*d, v))
            }
        })).await;
        for f in fetched {
            let (d, sb) = match f { Ok(x) => x, Err(e) => { if errors.len() < 10 { errors.push(e); } continue } };
            let games = &by_date[&d];
            for ev in sb["events"].as_array().into_iter().flatten() {
                let Some((eid, h, a, h1, a1, hf, af)) = espn_event(ev) else { continue };
                let (h, a) = (canon(sport, &h), canon(sport, &a));
                let Some(m) = games.iter().find(|m| m.id == eid).or_else(|| games.iter().find(|m| canon(sport, &m.home) == h && canon(sport, &m.away) == a)) else { continue };
                // the linescore must belong to this result
                if hf != m.hs || af != m.as_ || h1 > hf || a1 > af { mismatched += 1; continue; }
                put(&state.db, sport, &m.id, h1, a1, "espn").await?;
                stored += 1;
            }
        }
    }
    Ok(json!({ "missing": miss.len(), "dates": dates.len(), "stored": stored, "mismatched": mismatched, "errors": errors }))
}

/// MLB: StatsAPI schedule with linescores, one request per season.
async fn mlb(state: &State, since: NaiveDate) -> Result<Value, String> {
    let miss = missing(&state.db, "mlb", since).await?;
    let want: HashMap<&str, &Missing> = miss.iter().map(|m| (m.id.as_str(), m)).collect();
    let mut seasons: Vec<i32> = miss.iter().map(|m| m.date.year()).collect();
    seasons.sort(); seasons.dedup();
    let (mut stored, mut errors) = (0u64, Vec::new());
    for season in seasons {
        let url = format!("https://statsapi.mlb.com/api/v1/schedule?sportId=1&season={season}&gameType=R,F,D,L,W&hydrate=linescore\
&fields=dates,games,gamePk,linescore,innings,num,home,away,runs");
        let v: Value = match state.http.get(&url).timeout(std::time::Duration::from_secs(90)).send().await {
            Ok(r) if r.status().is_success() => match r.json().await { Ok(v) => v, Err(e) => { errors.push(e.without_url().to_string()); continue } },
            Ok(r) => { errors.push(format!("{} for {season}", r.status())); continue }
            Err(e) => { errors.push(e.without_url().to_string()); continue }
        };
        for d in v["dates"].as_array().into_iter().flatten() {
            for g in d["games"].as_array().into_iter().flatten() {
                let Some((pk, h, a)) = mlb_f5(g) else { continue };
                let Some(m) = want.get(pk.as_str()) else { continue };
                if h > m.hs || a > m.as_ { continue; }
                put(&state.db, "mlb", &pk, h, a, "mlb_statsapi").await?;
                stored += 1;
            }
        }
    }
    Ok(json!({ "missing": miss.len(), "stored": stored, "errors": errors }))
}

/// Fill the missing segments of one sport's finished games since `since`.
pub(crate) async fn refresh(state: &State, sport: &str, since: NaiveDate) -> Result<Value, String> {
    match sport {
        "nhl" => nhl(&state.db, since).await,
        "nba" | "nfl" => espn(state, sport, since).await,
        "mlb" => mlb(state, since).await,
        _ => Err(format!("no segments for {sport}")),
    }
}

/// Backfill on the first run (2019 on), then the last two weeks.
pub(crate) async fn refresh_due(state: &State, sport: &str) -> Result<Value, String> {
    let have: i64 = state.db.query_one("SELECT count(*) FROM game_segments WHERE sport=$1", &[&sport]).await.map_err(|e| dberr(&e))?.get(0);
    let since = if have < 500 { NaiveDate::from_ymd_opt(2019, 1, 1).unwrap() } else { Utc::now().date_naive() - Duration::days(14) };
    refresh(state, sport, since).await
}

#[derive(Deserialize)]
pub struct IngestQ { sport: Option<String>, since: Option<NaiveDate> }

pub async fn ingest_http(state: web::Data<State>, q: web::Query<IngestQ>) -> impl Responder {
    let sports: Vec<&str> = match q.sport.as_deref() {
        Some(s) if SPORTS.contains(&s) => vec![s],
        Some(_) => return HttpResponse::BadRequest().json(json!({ "error": "sport must be nba, nhl, nfl or mlb" })),
        None => SPORTS.to_vec(),
    };
    let since = q.since.unwrap_or_else(|| NaiveDate::from_ymd_opt(2019, 1, 1).unwrap());
    let mut out = serde_json::Map::new();
    for s in sports {
        out.insert(s.into(), match refresh(&state, s, since).await { Ok(v) => v, Err(e) => json!({ "error": e }) });
    }
    HttpResponse::Ok().json(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn espn_event_sums_the_first_two_periods() {
        let ev = json!({ "id": "401", "competitions": [{ "status": { "type": { "completed": true } }, "competitors": [
            { "homeAway": "home", "team": { "abbreviation": "ATL" }, "score": "36", "linescores": [{ "value": 7.0 }, { "value": 10.0 }, { "value": 3.0 }, { "value": 10.0 }, { "value": 6.0 }] },
            { "homeAway": "away", "team": { "abbreviation": "TB" }, "score": "30", "linescores": [{ "value": 10.0 }, { "value": 14.0 }, { "value": 3.0 }, { "value": 3.0 }] } ] }] });
        assert_eq!(espn_event(&ev), Some(("401".into(), "ATL".into(), "TB".into(), 17, 24, 36, 30)));
        let mut live = ev.clone();
        live["competitions"][0]["status"]["type"]["completed"] = json!(false);
        assert_eq!(espn_event(&live), None, "unfinished games are skipped");
    }

    #[test]
    fn joint_prices_every_first_segment_market() {
        let mut j = Joint::new(10);
        // home 3-1, 2-2, 1-2, 4-0 (equal weight)
        for (h, a) in [(3, 1), (2, 2), (1, 2), (4, 0)] { j.add(h, a); }
        let (h, a, t) = j.win();
        assert!((h - 0.5).abs() < 1e-12 && (a - 0.25).abs() < 1e-12 && (t - 0.25).abs() < 1e-12);
        // home -1.5 covers in 3-1 and 4-0
        assert!((j.p_over(|x, y| x - y, 1.5) - 0.5).abs() < 1e-12);
        // total 4.5: 4,4,3,4 -> never over
        assert_eq!(j.over_under(|x, y| x + y, 4.5), (0.0, 1.0));
        // pushes are excluded: total 4 lands on 4 three times
        assert!((j.p_over(|x, y| x + y, 4.0) - 0.0).abs() < 1e-12);
        let v = j.json("nba");
        assert_eq!(v["tag"], "1H");
        assert_eq!(v["margin_home"]["pmf_offset"], -1);
        assert!((v["home"]["mean"].as_f64().unwrap() - 2.5).abs() < 1e-9);
        let pmf: f64 = v["total"]["pmf"].as_array().unwrap().iter().map(|x| x.as_f64().unwrap()).sum();
        assert!((pmf - 1.0).abs() < 1e-9);
    }

    #[test]
    fn markets_map_to_outcomes_and_labels() {
        assert_eq!(outcome_key("spread_1h"), "h1_margin");
        assert_eq!(outcome_key("team_total_away"), "away_pts");
        assert_eq!(outcome_key("total"), "total");
        assert_eq!(base_market("team_total_home_1h"), "total");
        assert!(home_away("moneyline_1h") && !home_away("total_1h"));
        assert_eq!(market_label("nhl", "spread_1h"), "1P Puck Line");
        assert_eq!(market_label("mlb", "total_1h"), "F5 Total");
        assert_eq!(market_label("nba", "team_total_home"), "Home Team Total");
    }

    #[test]
    fn forecasts_derive_lines_from_the_full_game_market() {
        let mut j = Joint::new(80);
        for (h, a) in [(60, 50), (55, 58), (57, 57), (62, 49)] { j.add(h, a); }
        let over = |_home: bool, _l: f64| 0.5;
        // home favoured by 6, total 224
        let f = forecasts("nba", Some(6.0), Some(224.0), &j, &over);
        let get = |m: &str| f.iter().find(|x| x.market == m).unwrap();
        assert_eq!(get("h1_home_cover").line, 3.5);          // 0.595 * 6 = 3.57 -> 3.5
        assert_eq!(get("h1_total_over").line, 113.0);        // 0.505 * 224 = 113.1 -> 113
        assert_eq!(get("home_total_over").line, 115.5);      // (224 + 6) / 2 = 115 -> 115.5
        assert_eq!(get("away_total_over").line, 109.5);
        assert!(get("h1_home_win").push_void, "a tied half voids the moneyline");
        // no lines: NBA keeps only the 1H moneyline; NHL and MLB use their standard lines
        assert_eq!(forecasts("nba", None, None, &j, &over).len(), 1);
        let nhl = forecasts("nhl", None, None, &Joint::new(16), &over);
        assert!(nhl.iter().all(|x| !x.market.starts_with("h1_")), "no 1st-period forecasts without simulated periods");
        assert!(nhl.iter().any(|x| x.market == "home_total_over_2_5"));
    }

    #[test]
    fn team_totals_block_splits_margin_and_total() {
        let v = team_totals_json(&[2.0, -1.0], &[10.0, 7.0], Some(5.5), Some(3.5));
        assert_eq!(v["home"]["at_line"]["over"], 0.5); // home scored 6 and 3
        assert_eq!(v["away"]["at_line"]["over"], 1.0); // away scored 4 and 4: both over 3.5
        let _ = v;
    }

    #[test]
    fn mlb_f5_needs_five_full_innings() {
        let inn = |n: usize| (1..=n).map(|i| json!({ "num": i, "home": { "runs": 1 }, "away": { "runs": 0 } })).collect::<Vec<_>>();
        assert_eq!(mlb_f5(&json!({ "gamePk": 7, "linescore": { "innings": inn(9) } })), Some(("7".into(), 5, 0)));
        assert_eq!(mlb_f5(&json!({ "gamePk": 7, "linescore": { "innings": inn(4) } })), None);
        let mut short = inn(5);
        short[4]["home"] = json!({});
        assert_eq!(mlb_f5(&json!({ "gamePk": 7, "linescore": { "innings": short } })), None, "home did not bat the 5th");
    }
}
