//! Parlay Builder: joint probabilities, fair odds and EV for multi-leg tickets
//! across moneylines, spreads, totals, team totals, 1st-half / 1st-period /
//! first-5 markets and player props (NBA, NHL, NFL, MLB).
//!
//! Joint probability
//!   Every leg of one game is graded against the SAME simulated games of that
//!   sport's champion engine (`*_game::parlay_samples`), so pace, game script,
//!   blowouts, overtime, usage and teammate interactions carry into the joint
//!   outcome. Legs in different games are independent (separate simulations;
//!   no slate-wide factor is modelled), so a ticket's probability is the
//!   product of its per-game joint probabilities.
//!
//!   The simulators' marginals alone lose to closing lines, so each leg's
//!   level comes from the AI Picks blend (engine, player's own frequency and
//!   the de-vigged market, weights fitted on settled lines; blend.rs). The
//!   simulated games are re-weighted to those marginals with the least change
//!   (iterative proportional fitting = the maximum-entropy tilt): the
//!   dependence between legs is the simulator's, the levels are the blend's.
//!   A push or a void (player inactive) keeps its simulated share.
//!
//!   p_dep    joint P(all legs win) on the tilted simulations
//!   p_indep  product of the same tilted marginals (independence baseline)
//!   p_joint  = a * p_dep + (1 - a) * p_indep, a = dependence weight of the
//!              parlay model in force (parlay_models; learned, see parlay_store)
//!
//! Payout and EV
//!   A losing leg loses the ticket; a pushed or void leg drops out (the ticket
//!   pays the product of the remaining legs). Per game the expected payout is
//!   taken over the joint distribution of win/loss/push/void patterns, then
//!   multiplied across games:  EV per unit = E[payout] - 1.
//!   Fair odds = 1 / P(all legs win).
//!   The offered price is one book's: every leg must be quoted there. Cross-game
//!   parlays pay the product of the legs' prices (verified). Same-game legs are
//!   repriced by books (their SGP price is not in any feed), so such a price
//!   is labelled an estimate.
//!
//! Uncertainty
//!   mc_se    Monte Carlo standard error (Kish effective sample size of the
//!            tilted simulations)
//!   range    the joint under the raw simulator marginals and under the market
//!            marginals (same dependence): how much the answer depends on whose
//!            marginals are right
//!   ev_cons  conservative screening EV: marginals halfway between the blend and
//!            the market, minus two Monte Carlo standard errors of the payout.
//!            A screen, not a guarantee.
//!
//! Candidate generation: leg pool (filters, then the best `pool` legs by the
//! objective's single-leg proxy, plus locked legs), then a beam search over leg
//! counts with exact ticket evaluation at every step; per-game evaluations are
//! cached, so cross-game extensions cost O(1).
//!
//!   GET  /parlays/slate?sport=&date=&max_age=
//!   GET  /parlays/legs?sport=&date=&game=&q=&market=
//!   GET  /parlays/generate?sport=&date=&category=&legs=&objective=&...
//!   POST /parlays/evaluate      { sport, date?, legs: [leg id], book? }
//!   GET  /parlays/status?sport=  POST /parlays/cancel?sport=

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use actix_web::{web, HttpResponse, Responder};
use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_postgres::types::Json;

use crate::picks::{self, decimal, novig};
use crate::{blend, lines, segments, State};

pub const VOID: i16 = i16::MIN;
/// Parameter draws per simulated slate game (each redraws every rate from its posterior).
pub const DRAWS: usize = 50;
pub const SPORTS: [&str; 4] = ["nba", "nhl", "nfl", "mlb"];
pub const ENGINE: &str = "parlay-joint-v1";

const W: u8 = 1;
const L: u8 = 0;
const P: u8 = 2;
const V: u8 = 3;

fn dberr(e: &tokio_postgres::Error) -> String { e.as_db_error().map(|d| d.message().to_string()).unwrap_or_else(|| e.to_string()) }
fn internal(m: impl std::fmt::Display) -> HttpResponse {
    eprintln!("projection-service(parlay): {m}");
    HttpResponse::InternalServerError().json(json!({ "error": m.to_string() }))
}
fn bad(m: impl Into<String>) -> HttpResponse { HttpResponse::BadRequest().json(json!({ "error": m.into() })) }
fn r4(x: f64) -> f64 { (x * 10000.0).round() / 10000.0 }

/// Simulations per game (setting `parlay_sims_{sport}` overrides). NFL plays
/// out every snap and is ~100x slower per game than the others.
async fn sims_for(db: &tokio_postgres::Client, sport: &str) -> usize {
    let d = if sport == "nfl" { 4000.0 } else { 10000.0 };
    lines::setting(db, &format!("parlay_sims_{sport}"), d).await.clamp(1000.0, 50000.0) as usize
}
fn cache_minutes(sport: &str) -> i64 { if sport == "nfl" { 240 } else { 60 } }

// ---------------------------------------------------------------------------
// Time: slates are US Eastern dates
// ---------------------------------------------------------------------------

fn nth_sunday(y: i32, m: u32, n: u32) -> NaiveDate {
    let first = NaiveDate::from_ymd_opt(y, m, 1).unwrap();
    let off = (7 - first.weekday().num_days_from_sunday()) % 7;
    first + Duration::days((off + 7 * (n - 1)) as i64)
}
/// UTC offset of US Eastern time (EDT from the 2nd Sunday of March to the 1st Sunday of November).
pub fn et_offset_hours(t: DateTime<Utc>) -> i64 {
    let y = t.year();
    let start = Utc.from_utc_datetime(&nth_sunday(y, 3, 2).and_hms_opt(7, 0, 0).unwrap());
    let end = Utc.from_utc_datetime(&nth_sunday(y, 11, 1).and_hms_opt(6, 0, 0).unwrap());
    if t >= start && t < end { -4 } else { -5 }
}
pub fn et_date(t: DateTime<Utc>) -> NaiveDate { (t + Duration::hours(et_offset_hours(t))).date_naive() }

// ---------------------------------------------------------------------------
// Samples: per-simulation outcome columns of one game
// ---------------------------------------------------------------------------

pub struct Samples {
    pub n: usize,
    /// [away, home, segment away, segment home] per simulated game
    pub game: Vec<[i16; 4]>,
    /// (subject, market) -> value per simulated game (VOID = player inactive)
    pub cols: HashMap<(String, String), Vec<i16>>,
    pub missing: Vec<(String, String)>,
    pub ms: u64,
}
impl Samples {
    pub fn from_rows(rows: Vec<Vec<i16>>, found: Vec<(String, String)>, missing: Vec<(String, String)>, ms: u64) -> Samples {
        let game = rows.iter().map(|r| [r[0], r[1], r[2], r[3]]).collect();
        let cols = found.into_iter().enumerate().map(|(j, k)| (k, rows.iter().map(|r| r[4 + j]).collect())).collect();
        Samples { n: rows.len(), game, cols, missing, ms }
    }
}

/// The value a game market settles on: home margin, total or a team's points.
fn game_actual(market: &str, g: &[i16; 4]) -> f64 {
    let (a, h) = if market.ends_with("_1h") { (g[2] as f64, g[3] as f64) } else { (g[0] as f64, g[1] as f64) };
    match market.trim_end_matches("_1h") {
        "moneyline" | "spread" => h - a,
        "team_total_home" => h,
        "team_total_away" => a,
        _ => h + a,
    }
}
fn state_of(market: &str, side: &str, line: Option<f64>, actual: f64) -> u8 {
    match picks::result_of(market, side, line, actual) { "win" => W, "loss" => L, _ => P }
}

/// Win / loss / push / void of one leg in every simulated game.
pub fn leg_states(s: &Samples, market: &str, subject: &str, side: &str, line: Option<f64>) -> Option<Vec<u8>> {
    if subject == "game" {
        return Some(s.game.iter().map(|g| state_of(market, side, line, game_actual(market, g))).collect());
    }
    let col = s.cols.get(&(subject.to_string(), market.to_string()))?;
    Some(col.iter().map(|&v| if v == VOID { V } else { state_of(market, side, line, v as f64) }).collect())
}

// ---------------------------------------------------------------------------
// Joint evaluation of the legs of one game (pure)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct ModeEval {
    pub p_win: f64,
    pub p_loss: f64,
    pub e_pay: f64,
    /// effective number of simulations after tilting
    pub ess: f64,
    /// per leg: P(win), P(loss) on the tilted simulations
    pub leg_w: Vec<f64>,
    pub leg_l: Vec<f64>,
    /// per pair (i, j): P(both win)
    pub pairs: Vec<(usize, usize, f64)>,
    pub converged: bool,
}

/// Distinct win/loss/push/void patterns of `k` legs and their counts.
pub fn patterns(states: &[&[u8]]) -> Vec<(u32, f64)> {
    let n = states.first().map(|s| s.len()).unwrap_or(0);
    let mut m: HashMap<u32, f64> = HashMap::new();
    for i in 0..n {
        let mut c = 0u32;
        for (j, s) in states.iter().enumerate() { c |= (s[i] as u32) << (2 * j); }
        *m.entry(c).or_default() += 1.0;
    }
    let mut v: Vec<(u32, f64)> = m.into_iter().collect();
    v.sort_by_key(|x| x.0);
    v
}
fn st(code: u32, j: usize) -> u8 { ((code >> (2 * j)) & 3) as u8 }

/// Re-weight the patterns so each leg's P(win) hits its target, keeping each
/// leg's push+void share: iterative proportional fitting on the three cells
/// (win / loss / push-or-void) of every leg. Returns the pattern weights (sum 1).
pub fn tilt(pats: &[(u32, f64)], k: usize, targets: &[Option<f64>]) -> (Vec<f64>, bool) {
    let n: f64 = pats.iter().map(|p| p.1).sum();
    let mut w: Vec<f64> = pats.iter().map(|p| p.1 / n.max(1.0)).collect();
    let mut converged = true;
    if targets.iter().all(|t| t.is_none()) { return (w, true); }
    for _ in 0..200 {
        let mut worst = 0.0f64;
        for j in 0..k {
            let Some(t) = targets[j] else { continue };
            let (mut qw, mut ql) = (0.0, 0.0);
            for (x, (c, _)) in w.iter().zip(pats) { match st(*c, j) { W => qw += x, L => ql += x, _ => {} } }
            let pv = (1.0 - qw - ql).max(0.0);
            if qw <= 0.0 || ql <= 0.0 { converged = false; continue; }
            let t = t.clamp(1e-6, (1.0 - pv - 1e-6).max(1e-6));
            worst = worst.max((qw - t).abs());
            let (aw, al) = (t / qw, (1.0 - pv - t) / ql);
            for (x, (c, _)) in w.iter_mut().zip(pats) { match st(*c, j) { W => *x *= aw, L => *x *= al, _ => {} } }
        }
        if worst < 1e-9 { break; }
    }
    (w, converged)
}

/// Joint statistics of one game's legs on (tilted) pattern weights.
pub fn mode_eval(pats: &[(u32, f64)], w: &[f64], k: usize, decs: &[f64], converged: bool) -> ModeEval {
    let (mut p_win, mut p_loss, mut e_pay, mut ss) = (0.0, 0.0, 0.0, 0.0);
    let mut leg_w = vec![0.0; k];
    let mut leg_l = vec![0.0; k];
    let mut pairs: Vec<(usize, usize, f64)> = Vec::new();
    for i in 0..k { for j in i + 1..k { pairs.push((i, j, 0.0)); } }
    for ((c, cnt), x) in pats.iter().zip(w) {
        ss += x * x / cnt.max(1.0);
        let mut all = true;
        let mut lost = false;
        let mut pay = 1.0;
        for j in 0..k {
            match st(*c, j) {
                W => { leg_w[j] += x; pay *= decs[j]; }
                L => { leg_l[j] += x; all = false; lost = true; pay = 0.0; }
                _ => { all = false; }
            }
        }
        if all { p_win += x; }
        if lost { p_loss += x; } else { e_pay += x * pay; }
        for pr in pairs.iter_mut() { if st(*c, pr.0) == W && st(*c, pr.1) == W { pr.2 += x; } }
    }
    ModeEval { p_win, p_loss, e_pay, ess: if ss > 0.0 { 1.0 / ss } else { 0.0 }, leg_w, leg_l, pairs, converged }
}

