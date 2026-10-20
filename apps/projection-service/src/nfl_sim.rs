//! NFL play-level game simulator (pure; no I/O).
//!
//! One simulated game walks the real structure of football: kickoffs, downs and
//! distance, field position, the game clock (two-minute warning, quarter and
//! half breaks), punts, field goals, extra points / two-point tries, turnovers,
//! safeties and the 2025+ overtime rule (both teams possess, then sudden death;
//! regular-season games can end tied). Every player stat is DERIVED from those
//! simulated plays, so a QB's yards, his receivers' catches and the score all
//! come from the same game.
//!
//! Where each probability comes from (all fitted from nflverse play-by-play):
//! * play call — league pass rate by (down, distance, score, time) cell, shifted
//!   by the team's pass-rate-over-expected; 4th down go/punt/FG by (field
//!   position, distance, trailing late) cell;
//! * dropback outcome — sack / scramble / throw, then interception, completion:
//!   offense (QB, receiver) x defense via the odds-ratio method against league
//!   rates, each rate partially pooled with an empirical-Bayes strength;
//! * target and carry shares — recency-weighted team usage;
//! * yards — the player's own empirical gains mixed with the league gains for
//!   that part of the field (EB mixing weight), scaled by a shrunk defensive
//!   yards-allowed ratio; inside the 10 the league goal-to-go distribution;
//! * clock — empirical seconds-to-next-snap by (clock running?, situation);
//! * kickoffs, punts, turnover returns — empirical next-snap field position;
//! * FG make — league logistic in distance; XP and two-point rates.
//! Not modelled yet (documented): penalties as events, timeouts, onside kicks,
//! injuries / in-game QB changes, laterals, weather, home-field term.

use serde::{Deserialize, Serialize};

pub use crate::mlb_sim::{derive_seed, Rng};

// ---------------------------------------------------------------------------
// Situation buckets (shared by the fitter and the simulator)
// ---------------------------------------------------------------------------

/// Field-position bands for the league gain distributions (yards to the goal
/// line): 1-2, 3-5, 6-10, 11-15, 16-20, 21-25, 26-30, 31-35, 36-40, 41+.
/// Near the goal line the shape of real gains changes (more end-zone throws,
/// compressed runs), so gains there come from the band's own distribution.
pub const ZONES: usize = 10;
pub fn zone(yl: i32) -> usize {
    match yl { i32::MIN..=2 => 0, 3..=5 => 1, 6..=10 => 2, 11..=15 => 3, 16..=20 => 4, 21..=25 => 5, 26..=30 => 6, 31..=35 => 7, 36..=40 => 8, _ => 9 }
}
/// Down-and-distance groups for gains and dropback outcomes:
/// 0 = 1st down, 1 = later down & 1-3 to go, 2 = later & 4-7, 3 = later & 8+.
pub const DD: usize = 4;
pub fn dd_group(down: i32, togo: i32) -> usize {
    if down <= 1 { 0 } else if togo <= 3 { 1 } else if togo <= 7 { 2 } else { 3 }
}
/// Gain-distribution cell: field band x down-and-distance group.
pub const GAIN_CELLS: usize = ZONES * DD;
pub fn gain_cell(yl: i32, down: i32, togo: i32) -> usize {
    zone(yl) * DD + dd_group(down, togo)
}
fn togo_b(t: i32) -> usize {
    match t { i32::MIN..=2 => 0, 3..=5 => 1, 6..=9 => 2, 10 => 3, _ => 4 }
}
pub fn diff_b(d: i32) -> usize {
    match d { i32::MIN..=-15 => 0, -14..=-8 => 1, -7..=-1 => 2, 0 => 3, 1..=7 => 4, 8..=14 => 5, _ => 6 }
}
fn time_b(qtr: i32, clock: i32) -> usize {
    if qtr == 2 && clock <= 120 { 1 } else if qtr >= 4 && clock <= 120 { 3 } else if qtr >= 4 && clock <= 300 { 2 } else { 0 }
}
pub const PASS_CELLS: usize = 3 * 5 * 7 * 4;
pub fn pass_cell(down: i32, togo: i32, diff: i32, qtr: i32, clock: i32) -> usize {
    ((((down.clamp(1, 3) - 1) as usize * 5 + togo_b(togo)) * 7 + diff_b(diff)) * 4) + time_b(qtr, clock)
}
/// Marginal cell (down x distance) used when a situational cell is thin.
pub fn pass_marg(down: i32, togo: i32) -> usize {
    (down.clamp(1, 3) - 1) as usize * 5 + togo_b(togo)
}
pub const FOURTH_CELLS: usize = 10 * 4 * 2;
pub fn fourth_cell(yl: i32, togo: i32, diff: i32, qtr: i32) -> usize {
    let y = ((yl.clamp(1, 99) - 1) / 10) as usize;
    let t = match togo { i32::MIN..=1 => 0, 2..=3 => 1, 4..=6 => 2, _ => 3 };
    let late = (qtr >= 4 && diff < 0) as usize;
    (y * 4 + t) * 2 + late
}
/// 0 normal, 1 hurry-up (end of half, or trailing/tied late), 2 draining (leading late).
pub fn situation(qtr: i32, clock: i32, diff: i32) -> usize {
    if qtr == 2 && clock <= 120 { 1 } else if qtr >= 4 && clock <= 300 { if diff > 0 { 2 } else { 1 } } else { 0 }
}
/// Clock classes: 3 situations x 4 play outcomes, plus kicks. The outcome
/// matters because the clock rules differ: an in-bounds tackle keeps it
/// running, out of bounds stops it (restarting on the ready signal outside the
/// final minutes), an incompletion stops it until the snap.
pub const RUN_IN: usize = 0;
pub const OOB: usize = 1;
pub const INCOMPLETE: usize = 2;
pub const OTHER_STOP: usize = 3;
pub const CLOCK_CLASSES: usize = 13;
pub const KICK_CLASS: usize = 12;
pub fn clock_class(sit: usize, outcome: usize) -> usize {
    sit * 4 + outcome
}

// ---------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------

/// League-level parameters fitted from one season of play-by-play.
#[derive(Clone, Serialize, Deserialize)]
pub struct League {
    pub season: i32,
    pub pass_cells: Vec<[f64; 2]>, // [dropbacks, plays]
    pub pass_marg: Vec<[f64; 2]>,
    pub fourth_cells: Vec<[f64; 3]>, // [go, punt, fg]
    pub sack: f64,
    pub scramble: f64,
    pub int: f64,
    pub cmp: f64,
    pub kappa: Kappas,
    /// gains by `gain_cell` (field band x down-and-distance)
    pub cmp_yards: Vec<Vec<i32>>,
    pub run_yards: Vec<Vec<i32>>,
    pub scramble_yards: Vec<i32>,
    pub sack_yards: Vec<i32>, // yards LOST (positive)
    pub fumble_run: f64,
    pub fumble_rec: f64,
    pub oob_run: [f64; 3],
    pub oob_cmp: [f64; 3],
    pub elapsed: Vec<Vec<i32>>,
    pub kickoff_start: Vec<i32>, // receiving team's yardline_100 at its first snap; 0 = return TD
    pub punt_start: Vec<Vec<i32>>, // by punt-spot bucket; 0 = return TD
    pub int_return: Vec<i32>,    // new offense's yardline_100 minus (100 - old line of scrimmage)
    pub int_td: f64,
    pub fum_return: Vec<i32>,
    pub fum_td: f64,
    pub fg_a: f64,
    pub fg_b: f64,
    pub xp: f64,
    pub two_pt_go: Vec<f64>, // by diff bucket after the touchdown
    pub two_pt_success: f64,
    pub loc_pass: [f64; 3],
    pub loc_run: [f64; 3],
    /// home-field factors (sqrt of home/away ratios): yards per completion,
    /// yards per carry, and odds ratios for sack, interception and completion
    #[serde(default)]
    pub home: HomeField,
    /// probability a snap is wiped out by an accepted penalty (pre-snap or nullified play)
    #[serde(default)]
    pub pen_rate: f64,
    /// probability an accepted live-ball foul is added after a completed play
    #[serde(default)]
    pub pen_live_rate: f64,
    /// empirical penalties: [offence penalised (1/0), yards, automatic first down (1/0)]
    #[serde(default)]
    pub pen: Vec<[i32; 3]>,
    #[serde(default)]
    pub pen_live: Vec<[i32; 3]>,
    #[serde(default)]
    pub fumble_sack: f64,
    /// league mean gain per completion / designed run (all field positions)
    #[serde(default)]
    pub cmp_mean: f64,
    #[serde(default)]
    pub run_mean: f64,
    /// odds multipliers by down-and-distance group (league rate in the group
    /// vs overall) for sack, scramble, interception and completion
    #[serde(default = "ones")]
    pub dd_sack: [f64; DD],
    #[serde(default = "ones")]
    pub dd_scramble: [f64; DD],
    #[serde(default = "ones")]
    pub dd_int: [f64; DD],
    #[serde(default = "ones")]
    pub dd_cmp: [f64; DD],
}
fn ones() -> [f64; DD] { [1.0; DD] }

