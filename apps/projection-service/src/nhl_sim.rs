//! NHL event-level game simulator (pure; no I/O).
//!
//! Continuous time. Within a state (strength x who is on the power play x
//! goalies pulled) shot attempts and penalties arrive as competing Poisson
//! processes; the next event is drawn exactly (Gillespie), and state changes
//! (penalty expiry, goalie pull, period end) cut the draw.
//!
//! * Attempt -> blocked / missed / on goal (team x strength fractions).
//! * Shooter by his real share of the team's attempts at that strength; the
//!   location is drawn from his real attempts; on goal -> goal with the fitted
//!   expected-goals model at that spot x shooter finishing x goalie (both from
//!   goals vs expected goals, shrunk). Assists by assist share.
//! * Minor penalty -> 2:00 power play, ended early by a power-play goal.
//! * Trailing by 1-2 late in the 3rd the goalie is pulled at a time drawn from
//!   real pulls; 6v5 attack and empty-net rates are the league's.
//! * Regular season: 5-minute 3-on-3 overtime then a shootout; playoffs: 20-
//!   minute 5-on-5 sudden-death periods.
//! Not modelled: lines / shifts (shares stand in for deployment), fights,
//! majors beyond their duration, coincidental minors, delayed penalties.

use serde::{Deserialize, Serialize};

pub use crate::mlb_sim::{derive_seed, Rng};

pub const EV: usize = 0;
pub const PP: usize = 1;
pub const SH: usize = 2;