/// The four views of a game's legs: raw simulator, blend, market, halfway.
#[derive(Clone, Debug)]
pub struct GroupEval { pub raw: ModeEval, pub blend: ModeEval, pub market: ModeEval, pub half: ModeEval }

pub fn eval_group(states: &[&[u8]], t_blend: &[Option<f64>], t_market: &[Option<f64>], decs: &[f64]) -> GroupEval {
    let k = states.len();
    let pats = patterns(states);
    let run = |t: &[Option<f64>]| { let (w, ok) = tilt(&pats, k, t); mode_eval(&pats, &w, k, decs, ok) };
    let raw = run(&vec![None; k]);
    let half: Vec<Option<f64>> = t_blend.iter().zip(t_market).map(|(a, b)| match (a, b) { (Some(a), Some(b)) => Some((a + b) / 2.0), (a, _) => *a }).collect();
    GroupEval { raw, blend: run(t_blend), market: run(t_market), half: run(&half) }
}

// ---------------------------------------------------------------------------
// Slate: legs from verified quotes, priced against cached simulations
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Quote { pub book: String, pub price: i32, pub p_novig: Option<f64>, pub observed: DateTime<Utc>, pub one_sided: bool }

#[derive(Clone, Debug)]
pub struct Leg {
    pub id: String,
    pub game: String,
    pub market: String,
    pub subject: String,
    pub subject_name: Option<String>,
    pub team: Option<String>,
    pub side: String,
    pub line: Option<f64>,
    pub label: String,
    pub quotes: Vec<Quote>,
    pub p_market: Option<f64>,
    /// simulator: P(win | decided), push and void shares
    pub p_sim: Option<f64>,
    pub push: f64,
    pub void: f64,
    pub p_naive: Option<f64>,
    pub p_blend: Option<f64>,
    pub tier: &'static str,
    pub flags: Vec<String>,
    pub eligible: bool,
    pub states: Option<Arc<Vec<u8>>>,
}
impl Leg {
    pub fn kind(&self) -> &'static str { if self.subject == "game" { "game" } else { "prop" } }
    pub fn category(&self) -> &'static str { category(&self.market) }
    fn best(&self) -> Option<&Quote> { self.quotes.iter().max_by(|a, b| decimal(a.price).partial_cmp(&decimal(b.price)).unwrap()) }
    fn price_at(&self, book: &str) -> Option<i32> { self.quotes.iter().find(|q| q.book == book).map(|q| q.price) }
    /// unconditional P(win) target for the tilt
    fn target(&self, p: Option<f64>) -> Option<f64> { p.map(|p| p * (1.0 - self.push - self.void)) }
}

pub fn category(market: &str) -> &'static str {
    if !segments::is_game_market(market) { return "props"; }
    match market.trim_end_matches("_1h") { "moneyline" => "moneyline", "spread" => "spread", "total" => "total", _ => "team_total" }
}

fn leg_id(game: &str, market: &str, subject: &str, side: &str, line: Option<f64>) -> String {
    format!("{game}|{market}|{subject}|{side}|{}", line.map(|l| format!("{l}")).unwrap_or_default())
}

#[derive(Clone, Debug)]
pub struct SlateGame {
    pub id: String,
    pub home: String,
    pub away: String,
    pub starts_at: Option<DateTime<Utc>>,
    pub status: String,
    pub detail: Option<String>,
    pub sims: usize,
    pub sim_ms: u64,
    pub simulated_at: Option<DateTime<Utc>>,
    pub version: String,
    pub lineup: String,
    pub legs: usize,
    pub missing: usize,
}

pub struct Slate {
    pub sport: String,
    pub date: NaiveDate,
    pub games: Vec<SlateGame>,
    pub legs: Vec<Leg>,
    pub dep_weight: f64,
    pub model_version: String,
    pub max_age: i64,
    pub oldest_quote: Option<DateTime<Utc>>,
    pub newest_quote: Option<DateTime<Utc>>,
    /// per market: (legs, mean logit(model) - logit(market) over the slate's overs, applied?)
    pub bias: BTreeMap<String, (usize, f64, bool)>,
}

// --- simulation cache and background jobs ---

struct SimEntry { samples: Arc<Samples>, key: String, want: HashSet<(String, String)>, at: DateTime<Utc>, seed: u64 }
fn cache() -> &'static Mutex<HashMap<String, SimEntry>> { static C: OnceLock<Mutex<HashMap<String, SimEntry>>> = OnceLock::new(); C.get_or_init(|| Mutex::new(HashMap::new())) }
const CACHE_MAX: usize = 60;

#[derive(Clone, Default)]
pub struct Job { pub running: bool, pub cancel: bool, pub done: usize, pub total: usize, pub current: Option<String>, pub started: Option<DateTime<Utc>>, pub finished: Option<DateTime<Utc>>, pub errors: Vec<String> }
fn jobs() -> &'static Mutex<HashMap<String, Job>> { static J: OnceLock<Mutex<HashMap<String, Job>>> = OnceLock::new(); J.get_or_init(|| Mutex::new(HashMap::new())) }

#[derive(Clone)]
pub struct Need { pub game: String, pub version: String, pub cfg: Value, pub lineup: String, pub want: Vec<(String, String)> }

fn cache_key(sport: &str, game: &str) -> String { format!("{sport}:{game}") }

fn cached(sport: &str, n: &Need) -> Option<(Arc<Samples>, DateTime<Utc>, u64)> {
    let c = cache().lock().unwrap();
    let e = c.get(&cache_key(sport, &n.game))?;
    let fresh = Utc::now() - e.at < Duration::minutes(cache_minutes(sport));
    (fresh && e.key == format!("{}|{}", n.version, n.lineup) && n.want.iter().all(|w| e.want.contains(w) || e.samples.missing.contains(w)))
        .then(|| (e.samples.clone(), e.at, e.seed))
}

async fn simulate_game(state: &State, sport: &str, n: &Need) -> Result<(), String> {
    // keep what was already wanted so a new prop line does not evict older columns
    let mut want: HashSet<(String, String)> = n.want.iter().cloned().collect();
    if let Some(e) = cache().lock().unwrap().get(&cache_key(sport, &n.game)) { if e.key == format!("{}|{}", n.version, n.lineup) { want.extend(e.want.iter().cloned()); } }
    let mut want: Vec<(String, String)> = want.into_iter().collect();
    want.sort();
    let sims = sims_for(&state.db, sport).await;
    let seed = crate::mlb_sim::derive_seed(0x7061726c, &[sport, &n.game, &n.version, &n.lineup]);
    let s = match sport {
        "nba" => crate::nba_game::parlay_samples(state, &n.game, &n.lineup, &n.cfg, sims, seed, &want).await?,
        "nhl" => crate::nhl_game::parlay_samples(state, &n.game, &n.lineup, &n.cfg, sims, seed, &want).await?,
        "nfl" => crate::nfl_game::parlay_samples(state, &n.game, &n.lineup, &n.cfg, sims, seed, &want).await?,
        "mlb" => crate::mlb::parlay_samples(state, &n.game, &n.lineup, &n.cfg, sims, seed, &want).await?,
        _ => return Err(format!("unsupported sport {sport}")),
    };
    if s.n == 0 { return Err("the simulator returned no valid games".into()); }
    let mut c = cache().lock().unwrap();
    if c.len() >= CACHE_MAX {
        if let Some(old) = c.iter().min_by_key(|(_, e)| e.at).map(|(k, _)| k.clone()) { c.remove(&old); }
    }
    c.insert(cache_key(sport, &n.game), SimEntry { samples: Arc::new(s), key: format!("{}|{}", n.version, n.lineup), want: want.into_iter().collect(), at: Utc::now(), seed });
    Ok(())
}

/// Simulate every game that has no fresh cache entry, within `budget` (None =
/// all). Returns the games left over.
pub async fn ensure(state: &State, sport: &str, needs: &[Need], budget: Option<std::time::Duration>) -> (Vec<String>, Vec<(String, String)>) {
    let t0 = Instant::now();
    let (mut left, mut errors) = (Vec::new(), Vec::new());
    for n in needs {
        if cached(sport, n).is_some() { continue; }
        if budget.map(|b| t0.elapsed() > b).unwrap_or(false) || jobs().lock().unwrap().get(sport).map(|j| j.cancel).unwrap_or(false) { left.push(n.game.clone()); continue; }
        if let Some(j) = jobs().lock().unwrap().get_mut(sport) { j.current = Some(n.game.clone()); }
        if let Err(e) = simulate_game(state, sport, n).await { errors.push((n.game.clone(), e)); }
        if let Some(j) = jobs().lock().unwrap().get_mut(sport) { j.done += 1; }
    }
    (left, errors)
}

/// Finish a slate's simulations in the background (one job per sport).
fn spawn_job(state: web::Data<State>, sport: String, needs: Vec<Need>) {
    {
        let mut j = jobs().lock().unwrap();
        let e = j.entry(sport.clone()).or_default();
        if e.running { return; }
        *e = Job { running: true, total: needs.len(), started: Some(Utc::now()), ..Job::default() };
    }
    actix_web::rt::spawn(async move {
        let (_, errors) = ensure(&state, &sport, &needs, None).await;
        let mut j = jobs().lock().unwrap();
        if let Some(e) = j.get_mut(&sport) {
            e.running = false; e.cancel = false; e.current = None; e.finished = Some(Utc::now());
            e.errors = errors.into_iter().map(|(g, e)| format!("{g}: {e}")).collect();
        }
    });
}

// --- the slate ---

/// The parlay model in force: (version, dependence weight).
pub async fn model(db: &tokio_postgres::Client) -> (String, f64) {
    db.query_opt("SELECT version, dep_weight FROM parlay_models WHERE status='champion'", &[]).await.ok().flatten()
        .map(|r| (r.get(0), r.get(1))).unwrap_or_else(|| (format!("{ENGINE}+dep1.00"), 1.0))
}

struct Ledger { lineup: String, payload: Value }

fn team_of(payload: &Value, side: &str) -> String {
    match &payload[side] { Value::String(s) => s.clone(), v => v["abbr"].as_str().or(v["team"].as_str()).unwrap_or("").to_string() }
}