#[derive(Clone, Serialize, Deserialize)]
pub struct HomeField {
    pub ypr: f64,
    pub ypc: f64,
    pub sack: f64,
    pub int: f64,
    pub cmp: f64,
}
impl Default for HomeField {
    fn default() -> Self { HomeField { ypr: 1.0, ypc: 1.0, sack: 1.0, int: 1.0, cmp: 1.0 } }
}

/// Empirical-Bayes pooling strengths (prior sample sizes), fitted per season.
#[derive(Clone, Serialize, Deserialize)]
pub struct Kappas {
    pub sack: f64,
    pub scramble: f64,
    pub int: f64,
    pub cmp_qb: f64,
    pub catch: f64,
    pub def_sack: f64,
    pub def_int: f64,
    pub def_cmp: f64,
    pub ypc: f64,  // yards mixing weight, rushers
    pub ypr: f64,  // yards mixing weight, receivers
    pub def_yards: f64,
    pub proe: f64,
}

/// A rate with the evidence behind it (posterior mean, posterior sample size).
#[derive(Clone, Copy, Serialize, Deserialize, Debug)]
pub struct Rate {
    pub mean: f64,
    pub n: f64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Player {
    pub id: String,
    pub name: String,
    pub pos: String,
    /// probability the player is active for this game (1 = confirmed). In each
    /// parameter draw it is realised as 1 or 0; an inactive player's targets
    /// and carries are redistributed to the active players.
    #[serde(default = "one")]
    pub p_active: f64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Receiver {
    pub p: usize,
    pub share: f64,
    pub catch: Rate,
    pub yards: Vec<i32>,
    /// shrunk (own mean gain / league mean gain), used inside the 40
    #[serde(default = "one")]
    pub ratio: f64,
    pub loc: [f64; 3],
}
fn one() -> f64 { 1.0 }

#[derive(Clone, Serialize, Deserialize)]
pub struct Rusher {
    pub p: usize,
    pub share: f64,
    pub yards: Vec<i32>,
    #[serde(default = "one")]
    pub ratio: f64,
    pub loc: [f64; 3],
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Offense {
    pub qb: usize,
    pub kicker: Option<usize>,
    pub proe: f64, // logit shift of the pass rate
    pub sack: Rate,
    pub scramble: Rate,
    pub int: Rate,
    pub cmp: Rate,
    pub qb_scramble_yards: Vec<i32>,
    pub qb_loc: [f64; 3],
    pub receivers: Vec<Receiver>,
    pub rushers: Vec<Rusher>,
    /// effective number of targets / carries behind the shares (Dirichlet strength)
    pub share_n: [f64; 2],
    /// target-share-weighted mean catch rate of the receivers; a receiver's
    /// catch rate only redistributes completions within the team (the team
    /// rate comes from QB x defence), so QB accuracy that is already inside
    /// receivers' catch rates is not counted twice
    #[serde(default)]
    pub catch_ref: f64,
    /// replaces the starter in a draw where the starter is inactive
    #[serde(default)]
    pub backup_qb: Option<BackupQb>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct BackupQb {
    pub p: usize,
    pub sack: Rate,
    pub scramble: Rate,
    pub int: Rate,
    pub cmp: Rate,
    pub loc: [f64; 3],
}

pub fn catch_ref(receivers: &[Receiver]) -> f64 {
    let tot: f64 = receivers.iter().map(|r| r.share).sum();
    if tot <= 0.0 { return 1.0; }
    receivers.iter().map(|r| r.share * r.catch.mean).sum::<f64>() / tot
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Defense {
    pub sack: Rate,
    pub int: Rate,
    pub cmp: Rate,
    pub ypc_ratio: f64,
    pub ypr_ratio: f64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct TeamInput {
    pub abbr: String,
    pub off: Offense,
    pub def: Defense,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct GameInput {
    pub players: Vec<Player>,
    pub teams: [TeamInput; 2], // 0 = away, 1 = home
    pub league: League,
    pub postseason: bool,
}

// ---------------------------------------------------------------------------
// Player stat slots
// ---------------------------------------------------------------------------

pub const NSTAT: usize = 18;
pub const PASS_ATT: usize = 0;
pub const CMP: usize = 1;
pub const PASS_YDS: usize = 2;
pub const PASS_TD: usize = 3;
pub const INT: usize = 4;
pub const SACKED: usize = 5;
pub const RUSH_ATT: usize = 6;
pub const RUSH_YDS: usize = 7;
pub const RUSH_TD: usize = 8;
pub const TGT: usize = 9;
pub const REC: usize = 10;
pub const REC_YDS: usize = 11;
pub const REC_TD: usize = 12;
pub const FUM_LOST: usize = 13;
pub const FGA: usize = 14;
pub const FGM: usize = 15;
pub const XPA: usize = 16;
pub const XPM: usize = 17;
pub const STAT_KEYS: [&str; NSTAT] = [
    "pass_att", "completions", "pass_yds", "pass_td", "interceptions", "sacked", "carries", "rush_yds", "rush_td",
    "targets", "receptions", "rec_yds", "rec_td", "fumbles_lost", "fga", "fgm", "xpa", "xpm",
];

// ---------------------------------------------------------------------------
// Events (interactive mode)
// ---------------------------------------------------------------------------

#[derive(Clone, Serialize, Debug)]
pub struct Snapshot {
    pub qtr: i32,
    pub clock: i32,
    pub score: [i32; 2],
    pub possession: u8,
    pub down: i32,
    pub togo: i32,
    pub yardline_100: i32,
    /// ball spot on the absolute field, 0 = away goal line, 100 = home goal line
    pub ball_x: f64,
    pub first_down_x: Option<f64>,
}

#[derive(Clone, Serialize, Debug)]
pub struct Event {
    pub seq: u32,
    pub kind: &'static str,
    pub qtr: i32,
    pub clock_before: i32,
    pub clock_after: i32,
    pub offense: u8,
    pub down: i32,
    pub togo: i32,
    pub yardline_100: i32,
    pub yards: i32,
    pub passer: Option<usize>,
    pub receiver: Option<usize>,
    pub rusher: Option<usize>,
    pub kicker: Option<usize>,
    pub location: Option<&'static str>,
    pub x_from: f64,
    pub x_to: f64,
    pub first_down: bool,
    pub touchdown: bool,
    pub turnover: bool,
    pub points: i32,
    pub scoring_team: Option<u8>,
    pub text: String,
    pub state: Snapshot,
    /// box score after this event: (player index, stat line) for players with any stat
    pub box_score: Vec<(usize, [i32; NSTAT])>,
}

// ---------------------------------------------------------------------------
// Sampling helpers
// ---------------------------------------------------------------------------

fn pick<'a>(rng: &mut Rng, v: &'a [i32]) -> Option<i32> {
    if v.is_empty() { None } else { Some(v[(rng.next_u64() % v.len() as u64) as usize]) }
}
fn odds(p: f64) -> f64 {
    let p = p.clamp(1e-6, 1.0 - 1e-6);
    p / (1.0 - p)
}
fn from_odds(o: f64) -> f64 {
    o / (1.0 + o)
}
/// Odds-ratio combination of two sides against the league rate.
pub fn combine(a: f64, b: f64, lg: f64) -> f64 {
    from_odds(odds(a) * odds(b) / odds(lg))
}
fn logit(p: f64) -> f64 {
    odds(p).ln()
}
fn sigmoid(x: f64) -> f64 {
    1.0 / (1.0 + (-x).exp())
}
fn choose(rng: &mut Rng, w: &[f64]) -> usize {
    let tot: f64 = w.iter().sum();
    if tot <= 0.0 {
        return 0;
    }
    let mut u = rng.f64() * tot;
    for (i, x) in w.iter().enumerate() {
        if u < *x {
            return i;
        }
        u -= x;
    }
    w.len() - 1
}
const LOCS: [&str; 3] = ["left", "middle", "right"];
fn beta(rng: &mut Rng, r: Rate) -> f64 {
    let n = r.n.max(2.0);
    let a = (r.mean * n).max(0.05);
    let b = ((1.0 - r.mean) * n).max(0.05);
    let x = rng.gamma(a);
    let y = rng.gamma(b);
    if x + y <= 0.0 { r.mean } else { x / (x + y) }
}

/// Draw one plausible set of parameters from their posteriors (epistemic
/// uncertainty): every rate from its Beta posterior, shares from a Dirichlet.
pub fn perturb(g: &GameInput, rng: &mut Rng) -> GameInput {
    let mut out = g.clone();
    for t in out.teams.iter_mut() {
        let o = &mut t.off;
        o.sack.mean = beta(rng, o.sack);
        o.scramble.mean = beta(rng, o.scramble);
        o.int.mean = beta(rng, o.int);
        o.cmp.mean = beta(rng, o.cmp);
        for r in o.receivers.iter_mut() {
            r.catch.mean = beta(rng, r.catch);
        }
        let nr = o.share_n[0].max(5.0);
        let draws: Vec<f64> = o.receivers.iter().map(|r| rng.gamma((r.share * nr).max(0.05))).collect();
        let tot: f64 = draws.iter().sum();
        if tot > 0.0 { for (r, d) in o.receivers.iter_mut().zip(&draws) { r.share = d / tot; } }
        let nc = o.share_n[1].max(5.0);
        let draws: Vec<f64> = o.rushers.iter().map(|r| rng.gamma((r.share * nc).max(0.05))).collect();
        let tot: f64 = draws.iter().sum();
        if tot > 0.0 { for (r, d) in o.rushers.iter_mut().zip(&draws) { r.share = d / tot; } }
        if let Some(b) = o.backup_qb.as_mut() {
            b.sack.mean = beta(rng, b.sack);
            b.scramble.mean = beta(rng, b.scramble);
            b.int.mean = beta(rng, b.int);
            b.cmp.mean = beta(rng, b.cmp);
        }
        o.catch_ref = catch_ref(&o.receivers);
        let d = &mut t.def;
        d.sack.mean = beta(rng, d.sack);
        d.int.mean = beta(rng, d.int);
        d.cmp.mean = beta(rng, d.cmp);
    }
    realise_availability(&mut out, rng);
    out
}

/// Draw who is active (p_active -> 1 or 0), hand an inactive starter's job to
/// the backup QB and redistribute inactive players' targets and carries.
pub fn realise_availability(g: &mut GameInput, rng: &mut Rng) {
    for p in g.players.iter_mut() {
        if p.p_active < 1.0 {
            p.p_active = if rng.f64() < p.p_active { 1.0 } else { 0.0 };
        }
    }
    let active: Vec<bool> = g.players.iter().map(|p| p.p_active > 0.5).collect();
    for t in g.teams.iter_mut() {
        let o = &mut t.off;
        if !active[o.qb] {
            if let Some(b) = o.backup_qb.clone() {
                o.qb = b.p;
                o.sack = b.sack;
                o.scramble = b.scramble;
                o.int = b.int;
                o.cmp = b.cmp;
                o.qb_loc = b.loc;
                o.qb_scramble_yards.clear();
            }
        }
        let renorm = |shares: &mut [f64]| {
            let tot: f64 = shares.iter().sum();
            if tot > 0.0 { for x in shares.iter_mut() { *x /= tot; } }
        };
        let mut rs: Vec<f64> = o.receivers.iter().map(|r| if active[r.p] { r.share } else { 0.0 }).collect();
        renorm(&mut rs);
        for (r, s) in o.receivers.iter_mut().zip(rs) { r.share = s; }
        let mut cs: Vec<f64> = o.rushers.iter().map(|r| if active[r.p] { r.share } else { 0.0 }).collect();
        renorm(&mut cs);
        for (r, s) in o.rushers.iter_mut().zip(cs) { r.share = s; }
        o.catch_ref = catch_ref(&o.receivers);
        if let Some(k) = o.kicker { if !active[k] { o.kicker = None; } }
    }
}

// ---------------------------------------------------------------------------
// The game
// ---------------------------------------------------------------------------

pub struct GameResult {
    pub score: [i32; 2],
    pub stats: Vec<[i32; NSTAT]>,
    pub plays: [i32; 2],
    pub overtime: bool,
    /// who was active in this simulated game
    pub active: Vec<bool>,
}

enum Next {
    Kickoff(u8),
    Scrimmage,
}

struct G<'a> {
    g: &'a GameInput,
    rng: &'a mut Rng,
    qtr: i32,
    clock: i32,
    score: [i32; 2],
    pos: u8,
    yl: i32,
    down: i32,
    togo: i32,
    stats: Vec<[i32; NSTAT]>,
    plays: [i32; 2],
    ot_poss: [i32; 2],
    opening_receiver: u8,
    over: bool,
    tod: bool,
    events: Option<&'a mut Vec<Event>>,
}

fn period_len(qtr: i32) -> i32 {
    if qtr <= 4 { 900 } else { 600 }
}

type Before = (i32, i32, u8, i32, i32, i32);

impl<'a> G<'a> {
    fn diff(&self) -> i32 { self.score[self.pos as usize] - self.score[1 - self.pos as usize] }
    fn ball_x(&self, team: u8, yl: i32) -> f64 {
        // away attacks the home goal line (x = 100)
        if team == 0 { (100 - yl) as f64 } else { yl as f64 }
    }
    fn recording(&self) -> bool { self.events.is_some() }

    fn snapshot(&self) -> Snapshot {
        let fd = if self.down >= 1 && self.yl > self.togo {
            Some(self.ball_x(self.pos, self.yl - self.togo))
        } else { None };
        Snapshot {
            qtr: self.qtr, clock: self.clock, score: self.score, possession: self.pos,
            down: self.down, togo: self.togo, yardline_100: self.yl.clamp(0, 100),
            ball_x: self.ball_x(self.pos, self.yl.clamp(0, 100)), first_down_x: fd,
        }
    }

    /// Record an event (interactive mode only; `text` is built lazily so the
    /// ensemble pays nothing for it).
    #[allow(clippy::too_many_arguments)]
    fn emit(&mut self, kind: &'static str, before: Before, yards: i32, who: [Option<usize>; 4],
            location: Option<&'static str>, x: (f64, f64), flags: (bool, bool, bool), points: i32, scoring: Option<u8>,
            text: impl FnOnce() -> String) {
        if !self.recording() {
            return;
        }
        let state = self.snapshot();
        let box_score: Vec<(usize, [i32; NSTAT])> = self.stats.iter().enumerate()
            .filter(|(_, s)| s.iter().any(|v| *v != 0)).map(|(i, s)| (i, *s)).collect();
        let ev = self.events.as_mut().unwrap();
        let seq = ev.len() as u32 + 1;
        ev.push(Event {
            seq, kind, qtr: before.0, clock_before: before.1, clock_after: state.clock, offense: before.2,
            down: before.3, togo: before.4, yardline_100: before.5, yards,
            passer: who[0], receiver: who[1], rusher: who[2], kicker: who[3], location,
            x_from: x.0, x_to: x.1, first_down: flags.0, touchdown: flags.1, turnover: flags.2,
            points, scoring_team: scoring, text: text(), state, box_score,
        });
    }
    fn before(&self) -> Before {
        (self.qtr, self.clock, self.pos, self.down, self.togo, self.yl)
    }

    fn elapsed(&mut self, class: usize) -> i32 {
        let g = self.g;
        pick(self.rng, &g.league.elapsed[class]).unwrap_or(if class % 4 == RUN_IN && class != KICK_CLASS { 38 } else { 6 }).max(0)
    }

    /// Run the clock after a play; handles the two-minute warning and the end
    /// of a period. Returns true when the period ended on this play.
    fn tick(&mut self, secs: i32) -> bool {
        let before = self.clock;
        let after = before - secs;
        if (self.qtr == 2 || self.qtr == 4) && before > 120 && after < 120 {
            self.clock = if before - 6 < 120 { (before - 6).max(0) } else { 120 };
            if self.clock == 120 {
                let b = self.before();
                let x = self.ball_x(self.pos, self.yl.clamp(0, 100));
                self.emit("two_minute_warning", b, 0, [None; 4], None, (x, x), (false, false, false), 0, None, || "Two-minute warning".into());
            }
            return self.clock == 0;
        }
        self.clock = after.max(0);
        self.clock == 0
    }

    fn new_possession(&mut self, team: u8, yl: i32) {
        self.pos = team;
        self.yl = yl.clamp(1, 99);
        self.down = 1;
        self.togo = 10.min(self.yl);
        if self.qtr >= 5 {
            self.ot_poss[team as usize] += 1;
        }
    }

    fn ot_decided(&self) -> bool {
        self.qtr >= 5 && self.ot_poss[0] > 0 && self.ot_poss[1] > 0 && self.score[0] != self.score[1]
    }
    fn check_ot(&mut self) {
        if self.ot_decided() { self.over = true; }
    }

    fn spot_text(&self, team: u8, yl: i32) -> String {
        let own = &self.g.teams[team as usize].abbr;
        let opp = &self.g.teams[1 - team as usize].abbr;
        if yl > 50 { format!("{own} {}", 100 - yl) } else if yl == 50 { "midfield".into() } else { format!("{opp} {yl}") }
    }

    // ---- kicks ------------------------------------------------------------

    fn kickoff(&mut self, kicker: u8) -> Next {
        let g = self.g;
        let recv = 1 - kicker;
        let b = (self.qtr, self.clock, kicker, 0, 0, 65);
        let s = pick(self.rng, &g.league.kickoff_start).unwrap_or(70);
        let secs = self.elapsed(KICK_CLASS).min(12);
        let k = g.teams[kicker as usize].off.kicker;
        let x_from = self.ball_x(kicker, 65);
        let ended = self.tick(secs);
        let abbr_r = &g.teams[recv as usize].abbr;
        if s == 0 {
            self.pos = recv;
            self.yl = 0;
            self.down = 0;
            self.score[recv as usize] += 6;
            let x = self.ball_x(recv, 0);
            self.emit("kickoff", b, 0, [None, None, None, k], None, (x_from, x), (false, true, false), 6, Some(recv),
                || format!("Kickoff returned for a TOUCHDOWN by {abbr_r}"));
            let next = self.try_after(recv);
            if ended && !self.over { self.end_period(); }
            return next;
        }
        self.new_possession(recv, s);
        let x = self.ball_x(recv, self.yl);
        let spot = if self.recording() { self.spot_text(recv, self.yl) } else { String::new() };
        self.emit("kickoff", b, 0, [None, None, None, k], None, (x_from, x), (false, false, false), 0, None,
            || format!("Kickoff — {abbr_r} ball at {spot}"));
        self.check_ot();
        if ended { self.end_period(); }
        Next::Scrimmage
    }

    fn try_after(&mut self, team: u8) -> Next {
        let g = self.g;
        self.pos = team;
        let diff = self.score[team as usize] - self.score[1 - team as usize];
        let go2 = g.league.two_pt_go.get(diff_b(diff)).copied().unwrap_or(0.1);
        let k = g.teams[team as usize].off.kicker;
        let goal = self.ball_x(team, 0);
        if self.rng.f64() < go2 {
            let b = (self.qtr, self.clock, team, 0, 0, 2);
            let ok = self.rng.f64() < g.league.two_pt_success;
            if ok { self.score[team as usize] += 2; }
            self.yl = 2;
            self.down = 0;
            let x = self.ball_x(team, 2);
            self.emit("two_point", b, 0, [None; 4], None, (x, goal), (false, false, false), if ok { 2 } else { 0 }, ok.then_some(team),
                || format!("Two-point attempt {}", if ok { "is GOOD" } else { "fails" }));
        } else {
            let b = (self.qtr, self.clock, team, 0, 0, 15);
            let ok = self.rng.f64() < g.league.xp;
            if let Some(kk) = k {
                self.stats[kk][XPA] += 1;
                if ok { self.stats[kk][XPM] += 1; }
            }
            if ok { self.score[team as usize] += 1; }
            self.yl = 15;
            self.down = 0;
            let x = self.ball_x(team, 15);
            let who = k.map(|kk| g.players[kk].name.as_str()).unwrap_or("Extra point");
            self.emit("extra_point", b, 0, [None, None, None, k], None, (x, goal), (false, false, false),
                if ok { 1 } else { 0 }, ok.then_some(team), || format!("{who} extra point is {}", if ok { "GOOD" } else { "NO GOOD" }));
        }
        self.check_ot();
        Next::Kickoff(team)
    }

    fn field_goal(&mut self) -> Next {
        let g = self.g;
        let b = self.before();
        let team = self.pos;
        let dist = self.yl + 18;
        let p = sigmoid(g.league.fg_a + g.league.fg_b * dist as f64);
        let ok = self.rng.f64() < p;
        let k = g.teams[team as usize].off.kicker;
        if let Some(kk) = k {
            self.stats[kk][FGA] += 1;
            if ok { self.stats[kk][FGM] += 1; }
        }
        let secs = self.elapsed(KICK_CLASS).min(10);
        let ended = self.tick(secs);
        let x_from = self.ball_x(team, self.yl);
        let goal = self.ball_x(team, 0);
        let who = k.map(|kk| g.players[kk].name.as_str()).unwrap_or("Field goal");
        if ok {
            self.score[team as usize] += 3;
            self.down = 0;
            self.emit("field_goal", b, 0, [None, None, None, k], None, (x_from, goal), (false, false, false), 3, Some(team),
                || format!("{who} {dist}-yard field goal is GOOD"));
            self.check_ot();
            if ended { self.end_period(); }
            return Next::Kickoff(team);
        }
        let opp = 1 - team;
        let new_yl = (100 - (self.yl + 8)).min(80);
        self.new_possession(opp, new_yl);
        self.emit("field_goal", b, 0, [None, None, None, k], None, (x_from, goal), (false, false, true), 0, None,
            || format!("{who} {dist}-yard field goal is NO GOOD"));
        self.check_ot();
        if ended { self.end_period(); }
        Next::Scrimmage
    }

    fn punt(&mut self) -> Next {
        let g = self.g;
        let b = self.before();
        let team = self.pos;
        let samples = &g.league.punt_start;
        let mut bk = (((self.yl.clamp(1, 99) - 1) / 10) as usize).min(samples.len().saturating_sub(1));
        while bk > 0 && samples[bk].is_empty() { bk -= 1; }
        let s = samples.get(bk).and_then(|v| pick(self.rng, v)).unwrap_or(80);
        let secs = self.elapsed(KICK_CLASS).min(15);
        let ended = self.tick(secs);
        let opp = 1 - team;
        let x_from = self.ball_x(team, self.yl);
        let abbr = &g.teams[opp as usize].abbr;
        if s == 0 {
            self.pos = opp;
            self.yl = 0;
            self.down = 0;
            self.score[opp as usize] += 6;
            let x = self.ball_x(opp, 0);
            self.emit("punt", b, 0, [None; 4], None, (x_from, x), (false, true, true), 6, Some(opp),
                || format!("Punt returned for a TOUCHDOWN by {abbr}"));
            let next = self.try_after(opp);
            if ended && !self.over { self.end_period(); }
            return next;
        }
        self.new_possession(opp, s);
        let x = self.ball_x(opp, self.yl);
        let spot = if self.recording() { self.spot_text(opp, self.yl) } else { String::new() };
        self.emit("punt", b, 0, [None; 4], None, (x_from, x), (false, false, true), 0, None, || format!("Punt — {abbr} ball at {spot}"));
        self.check_ot();
        if ended { self.end_period(); }
        Next::Scrimmage
    }

    fn end_period(&mut self) {
        if self.over { return; }
        let q = self.qtr;
        let tied = self.score[0] == self.score[1];
        if (q == 4 && !tied) || (q >= 5 && (!self.g.postseason || !tied)) {
            self.over = true;
            return;
        }
        let b = self.before();
        let x = self.ball_x(self.pos, self.yl.clamp(0, 100));
        let label = match q { 1 => "End of 1st quarter", 2 => "Halftime", 3 => "End of 3rd quarter", 4 => "End of regulation — overtime", _ => "End of overtime period" };
        self.qtr += 1;
        self.clock = period_len(self.qtr);
        self.emit("end_period", b, 0, [None; 4], None, (x, x), (false, false, false), 0, None, || label.into());
    }

    // ---- scrimmage --------------------------------------------------------

    fn scrimmage(&mut self) -> Next {
        let g = self.g;
        let lg = &g.league;
        let team = self.pos as usize;
        let diff = self.diff();
        let (q, c) = (self.qtr, self.clock);
        // kneel to end the game / half
        if q >= 4 && diff > 0 && self.down <= 3 && c <= 40 * (4 - self.down) + 2 {
            return self.kneel();
        }
        if q == 2 && c <= 30 && self.yl >= 70 && self.down <= 3 {
            return self.kneel();
        }
        // last-play field goal
        let fg_range = self.yl + 18 <= 58;
        if c <= 12 && fg_range && (q == 2 || (q >= 4 && (-3..=0).contains(&diff))) {
            return self.field_goal();
        }
        if self.down == 4 {
            let w0 = lg.fourth_cells[fourth_cell(self.yl, self.togo, diff, q)];
            let mut w = if w0.iter().sum::<f64>() < 3.0 {
                [1.0, if self.yl > 40 { 4.0 } else { 0.5 }, if self.yl + 18 <= 55 { 4.0 } else { 0.0 }]
            } else { w0 };
            if self.yl + 18 > 64 { w[2] = 0.0; }
            if q >= 4 && c <= 120 && (-3..0).contains(&diff) && fg_range { w = [0.2, 0.0, 1.0]; }
            if q >= 4 && c <= 300 && diff < -3 { w[1] *= 0.1; if diff < -8 { w[2] *= 0.1; } }
            match choose(self.rng, &w) {
                1 => return self.punt(),
                2 => return self.field_goal(),
                _ => {}
            }
        }
        // accepted penalty that wipes out the snap (false start, holding, ...)
        if !lg.pen.is_empty() && self.rng.f64() < lg.pen_rate {
            let pen = lg.pen[(self.rng.next_u64() % lg.pen.len() as u64) as usize];
            return self.penalty(pen);
        }
        let [d, n] = lg.pass_cells[pass_cell(self.down, self.togo, diff, q, c)];
        let base = {
            let [dm, nm] = lg.pass_marg[pass_marg(self.down, self.togo)];
            let prior = if nm > 0.0 { dm / nm } else { 0.58 };
            (d + 25.0 * prior) / (n + 25.0)
        };
        let p_pass = sigmoid(logit(base) + g.teams[team].off.proe);
        self.plays[team] += 1;
        if self.rng.f64() < p_pass { self.pass_play() } else { self.run_play() }
    }

    /// Enforce a penalty at the current spot (half the distance near a goal
    /// line). Clock time is already inside the empirical snap-to-snap times.
    fn penalty(&mut self, pen: [i32; 3]) -> Next {
        let g = self.g;
        let b = self.before();
        let x0 = self.ball_x(self.pos, self.yl);
        let (on_offense, yards, auto_fd) = (pen[0] == 1, pen[1].max(0), pen[2] == 1);
        let mut fd = false;
        let y;
        if on_offense {
            y = if self.yl + yards > 99 { (100 - self.yl) / 2 } else { yards };
            self.yl += y;
            self.togo += y;
        } else {
            y = if yards >= self.yl { self.yl / 2 } else { yards };
            self.yl -= y;
            self.togo -= y;
            if auto_fd || self.togo <= 0 {
                self.down = 1;
                self.togo = 10.min(self.yl);
                fd = true;
            }
        }
        let x1 = self.ball_x(self.pos, self.yl);
        let team = if on_offense { self.pos } else { 1 - self.pos };
        let abbr = &g.teams[team as usize].abbr;
        self.emit("penalty", b, if on_offense { -y } else { y }, [None; 4], None, (x0, x1), (fd, false, false), 0, None,
            || format!("PENALTY on {abbr}, {y} yards{}", if fd { ", automatic first down" } else { "" }));
        Next::Scrimmage
    }

    /// Live-ball foul enforced after a completed play, with probability from the data.
    fn maybe_live_penalty(&mut self) {
        let lg = &self.g.league;
        if self.tod || self.down < 1 || lg.pen_live.is_empty() || self.rng.f64() >= lg.pen_live_rate {
            return;
        }
        let pen = lg.pen_live[(self.rng.next_u64() % lg.pen_live.len() as u64) as usize];
        self.penalty(pen);
    }

    fn kneel(&mut self) -> Next {
        let g = self.g;
        let b = self.before();
        let qb = g.teams[self.pos as usize].off.qb;
        self.stats[qb][RUSH_ATT] += 1;
        self.stats[qb][RUSH_YDS] -= 1;
        self.plays[self.pos as usize] += 1;
        let x0 = self.ball_x(self.pos, self.yl);
        self.yl = (self.yl + 1).min(99);
        let secs = 40.min(self.clock).max(1);
        let ended = self.tick(secs);
        self.advance_down(-1);
        let x1 = self.ball_x(self.pos, self.yl);
        self.emit("kneel", b, -1, [None, None, Some(qb), None], Some("middle"), (x0, x1), (false, false, false), 0, None,
            || format!("{} kneels", g.players[qb].name));
        self.after_play(ended)
    }

    /// Down/distance after a non-scoring, non-turnover gain. Returns true on a first down.
    fn advance_down(&mut self, yards: i32) -> bool {
        if yards >= self.togo {
            self.down = 1;
            self.togo = 10.min(self.yl);
            true
        } else {
            self.down += 1;
            self.togo -= yards;
            if self.down > 4 {
                // turnover on downs: flip before the play's event is recorded
                let opp = 1 - self.pos;
                let yl = 100 - self.yl;
                self.new_possession(opp, yl);
                self.tod = true;
            }
            false
        }
    }

    fn after_play(&mut self, ended: bool) -> Next {
        if self.tod {
            self.tod = false;
            let g = self.g;
            let b = self.before();
            let x = self.ball_x(self.pos, self.yl);
            let team = self.pos;
            self.emit("turnover_on_downs", b, 0, [None; 4], None, (x, x), (false, false, true), 0, None,
                || format!("Turnover on downs — {} ball", g.teams[team as usize].abbr));
            self.check_ot();
        }
        if ended { self.end_period(); }
        Next::Scrimmage
    }

    /// Gain on a play: drawn from the league's real gains in the same field
    /// band and down-and-distance group (so 3rd-and-1 runs and 1st-and-10
    /// runs differ, and gains compress near the goal line), then positive
    /// gains are scaled by the player's shrunk yards-per-play ratio and the
    /// defence's shrunk yards-allowed ratio.
    fn yards_mix(&mut self, player_ratio: f64, league: &[Vec<i32>], def_ratio: f64) -> i32 {
        let cell = if league.len() == 1 { 0 } else { gain_cell(self.yl, self.down, self.togo).min(league.len() - 1) };
        let raw = pick(self.rng, &league[cell]).unwrap_or(0);
        let scale = player_ratio * def_ratio;
        if raw > 0 && (scale - 1.0).abs() > 1e-9 {
            let s = raw as f64 * scale;
            let fl = s.floor();
            fl as i32 + (self.rng.f64() < s - fl) as i32
        } else { raw }
    }

    fn sit(&self) -> usize { situation(self.qtr, self.clock, self.diff()) }
    /// Home-field factor for the team with the ball (home: f, away: 1/f).
    fn hf(&self, f: f64) -> f64 { if self.pos == 1 { f } else { 1.0 / f } }

    fn pass_play(&mut self) -> Next {
        let g = self.g;
        let lg = &g.league;
        let b = self.before();
        let t = self.pos as usize;
        let off = &g.teams[t].off;
        let def = &g.teams[1 - t].def;
        let qb = off.qb;
        let qbn = g.players[qb].name.as_str();
        let hfa = &lg.home;
        let ddg = dd_group(self.down, self.togo);
        let p_sack = from_odds(odds(combine(off.sack.mean, def.sack.mean, lg.sack)) * self.hf(hfa.sack) * lg.dd_sack[ddg]);
        let p_scr = from_odds(odds(off.scramble.mean) * lg.dd_scramble[ddg]);
        let u = self.rng.f64();
        let x0 = self.ball_x(self.pos, self.yl);
        let sit = self.sit();
        if u < p_sack {
            let loss = pick(self.rng, &lg.sack_yards).unwrap_or(7).max(0);
            self.stats[qb][SACKED] += 1;
            if self.yl + loss >= 100 {
                return self.safety(b, x0, move || format!("{qbn} sacked in the end zone — SAFETY"));
            }
            self.yl += loss;
            if self.rng.f64() < lg.fumble_sack {
                self.stats[qb][FUM_LOST] += 1;
                return self.turnover(b, x0, false, None, Some(qb), "middle");
            }
            let secs = self.elapsed(clock_class(sit, RUN_IN));
            let ended = self.tick(secs);
            self.advance_down(-loss);
            let x1 = self.ball_x(self.pos, self.yl);
            self.emit("sack", b, -loss, [Some(qb), None, None, None], None, (x0, x1), (false, false, false), 0, None,
                || format!("{qbn} sacked for a loss of {loss}"));
            if !ended { self.maybe_live_penalty(); }
            return self.after_play(ended);
        }
        if u < p_sack + p_scr * (1.0 - p_sack) {
            let loc = LOCS[choose(self.rng, &off.qb_loc)];
            let y = self.yards_mix(1.0, std::slice::from_ref(&lg.scramble_yards), 1.0);
            return self.ball_carrier(b, x0, qb, y, true, loc, "scramble");
        }
        // pass attempt
        self.stats[qb][PASS_ATT] += 1;
        let ri = choose(self.rng, &off.receivers.iter().map(|r| r.share).collect::<Vec<_>>());
        let rec = off.receivers.get(ri);
        let loc = LOCS[choose(self.rng, &rec.map(|r| r.loc).unwrap_or(lg.loc_pass))];
        if let Some(r) = rec { self.stats[r.p][TGT] += 1; }
        let p_int = from_odds(odds(combine(off.int.mean, def.int.mean, lg.int)) * self.hf(hfa.int) * lg.dd_int[ddg]);
        if self.rng.f64() < p_int {
            self.stats[qb][INT] += 1;
            return self.turnover(b, x0, true, Some(qb), rec.map(|r| r.p), loc);
        }
        let Some(rec) = rec else {
            let secs = self.elapsed(clock_class(sit, INCOMPLETE));
            let ended = self.tick(secs);
            self.advance_down(0);
            self.emit("pass_incomplete", b, 0, [Some(qb), None, None, None], Some(loc), (x0, x0), (false, false, false), 0, None,
                || format!("{qbn} pass incomplete"));
            if !ended { self.maybe_live_penalty(); }
            return self.after_play(ended);
        };
        let rn = g.players[rec.p].name.as_str();
        let cref = if off.catch_ref > 0.0 { off.catch_ref } else { catch_ref(&off.receivers) };
        let team = from_odds(odds(combine(off.cmp.mean, def.cmp.mean, lg.cmp)) * self.hf(hfa.cmp) * lg.dd_cmp[ddg]);
        let p_cmp = (team * rec.catch.mean / cref).min(0.97);
        if self.rng.f64() >= p_cmp {
            let secs = self.elapsed(clock_class(sit, INCOMPLETE));
            let ended = self.tick(secs);
            self.advance_down(0);
            self.emit("pass_incomplete", b, 0, [Some(qb), Some(rec.p), None, None], Some(loc), (x0, x0), (false, false, false), 0, None,
                || format!("{qbn} pass incomplete {loc}, intended for {rn}"));
            if !ended { self.maybe_live_penalty(); }
            return self.after_play(ended);
        }
        let mut y = self.yards_mix(rec.ratio, &lg.cmp_yards, def.ypr_ratio * self.hf(hfa.ypr));
        y = y.min(self.yl).max(self.yl - 99);
        self.stats[qb][CMP] += 1;
        self.stats[qb][PASS_YDS] += y;
        self.stats[rec.p][REC] += 1;
        self.stats[rec.p][REC_YDS] += y;
        if y >= self.yl {
            self.stats[qb][PASS_TD] += 1;
            self.stats[rec.p][REC_TD] += 1;
            return self.touchdown(b, x0, y, [Some(qb), Some(rec.p), None, None], Some(loc), "pass_complete",
                move || format!("{qbn} pass {loc} to {rn} for {y} yards, TOUCHDOWN"));
        }
        self.yl -= y;
        if self.rng.f64() < lg.fumble_rec {
            self.stats[rec.p][FUM_LOST] += 1;
            return self.turnover(b, x0, false, None, Some(rec.p), loc);
        }
        let oob = self.rng.f64() < lg.oob_cmp[sit];
        let secs = self.elapsed(clock_class(sit, if oob { OOB } else { RUN_IN }));
        let ended = self.tick(secs);
        let fd = self.advance_down(y);
        let x1 = self.ball_x(self.pos, self.yl);
        self.emit("pass_complete", b, y, [Some(qb), Some(rec.p), None, None], Some(loc), (x0, x1), (fd, false, false), 0, None,
            || format!("{qbn} pass {loc} to {rn} for {y} yard{}{}", if y.abs() == 1 { "" } else { "s" }, if fd { ", first down" } else { "" }));
        if !ended { self.maybe_live_penalty(); }
        self.after_play(ended)
    }

    fn run_play(&mut self) -> Next {
        let g = self.g;
        let lg = &g.league;
        let b = self.before();
        let t = self.pos as usize;
        let off = &g.teams[t].off;
        let x0 = self.ball_x(self.pos, self.yl);
        let league = &lg.run_yards;
        let ri = choose(self.rng, &off.rushers.iter().map(|r| r.share).collect::<Vec<_>>());
        let Some(r) = off.rushers.get(ri) else {
            let y = self.yards_mix(1.0, league, 1.0);
            return self.ball_carrier(b, x0, off.qb, y, false, "middle", "rush");
        };
        let loc = LOCS[choose(self.rng, &r.loc)];
        let y = self.yards_mix(r.ratio, league, g.teams[1 - t].def.ypc_ratio * self.hf(lg.home.ypc));
        self.ball_carrier(b, x0, r.p, y, false, loc, "rush")
    }

    /// Shared resolution for designed runs and scrambles.
    #[allow(clippy::too_many_arguments)]
    fn ball_carrier(&mut self, b: Before, x0: f64, p: usize, y: i32, scramble: bool, loc: &'static str, kind: &'static str) -> Next {
        let g = self.g;
        let sit = self.sit();
        let nm = g.players[p].name.as_str();
        let verb = if scramble { "scrambles" } else { "rush" };
        let y = y.min(self.yl);
        self.stats[p][RUSH_ATT] += 1;
        self.stats[p][RUSH_YDS] += y;
        if self.yl - y >= 100 {
            return self.safety(b, x0, move || format!("{nm} {verb} {loc}, tackled in the end zone — SAFETY"));
        }
        let who = if scramble { [Some(p), None, Some(p), None] } else { [None, None, Some(p), None] };
        if y >= self.yl {
            self.stats[p][RUSH_TD] += 1;
            return self.touchdown(b, x0, y, who, Some(loc), kind, move || format!("{nm} {verb} {loc} for {y} yards, TOUCHDOWN"));
        }
        self.yl -= y;
        if self.rng.f64() < g.league.fumble_run {
            self.stats[p][FUM_LOST] += 1;
            return self.turnover(b, x0, false, None, Some(p), loc);
        }
        let oob = self.rng.f64() < g.league.oob_run[sit];
        let secs = self.elapsed(clock_class(sit, if oob { OOB } else { RUN_IN }));
        let ended = self.tick(secs);
        let fd = self.advance_down(y);
        let x1 = self.ball_x(self.pos, self.yl);
        self.emit(kind, b, y, who, Some(loc), (x0, x1), (fd, false, false), 0, None,
            || format!("{nm} {verb} {loc} for {y} yard{}{}", if y.abs() == 1 { "" } else { "s" }, if fd { ", first down" } else { "" }));
        if !ended { self.maybe_live_penalty(); }
        self.after_play(ended)
    }

    #[allow(clippy::too_many_arguments)]
    fn touchdown(&mut self, b: Before, x0: f64, y: i32, who: [Option<usize>; 4], loc: Option<&'static str>,
                 kind: &'static str, text: impl FnOnce() -> String) -> Next {
        let team = self.pos;
        self.score[team as usize] += 6;
        let secs = self.elapsed(clock_class(self.sit(), OTHER_STOP)).min(10);
        let ended = self.tick(secs);
        self.yl = 0;
        self.down = 0;
        let x1 = self.ball_x(team, 0);
        self.emit(kind, b, y, who, loc, (x0, x1), (true, true, false), 6, Some(team), text);
        let next = self.try_after(team);
        if ended && !self.over { self.end_period(); }
        next
    }

    fn safety(&mut self, b: Before, x0: f64, text: impl FnOnce() -> String) -> Next {
        let team = self.pos;
        let opp = 1 - team;
        self.score[opp as usize] += 2;
        let secs = self.elapsed(clock_class(self.sit(), OTHER_STOP)).min(10);
        let ended = self.tick(secs);
        self.yl = 100;
        self.down = 0;
        let x1 = self.ball_x(team, 100);
        self.emit("safety", b, 0, [None; 4], None, (x0, x1), (false, false, false), 2, Some(opp), text);
        if self.qtr >= 5 { self.over = true; }
        if ended { self.end_period(); }
        // free kick by the team that conceded
        Next::Kickoff(team)
    }

    /// Interception (at the line of scrimmage) or lost fumble (at the spot
    /// already in `self.yl`): empirical return, or a return touchdown.
    fn turnover(&mut self, b: Before, x0: f64, interception: bool, qb: Option<usize>, other: Option<usize>, loc: &'static str) -> Next {
        let g = self.g;
        let lg = &g.league;
        let team = self.pos;
        let opp = 1 - team;
        let (td_rate, returns) = if interception { (lg.int_td, &lg.int_return) } else { (lg.fum_td, &lg.fum_return) };
        let qbn = qb.map(|q| g.players[q].name.as_str()).unwrap_or("");
        let on = other.map(|o| g.players[o].name.as_str());
        let oabbr = &g.teams[opp as usize].abbr;
        let lead = move || if interception {
            format!("{qbn} pass {loc} INTERCEPTED{}", on.map(|n| format!(" (intended for {n})")).unwrap_or_default())
        } else {
            format!("{} FUMBLES, recovered by {oabbr}", on.unwrap_or("Ball carrier"))
        };
        let secs = self.elapsed(clock_class(self.sit(), OTHER_STOP)).min(12);
        let ended = self.tick(secs);
        let kind = if interception { "interception" } else { "fumble_lost" };
        let who = if interception { [qb, other, None, None] } else { [None, other, other, None] };
        if self.rng.f64() < td_rate {
            self.pos = opp;
            self.yl = 0;
            self.down = 0;
            self.score[opp as usize] += 6;
            let x1 = self.ball_x(opp, 0);
            self.emit(kind, b, 0, who, Some(loc), (x0, x1), (false, true, true), 6, Some(opp), || format!("{}, returned for a TOUCHDOWN", lead()));
            let next = self.try_after(opp);
            if ended && !self.over { self.end_period(); }
            return next;
        }
        let delta = pick(self.rng, returns).unwrap_or(0);
        self.new_possession(opp, 100 - self.yl + delta);
        let x1 = self.ball_x(opp, self.yl);
        let spot = if self.recording() { self.spot_text(opp, self.yl) } else { String::new() };
        self.emit(kind, b, 0, who, Some(loc), (x0, x1), (false, false, true), 0, None, || format!("{} — {oabbr} ball at {spot}", lead()));
        self.check_ot();
        if ended { self.end_period(); }
        Next::Scrimmage
    }
}

/// Play one full game. `events` = Some(...) records the authoritative event log.
pub fn play_game(g: &GameInput, rng: &mut Rng, events: Option<&mut Vec<Event>>) -> GameResult {
    let opening = (rng.f64() < 0.5) as u8;
    let mut s = G {
        g, rng, qtr: 1, clock: 900, score: [0, 0], pos: opening, yl: 70, down: 1, togo: 10,
        stats: vec![[0; NSTAT]; g.players.len()], plays: [0, 0], ot_poss: [0, 0], opening_receiver: opening,
        over: false, tod: false, events,
    };
    let mut next = Next::Kickoff(1 - opening);
    let mut guard = 0;
    let mut half_kicked = false;
    let mut ot_kicked = 0;
    while !s.over && guard < 600 {
        guard += 1;
        // period transitions that need a kickoff
        if s.qtr == 3 && !half_kicked && s.clock == 900 {
            half_kicked = true;
            next = Next::Kickoff(s.opening_receiver);
        }
        if s.qtr >= 5 && ot_kicked < s.qtr - 4 && s.clock == period_len(s.qtr) {
            if ot_kicked == 0 {
                let recv = (s.rng.f64() < 0.5) as u8;
                next = Next::Kickoff(1 - recv);
            }
            ot_kicked = s.qtr - 4;
        }
        next = match next {
            Next::Kickoff(k) => s.kickoff(k),
            Next::Scrimmage => s.scrimmage(),
        };
        if s.over { break; }
    }
    let b = s.before();
    let x = s.ball_x(s.pos, s.yl.clamp(0, 100));
    let txt = format!("FINAL{}: {} {} — {} {}", if s.qtr >= 5 { " (OT)" } else { "" },
        g.teams[0].abbr, s.score[0], g.teams[1].abbr, s.score[1]);
    s.clock = 0;
    s.emit("game_end", b, 0, [None; 4], None, (x, x), (false, false, false), 0, None, move || txt);
    GameResult { score: s.score, stats: s.stats, plays: s.plays, overtime: s.qtr >= 5,
        active: g.players.iter().map(|p| p.p_active > 0.5).collect() }
}

// ---------------------------------------------------------------------------
// Ensemble
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
pub struct Hist {
    pub counts: Vec<u64>, // index = value - offset
    pub offset: i32,
    pub n: u64,
    pub sum: f64,
}
impl Hist {
    fn new(offset: i32) -> Self { Hist { counts: Vec::new(), offset, n: 0, sum: 0.0 } }
    fn add(&mut self, v: i32) {
        let i = (v - self.offset).max(0) as usize;
        if i >= self.counts.len() { self.counts.resize(i + 1, 0); }
        self.counts[i] += 1;
        self.n += 1;
        self.sum += v as f64;
    }
    fn merge(&mut self, o: &Hist) {
        if o.counts.len() > self.counts.len() { self.counts.resize(o.counts.len(), 0); }
        for (i, c) in o.counts.iter().enumerate() { self.counts[i] += c; }
        self.n += o.n;
        self.sum += o.sum;
    }
    pub fn mean(&self) -> f64 { if self.n == 0 { 0.0 } else { self.sum / self.n as f64 } }
    pub fn quantile(&self, q: f64) -> i32 {
        let target = (q * self.n as f64).ceil().max(1.0) as u64;
        let mut acc = 0;
        for (i, c) in self.counts.iter().enumerate() {
            acc += c;
            if acc >= target { return i as i32 + self.offset; }
        }
        self.offset + self.counts.len() as i32 - 1
    }
    /// (P(over), P(under), P(push), MC standard error of P(over))
    pub fn line(&self, line: f64) -> (f64, f64, f64, f64) {
        let (mut o, mut u, mut p) = (0u64, 0u64, 0u64);
        for (i, c) in self.counts.iter().enumerate() {
            let v = (i as i32 + self.offset) as f64;
            if v > line { o += c } else if v < line { u += c } else { p += c }
        }
        let n = self.n.max(1) as f64;
        let po = o as f64 / n;
        (po, u as f64 / n, p as f64 / n, (po * (1.0 - po) / n).sqrt())
    }
    pub fn pmf(&self) -> (i32, Vec<f64>) {
        let n = self.n.max(1) as f64;
        (self.offset, self.counts.iter().map(|c| *c as f64 / n).collect())
    }
}

pub struct Ensemble {
    pub n: u64,
    pub score: [Hist; 2],
    pub total: Hist,
    pub margin: Hist, // home - away
    pub home_win: u64,
    pub away_win: u64,
    pub ties: u64,
    pub overtime: u64,
    pub plays: [Hist; 2],
    pub player: Vec<Vec<Hist>>, // [player][stat]
    pub anytime_td: Vec<u64>,
    /// games in which each player was active (player stats are conditional on this)
    pub active_n: Vec<u64>,
}

impl Ensemble {
    fn new(np: usize) -> Self {
        let ph = || (0..NSTAT).map(|k| Hist::new(if matches!(k, PASS_YDS | RUSH_YDS | REC_YDS) { -30 } else { 0 })).collect::<Vec<_>>();
        Ensemble {
            n: 0, score: [Hist::new(0), Hist::new(0)], total: Hist::new(0), margin: Hist::new(-80),
            home_win: 0, away_win: 0, ties: 0, overtime: 0, plays: [Hist::new(0), Hist::new(0)],
            player: (0..np).map(|_| ph()).collect(), anytime_td: vec![0; np], active_n: vec![0; np],
        }
    }
    fn add(&mut self, r: &GameResult) {
        self.n += 1;
        self.score[0].add(r.score[0]);
        self.score[1].add(r.score[1]);
        self.total.add(r.score[0] + r.score[1]);
        self.margin.add(r.score[1] - r.score[0]);
        match r.score[1].cmp(&r.score[0]) {
            std::cmp::Ordering::Greater => self.home_win += 1,
            std::cmp::Ordering::Less => self.away_win += 1,
            _ => self.ties += 1,
        }
        self.overtime += r.overtime as u64;
        self.plays[0].add(r.plays[0]);
        self.plays[1].add(r.plays[1]);
        for (i, s) in r.stats.iter().enumerate() {
            // player props are conditional on playing (books void inactive players)
            if !r.active.get(i).copied().unwrap_or(true) { continue; }
            self.active_n[i] += 1;
            for k in 0..NSTAT { self.player[i][k].add(s[k]); }
            if s[RUSH_TD] + s[REC_TD] > 0 { self.anytime_td[i] += 1; }
        }
    }
    fn merge(&mut self, o: &Ensemble) {
        self.n += o.n;
        for k in 0..2 { self.score[k].merge(&o.score[k]); self.plays[k].merge(&o.plays[k]); }
        self.total.merge(&o.total);
        self.margin.merge(&o.margin);
        self.home_win += o.home_win; self.away_win += o.away_win; self.ties += o.ties; self.overtime += o.overtime;
        for (a, b) in self.player.iter_mut().zip(&o.player) { for (x, y) in a.iter_mut().zip(b) { x.merge(y); } }
        for (a, b) in self.anytime_td.iter_mut().zip(&o.anytime_td) { *a += b; }
        for (a, b) in self.active_n.iter_mut().zip(&o.active_n) { *a += b; }
    }
}

/// Monte Carlo ensemble: `n` games in `draws` parameter batches (each batch
/// redraws every rate/share from its posterior). Batches are independent and
/// merged in order, so the result depends only on (inputs, n, draws, seed) —
/// not on the number of threads.
pub fn simulate(g: &GameInput, n: usize, draws: usize, seed: u64) -> Ensemble {
    let draws = draws.clamp(1, n.max(1));
    let threads = std::thread::available_parallelism().map(|x| x.get()).unwrap_or(2).clamp(1, 4);
    let per = n / draws;
    let extra = n % draws;
    let batches: Vec<(usize, usize)> = (0..draws).map(|b| (b, per + (b < extra) as usize)).collect();
    let chunks: Vec<Vec<(usize, usize)>> = (0..threads).map(|t| batches.iter().filter(|(b, _)| b % threads == t).cloned().collect()).collect();
    let np = g.players.len();
    let parts: Vec<Ensemble> = std::thread::scope(|sc| {
        let hs: Vec<_> = chunks.iter().map(|ch| sc.spawn(move || {
            let mut e = Ensemble::new(np);
            for (b, m) in ch {
                let mut prng = Rng::new(derive_seed(seed, &["params", &b.to_string()]));
                let gi = perturb(g, &mut prng);
                let mut rng = Rng::new(derive_seed(seed, &["games", &b.to_string()]));
                for _ in 0..*m {
                    let r = play_game(&gi, &mut rng, None);
                    e.add(&r);
                }
            }
            e
        })).collect();
        hs.into_iter().map(|h| h.join().expect("sim thread")).collect()
    });
    let mut out = Ensemble::new(np);
    for p in &parts { out.merge(p); }
    out
}

/// Interactive mode: one seeded game with its event log.
pub fn single_game(g: &GameInput, seed: u64) -> (GameResult, Vec<Event>) {
    let mut prng = Rng::new(derive_seed(seed, &["params", "single"]));
    let gi = perturb(g, &mut prng);
    let mut rng = Rng::new(derive_seed(seed, &["games", "single"]));
    let mut ev = Vec::new();
    let r = play_game(&gi, &mut rng, Some(&mut ev));
    (r, ev)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn toy_league() -> League {
        let mut pass_cells = vec![[55.0, 100.0]; PASS_CELLS];
        for c in pass_cells.iter_mut() { c[0] = 58.0; }
        let mut fourth = vec![[1.0, 8.0, 1.0]; FOURTH_CELLS];
        for (i, c) in fourth.iter_mut().enumerate() {
            let y = i / 8;
            if y <= 3 { *c = [3.0, 0.0, 7.0]; }
        }
        let gains: Vec<i32> = vec![-2, 0, 1, 2, 3, 3, 4, 4, 5, 6, 7, 8, 10, 12, 15, 22, 35];
        let cmp: Vec<i32> = vec![0, 3, 5, 6, 7, 8, 9, 10, 11, 12, 14, 16, 18, 22, 28, 40];
        League {
            season: 2025, pass_cells, pass_marg: vec![[58.0, 100.0]; 15], fourth_cells: fourth,
            sack: 0.07, scramble: 0.04, int: 0.023, cmp: 0.65,
            kappa: Kappas { sack: 200.0, scramble: 200.0, int: 400.0, cmp_qb: 150.0, catch: 60.0, def_sack: 400.0, def_int: 400.0, def_cmp: 400.0, ypc: 80.0, ypr: 40.0, def_yards: 500.0, proe: 300.0 },
            cmp_yards: (0..GAIN_CELLS).map(|c| if c / DD <= 2 { vec![1, 2, 3, 4, 5, 6, 8, 10] } else { cmp.clone() }).collect(),
            run_yards: (0..GAIN_CELLS).map(|c| if c / DD <= 2 { vec![-1, 0, 1, 1, 2, 3, 5] } else { gains.clone() }).collect(),
            cmp_mean: 11.0, run_mean: 4.3, dd_sack: [1.0; DD], dd_scramble: [1.0; DD], dd_int: [1.0; DD], dd_cmp: [1.0; DD],
            scramble_yards: vec![2, 4, 6, 8, 10], sack_yards: vec![5, 6, 7, 8, 9],
            fumble_run: 0.008, fumble_rec: 0.006, oob_run: [0.05, 0.15, 0.02], oob_cmp: [0.15, 0.35, 0.05],
            elapsed: vec![vec![35, 38, 40], vec![25, 30], vec![5, 6], vec![5, 7], vec![15, 20], vec![6, 8], vec![5, 6], vec![5, 7],
                          vec![40, 42], vec![25, 30], vec![5, 6], vec![5, 7], vec![5, 6]],
            kickoff_start: vec![70, 70, 70, 72, 68, 75, 0], punt_start: vec![vec![80, 85, 88, 90, 92, 0]; 10],
            int_return: vec![-5, 0, 5, 10], int_td: 0.05, fum_return: vec![-2, 0, 3], fum_td: 0.04,
            fg_a: 5.8, fg_b: -0.105, xp: 0.95, two_pt_go: vec![0.1; 7], two_pt_success: 0.48,
            loc_pass: [0.37, 0.24, 0.39], loc_run: [0.38, 0.24, 0.38], home: HomeField::default(),
            pen_rate: 0.06, pen_live_rate: 0.03, pen: vec![[1, 5, 0], [1, 10, 0], [0, 5, 0], [0, 15, 1]],
            pen_live: vec![[0, 15, 1], [1, 10, 0]], fumble_sack: 0.1,
        }
    }
    pub fn toy_game() -> GameInput {
        let mut players = Vec::new();
        let team = |abbr: &str, players: &mut Vec<Player>| {
            let base = players.len();
            for (n, pos) in [("QB", "QB"), ("RB", "RB"), ("WR1", "WR"), ("WR2", "WR"), ("TE", "TE"), ("K", "K")] {
                players.push(Player { id: format!("{abbr}-{n}"), name: format!("{abbr} {n}"), pos: pos.into(), p_active: 1.0 });
            }
            let r = |p: usize, share: f64| Receiver { p, share, catch: Rate { mean: 0.65, n: 100.0 }, yards: vec![3, 5, 8, 12, 20], ratio: 1.0, loc: [0.4, 0.2, 0.4] };
            TeamInput {
                abbr: abbr.into(),
                off: Offense {
                    qb: base, kicker: Some(base + 5), proe: 0.0,
                    sack: Rate { mean: 0.07, n: 300.0 }, scramble: Rate { mean: 0.04, n: 300.0 },
                    int: Rate { mean: 0.023, n: 500.0 }, cmp: Rate { mean: 0.65, n: 400.0 },
                    qb_scramble_yards: vec![4, 6, 9], qb_loc: [0.4, 0.2, 0.4],
                    receivers: vec![r(base + 2, 0.3), r(base + 3, 0.25), r(base + 4, 0.2), r(base + 1, 0.25)],
                    rushers: vec![Rusher { p: base + 1, share: 0.8, yards: vec![1, 2, 3, 4, 6, 9], ratio: 1.0, loc: [0.4, 0.2, 0.4] },
                                  Rusher { p: base, share: 0.2, yards: vec![2, 5], ratio: 1.0, loc: [0.3, 0.4, 0.3] }],
                    share_n: [300.0, 200.0], catch_ref: 0.0, backup_qb: None,
                },
                def: Defense { sack: Rate { mean: 0.07, n: 400.0 }, int: Rate { mean: 0.023, n: 400.0 }, cmp: Rate { mean: 0.65, n: 400.0 }, ypc_ratio: 1.0, ypr_ratio: 1.0 },
            }
        };
        let a = team("AWY", &mut players);
        let h = team("HOM", &mut players);
        GameInput { players, teams: [a, h], league: toy_league(), postseason: false }
    }

    #[test]
    fn game_invariants() {
        let g = toy_game();
        for seed in 0..300u64 {
            let mut ev = Vec::new();
            let mut rng = Rng::new(seed);
            let r = play_game(&g, &mut rng, Some(&mut ev));
            // score = sum of scoring events
            let mut pts = [0, 0];
            for e in &ev { if let Some(t) = e.scoring_team { pts[t as usize] += e.points; } }
            assert_eq!(pts, r.score, "seed {seed}");
            // sequence numbers are contiguous; last is game_end with final score
            for (i, e) in ev.iter().enumerate() { assert_eq!(e.seq as usize, i + 1); }
            let last = ev.last().unwrap();
            assert_eq!(last.kind, "game_end");
            assert_eq!(last.state.score, r.score);
            // regulation games are decided unless OT ran out
            if !r.overtime { assert_ne!(r.score[0], r.score[1], "seed {seed}"); }
            // box score: QB passing yards == his receivers' yards; TDs consistent with scoring
            for t in 0..2 {
                let qb = g.teams[t].off.qb;
                let rec_yds: i32 = g.teams[t].off.receivers.iter().map(|x| r.stats[x.p][REC_YDS]).sum();
                let rec: i32 = g.teams[t].off.receivers.iter().map(|x| r.stats[x.p][REC]).sum();
                assert_eq!(r.stats[qb][PASS_YDS], rec_yds);
                assert_eq!(r.stats[qb][CMP], rec);
                assert!(r.stats[qb][CMP] <= r.stats[qb][PASS_ATT]);
            }
            // clock and downs are always legal
            for e in &ev {
                assert!(e.state.clock >= 0 && e.state.clock <= 900);
                assert!((0..=4).contains(&e.state.down));
                assert!(e.state.ball_x >= 0.0 && e.state.ball_x <= 100.0);
            }
        }
    }

    #[test]
    fn reproducible() {
        let g = toy_game();
        let (a, ea) = single_game(&g, 42);
        let (b, eb) = single_game(&g, 42);
        assert_eq!(a.score, b.score);
        assert_eq!(ea.len(), eb.len());
        let e1 = simulate(&g, 2000, 20, 7);
        let e2 = simulate(&g, 2000, 20, 7);
        assert_eq!(e1.home_win, e2.home_win);
        assert_eq!(e1.total.sum, e2.total.sum);
        assert_eq!(e1.n, 2000);
    }

    #[test]
    fn symmetric_teams_are_close_to_even() {
        let g = toy_game();
        let e = simulate(&g, 20000, 50, 1);
        let hw = e.home_win as f64 / e.n as f64;
        let aw = e.away_win as f64 / e.n as f64;
        assert!((hw - aw).abs() < 0.03, "home {hw} away {aw}");
        let plays = e.plays[0].mean() + e.plays[1].mean();
        assert!(plays > 80.0 && plays < 200.0, "plays {plays}");
    }

    #[test]
    fn inactive_players_get_nothing_and_backup_qb_plays() {
        let mut g = toy_game();
        // away WR1 (index 2) and away QB (index 0) ruled out; the RB is a backup QB stand-in
        g.players[2].p_active = 0.0;
        g.players[0].p_active = 0.0;
        g.teams[0].off.backup_qb = Some(BackupQb { p: 3, sack: Rate { mean: 0.08, n: 50.0 }, scramble: Rate { mean: 0.03, n: 50.0 },
            int: Rate { mean: 0.03, n: 50.0 }, cmp: Rate { mean: 0.6, n: 50.0 }, loc: [0.4, 0.2, 0.4] });
        let e = simulate(&g, 3000, 10, 3);
        assert_eq!(e.active_n[2], 0);
        assert_eq!(e.active_n[0], 0);
        assert_eq!(e.active_n[3], 3000);
        assert!(e.player[3][PASS_ATT].mean() > 20.0, "backup QB throws");
        // a 50% player is active in roughly half the draws
        let mut g2 = toy_game();
        g2.players[3].p_active = 0.5;
        let e2 = simulate(&g2, 4000, 200, 9);
        let f = e2.active_n[3] as f64 / 4000.0;
        assert!((f - 0.5).abs() < 0.12, "active share {f}");
    }

    #[test]
    fn combine_identity() {
        assert!((combine(0.3, 0.2, 0.2) - 0.3).abs() < 1e-9);
        assert!((combine(0.2, 0.3, 0.2) - 0.3).abs() < 1e-9);
    }
}