#[derive(Clone, Copy, Serialize, Deserialize, Debug)]
pub struct Xg {
    pub b0: f64,
    pub dist: f64,
    pub angle: f64,
    pub pp: f64,
    /// shot-type offsets: wrist snap slap backhand tip-in deflected wrap-around other
    pub types: [f64; 8],
}
pub const SHOT_TYPES: [&str; 8] = ["wrist", "snap", "slap", "backhand", "tip-in", "deflected", "wrap-around", "other"];
pub fn shot_type_idx(s: &str) -> usize { SHOT_TYPES.iter().position(|t| *t == s).unwrap_or(7) }
impl Xg {
    pub fn p(&self, x: f64, y: f64, ty: usize, pp: bool) -> f64 {
        let dx = (89.0 - x).max(0.5);
        let dist = (dx * dx + y * y).sqrt();
        let angle = (y.abs() / dx).atan();
        let z = self.b0 + self.dist * dist + self.angle * angle + if pp { self.pp } else { 0.0 } + self.types[ty.min(7)];
        1.0 / (1.0 + (-z).exp())
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct League {
    pub season: i32,
    /// attempts per 60 for the team at EV / on the PP / shorthanded
    pub att60: [f64; 3],
    /// fractions of attempts blocked and missed, by strength
    pub blocked: [f64; 3],
    pub missed: [f64; 3],
    pub pen60: f64,                // minors taken per team per 60
    pub pen_durations: Vec<i32>,   // minutes, empirical
    pub xg: Xg,
    pub p_assist1: [f64; 3],
    pub p_assist2: [f64; 3],
    pub ot3_att60: f64,            // 3-on-3 attempts per team per 60
    pub so_pct: f64,               // shootout attempt success
    pub pull_secs_left: Vec<i32>,  // seconds left when trailing teams pulled (1-2 goal deficits)
    pub pull_rate: f64,            // share of eligible situations with a pull
    pub en_att60: f64,             // attempts per 60 by the team facing the empty net
    pub en_goal_pct: f64,          // goals per unblocked attempt on an empty net
    pub sixv5_att60: f64,          // attempts per 60 by the team with six skaters
    pub home_edge: f64,            // attempt-rate multiplier for the home team
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Skater {
    pub id: i64,
    pub name: String,
    pub pos: String,
    #[serde(default = "one")]
    pub p_active: f64,
    pub att_share: [f64; 3],
    pub ast_share: [f64; 3],
    /// odds multiplier on goal given on goal (finishing)
    pub finish: f64,
    pub block_share: f64,
    pub shots: Vec<(f32, f32, u8)>,
}
fn one() -> f64 { 1.0 }

#[derive(Clone, Serialize, Deserialize)]
pub struct Goalie {
    pub id: i64,
    pub name: String,
    /// odds multiplier on goals against (below 1 = better than expected)
    pub factor: f64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Team {
    pub abbr: String,
    pub skaters: Vec<Skater>,
    pub goalie: Goalie,
    /// multipliers vs league: offence attempt rate by strength, defence attempts allowed, penalties taken
    pub off: [f64; 3],
    pub def: [f64; 3],
    pub pen: f64,
    pub block: f64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct GameInput {
    pub teams: [Team; 2],
    pub league: League,
    pub playoff: bool,
}

// ---------------------------------------------------------------------------

fn choose(rng: &mut Rng, w: &[f64]) -> usize {
    let tot: f64 = w.iter().sum();
    if tot <= 0.0 { return (rng.next_u64() % w.len().max(1) as u64) as usize; }
    let mut u = rng.f64() * tot;
    for (i, x) in w.iter().enumerate() { if u < *x { return i; } u -= x; }
    w.len() - 1
}
fn exp(rng: &mut Rng, rate_per_sec: f64) -> f64 {
    if rate_per_sec <= 0.0 { return f64::INFINITY; }
    -(rng.f64().max(1e-12)).ln() / rate_per_sec
}
fn lognormal(rng: &mut Rng, sd: f64) -> f64 {
    let u1 = rng.f64().max(1e-12);
    let u2 = rng.f64();
    let z = (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos();
    (sd * z - 0.5 * sd * sd).exp()
}

/// One posterior-ish draw: team rates, finishing and goalies vary by their
/// uncertainty; availability is realised.
pub fn perturb(g: &GameInput, rng: &mut Rng) -> GameInput {
    let mut out = g.clone();
    for t in out.teams.iter_mut() {
        for k in 0..3 { t.off[k] *= lognormal(rng, 0.05); t.def[k] *= lognormal(rng, 0.05); }
        t.goalie.factor *= lognormal(rng, 0.06);
        for s in t.skaters.iter_mut() {
            s.finish *= lognormal(rng, 0.08);
            if s.p_active < 1.0 { s.p_active = if rng.f64() < s.p_active { 1.0 } else { 0.0 }; }
        }
    }
    out
}

pub const NST: usize = 6;
pub const SOG: usize = 0;
pub const GOALS: usize = 1;
pub const ASSISTS: usize = 2;
pub const ATT: usize = 3;
pub const BLOCKS: usize = 4;
pub const PIM: usize = 5;

#[derive(Clone, Serialize, Debug)]
pub struct Snapshot {
    pub period: i32,
    pub clock: f64,              // seconds left in the period
    pub score: [i32; 2],
    pub shots: [i32; 2],         // shots on goal
    pub skaters: [i32; 2],
    pub pulled: [bool; 2],
}

#[derive(Clone, Serialize, Debug)]
pub struct Event {
    pub seq: u32,
    /// attempt, goal, penalty, penalty_end, goalie_pulled, end_period, shootout, game_end
    pub kind: &'static str,
    pub period: i32,
    pub clock: f64,
    pub team: u8,
    pub player: Option<usize>,
    pub assist1: Option<usize>,
    pub assist2: Option<usize>,
    pub blocker: Option<usize>,
    /// blocked / missed / sog / goal
    pub result: &'static str,
    pub strength: &'static str,
    pub x: Option<f32>,
    pub y: Option<f32>,
    pub shot_type: Option<&'static str>,
    pub xg: f64,
    pub empty_net: bool,
    pub minutes: i32,
    pub state: Snapshot,
    pub box_score: [Vec<[i32; NST]>; 2],
    pub saves: [i32; 2],
}

pub struct GameResult {
    pub score: [i32; 2],
    pub stats: [Vec<[i32; NST]>; 2],
    pub active: [Vec<bool>; 2],
    pub shots: [i32; 2],
    pub saves: [i32; 2],
    pub overtime: bool,
    pub shootout: bool,
}

struct Pen { team: usize, left: f64, minor: bool }

struct G<'a> {
    g: &'a GameInput,
    rng: &'a mut Rng,
    period: i32,
    clock: f64,
    score: [i32; 2],
    shots: [i32; 2],
    saves: [i32; 2],
    stats: [Vec<[i32; NST]>; 2],
    pens: Vec<Pen>,
    pulled: [bool; 2],
    pull_at: [f64; 2],
    events: Option<&'a mut Vec<Event>>,
}

impl<'a> G<'a> {
    fn skaters(&self, t: usize) -> i32 {
        let ot3 = !self.g.playoff && self.period == 4;
        let base = if ot3 { 3 } else { 5 };
        let boxed = self.pens.iter().filter(|p| p.team == t).count() as i32;
        (base - boxed).max(3) + self.pulled[t] as i32
    }
    fn strength(&self, t: usize) -> usize {
        let (a, b) = (self.skaters(t) - self.pulled[t] as i32, self.skaters(1 - t) - self.pulled[1 - t] as i32);
        if a > b { PP } else if a < b { SH } else { EV }
    }
    fn label(&self, t: usize) -> &'static str {
        if self.pulled[1 - t] { "EN" } else { ["EV", "PP", "SH"][self.strength(t)] }
    }
    fn snap(&self) -> Snapshot {
        Snapshot { period: self.period, clock: (self.clock * 10.0).round() / 10.0, score: self.score, shots: self.shots,
            skaters: [self.skaters(0), self.skaters(1)], pulled: self.pulled }
    }
    #[allow(clippy::too_many_arguments)]
    fn emit(&mut self, kind: &'static str, team: usize, player: Option<usize>, a1: Option<usize>, a2: Option<usize>, blocker: Option<usize>,
            result: &'static str, xy: Option<(f32, f32, u8)>, xg: f64, en: bool, minutes: i32) {
        if self.events.is_none() { return; }
        let strength = self.label(team);
        let state = self.snap();
        let box_score = self.stats.clone();
        let saves = self.saves;
        let ev = self.events.as_mut().unwrap();
        let seq = ev.len() as u32 + 1;
        ev.push(Event { seq, kind, period: self.period, clock: state.clock, team: team as u8, player, assist1: a1, assist2: a2, blocker, result, strength,
            x: xy.map(|v| v.0), y: xy.map(|v| v.1), shot_type: xy.map(|v| SHOT_TYPES[v.2 as usize % 8]), xg: (xg * 1000.0).round() / 1000.0, empty_net: en, minutes,
            state, box_score, saves });
    }

    /// attempt rate per second for team t in the current state
    fn att_rate(&self, t: usize) -> f64 {
        let lg = &self.g.league;
        let (ot, dt) = (&self.g.teams[t], &self.g.teams[1 - t]);
        let home = if t == 1 { lg.home_edge } else { 1.0 / lg.home_edge };
        let base = if !self.g.playoff && self.period == 4 { lg.ot3_att60 }
            else if self.pulled[t] { lg.sixv5_att60 }
            else if self.pulled[1 - t] { lg.en_att60 }
            else { lg.att60[self.strength(t)] };
        let s = self.strength(t);
        let ds = match s { PP => SH, SH => PP, _ => EV }; // the defence is shorthanded when the shooter is on the power play
        base * ot.off[s] * dt.def[ds] * home / 3600.0
    }
    fn pen_rate(&self, t: usize) -> f64 {
        if self.pens.len() >= 2 || (!self.g.playoff && self.period == 4) { return 0.0; }
        self.g.league.pen60 * self.g.teams[t].pen / 3600.0
    }

    fn attempt(&mut self, t: usize) {
        let g = self.g;
        let lg = &g.league;
        let d = 1 - t;
        let (ot, dt) = (&g.teams[t], &g.teams[d]);
        let s = self.strength(t);
        let en = self.pulled[d];
        let w: Vec<f64> = ot.skaters.iter().map(|k| if k.p_active > 0.5 { k.att_share[s].max(1e-4) } else { 0.0 }).collect();
        let si = choose(self.rng, &w);
        let sk = &ot.skaters[si];
        self.stats[t][si][ATT] += 1;
        let xy = if sk.shots.is_empty() { (60.0f32, 0.0f32, 0u8) } else { sk.shots[(self.rng.next_u64() % sk.shots.len() as u64) as usize] };
        // blocked / missed
        let r = self.rng.f64();
        let p_block = (lg.blocked[s] * dt.block).min(0.6);
        if r < p_block {
            let bw: Vec<f64> = dt.skaters.iter().map(|k| if k.p_active > 0.5 { k.block_share.max(1e-4) } else { 0.0 }).collect();
            let bi = choose(self.rng, &bw);
            self.stats[d][bi][BLOCKS] += 1;
            self.emit("attempt", t, Some(si), None, None, Some(bi), "blocked", Some(xy), 0.0, en, 0);
            return;
        }
        if r < p_block + lg.missed[s] * (1.0 - p_block) / (1.0 - lg.blocked[s]).max(0.1) {
            self.emit("attempt", t, Some(si), None, None, None, "missed", Some(xy), 0.0, en, 0);
            return;
        }
        // on goal
        self.stats[t][si][SOG] += 1;
        self.shots[t] += 1;
        let (goal, xg) = if en {
            let p = lg.en_goal_pct;
            (self.rng.f64() < p, p)
        } else {
            let xg = lg.xg.p(xy.0 as f64, xy.1 as f64, xy.2 as usize, s == PP);
            let o = xg / (1.0 - xg) * sk.finish * dt.goalie.factor;
            (self.rng.f64() < o / (1.0 + o), xg)
        };
        if !goal {
            if !en { self.saves[d] += 1; }
            self.emit("attempt", t, Some(si), None, None, None, "sog", Some(xy), xg, en, 0);
            return;
        }
        self.stats[t][si][GOALS] += 1;
        self.score[t] += 1;
        let mates: Vec<usize> = (0..ot.skaters.len()).filter(|&i| i != si && ot.skaters[i].p_active > 0.5).collect();
        let mut a1 = None;
        let mut a2 = None;
        if !mates.is_empty() && self.rng.f64() < lg.p_assist1[s] {
            let aw: Vec<f64> = mates.iter().map(|&i| ot.skaters[i].ast_share[s].max(1e-4)).collect();
            let i1 = mates[choose(self.rng, &aw)];
            self.stats[t][i1][ASSISTS] += 1;
            a1 = Some(i1);
            let rest: Vec<usize> = mates.iter().copied().filter(|&i| i != i1).collect();
            if !rest.is_empty() && self.rng.f64() < lg.p_assist2[s] / lg.p_assist1[s].max(1e-6) {
                let aw: Vec<f64> = rest.iter().map(|&i| ot.skaters[i].ast_share[s].max(1e-4)).collect();
                let i2 = rest[choose(self.rng, &aw)];
                self.stats[t][i2][ASSISTS] += 1;
                a2 = Some(i2);
            }
        }
        // a power-play goal ends the earliest minor on the other team
        if s == PP {
            if let Some(k) = self.pens.iter().position(|p| p.team == d && p.minor) { self.pens.remove(k); }
        }
        self.emit("goal", t, Some(si), a1, a2, None, "goal", Some(xy), xg, en, 0);
        // goalie returns once the deficit is gone
        for k in 0..2 { if self.pulled[k] && self.score[k] >= self.score[1 - k] { self.pulled[k] = false; } }
    }

    fn penalty(&mut self, t: usize) {
        let lg = &self.g.league;
        let dur = if lg.pen_durations.is_empty() { 2 } else { lg.pen_durations[(self.rng.next_u64() % lg.pen_durations.len() as u64) as usize] };
        let w: Vec<f64> = self.g.teams[t].skaters.iter().map(|k| if k.p_active > 0.5 { 1.0 } else { 0.0 }).collect();
        let pi = choose(self.rng, &w);
        self.stats[t][pi][PIM] += dur;
        self.pens.push(Pen { team: t, left: dur as f64 * 60.0, minor: dur <= 2 });
        self.emit("penalty", t, Some(pi), None, None, None, "", None, 0.0, false, dur);
    }

    /// Advance the clock by dt, ticking penalties; emits penalty ends.
    fn advance(&mut self, dt: f64) {
        self.clock -= dt;
        for p in self.pens.iter_mut() { p.left -= dt; }
        while let Some(k) = self.pens.iter().position(|p| p.left <= 1e-9) {
            let p = self.pens.remove(k);
            self.emit("penalty_end", p.team, None, None, None, None, "", None, 0.0, false, 0);
        }
    }

    fn play_period(&mut self, length: f64) {
        self.clock = length;
        let sudden = self.period >= 4;
        // goalie-pull times for this period (3rd only)
        self.pull_at = [-1.0, -1.0];
        if self.period == 3 {
            for t in 0..2 {
                if self.rng.f64() < self.g.league.pull_rate && !self.g.league.pull_secs_left.is_empty() {
                    let v = &self.g.league.pull_secs_left;
                    self.pull_at[t] = v[(self.rng.next_u64() % v.len() as u64) as usize] as f64;
                }
            }
        }
        while self.clock > 0.0 {
            // pull the goalie when trailing by 1-2 at the drawn time
            for t in 0..2 {
                let deficit = self.score[1 - t] - self.score[t];
                if self.period == 3 && !self.pulled[t] && (1..=2).contains(&deficit) && self.pull_at[t] > 0.0 && self.clock <= self.pull_at[t] {
                    self.pulled[t] = true;
                    self.emit("goalie_pulled", t, None, None, None, None, "", None, 0.0, false, 0);
                }
                if self.pulled[t] && deficit <= 0 { self.pulled[t] = false; }
            }
            let rates = [self.att_rate(0), self.att_rate(1), self.pen_rate(0), self.pen_rate(1)];
            let tot: f64 = rates.iter().sum();
            let next = exp(self.rng, tot);
            // the next state change: penalty expiry, pull time, period end
            let mut horizon = self.clock;
            for p in &self.pens { horizon = horizon.min(p.left); }
            for t in 0..2 { if self.period == 3 && !self.pulled[t] && self.pull_at[t] > 0.0 && self.pull_at[t] < self.clock { horizon = horizon.min(self.clock - self.pull_at[t]); } }
            if next >= horizon {
                self.advance(horizon.max(1e-6));
                continue;
            }
            self.advance(next);
            let k = choose(self.rng, &rates);
            if k < 2 { self.attempt(k); } else { self.penalty(k - 2); }
            if sudden && self.score[0] != self.score[1] { return; }
        }
        self.clock = 0.0;
    }
}

pub fn play_game(g: &GameInput, rng: &mut Rng, events: Option<&mut Vec<Event>>) -> GameResult {
    let n = [g.teams[0].skaters.len(), g.teams[1].skaters.len()];
    let mut s = G { g, rng, period: 1, clock: 1200.0, score: [0, 0], shots: [0, 0], saves: [0, 0],
        stats: [vec![[0; NST]; n[0]], vec![[0; NST]; n[1]]], pens: Vec::new(), pulled: [false, false], pull_at: [-1.0, -1.0], events };
    let mut shootout = false;
    for p in 1..=3 {
        s.period = p;
        s.play_period(1200.0);
        if p < 3 || s.score[0] == s.score[1] { s.emit("end_period", 0, None, None, None, None, "", None, 0.0, false, 0); }
        s.pulled = [false, false];
    }
    if s.score[0] == s.score[1] {
        if g.playoff {
            let mut p = 4;
            while s.score[0] == s.score[1] && p < 12 {
                s.period = p;
                s.play_period(1200.0);
                if s.score[0] == s.score[1] { s.emit("end_period", 0, None, None, None, None, "", None, 0.0, false, 0); }
                p += 1;
            }
        } else {
            s.period = 4;
            s.pens.clear();
            s.play_period(300.0);
            if s.score[0] == s.score[1] {
                s.emit("end_period", 0, None, None, None, None, "", None, 0.0, false, 0);
                // shootout: three rounds then sudden death
                s.period = 5;
                shootout = true;
                let p = g.league.so_pct;
                let mut goals = [0, 0];
                let mut round = 0;
                loop {
                    round += 1;
                    for t in 0..2 { if s.rng.f64() < p { goals[t] += 1; } }
                    if round >= 3 && goals[0] != goals[1] { break; }
                    if round < 3 {
                        let left = 3 - round;
                        if goals[0] > goals[1] + left || goals[1] > goals[0] + left { break; }
                    }
                    if round > 20 { goals[(s.rng.f64() < 0.5) as usize] += 1; break; }
                }
                let w = if goals[0] > goals[1] { 0 } else { 1 };
                s.score[w] += 1;
                s.emit("shootout", w, None, None, None, None, "", None, 0.0, false, goals[w]);
            }
        }
    }
    s.clock = 0.0;
    s.emit("game_end", 0, None, None, None, None, "", None, 0.0, false, 0);
    let active = [0, 1].map(|t| g.teams[t].skaters.iter().map(|k| k.p_active > 0.5).collect::<Vec<_>>());
    GameResult { score: s.score, stats: s.stats, active, shots: s.shots, saves: s.saves, overtime: s.period >= 4, shootout }
}

pub fn single_game(g: &GameInput, seed: u64) -> (GameResult, Vec<Event>) {
    let mut prng = Rng::new(derive_seed(seed, &["params", "single"]));
    let gi = perturb(g, &mut prng);
    let mut rng = Rng::new(derive_seed(seed, &["games", "single"]));
    let mut ev = Vec::new();
    let r = play_game(&gi, &mut rng, Some(&mut ev));
    (r, ev)
}

// ---------------------------------------------------------------------------
// Ensemble
// ---------------------------------------------------------------------------

pub use crate::nba_sim::Hist;

pub const PROPS: [&str; 5] = ["sog", "goals", "assists", "points", "blocks"];

pub struct Ensemble {
    pub n: u64,
    pub score: [Hist; 2],
    pub total: Hist,
    pub home_win: u64,
    pub regulation_tie: u64,
    pub home_puckline: u64, // home wins by 2+
    pub away_puckline: u64,
    pub shots: [Hist; 2],
    pub saves: [Hist; 2],
    pub player: [Vec<Vec<Hist>>; 2],
    pub active_n: [Vec<u64>; 2],
}
impl Ensemble {
    fn new(n: [usize; 2]) -> Self {
        Ensemble { n: 0, score: [Hist::new(), Hist::new()], total: Hist::new(), home_win: 0, regulation_tie: 0, home_puckline: 0, away_puckline: 0,
            shots: [Hist::new(), Hist::new()], saves: [Hist::new(), Hist::new()],
            player: [0, 1].map(|t| (0..n[t]).map(|_| (0..PROPS.len()).map(|_| Hist::new()).collect()).collect()), active_n: [vec![0; n[0]], vec![0; n[1]]] }
    }
    fn add(&mut self, r: &GameResult) {
        self.n += 1;
        for t in 0..2 { self.score[t].add(r.score[t]); self.shots[t].add(r.shots[t]); self.saves[t].add(r.saves[t]); }
        self.total.add(r.score[0] + r.score[1]);
        self.home_win += (r.score[1] > r.score[0]) as u64;
        self.regulation_tie += r.overtime as u64;
        self.home_puckline += (r.score[1] - r.score[0] >= 2) as u64;
        self.away_puckline += (r.score[0] - r.score[1] >= 2) as u64;
        for t in 0..2 {
            for (i, st) in r.stats[t].iter().enumerate() {
                if !r.active[t][i] { continue; }
                self.active_n[t][i] += 1;
                let v = [st[SOG], st[GOALS], st[ASSISTS], st[GOALS] + st[ASSISTS], st[BLOCKS]];
                for (k, x) in v.iter().enumerate() { self.player[t][i][k].add(*x); }
            }
        }
    }
    fn merge(&mut self, o: &Ensemble) {
        self.n += o.n;
        for t in 0..2 {
            self.score[t].merge(&o.score[t]); self.shots[t].merge(&o.shots[t]); self.saves[t].merge(&o.saves[t]);
            for (a, b) in self.player[t].iter_mut().zip(&o.player[t]) { for (x, y) in a.iter_mut().zip(b) { x.merge(y); } }
            for (a, b) in self.active_n[t].iter_mut().zip(&o.active_n[t]) { *a += b; }
        }
        self.total.merge(&o.total);
        self.home_win += o.home_win; self.regulation_tie += o.regulation_tie; self.home_puckline += o.home_puckline; self.away_puckline += o.away_puckline;
    }
}

pub fn simulate(g: &GameInput, n: usize, draws: usize, seed: u64) -> Ensemble {
    let draws = draws.clamp(1, n.max(1));
    let threads = std::thread::available_parallelism().map(|x| x.get()).unwrap_or(2).clamp(1, 4);
    let per = n / draws;
    let extra = n % draws;
    let batches: Vec<(usize, usize)> = (0..draws).map(|b| (b, per + (b < extra) as usize)).collect();
    let np = [g.teams[0].skaters.len(), g.teams[1].skaters.len()];
    let parts: Vec<Ensemble> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..threads).map(|t| {
            let mine: Vec<(usize, usize)> = batches.iter().filter(|(b, _)| b % threads == t).cloned().collect();
            sc.spawn(move || {
                let mut e = Ensemble::new(np);
                for (b, m) in mine {
                    let mut prng = Rng::new(derive_seed(seed, &["params", &b.to_string()]));
                    let gi = perturb(g, &mut prng);
                    let mut rng = Rng::new(derive_seed(seed, &["games", &b.to_string()]));
                    for _ in 0..m { let r = play_game(&gi, &mut rng, None); e.add(&r); }
                }
                e
            })
        }).collect();
        hs.into_iter().map(|h| h.join().expect("sim thread")).collect()
    });
    let mut out = Ensemble::new(np);
    for p in &parts { out.merge(p); }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    fn toy(playoff: bool) -> GameInput {
        let lg = League { season: 2026, att60: [55.0, 95.0, 20.0], blocked: [0.25, 0.15, 0.3], missed: [0.22, 0.22, 0.2], pen60: 3.0,
            pen_durations: vec![2, 2, 2, 2, 4, 5], xg: Xg { b0: -1.2, dist: -0.055, angle: -0.6, pp: 0.25, types: [0.0, 0.05, 0.0, -0.1, 0.2, 0.2, -0.3, 0.0] },
            p_assist1: [0.88, 0.95, 0.75], p_assist2: [0.70, 0.82, 0.5], ot3_att60: 80.0, so_pct: 0.32, pull_secs_left: vec![90, 100, 110, 120, 140],
            pull_rate: 0.8, en_att60: 60.0, en_goal_pct: 0.3, sixv5_att60: 110.0, home_edge: 1.03 };
        let team = |abbr: &str| Team { abbr: abbr.into(),
            skaters: (0..18).map(|i| Skater { id: i, name: format!("S{i}"), pos: if i < 12 { "F".into() } else { "D".into() }, p_active: 1.0,
                att_share: [1.0 / 18.0; 3], ast_share: [1.0 / 18.0; 3], finish: 1.0, block_share: if i < 12 { 0.03 } else { 0.1 },
                shots: vec![(70.0, 5.0, 0), (55.0, -20.0, 1), (80.0, 2.0, 4)] }).collect(),
            goalie: Goalie { id: 99, name: "G".into(), factor: 1.0 }, off: [1.0; 3], def: [1.0; 3], pen: 1.0, block: 1.0 };
        GameInput { teams: [team("AWY"), team("HOM")], league: lg, playoff }
    }
    #[test]
    fn invariants() {
        for playoff in [false, true] {
            let g = toy(playoff);
            for seed in 0..150u64 {
                let (r, ev) = single_game(&g, seed);
                assert_ne!(r.score[0], r.score[1]);
                for t in 0..2 {
                    let goals: i32 = r.stats[t].iter().map(|s| s[GOALS]).sum();
                    let sog: i32 = r.stats[t].iter().map(|s| s[SOG]).sum();
                    assert_eq!(sog, r.shots[t]);
                    let extra = r.score[t] - goals;
                    assert!(extra == 0 || (r.shootout && extra == 1), "score {} goals {goals}", r.score[t]);
                    assert!(r.saves[1 - t] + goals <= sog);
                }
                if playoff { assert!(!r.shootout); }
                assert_eq!(ev.last().unwrap().kind, "game_end");
                for (i, e) in ev.iter().enumerate() { assert_eq!(e.seq as usize, i + 1); assert!(e.state.clock >= -1e-6); }
            }
        }
    }
    #[test]
    fn realistic() {
        let e = simulate(&toy(false), 20000, 50, 7);
        let tot = e.total.mean();
        assert!(tot > 4.0 && tot < 8.5, "goals {tot}");
        let sog = e.shots[0].mean();
        assert!(sog > 20.0 && sog < 40.0, "sog {sog}");
        let e2 = simulate(&toy(false), 20000, 50, 7);
        assert_eq!(e.total.sum, e2.total.sum);
    }
}