/// Build the slate for one sport and US-Eastern date: legs from the freshest
/// quote per book (`max_age` minutes), and the simulations they need.
pub async fn load(state: &State, sport: &str, date: NaiveDate, max_age: i64) -> Result<(Slate, Vec<Need>), String> {
    let db = &state.db;
    let rows = db.query("SELECT DISTINCT ON (book, game_id, market, subject, side) book, game_id, market, subject, subject_name, side, line, price, observed_at, starts_at \
        FROM market_lines WHERE sport=$1 AND game_id IS NOT NULL AND starts_at > now() + interval '5 minutes' AND observed_at > now() - make_interval(mins => $2::int) \
        ORDER BY book, game_id, market, subject, side, observed_at DESC", &[&sport, &(max_age as i32)]).await.map_err(|e| dberr(&e))?;
    type Side = (Option<f64>, i32, DateTime<Utc>);
    let mut sides: HashMap<(String, String, String, String, String), Side> = HashMap::new(); // (book, game, market, subject, side)
    let mut names: HashMap<String, String> = HashMap::new();
    let mut starts: HashMap<String, DateTime<Utc>> = HashMap::new();
    for r in &rows {
        let start: Option<DateTime<Utc>> = r.get(9);
        let Some(start) = start else { continue };
        if et_date(start) != date { continue; }
        let game: String = r.get(1);
        starts.insert(game.clone(), start);
        if let Some(n) = r.get::<_, Option<String>>(4) { names.insert(r.get(3), n); }
        sides.insert((r.get(0), game, r.get(2), r.get(3), r.get(5)), (r.get(6), r.get(7), r.get(8)));
    }
    // legs: (game, market, subject, side, line) -> quotes
    let mut quotes: BTreeMap<(String, String, String, String, String), (Option<f64>, Vec<Quote>)> = BTreeMap::new();
    for ((book, game, market, subject, side), (line, price, obs)) in &sides {
        let partner = sides.get(&(book.clone(), game.clone(), market.clone(), subject.clone(), picks::other(side).to_string()));
        let (p_novig, one_sided) = match partner {
            Some((ol, op, _)) => {
                let same = match segments::base_market(market) {
                    "moneyline" if segments::is_game_market(market) => true,
                    "spread" if segments::is_game_market(market) => matches!((line, ol), (Some(a), Some(b)) if (a + b).abs() < 1e-9),
                    _ => line == ol,
                };
                (same.then(|| novig(*price, *op)), false)
            }
            None if side == "over" && lines::yes_no_market(market) && *line == Some(0.5) => (Some(novig(*price, picks::complement_price(*price))), true),
            None => (None, true),
        };
        let lk = line.map(|l| format!("{l}")).unwrap_or_default();
        quotes.entry((game.clone(), market.clone(), subject.clone(), side.clone(), lk)).or_insert((*line, Vec::new())).1
            .push(Quote { book: book.clone(), price: *price, p_novig, observed: *obs, one_sided });
    }
    let games: Vec<String> = starts.keys().cloned().collect();

    // the champion engine and its latest frozen forecast per game (teams, players, lineup mode)
    let champ = db.query_opt("SELECT version, config FROM model_registry WHERE sport=$1 AND status='champion'", &[&sport]).await.map_err(|e| dberr(&e))?;
    let (version, cfg): (String, Value) = match champ { Some(r) => (r.get(0), r.get::<_, Json<Value>>(1).0), None => return Err(format!("no champion engine for {sport}")) };
    let led = db.query("SELECT DISTINCT ON (game_id) game_id, lineup_mode, payload FROM prediction_ledger WHERE sport=$1 AND game_id = ANY($2) \
        ORDER BY game_id, (model_version=$3) DESC, (lineup_mode='confirmed') DESC, id DESC", &[&sport, &games, &version]).await.map_err(|e| dberr(&e))?;
    let ledgers: HashMap<String, Ledger> = led.iter().map(|r| (r.get(0), Ledger { lineup: r.get(1), payload: r.get::<_, Json<Value>>(2).0 })).collect();

    // blend inputs: evidence weights, tiers and each prop player's history
    let champs = picks::champions(db).await?;
    let ev = picks::evidence(db, &champs).await?;
    let default_w = lines::setting(db, "blend_default_w", 0.2).await;
    let pids: Vec<String> = quotes.keys().filter(|k| k.2 != "game").map(|k| k.2.clone()).collect::<HashSet<_>>().into_iter().collect();
    let hist = picks::player_history(db, sport, &pids).await?;

    let mut needs = Vec::new();
    let mut slate_games = Vec::new();
    let mut want_by_game: HashMap<String, Vec<(String, String)>> = HashMap::new();
    for (g, m, s, _, _) in quotes.keys() { if s != "game" { want_by_game.entry(g.clone()).or_default().push((s.clone(), m.clone())); } }
    for g in &games {
        let lineup = ledgers.get(g).map(|l| l.lineup.clone()).unwrap_or_else(|| "projected".into());
        let lineup = if lineup == "actual" { "confirmed".to_string() } else { lineup };
        let mut want = want_by_game.remove(g).unwrap_or_default();
        want.sort(); want.dedup();
        let need = Need { game: g.clone(), version: version.clone(), cfg: cfg.clone(), lineup: lineup.clone(), want };
        let (home, away) = ledgers.get(g).map(|l| (team_of(&l.payload, "home"), team_of(&l.payload, "away"))).unwrap_or_default();
        slate_games.push(SlateGame { id: g.clone(), home, away, starts_at: starts.get(g).copied(), status: "pending".into(), detail: None, sims: 0, sim_ms: 0,
            simulated_at: None, version: version.clone(), lineup, legs: 0, missing: 0 });
        if ledgers.contains_key(g) { needs.push(need); }
    }
    slate_games.sort_by_key(|g| (g.starts_at, g.id.clone()));
    needs.sort_by_key(|n| starts.get(&n.game).copied());

    let (dep_version, dep_weight) = model(db).await;
    let mut legs = Vec::new();
    for ((game, market, subject, side, _), (line, qs)) in quotes {
        let Some(sg) = slate_games.iter().find(|x| x.id == game) else { continue };
        let ledger = ledgers.get(&game);
        let mut flags = Vec::new();
        let team = if subject == "game" {
            match (market.trim_end_matches("_1h"), side.as_str()) {
                ("moneyline" | "spread", "home") | ("team_total_home", _) => Some(sg.home.clone()),
                ("moneyline" | "spread", _) | ("team_total_away", _) => Some(sg.away.clone()),
                _ => None,
            }
        } else { ledger.and_then(|l| picks::player_entry(&l.payload, &subject)).map(|x| x.1) };
        let name = names.get(&subject).cloned().or_else(|| ledger.and_then(|l| picks::player_entry(&l.payload, &subject)).and_then(|x| x.0["name"].as_str().map(String::from)));
        let base = picks::pick_label(sport, &market, &side, line, &sg.home, &sg.away);
        let label = match &name { Some(n) if subject != "game" => format!("{n} {base}"), _ => base };
        let pm: Vec<f64> = qs.iter().filter_map(|q| q.p_novig).collect();
        let p_market = (!pm.is_empty()).then(|| pm.iter().sum::<f64>() / pm.len() as f64);
        if p_market.is_none() { flags.push("no two-way price to de-vig".to_string()); }
        if qs.iter().all(|q| q.one_sided) && p_market.is_some() { flags.push("yes-only price: market probability includes the vig".into()); }
        let p_naive = if subject != "game" {
            hist.get(&subject).and_then(|h| {
                let vals: Vec<f64> = h.iter().filter_map(|(m, _)| m.get(market.as_str()).copied()).collect();
                (vals.len() >= 5).then(|| blend::freq_over(&vals, line.unwrap_or(0.0))).map(|o| if side == "over" { o } else { 1.0 - o })
            })
        } else { None };
        let (tier, _) = picks::prop_tier(&ev, sport, &market);
        legs.push(Leg { id: leg_id(&game, &market, &subject, &side, line), game, market, subject, subject_name: name, team, side, line, label, quotes: qs,
            p_market, p_sim: None, push: 0.0, void: 0.0, p_naive, p_blend: None, tier, flags, eligible: false, states: None });
    }

    // attach cached simulations
    let mut oldest: Option<DateTime<Utc>> = None;
    let mut newest: Option<DateTime<Utc>> = None;
    for l in &legs { for q in &l.quotes { oldest = Some(oldest.map_or(q.observed, |o| o.min(q.observed))); newest = Some(newest.map_or(q.observed, |o| o.max(q.observed))); } }
    for sg in slate_games.iter_mut() {
        let Some(need) = needs.iter().find(|n| n.game == sg.id) else {
            sg.status = "excluded".into();
            sg.detail = Some("no frozen forecast yet (the learning loop freezes games within 36 hours of the start)".into());
            continue;
        };
        if let Some(e) = cache().lock().unwrap().get(&cache_key(sport, &sg.id)) { if e.key != format!("{}|{}", need.version, need.lineup) { sg.detail = Some("lineup or engine changed: re-simulating".into()); } }
        match cached(sport, need) {
            Some((s, at, _)) => { sg.status = "simulated".into(); sg.sims = s.n; sg.sim_ms = s.ms; sg.simulated_at = Some(at); sg.missing = s.missing.len(); }
            None => { sg.status = "pending".into(); }
        }
    }
    let samples: HashMap<String, Arc<Samples>> = needs.iter().filter_map(|n| cached(sport, n).map(|(s, _, _)| (n.game.clone(), s))).collect();
    for l in legs.iter_mut() {
        let Some(s) = samples.get(&l.game) else { l.flags.push("game not simulated yet".into()); continue };
        let Some(states) = leg_states(s, &l.market, &l.subject, &l.side, l.line) else { l.flags.push("player not in the simulated lineup".into()); continue };
        let n = states.len().max(1) as f64;
        let (w, lo, p) = states.iter().fold((0.0, 0.0, 0.0), |a, x| match *x { W => (a.0 + 1.0, a.1, a.2), L => (a.0, a.1 + 1.0, a.2), P => (a.0, a.1, a.2 + 1.0), _ => a });
        l.push = p / n;
        l.void = (n - w - lo - p) / n;
        l.p_sim = (w + lo > 0.0).then(|| w / (w + lo));
        if let Some(ps) = l.p_sim {
            let wts = blend::weights(ev.get(&(sport.to_string(), l.market.clone())).map(|v| v.as_slice()).unwrap_or(&[]), default_w);
            l.p_blend = Some(blend::apply(&wts, ps, l.p_naive, l.p_market.unwrap_or(ps)).clamp(0.001, 0.999));
        }
        l.states = Some(Arc::new(states));
    }
    // Market-anchored recentering: across one market's lines on a slate the
    // book is close to calibrated on average, so a model that leans the same way
    // on nearly every line is biased (minutes, early-season rates), not finding
    // value everywhere. Shift each market's blend (logit scale) so its average
    // over-probability matches the market's; each leg keeps its relative signal.
    let recenter = lines::setting(db, "parlay_recenter", 1.0).await > 0.5;
    let bias = recenter_markets(&mut legs, recenter);
    for l in legs.iter_mut() {
        if l.states.is_none() { continue; }
        if l.void > 0.5 { l.flags.push(format!("player inactive in {:.0}% of simulations", l.void * 100.0)); }
        if let (Some(pb), Some(pm)) = (l.p_blend, l.p_market) { if (pb - pm).abs() > 0.25 { l.flags.push("model and market disagree by more than 25 points: likely a data problem".into()); } }
        // the raw simulator this far from the market usually means thin or stale player data (rookies, trades, role changes)
        if let (Some(ps), Some(pm)) = (l.p_sim, l.p_market) { if (ps - pm).abs() > 0.30 { l.flags.push(format!("simulator {:.0}% vs market {:.0}%: likely thin or stale player data", ps * 100.0, pm * 100.0)); } }
        if let Some(pm) = l.p_market { if !(0.04..=0.96).contains(&pm) { l.flags.push("near-certain line".into()); } }
        if l.p_sim.map(|p| !(0.01..=0.99).contains(&p)).unwrap_or(true) { l.flags.push("simulator gives it no real chance either way".into()); }
        l.eligible = l.p_blend.is_some() && l.p_market.is_some() && l.void <= 0.5
            && !l.flags.iter().any(|f| f.starts_with("model and market") || f.starts_with("near-certain") || f.starts_with("simulator"));
    }
    for sg in slate_games.iter_mut() { sg.legs = legs.iter().filter(|l| l.game == sg.id).count(); }
    Ok((Slate { sport: sport.to_string(), date, games: slate_games, legs, dep_weight, model_version: dep_version, max_age, oldest_quote: oldest, newest_quote: newest, bias }, needs))
}

fn logit(p: f64) -> f64 { let p = p.clamp(1e-6, 1.0 - 1e-6); (p / (1.0 - p)).ln() }
fn sigm(x: f64) -> f64 { 1.0 / (1.0 + (-x).exp()) }

/// Per over/under market with at least 8 lines: the mean logit gap between the
/// blend and the market over the slate's overs; when `on`, every leg of that
/// market is shifted by it (overs down, unders up for a positive gap).
pub fn recenter_markets(legs: &mut [Leg], on: bool) -> BTreeMap<String, (usize, f64, bool)> {
    let mut acc: HashMap<String, (usize, f64)> = HashMap::new();
    for l in legs.iter() {
        if l.side != "over" { continue; }
        if let (Some(pb), Some(pm)) = (l.p_blend, l.p_market) { let e = acc.entry(l.market.clone()).or_default(); e.0 += 1; e.1 += logit(pb) - logit(pm); }
    }
    let bias: BTreeMap<String, (usize, f64, bool)> = acc.into_iter().map(|(m, (n, sum))| (m, (n, sum / n as f64, on && n >= 8))).collect();
    for l in legs.iter_mut() {
        let Some((n, b, apply)) = bias.get(&l.market).copied() else { continue };
        if !apply || l.p_blend.is_none() || !matches!(l.side.as_str(), "over" | "under") { continue; }
        let shift = if l.side == "over" { -b } else { b };
        l.p_blend = l.p_blend.map(|p| sigm(logit(p) + shift).clamp(0.001, 0.999));
        if b.abs() > 0.1 { l.flags.push(format!("recentred: the model leaned {} on {} of today's {} lines (mean {:+.2} logit)", if b < 0.0 { "under" } else { "over" }, n, picks::stat_label(&l.market), b)); }
    }
    bias
}

// ---------------------------------------------------------------------------
// Tickets
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Ticket {
    pub legs: Vec<usize>,
    pub book: Option<String>,
    pub decs: Vec<f64>,
    pub prices: Vec<i32>,
    pub price_decimal: f64,
    /// product of the single-leg prices (what a cross-game parlay pays)
    pub price_product: f64,
    /// same-game discount applied to the product (1 = none)
    pub sgp_adjust: f64,
    pub price_verified: bool,
    pub price_note: Option<String>,
    pub p_joint: f64,
    pub p_dep: f64,
    pub p_indep: f64,
    pub p_raw: f64,
    pub p_market: f64,
    pub p_market_indep: Option<f64>,
    pub p_half: f64,
    pub p_loss: f64,
    pub e_pay: f64,
    pub ev: f64,
    pub ev_cons: f64,
    pub mc_se: f64,
    pub ess: f64,
    pub same_game: bool,
    pub groups: Vec<(String, Vec<usize>, GroupEval)>,
    pub warnings: Vec<String>,
}
impl Ticket {
    pub fn fair_decimal(&self) -> Option<f64> { (self.p_joint > 0.0).then(|| 1.0 / self.p_joint) }
    pub fn lift(&self) -> f64 { if self.p_indep > 0.0 { self.p_joint / self.p_indep } else { 1.0 } }
    pub fn range(&self) -> (f64, f64) {
        let v = [self.p_joint, self.p_raw, self.p_market];
        (v.iter().cloned().fold(f64::MAX, f64::min), v.iter().cloned().fold(f64::MIN, f64::max))
    }
}

/// The book every leg can be placed at with the best combined price (None = no single book).
fn choose_book(legs: &[&Leg], only: Option<&HashSet<String>>, want: Option<&str>) -> Option<(String, Vec<i32>)> {
    let mut books: Option<HashSet<String>> = None;
    for l in legs {
        let b: HashSet<String> = l.quotes.iter().map(|q| q.book.clone()).collect();
        books = Some(match books { None => b, Some(x) => x.intersection(&b).cloned().collect() });
    }
    let mut books: Vec<String> = books?.into_iter().filter(|b| only.map(|o| o.contains(b)).unwrap_or(true)).collect();
    if let Some(w) = want { books.retain(|b| b == w); }
    books.sort();
    books.into_iter().map(|b| { let p: Vec<i32> = legs.iter().map(|l| l.price_at(&b).unwrap()).collect(); (b, p) })
        .max_by(|a, b| a.1.iter().map(|p| decimal(*p)).product::<f64>().partial_cmp(&b.1.iter().map(|p| decimal(*p)).product::<f64>()).unwrap())
}

type GroupCache = HashMap<(Vec<usize>, Vec<i32>), GroupEval>;

/// Evaluate a ticket. `cache` memoizes per-game evaluations.
pub fn evaluate(slate: &Slate, idx: &[usize], books: Option<&HashSet<String>>, want_book: Option<&str>, cache: &mut GroupCache) -> Result<Ticket, String> {
    let legs: Vec<&Leg> = idx.iter().map(|i| &slate.legs[*i]).collect();
    if legs.is_empty() { return Err("a ticket needs at least one leg".into()); }
    for l in &legs { if l.states.is_none() || l.p_blend.is_none() { return Err(format!("{}: {}", l.label, l.flags.first().cloned().unwrap_or_else(|| "cannot be priced".into()))); } }
    // conflicts: the same market on the same subject twice (either side or another line)
    let mut seen: HashSet<(&str, &str, &str)> = HashSet::new();
    for l in &legs { if !seen.insert((&l.game, &l.market, &l.subject)) { return Err(format!("duplicate market: {} appears twice (books take one selection per market)", l.label)); } }
    let mut warnings = Vec::new();
    let (book, prices, verified_book) = match choose_book(&legs, books, want_book) {
        Some((b, p)) => (Some(b), p, true),
        None => {
            warnings.push("no single book quotes every leg: price shown is each leg's best price, which cannot be placed as one ticket".into());
            (None, legs.iter().map(|l| l.best().map(|q| q.price).unwrap_or(-110)).collect(), false)
        }
    };
    let decs: Vec<f64> = prices.iter().map(|p| decimal(*p)).collect();
    // group legs by game
    let mut by_game: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (j, l) in legs.iter().enumerate() { by_game.entry(&l.game).or_default().push(j); }
    let same_game = by_game.values().any(|v| v.len() > 1);
    let mut groups = Vec::new();
    let (mut p_dep, mut p_raw, mut p_mkt, mut p_half, mut no_loss, mut e_dep, mut e_half) = (1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0);
    let (mut rel_var, mut ess_min) = (0.0, f64::MAX);
    // a book prices a same-game parlay for the legs' correlation; assume it knows
    // the dependence as well as the simulator does: at market marginals, the
    // correlated joint vs the independent product
    let mut sgp_adjust = 1.0;
    let (mut p_indep, mut e_indep, mut p_half_indep, mut e_half_indep) = (1.0, 1.0, 1.0, 1.0);
    for (game, js) in &by_game {
        let gl: Vec<usize> = js.iter().map(|j| idx[*j]).collect();
        let gp: Vec<i32> = js.iter().map(|j| prices[*j]).collect();
        let key = (gl.clone(), gp.clone());
        let ge = match cache.get(&key) {
            Some(g) => g.clone(),
            None => {
                let states: Vec<&[u8]> = gl.iter().map(|i| slate.legs[*i].states.as_ref().unwrap().as_slice()).collect();
                let tb: Vec<Option<f64>> = gl.iter().map(|i| { let l = &slate.legs[*i]; l.target(l.p_blend) }).collect();
                let tm: Vec<Option<f64>> = gl.iter().map(|i| { let l = &slate.legs[*i]; l.target(l.p_market.or(l.p_blend)) }).collect();
                let gd: Vec<f64> = gp.iter().map(|p| decimal(*p)).collect();
                let g = eval_group(&states, &tb, &tm, &gd);
                cache.insert(key, g.clone());
                g
            }
        };
        if gl.len() > 1 && ge.market.p_win > 0.0 {
            let ind: f64 = ge.market.leg_w.iter().product();
            sgp_adjust *= (ind / ge.market.p_win).min(1.0);
        }
        if !ge.blend.converged { warnings.push(format!("{game}: a leg never wins or never loses in the simulations; its marginal could not be matched")); }
        for (a, b, pij) in &ge.blend.pairs { if *pij < 1e-4 { warnings.push(format!("contradictory legs: {} and {} (almost never win together)", slate.legs[gl[*a]].label, slate.legs[gl[*b]].label)); } }
        p_dep *= ge.blend.p_win; p_raw *= ge.raw.p_win; p_mkt *= ge.market.p_win; p_half *= ge.half.p_win;
        no_loss *= 1.0 - ge.blend.p_loss; e_dep *= ge.blend.e_pay; e_half *= ge.half.e_pay;
        ess_min = ess_min.min(ge.blend.ess);
        if ge.blend.p_win > 0.0 { rel_var += (1.0 - ge.blend.p_win) / (ge.blend.p_win * ge.blend.ess.max(1.0)); }
        for (j, d) in js.iter().map(|j| decs[*j]).enumerate() {
            let (w, l) = (ge.blend.leg_w[j], ge.blend.leg_l[j]);
            p_indep *= w; e_indep *= w * d + (1.0 - w - l);
            let (wh, lh) = (ge.half.leg_w[j], ge.half.leg_l[j]);
            p_half_indep *= wh; e_half_indep *= wh * d + (1.0 - wh - lh);
        }
        groups.push((game.to_string(), gl, ge));
    }
    let a = slate.dep_weight;
    let p_joint = a * p_dep + (1.0 - a) * p_indep;
    let e_pay = a * e_dep + (1.0 - a) * e_indep;
    let p_h = a * p_half + (1.0 - a) * p_half_indep;
    let e_h = a * e_half + (1.0 - a) * e_half_indep;
    let price_product: f64 = decs.iter().product();
    // pushes and voids drop legs; the discount scales whatever the ticket pays
    let price_decimal = price_product * sgp_adjust;
    let (e_pay, e_h) = (e_pay * sgp_adjust, e_h * sgp_adjust);
    let mc_se = p_dep * rel_var.sqrt();
    let ev = e_pay - 1.0;
    let ev_cons = e_h - 1.0 - 2.0 * mc_se * price_decimal;
    let pm: Vec<f64> = legs.iter().filter_map(|l| l.p_market).collect();
    let p_market_indep = (pm.len() == legs.len()).then(|| pm.iter().product());
    let price_note = if !verified_book { Some("best price per leg; no single book offers all legs".to_string()) }
        else if same_game { Some(format!("estimated same-game price: the product of the leg prices ({}) discounted by the correlation a book prices in (x{:.2}, the simulated dependence at market probabilities). Actual SGP prices are not in any feed; check the book", fmt_dec(price_product), sgp_adjust)) }
        else { None };
    if p_joint < 1e-4 { warnings.push("the legs almost never all win together".into()); }
    Ok(Ticket { legs: idx.to_vec(), book, decs, prices, price_decimal, price_product, sgp_adjust, price_verified: verified_book && !same_game, price_note, p_joint, p_dep, p_indep, p_raw, p_market: p_mkt,
        p_market_indep, p_half: p_h, p_loss: 1.0 - no_loss, e_pay, ev, ev_cons, mc_se, ess: if ess_min == f64::MAX { 0.0 } else { ess_min }, same_game, groups, warnings })
}

// ---------------------------------------------------------------------------
// JSON
// ---------------------------------------------------------------------------

fn fmt_dec(d: f64) -> String { let a = american(d); if a > 0 { format!("+{a}") } else { a.to_string() } }
pub fn american(dec: f64) -> i64 { if dec >= 2.0 { ((dec - 1.0) * 100.0).round() as i64 } else if dec > 1.0 { (-100.0 / (dec - 1.0)).round() as i64 } else { 0 } }

pub fn leg_json(l: &Leg, sport: &str, slate: &Slate) -> Value {
    let g = slate.games.iter().find(|g| g.id == l.game);
    let mut qs: Vec<&Quote> = l.quotes.iter().collect();
    qs.sort_by(|a, b| decimal(b.price).partial_cmp(&decimal(a.price)).unwrap());
    json!({
        "id": l.id, "sport": sport, "game": l.game, "game_label": g.map(|g| format!("{} @ {}", g.away, g.home)), "starts_at": g.and_then(|g| g.starts_at),
        "market": l.market, "category": l.category(), "kind": l.kind(), "subject": l.subject, "subject_name": l.subject_name, "team": l.team,
        "side": l.side, "line": l.line, "label": l.label, "market_label": if l.subject == "game" { segments::market_label(sport, &l.market).to_string() } else { picks::stat_label(&l.market).to_string() },
        "best": l.best().map(|q| json!({ "book": q.book, "book_title": picks::book_title(&q.book), "price": q.price })),
        "quotes": qs.iter().map(|q| json!({ "book": q.book, "book_title": picks::book_title(&q.book), "price": q.price, "p_novig": q.p_novig.map(r4), "observed_at": q.observed, "one_sided": q.one_sided })).collect::<Vec<_>>(),
        "p_market": l.p_market.map(r4), "p_sim": l.p_sim.map(r4), "p_naive": l.p_naive.map(r4), "p_blend": l.p_blend.map(r4), "push": r4(l.push), "void": r4(l.void),
        "ev": match (l.p_blend, l.best()) { (Some(p), Some(q)) => Some(r4(p * (1.0 - l.push - l.void) * decimal(q.price) + l.push + l.void - 1.0)), _ => None },
        "tier": l.tier, "flags": l.flags, "eligible": l.eligible,
    })
}

pub fn ticket_json(t: &Ticket, slate: &Slate) -> Value {
    let legs: Vec<Value> = t.legs.iter().enumerate().map(|(j, i)| {
        let l = &slate.legs[*i];
        let (ge, pos) = t.groups.iter().find_map(|(_, gl, ge)| gl.iter().position(|x| x == i).map(|p| (ge, p))).unwrap();
        let mut v = leg_json(l, &slate.sport, slate);
        v["price"] = json!(t.prices[j]);
        v["decimal"] = json!(r4(t.decs[j]));
        v["p_win"] = json!(r4(ge.blend.leg_w[pos]));
        v["p_loss"] = json!(r4(ge.blend.leg_l[pos]));
        v
    }).collect();
    let mut pairs = Vec::new();
    let mut games = Vec::new();
    for (game, gl, ge) in &t.groups {
        let g = slate.games.iter().find(|x| x.id == *game);
        games.push(json!({ "game": game, "label": g.map(|g| format!("{} @ {}", g.away, g.home)), "legs": gl.len(), "p_win": r4(ge.blend.p_win), "p_win_raw": r4(ge.raw.p_win),
            "p_win_market": r4(ge.market.p_win), "p_indep": r4(ge.blend.leg_w.iter().product()), "ess": ge.blend.ess.round(), "sims": g.map(|g| g.sims) }));
        for (a, b, pij) in &ge.blend.pairs {
            let (pa, pb) = (ge.blend.leg_w[*a], ge.blend.leg_w[*b]);
            pairs.push(json!({ "a": slate.legs[gl[*a]].label, "b": slate.legs[gl[*b]].label, "a_id": slate.legs[gl[*a]].id, "b_id": slate.legs[gl[*b]].id,
                "p_a": r4(pa), "p_b": r4(pb), "p_both": r4(*pij), "p_indep": r4(pa * pb), "lift": if pa * pb > 0.0 { r4(pij / (pa * pb)) } else { 1.0 },
                "p_b_given_a": if pa > 0.0 { r4(pij / pa) } else { 0.0 } }));
        }
    }
    let (lo, hi) = t.range();
    let starts = t.legs.iter().filter_map(|i| slate.games.iter().find(|g| g.id == slate.legs[*i].game).and_then(|g| g.starts_at)).min();
    let quotes_at = t.legs.iter().enumerate().filter_map(|(j, i)| {
        let l = &slate.legs[*i];
        match &t.book { Some(b) => l.quotes.iter().find(|q| &q.book == b).map(|q| q.observed), None => l.quotes.iter().find(|q| q.price == t.prices[j]).map(|q| q.observed) }
    }).min();
    let mut cats: Vec<&str> = t.legs.iter().map(|i| slate.legs[*i].category()).collect();
    cats.sort(); cats.dedup();
    json!({
        "key": ticket_key(&slate.sport, t, slate), "sport": slate.sport, "date": slate.date, "n_legs": t.legs.len(), "categories": cats, "same_game": t.same_game,
        "starts_at": starts, "quotes_at": quotes_at,
        "book": t.book, "book_title": t.book.as_deref().map(picks::book_title), "price_decimal": r4(t.price_decimal), "price_american": american(t.price_decimal),
        "price_product": r4(t.price_product), "sgp_adjust": r4(t.sgp_adjust),
        "price_verified": t.price_verified, "price_note": t.price_note,
        "p_joint": r4(t.p_joint), "p_dep": r4(t.p_dep), "p_indep": r4(t.p_indep), "p_raw": r4(t.p_raw), "p_market": r4(t.p_market), "p_market_indep": t.p_market_indep.map(r4),
        "p_cons": r4(t.p_half), "p_loss": r4(t.p_loss), "lift": r4(t.lift()), "range": [r4(lo), r4(hi)],
        "fair_decimal": t.fair_decimal().map(r4), "fair_american": t.fair_decimal().map(american),
        "e_payout": r4(t.e_pay), "ev": r4(t.ev), "ev_cons": r4(t.ev_cons), "mc_se": r4(t.mc_se), "ess": t.ess.round(),
        "edge": r4(t.p_joint - 1.0 / t.price_decimal), "edge_rel": if t.price_decimal > 0.0 { r4(t.p_joint * t.price_decimal - 1.0) } else { 0.0 },
        "dep_weight": slate.dep_weight, "model_version": slate.model_version,
        "legs": legs, "games": games, "pairs": pairs, "warnings": t.warnings, "explain": explain(t, slate),
    })
}

/// FNV-1a 64: a stable id for a ticket (legs + book).
fn fnv(s: &str) -> u64 { s.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3)) }
pub fn ticket_key(sport: &str, t: &Ticket, slate: &Slate) -> String {
    let mut ids: Vec<&str> = t.legs.iter().map(|i| slate.legs[*i].id.as_str()).collect();
    ids.sort();
    format!("{:016x}", fnv(&format!("{sport}#{}#{}", ids.join(";"), t.book.as_deref().unwrap_or("mixed"))))
}

/// Plain-language reasons: the legs' edges and the strongest dependencies.
fn explain(t: &Ticket, slate: &Slate) -> Vec<String> {
    let mut out = Vec::new();
    for i in &t.legs {
        let l = &slate.legs[*i];
        if let (Some(pb), Some(pm)) = (l.p_blend, l.p_market) {
            let src = if l.subject == "game" { "engine + market blend" } else { "engine, recent form and market blend" };
            out.push(format!("{}: {:.1}% ({src}) vs {:.1}% de-vigged market{}", l.label, pb * 100.0, pm * 100.0,
                l.p_sim.map(|ps| format!("; simulator alone {:.1}%", ps * 100.0)).unwrap_or_default()));
        }
    }
    let mut pairs: Vec<(f64, String)> = Vec::new();
    for (_, gl, ge) in &t.groups {
        for (a, b, pij) in &ge.blend.pairs {
            let ind = ge.blend.leg_w[*a] * ge.blend.leg_w[*b];
            if ind > 0.0 { pairs.push((pij / ind, format!("{} and {}", slate.legs[gl[*a]].label, slate.legs[gl[*b]].label))); }
        }
    }
    pairs.sort_by(|a, b| (b.0 - 1.0).abs().partial_cmp(&(a.0 - 1.0).abs()).unwrap());
    for (lift, what) in pairs.iter().take(3) {
        if (lift - 1.0).abs() < 0.03 { continue; }
        out.push(format!("{what} win together {:.2}x as often as if independent ({})", lift, if *lift > 1.0 { "positive dependence: same game script" } else { "negative dependence: they compete for the same outcomes" }));
    }
    if t.groups.len() > 1 { out.push(format!("{} games: legs in different games are treated as independent (separate simulations)", t.groups.len())); }
    if t.same_game { out.push("same-game legs were graded on the same simulated games, so their joint probability includes their dependence".into()); }
    out
}

// ---------------------------------------------------------------------------
// Candidate generation
// ---------------------------------------------------------------------------

#[derive(Deserialize, Clone, Default)]
pub struct GenQ {
    pub sport: Option<String>,
    pub date: Option<NaiveDate>,
    /// moneyline | spread | total | team_total | props | mixed (any market)
    pub category: Option<String>,
    pub markets: Option<String>,
    pub props: Option<String>,
    pub legs: Option<usize>,
    /// ev | ev_cons | prob | odds_high | odds_low | uncertainty
    pub objective: Option<String>,
    pub books: Option<String>,
    pub games: Option<String>,
    pub exclude_games: Option<String>,
    pub teams: Option<String>,
    pub exclude_teams: Option<String>,
    pub players: Option<String>,
    pub exclude_players: Option<String>,
    pub exclude_legs: Option<String>,
    pub locked: Option<String>,
    pub side: Option<String>,
    pub min_odds: Option<f64>,
    pub max_odds: Option<f64>,
    pub min_prob: Option<f64>,
    pub max_prob: Option<f64>,
    pub min_leg_prob: Option<f64>,
    pub min_ev: Option<f64>,
    pub min_ev_cons: Option<f64>,
    /// any | same | cross
    pub scope: Option<String>,
    pub max_per_game: Option<usize>,
    pub max_per_player: Option<usize>,
    pub max_per_team: Option<usize>,
    pub min_lift: Option<f64>,
    pub max_lift: Option<f64>,
    pub max_range: Option<f64>,
    /// proven | baseline | any (model confidence of each leg's market)
    pub min_tier: Option<String>,
    pub verified_only: Option<bool>,
    pub include_flagged: Option<bool>,
    pub max_age: Option<i64>,
    pub limit: Option<usize>,
    pub pool: Option<usize>,
    pub beam: Option<usize>,
}

fn list(s: &Option<String>) -> Vec<String> { s.as_deref().unwrap_or("").split(',').map(|x| x.trim().to_lowercase()).filter(|x| !x.is_empty()).collect() }

fn objective_score(obj: &str, t: &Ticket) -> f64 {
    match obj {
        "prob" => t.p_joint,
        "odds_high" => t.price_decimal,
        "odds_low" => -t.price_decimal,
        "uncertainty" => { let (lo, hi) = t.range(); -((hi - lo) / t.p_joint.max(1e-9) + t.mc_se / t.p_joint.max(1e-9)) }
        "ev" => t.ev,
        _ => t.ev_cons,
    }
}
fn leg_proxy(obj: &str, l: &Leg) -> f64 {
    let (Some(pb), Some(q)) = (l.p_blend, l.best()) else { return f64::MIN };
    let d = decimal(q.price);
    let pw = pb * (1.0 - l.push - l.void);
    let ev = pw * d + l.push + l.void - 1.0;
    match obj {
        "prob" => pb,
        "odds_high" => d,
        "odds_low" => -d,
        "uncertainty" => -(l.p_sim.unwrap_or(pb) - l.p_market.unwrap_or(pb)).abs(),
        "ev" => ev,
        _ => { let pm = l.p_market.unwrap_or(pb); ((pb + pm) / 2.0) * (1.0 - l.push - l.void) * d + l.push + l.void - 1.0 }
    }
}

fn matches_any(hay: &[String], l: &Leg) -> bool {
    let name = l.subject_name.clone().unwrap_or_default().to_lowercase();
    hay.iter().any(|h| *h == l.subject.to_lowercase() || (!name.is_empty() && name.contains(h.as_str())))
}

/// Legs the query allows (before pooling).
fn leg_filter(q: &GenQ, l: &Leg) -> bool {
    let cat = q.category.as_deref().unwrap_or("mixed");
    if cat != "mixed" && cat != "any" && l.category() != cat { return false; }
    let markets = list(&q.markets);
    if !markets.is_empty() && !markets.contains(&l.market.to_lowercase()) { return false; }
    let props = list(&q.props);
    if !props.is_empty() && l.kind() == "prop" && !props.contains(&l.market.to_lowercase()) { return false; }
    let games = list(&q.games);
    if !games.is_empty() && !games.contains(&l.game.to_lowercase()) { return false; }
    if list(&q.exclude_games).contains(&l.game.to_lowercase()) { return false; }
    let team = l.team.clone().unwrap_or_default().to_lowercase();
    let teams = list(&q.teams);
    if !teams.is_empty() && !teams.contains(&team) { return false; }
    if !team.is_empty() && list(&q.exclude_teams).contains(&team) { return false; }
    let players = list(&q.players);
    if !players.is_empty() && (l.kind() != "prop" || !matches_any(&players, l)) { return false; }
    if l.kind() == "prop" && matches_any(&list(&q.exclude_players), l) { return false; }
    if list(&q.exclude_legs).contains(&l.id.to_lowercase()) { return false; }
    if let Some(s) = q.side.as_deref() { if s != "any" && s != l.side { return false; } }
    if let Some(m) = q.min_leg_prob { if l.p_blend.unwrap_or(0.0) < m { return false; } }
    let books: Vec<String> = list(&q.books);
    if !books.is_empty() && !l.quotes.iter().any(|x| books.contains(&x.book.to_lowercase())) { return false; }
    match q.min_tier.as_deref() {
        Some("proven") if l.tier != "proven" => return false,
        Some("baseline") if !matches!(l.tier, "proven" | "baseline") => return false,
        _ => {}
    }
    l.eligible || (q.include_flagged.unwrap_or(false) && l.p_blend.is_some())
}

/// Whether adding `j` to the partial ticket keeps it within the exposure and scope limits.
fn can_add(q: &GenQ, slate: &Slate, t: &[usize], j: usize) -> bool {
    if t.contains(&j) { return false; }
    let l = &slate.legs[j];
    let mut per_game = 1;
    let mut per_player = 1;
    let mut per_team = 1;
    for &i in t {
        let o = &slate.legs[i];
        if o.game == l.game {
            if o.market == l.market && o.subject == l.subject { return false; }
            per_game += 1;
        }
        if l.kind() == "prop" && o.subject == l.subject { per_player += 1; }
        if l.team.is_some() && o.team == l.team { per_team += 1; }
    }
    match q.scope.as_deref() {
        Some("same") if t.first().map(|i| slate.legs[*i].game != l.game).unwrap_or(false) => return false,
        Some("cross") if per_game > 1 => return false,
        _ => {}
    }
    per_game <= q.max_per_game.unwrap_or(usize::MAX) && per_player <= q.max_per_player.unwrap_or(2) && per_team <= q.max_per_team.unwrap_or(usize::MAX)
}

fn ticket_filter(q: &GenQ, t: &Ticket) -> bool {
    if let Some(m) = q.min_odds { if t.price_decimal < m { return false; } }
    if let Some(m) = q.max_odds { if t.price_decimal > m { return false; } }
    if let Some(m) = q.min_prob { if t.p_joint < m { return false; } }
    if let Some(m) = q.max_prob { if t.p_joint > m { return false; } }
    if let Some(m) = q.min_ev { if t.ev < m { return false; } }
    if let Some(m) = q.min_ev_cons { if t.ev_cons < m { return false; } }
    if let Some(m) = q.min_lift { if t.lift() < m { return false; } }
    if let Some(m) = q.max_lift { if t.lift() > m { return false; } }
    if let Some(m) = q.max_range { let (lo, hi) = t.range(); if hi - lo > m { return false; } }
    if q.verified_only.unwrap_or(false) && !t.price_verified { return false; }
    if t.warnings.iter().any(|w| w.starts_with("contradictory") || w.starts_with("the legs almost never")) { return false; }
    true
}

pub struct Generated { pub tickets: Vec<Ticket>, pub pool: usize, pub eligible: usize, pub evaluated: usize, pub ms: u64, pub notes: Vec<String> }

/// Beam search for the best `legs`-leg tickets under the objective.
pub fn generate(slate: &Slate, q: &GenQ) -> Result<Generated, String> {
    let t0 = Instant::now();
    let k = q.legs.unwrap_or(2).clamp(1, 10);
    let obj = q.objective.as_deref().unwrap_or("ev_cons");
    let books: HashSet<String> = list(&q.books).into_iter().collect();
    let books = (!books.is_empty()).then_some(books);
    let mut notes = Vec::new();
    // locked legs are kept whatever the filters say
    let locked: Vec<usize> = list(&q.locked).iter().filter_map(|id| slate.legs.iter().position(|l| l.id.to_lowercase() == *id)).collect();
    if locked.len() != list(&q.locked).len() { notes.push("a locked leg is no longer offered at the requested line and was not substituted".into()); }
    if locked.len() > k { return Err(format!("{} legs are locked but the ticket has {k}", locked.len())); }
    let mut cand: Vec<usize> = (0..slate.legs.len()).filter(|i| !locked.contains(i) && leg_filter(q, &slate.legs[*i])).collect();
    let eligible = cand.len();
    cand.sort_by(|a, b| leg_proxy(obj, &slate.legs[*b]).partial_cmp(&leg_proxy(obj, &slate.legs[*a])).unwrap());
    let pool_n = q.pool.unwrap_or(40).clamp(5, 150);
    // keep the pool diverse: at most pool/4 legs from one game unless only one game
    let mut pool: Vec<usize> = Vec::new();
    let mut per_game: HashMap<&str, usize> = HashMap::new();
    let cap = if q.scope.as_deref() == Some("same") { pool_n } else { (pool_n / 4).max(6) };
    for i in &cand {
        let g = slate.legs[*i].game.as_str();
        let c = per_game.entry(g).or_default();
        if *c >= cap { continue; }
        *c += 1;
        pool.push(*i);
        if pool.len() >= pool_n { break; }
    }
    let beam_w = q.beam.unwrap_or(150).clamp(10, 1000);
    let mut gc: GroupCache = HashMap::new();
    let mut evaluated = 0usize;
    let mut beam: Vec<(Vec<usize>, Option<Ticket>)> = vec![(locked.clone(), None)];
    if !locked.is_empty() {
        let t = evaluate(slate, &locked, books.as_ref(), None, &mut gc)?;
        evaluated += 1;
        beam = vec![(locked.clone(), Some(t))];
    }
    for _ in locked.len()..k {
        let mut next: Vec<(Vec<usize>, Ticket, f64)> = Vec::new();
        let mut seen: HashSet<Vec<usize>> = HashSet::new();
        for (t, _) in &beam {
            for &j in &pool {
                if !can_add(q, slate, t, j) { continue; }
                let mut t2 = t.clone();
                t2.push(j);
                let mut key = t2.clone();
                key.sort();
                if !seen.insert(key) { continue; }
                let Ok(tk) = evaluate(slate, &t2, books.as_ref(), None, &mut gc) else { continue };
                evaluated += 1;
                if tk.book.is_none() { continue; } // not placeable as one ticket
                let s = objective_score(obj, &tk);
                next.push((t2, tk, s));
            }
        }
        next.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap());
        next.truncate(beam_w);
        beam = next.into_iter().map(|(t, tk, _)| (t, Some(tk))).collect();
        if beam.is_empty() { break; }
    }
    let mut out: Vec<Ticket> = beam.into_iter().filter_map(|(_, t)| t).filter(|t| t.legs.len() == k && ticket_filter(q, t)).collect();
    out.sort_by(|a, b| objective_score(obj, b).partial_cmp(&objective_score(obj, a)).unwrap());
    out.truncate(q.limit.unwrap_or(25).clamp(1, 100));
    if eligible > pool.len() { notes.push(format!("{} eligible legs; the best {} by the objective's single-leg score were combined", eligible, pool.len())); }
    Ok(Generated { tickets: out, pool: pool.len(), eligible, evaluated, ms: t0.elapsed().as_millis() as u64, notes })
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

fn sport_of(s: &Option<String>) -> Result<String, HttpResponse> {
    let s = s.clone().unwrap_or_else(|| "nba".into()).to_lowercase();
    if SPORTS.contains(&s.as_str()) { Ok(s) } else { Err(bad(format!("sport must be one of {}", SPORTS.join(", ")))) }
}
fn today() -> NaiveDate { et_date(Utc::now()) }

/// Load the slate, simulate what fits in `budget`, finish the rest in the background.
async fn ready_slate(state: &web::Data<State>, sport: &str, date: NaiveDate, max_age: i64, budget_secs: u64) -> Result<(Slate, Vec<String>), String> {
    let (_, needs) = load(state, sport, date, max_age).await?;
    let (left, errors) = ensure(state, sport, &needs, Some(std::time::Duration::from_secs(budget_secs))).await;
    if !left.is_empty() { spawn_job(state.clone(), sport.to_string(), needs.into_iter().filter(|n| left.contains(&n.game)).collect()); }
    let (mut slate, _) = load(state, sport, date, max_age).await?;
    for (g, e) in errors { if let Some(sg) = slate.games.iter_mut().find(|x| x.id == g) { sg.status = "error".into(); sg.detail = Some(e); } }
    if let Some(j) = jobs().lock().unwrap().get(sport) {
        for sg in slate.games.iter_mut() {
            if sg.status == "error" { continue; }
            if let Some(e) = j.errors.iter().find(|e| e.starts_with(&format!("{}:", sg.id))) { if sg.status != "simulated" { sg.status = "error".into(); sg.detail = Some(e.clone()); } }
        }
    }
    Ok((slate, left))
}

fn coverage(slate: &Slate) -> Value {
    let by = |s: &str| slate.games.iter().filter(|g| g.status == s).count();
    let mut cats: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for l in &slate.legs { let e = cats.entry(l.category()).or_default(); e.0 += 1; if l.eligible { e.1 += 1; } }
    json!({
        "games": slate.games.len(), "simulated": by("simulated"), "pending": by("pending"), "excluded": by("excluded"), "errors": by("error"),
        "legs": slate.legs.len(), "eligible": slate.legs.iter().filter(|l| l.eligible).count(),
        "by_category": cats.iter().map(|(k, v)| (k.to_string(), json!({ "legs": v.0, "eligible": v.1 }))).collect::<serde_json::Map<_, _>>(),
        "oldest_quote": slate.oldest_quote, "newest_quote": slate.newest_quote, "max_age_minutes": slate.max_age,
        "model_bias": slate.bias.iter().map(|(m, (n, b, on))| json!({ "market": m, "label": if segments::is_game_market(m) { segments::market_label(&slate.sport, m) } else { picks::stat_label(m).to_string() },
            "lines": n, "mean_logit": r4(*b), "recentred": on })).collect::<Vec<_>>(),
    })
}

fn games_json(slate: &Slate) -> Vec<Value> {
    slate.games.iter().map(|g| json!({ "id": g.id, "label": format!("{} @ {}", g.away, g.home), "home": g.home, "away": g.away, "starts_at": g.starts_at,
        "status": g.status, "detail": g.detail, "sims": g.sims, "sim_ms": g.sim_ms, "simulated_at": g.simulated_at, "engine": g.version, "lineup": g.lineup,
        "legs": g.legs, "eligible": slate.legs.iter().filter(|l| l.game == g.id && l.eligible).count(), "missing_columns": g.missing })).collect()
}

fn job_json(sport: &str) -> Value {
    let j = jobs().lock().unwrap().get(sport).cloned().unwrap_or_default();
    json!({ "running": j.running, "done": j.done, "total": j.total, "current": j.current, "started": j.started, "finished": j.finished, "errors": j.errors })
}

#[derive(Deserialize)]
pub struct SlateQ { sport: Option<String>, date: Option<NaiveDate>, max_age: Option<i64> }

pub async fn slate_http(state: web::Data<State>, q: web::Query<SlateQ>) -> impl Responder {
    let sport = match sport_of(&q.sport) { Ok(s) => s, Err(r) => return r };
    let date = q.date.unwrap_or_else(today);
    match ready_slate(&state, &sport, date, q.max_age.unwrap_or(180).clamp(5, 1440), 20).await {
        Ok((slate, left)) => {
            let mut markets: BTreeMap<String, (String, &str, usize)> = BTreeMap::new();
            for l in &slate.legs {
                let lab = if l.subject == "game" { segments::market_label(&sport, &l.market).to_string() } else { picks::stat_label(&l.market).to_string() };
                markets.entry(l.market.clone()).or_insert((lab, l.category(), 0)).2 += 1;
            }
            let mut books: BTreeMap<String, usize> = BTreeMap::new();
            for l in &slate.legs { for q in &l.quotes { *books.entry(q.book.clone()).or_default() += 1; } }
            HttpResponse::Ok().json(json!({
                "sport": sport, "date": date, "timezone": "America/New_York", "utc_offset": et_offset_hours(Utc::now()), "generated_at": Utc::now(),
                "model_version": slate.model_version, "dep_weight": slate.dep_weight, "coverage": coverage(&slate), "games": games_json(&slate), "pending": left, "job": job_json(&sport),
                "markets": markets.iter().map(|(k, v)| json!({ "key": k, "label": v.0, "category": v.1, "legs": v.2 })).collect::<Vec<_>>(),
                "books": books.iter().map(|(b, n)| json!({ "key": b, "title": picks::book_title(b), "legs": n })).collect::<Vec<_>>(),
                "live": false, "note": "pre-game markets only: legs drop out five minutes before their game starts",
            }))
        }
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
pub struct LegsQ { sport: Option<String>, date: Option<NaiveDate>, game: Option<String>, q: Option<String>, market: Option<String>, category: Option<String>, max_age: Option<i64>, limit: Option<usize> }

pub async fn legs_http(state: web::Data<State>, q: web::Query<LegsQ>) -> impl Responder {
    let sport = match sport_of(&q.sport) { Ok(s) => s, Err(r) => return r };
    let date = q.date.unwrap_or_else(today);
    let (slate, _) = match load(&state, &sport, date, q.max_age.unwrap_or(180).clamp(5, 1440)).await { Ok(x) => x, Err(e) => return internal(e) };
    let needle = q.q.clone().unwrap_or_default().to_lowercase();
    let mut legs: Vec<&Leg> = slate.legs.iter().filter(|l| q.game.as_deref().map(|g| g == l.game).unwrap_or(true)
        && q.market.as_deref().map(|m| m == l.market).unwrap_or(true)
        && q.category.as_deref().map(|c| c == "mixed" || c == l.category()).unwrap_or(true)
        && (needle.is_empty() || l.label.to_lowercase().contains(&needle) || l.team.clone().unwrap_or_default().to_lowercase().contains(&needle))).collect();
    legs.sort_by(|a, b| (a.game.as_str(), a.kind(), a.subject_name.as_deref().unwrap_or(""), a.market.as_str(), a.line.map(|x| (x * 10.0) as i64), a.side.as_str())
        .cmp(&(b.game.as_str(), b.kind(), b.subject_name.as_deref().unwrap_or(""), b.market.as_str(), b.line.map(|x| (x * 10.0) as i64), b.side.as_str())));
    let total = legs.len();
    legs.truncate(q.limit.unwrap_or(2000).clamp(1, 5000));
    HttpResponse::Ok().json(json!({ "sport": sport, "date": date, "total": total, "legs": legs.iter().map(|l| leg_json(l, &sport, &slate)).collect::<Vec<_>>() }))
}

pub async fn generate_http(state: web::Data<State>, q: web::Query<GenQ>) -> impl Responder {
    let sport = match sport_of(&q.sport) { Ok(s) => s, Err(r) => return r };
    let date = q.date.unwrap_or_else(today);
    let (slate, left) = match ready_slate(&state, &sport, date, q.max_age.unwrap_or(180).clamp(5, 1440), 20).await { Ok(x) => x, Err(e) => return internal(e) };
    let q2 = q.into_inner();
    let res = web::block(move || { let g = generate(&slate, &q2); (slate, g, q2) }).await;
    let (slate, g, q2) = match res { Ok(x) => x, Err(e) => return internal(e) };
    match g {
        Ok(g) => HttpResponse::Ok().json(json!({
            "sport": sport, "date": date, "timezone": "America/New_York", "generated_at": Utc::now(), "legs": q2.legs.unwrap_or(2), "objective": q2.objective.clone().unwrap_or_else(|| "ev_cons".into()),
            "category": q2.category.clone().unwrap_or_else(|| "mixed".into()), "coverage": coverage(&slate), "games": games_json(&slate), "pending": left, "job": job_json(&sport),
            "model_version": slate.model_version, "dep_weight": slate.dep_weight,
            "stats": { "eligible_legs": g.eligible, "pool": g.pool, "evaluated": g.evaluated, "ms": g.ms }, "notes": g.notes,
            "tickets": g.tickets.iter().map(|t| ticket_json(t, &slate)).collect::<Vec<_>>(),
        })),
        Err(e) => bad(e),
    }
}

#[derive(Deserialize)]
pub struct EvalBody { pub sport: Option<String>, pub date: Option<NaiveDate>, pub legs: Vec<String>, pub book: Option<String>, pub max_age: Option<i64> }

/// Evaluate a custom ticket; legs that are no longer offered are reported, never substituted.
pub async fn evaluate_http(state: web::Data<State>, body: web::Json<EvalBody>) -> impl Responder {
    let sport = match sport_of(&body.sport) { Ok(s) => s, Err(r) => return r };
    let date = body.date.unwrap_or_else(today);
    let (slate, left) = match ready_slate(&state, &sport, date, body.max_age.unwrap_or(180).clamp(5, 1440), 25).await { Ok(x) => x, Err(e) => return internal(e) };
    let mut idx = Vec::new();
    let mut missing = Vec::new();
    for id in &body.legs { match slate.legs.iter().position(|l| &l.id == id) { Some(i) => idx.push(i), None => missing.push(id.clone()) } }
    let mut problems: Vec<String> = missing.iter().map(|m| format!("not offered now (line moved, market pulled or game started): {m}")).collect();
    let mut gc = HashMap::new();
    let t = if idx.is_empty() { Err("no leg of this ticket is currently offered".to_string()) } else { evaluate(&slate, &idx, None, body.book.as_deref(), &mut gc) };
    // a requested book that lacks a leg: fall back to the best single book, and say so
    let t = match (t, body.book.as_deref()) {
        (Err(_), Some(b)) if !idx.is_empty() => { problems.push(format!("{} does not quote every leg; showing the best book that does", picks::book_title(b))); evaluate(&slate, &idx, None, None, &mut gc) }
        (Ok(t), Some(b)) if t.book.as_deref() != Some(b) => { problems.push(format!("{} does not quote every leg; showing the best book that does", picks::book_title(b))); evaluate(&slate, &idx, None, None, &mut gc) }
        (t, _) => t,
    };
    // books that quote every leg, with their combined price
    let legs: Vec<&Leg> = idx.iter().map(|i| &slate.legs[*i]).collect();
    let mut all_books: Vec<(String, f64)> = Vec::new();
    if !legs.is_empty() {
        let mut bs: HashSet<String> = legs[0].quotes.iter().map(|q| q.book.clone()).collect();
        for l in &legs[1..] { let b: HashSet<String> = l.quotes.iter().map(|q| q.book.clone()).collect(); bs = bs.intersection(&b).cloned().collect(); }
        for b in bs { all_books.push((b.clone(), legs.iter().map(|l| decimal(l.price_at(&b).unwrap())).product())); }
        all_books.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    }
    let books_json: Vec<Value> = all_books.iter().map(|(b, d)| json!({ "book": b, "book_title": picks::book_title(b), "price_decimal": r4(*d), "price_american": american(*d) })).collect();
    match t {
        Ok(t) => HttpResponse::Ok().json(json!({ "ok": true, "problems": problems, "pending": left, "books": books_json, "ticket": ticket_json(&t, &slate) })),
        Err(e) => { problems.push(e); HttpResponse::Ok().json(json!({ "ok": false, "problems": problems, "pending": left, "books": books_json, "ticket": Value::Null })) }
    }
}

#[derive(Deserialize)]
pub struct SportQ { pub sport: Option<String> }

pub async fn status_http(q: web::Query<SportQ>) -> impl Responder {
    let sports: Vec<String> = match &q.sport { Some(s) => vec![s.to_lowercase()], None => SPORTS.iter().map(|s| s.to_string()).collect() };
    let c = cache().lock().unwrap();
    let out: Vec<Value> = sports.iter().map(|s| {
        let games: Vec<Value> = c.iter().filter(|(k, _)| k.starts_with(&format!("{s}:"))).map(|(k, e)| json!({ "game": k.split_once(':').map(|x| x.1), "sims": e.samples.n,
            "ms": e.samples.ms, "at": e.at, "engine": e.key, "columns": e.samples.cols.len(), "missing": e.samples.missing.len(), "seed": e.seed })).collect();
        json!({ "sport": s, "cached_games": games, "job": job_json(s) })
    }).collect();
    HttpResponse::Ok().json(json!({ "engine": ENGINE, "cache_limit": CACHE_MAX, "sports": out }))
}

pub async fn cancel_http(q: web::Query<SportQ>) -> impl Responder {
    let s = q.sport.clone().unwrap_or_default().to_lowercase();
    let mut j = jobs().lock().unwrap();
    match j.get_mut(&s) { Some(e) if e.running => { e.cancel = true; HttpResponse::Ok().json(json!({ "cancelled": true })) } _ => HttpResponse::Ok().json(json!({ "cancelled": false, "note": "no job running" })) }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn bern(n: usize, p: f64, seed: u64) -> Vec<u8> {
        let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (0..n).map(|_| { x ^= x << 13; x ^= x >> 7; x ^= x << 17; if ((x >> 11) as f64 / (1u64 << 53) as f64) < p { W } else { L } }).collect()
    }

    #[test]
    fn eastern_dates_follow_daylight_saving() {
        let t = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc);
        assert_eq!(et_offset_hours(t("2026-10-28T02:00:00Z")), -4);
        assert_eq!(et_date(t("2026-10-28T02:00:00Z")), NaiveDate::from_ymd_opt(2026, 10, 27).unwrap(), "10 pm EDT is still the 27th");
        assert_eq!(et_offset_hours(t("2026-11-02T12:00:00Z")), -5);
        assert_eq!(et_offset_hours(t("2026-03-08T06:59:00Z")), -5);
        assert_eq!(et_offset_hours(t("2026-03-08T07:00:00Z")), -4);
    }

    #[test]
    fn game_markets_settle_on_the_right_columns() {
        let g = [100i16, 108, 50, 49]; // away 100, home 108; 1st half away 50, home 49
        assert_eq!(state_of("moneyline", "home", None, game_actual("moneyline", &g)), W);
        assert_eq!(state_of("moneyline_1h", "home", None, game_actual("moneyline_1h", &g)), L);
        assert_eq!(state_of("spread", "home", Some(-8.0), game_actual("spread", &g)), P);
        assert_eq!(state_of("spread", "away", Some(8.5), game_actual("spread", &g)), W);
        assert_eq!(state_of("total", "over", Some(207.5), game_actual("total", &g)), W);
        assert_eq!(state_of("team_total_away", "under", Some(100.5), game_actual("team_total_away", &g)), W);
        assert_eq!(state_of("team_total_home_1h", "over", Some(49.5), game_actual("team_total_home_1h", &g)), L);
        let tie = [20i16, 20, 10, 10];
        assert_eq!(state_of("moneyline", "away", None, game_actual("moneyline", &tie)), P, "a tie refunds a two-way moneyline");
    }

    #[test]
    fn independent_legs_multiply_and_dependent_ones_do_not() {
        let n = 200_000;
        let a = bern(n, 0.5, 1);
        let b = bern(n, 0.5, 2);
        let g = eval_group(&[&a, &b], &[None, None], &[None, None], &[2.0, 2.0]);
        assert!((g.raw.p_win - 0.25).abs() < 0.005, "independent: {}", g.raw.p_win);
        // b = a: perfectly positively dependent
        let g = eval_group(&[&a, &a], &[None, None], &[None, None], &[2.0, 2.0]);
        assert!((g.raw.p_win - 0.5).abs() < 0.005, "same outcome twice: {}", g.raw.p_win);
        // complement: never both
        let c: Vec<u8> = a.iter().map(|x| if *x == W { L } else { W }).collect();
        let g = eval_group(&[&a, &c], &[None, None], &[None, None], &[2.0, 2.0]);
        assert_eq!(g.raw.p_win, 0.0);
        // nonlinear: c wins when exactly one of a, b wins (XOR) -> pairwise independent, jointly impossible with both
        let x: Vec<u8> = a.iter().zip(&b).map(|(p, q)| if (*p == W) != (*q == W) { W } else { L }).collect();
        let g = eval_group(&[&a, &b, &x], &[None; 3], &[None; 3], &[2.0; 3]);
        for (_, _, pij) in &g.raw.pairs { assert!((pij - 0.25).abs() < 0.005, "pairwise independent: {pij}"); }
        assert_eq!(g.raw.p_win, 0.0, "but never all three: products of marginals (0.125) would be wrong");
    }

    #[test]
    fn tilting_matches_marginals_and_keeps_dependence() {
        let n = 100_000;
        let a = bern(n, 0.5, 3);
        // b agrees with a 80% of the time
        let flip = bern(n, 0.2, 4);
        let b: Vec<u8> = a.iter().zip(&flip).map(|(x, f)| if *f == W { if *x == W { L } else { W } } else { *x }).collect();
        let g = eval_group(&[&a, &b], &[Some(0.6), Some(0.55)], &[None, None], &[1.9, 1.9]);
        assert!((g.blend.leg_w[0] - 0.6).abs() < 1e-6 && (g.blend.leg_w[1] - 0.55).abs() < 1e-6);
        assert!(g.blend.p_win > 0.6 * 0.55 + 0.05, "positive dependence survives: {}", g.blend.p_win);
        assert!(g.blend.p_win <= 0.55 + 1e-9, "Frechet upper bound");
        assert!(g.blend.ess > 0.5 * n as f64, "a mild tilt keeps most of the sample: {}", g.blend.ess);
    }

    #[test]
    fn pushes_and_voids_return_the_stake() {
        // leg 1 always wins at 2.0; leg 2 pushes half the time, wins otherwise at 3.0
        let a = vec![W; 4];
        let b = vec![P, W, P, W];
        let g = eval_group(&[&a, &b], &[None, None], &[None, None], &[2.0, 3.0]);
        assert!((g.raw.e_pay - (0.5 * 2.0 + 0.5 * 6.0)).abs() < 1e-12, "{}", g.raw.e_pay);
        assert!((g.raw.p_win - 0.5).abs() < 1e-12);
        assert_eq!(g.raw.p_loss, 0.0);
        let c = vec![V, V, L, W];
        let g = eval_group(&[&c], &[None], &[None], &[2.5]);
        assert!((g.raw.e_pay - (0.5 * 1.0 + 0.25 * 2.5)).abs() < 1e-12);
        // tilting keeps the push/void share and moves win vs loss only
        let g = eval_group(&[&c], &[Some(0.4)], &[None], &[2.5]);
        assert!((g.blend.leg_w[0] - 0.4).abs() < 1e-9 && (g.blend.leg_l[0] - 0.1).abs() < 1e-9);
    }

    #[test]
    fn odds_conversions() {
        assert_eq!(american(2.5), 150);
        assert_eq!(american(1.5), -200);
        assert_eq!(american(2.0), 100);
        assert!((decimal(-110) - 1.909090909).abs() < 1e-6);
    }

    fn leg(id: &str, game: &str, market: &str, subject: &str, side: &str, p: f64, states: Vec<u8>, books: &[(&str, i32)]) -> Leg {
        Leg { id: id.into(), game: game.into(), market: market.into(), subject: subject.into(), subject_name: None, team: None, side: side.into(), line: Some(0.5),
            label: id.into(), quotes: books.iter().map(|(b, pr)| Quote { book: b.to_string(), price: *pr, p_novig: Some(p), observed: Utc::now(), one_sided: false }).collect(),
            p_market: Some(p), p_sim: Some(p), push: 0.0, void: 0.0, p_naive: None, p_blend: Some(p), tier: "baseline", flags: vec![], eligible: true, states: Some(Arc::new(states)) }
    }
    fn slate(legs: Vec<Leg>, dep: f64) -> Slate {
        Slate { sport: "nba".into(), date: NaiveDate::from_ymd_opt(2026, 10, 27).unwrap(), games: vec![], legs, dep_weight: dep, model_version: "t".into(), max_age: 180, oldest_quote: None, newest_quote: None, bias: BTreeMap::new() }
    }

    #[test]
    fn tickets_price_at_one_book_and_flag_same_game() {
        let n = 50_000;
        let a = bern(n, 0.5, 5);
        let b = bern(n, 0.5, 6);
        let s = slate(vec![
            leg("a", "g1", "pts", "p1", "over", 0.5, a.clone(), &[("dk", 100), ("fd", -110)]),
            leg("b", "g2", "pts", "p2", "over", 0.5, b.clone(), &[("dk", -110), ("fd", 100)]),
            leg("c", "g1", "reb", "p1", "over", 0.5, a.clone(), &[("dk", 100)]),
            leg("d", "g1", "pts", "p1", "under", 0.5, a.clone(), &[("dk", 100)]),
        ], 1.0);
        let mut gc = HashMap::new();
        let t = evaluate(&s, &[0, 1], None, None, &mut gc).unwrap();
        assert!(t.price_verified && !t.same_game);
        assert!((t.price_decimal - 2.0 * decimal(-110)).abs() < 1e-9, "one book, not the best price of each leg: {}", t.price_decimal);
        assert!((t.p_joint - 0.25).abs() < 0.01 && (t.lift() - 1.0).abs() < 1e-9, "cross-game: product");
        assert!((t.ev - (t.p_joint * t.price_decimal - 1.0)).abs() < 1e-9, "no pushes: EV = p x O - 1");
        let t = evaluate(&s, &[0, 2], None, None, &mut gc).unwrap();
        assert!(t.same_game && !t.price_verified && t.price_note.is_some());
        assert!((t.p_joint - 0.5).abs() < 0.01, "identical outcomes: joint = marginal, not its square ({})", t.p_joint);
        assert!((t.sgp_adjust - 0.5).abs() < 0.02 && (t.price_decimal - t.price_product * t.sgp_adjust).abs() < 1e-9,
            "a book pays the correlated price, not the product: x{}", t.sgp_adjust);
        assert!(t.ev < 0.05, "no free money from correlation the book also sees: {}", t.ev);
        assert!(evaluate(&s, &[0, 3], None, None, &mut gc).unwrap_err().starts_with("duplicate market"));
        // dependence weight 0 = independence baseline
        let s0 = slate(s.legs.clone(), 0.0);
        let t0 = evaluate(&s0, &[0, 2], None, None, &mut HashMap::new()).unwrap();
        assert!((t0.p_joint - 0.25).abs() < 0.01, "{}", t0.p_joint);
    }

    #[test]
    fn recentering_removes_a_slate_wide_lean_but_keeps_the_ranking() {
        // the model leans under on every points line by ~0.4 logit, more on player 0
        let mut legs: Vec<Leg> = (0..10).flat_map(|i| {
            let pb = sigm(logit(0.5) - 0.4 - if i == 0 { 0.3 } else { 0.0 });
            let mut o = leg(&format!("o{i}"), "g", "pts", &format!("p{i}"), "over", 0.5, vec![W, L], &[("dk", -110)]);
            o.p_blend = Some(pb);
            let mut u = leg(&format!("u{i}"), "g", "pts", &format!("p{i}"), "under", 0.5, vec![W, L], &[("dk", -110)]);
            u.p_blend = Some(1.0 - pb);
            [o, u]
        }).collect();
        let b = recenter_markets(&mut legs, true);
        assert!((b["pts"].1 + 0.43).abs() < 0.01 && b["pts"].2);
        let over: Vec<f64> = legs.iter().filter(|l| l.side == "over").map(|l| l.p_blend.unwrap()).collect();
        let mean: f64 = over.iter().map(|p| logit(*p)).sum::<f64>() / over.len() as f64;
        assert!(mean.abs() < 1e-9, "average lean removed");
        assert!(over[0] < over[1], "player 0 still the strongest under");
        assert!((legs[1].p_blend.unwrap() + legs[0].p_blend.unwrap() - 1.0).abs() < 1e-9, "sides stay complementary");
        // fewer than 8 lines: reported, not applied
        let mut few: Vec<Leg> = legs.iter().take(6).cloned().collect();
        let before = few[0].p_blend;
        assert!(!recenter_markets(&mut few, true)["pts"].2);
        assert_eq!(few[0].p_blend, before);
    }

    #[test]
    fn beam_search_respects_locks_scope_and_ranks() {
        let n = 20_000;
        let mk = |i: usize, g: &str, p: f64, price: i32| leg(&format!("l{i}"), g, &format!("m{i}"), &format!("s{i}"), "over", p, bern(n, p, 100 + i as u64), &[("dk", price)]);
        let s = slate(vec![mk(0, "g1", 0.6, 100), mk(1, "g2", 0.55, 100), mk(2, "g3", 0.5, 100), mk(3, "g4", 0.45, 100), mk(4, "g1", 0.5, 100)], 1.0);
        let q = GenQ { legs: Some(2), objective: Some("ev".into()), scope: Some("cross".into()), ..GenQ::default() };
        let g = generate(&s, &q).unwrap();
        let best: HashSet<String> = g.tickets[0].legs.iter().map(|i| s.legs[*i].id.clone()).collect();
        assert_eq!(best, ["l0".to_string(), "l1".to_string()].into_iter().collect(), "highest-EV pair");
        assert!(g.tickets.iter().all(|t| !t.same_game));
        for w in g.tickets.windows(2) { assert!(w[0].ev >= w[1].ev - 1e-12); }
        let q = GenQ { legs: Some(2), locked: Some("l3".into()), objective: Some("prob".into()), ..GenQ::default() };
        let g = generate(&s, &q).unwrap();
        assert!(g.tickets.iter().all(|t| t.legs.contains(&3)), "locked leg kept");
        let q = GenQ { legs: Some(2), min_prob: Some(0.5), ..GenQ::default() };
        assert!(generate(&s, &q).unwrap().tickets.is_empty(), "no pair reaches 50%");
        let q = GenQ { legs: Some(2), scope: Some("same".into()), ..GenQ::default() };
        assert!(generate(&s, &q).unwrap().tickets.iter().all(|t| t.same_game));
    }
}
