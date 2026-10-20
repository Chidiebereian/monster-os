//! NBA possession-level game simulator (pure; no I/O).
//!
//! One simulated game: four 12-minute quarters (5-minute overtimes until
//! decided), possessions alternating, each possession resolved event by event:
//! the ball-handler finishing it is chosen by usage among the five on the
//! floor; the outcome is a turnover (maybe a steal), a two-shot free-throw
//! trip, or a 2/3-point attempt (make probability = shooter x defence odds
//! ratio vs the league); made shots may be assisted, misses may be blocked
//! and are rebounded by the offence (continuing the possession) or defence.
//!
//! Rotation: each active player's minutes are drawn from his own recent games
//! and scaled to the team total; a greedy two-minute stint schedule (starters
//! open the 1st and 3rd quarters, players already on the floor are kept)
//! decides who is on the court. Pace sets the possession length.
//! Every box-score number is derived from those simulated possessions.
//! Shot locations are drawn from the shooter's real ESPN shot chart (display).
//!
//! Not modelled (documented): fouls / foul trouble / end-game fouling, and-1s,
//! timeouts, positions in lineups, garbage-time rotations, injuries in-game.

use serde::{Deserialize, Serialize};

pub use crate::mlb_sim::{derive_seed, Rng};

// stat slots
pub const NST: usize = 14;
pub const SEC: usize = 0;
pub const PTS: usize = 1;
pub const FGM: usize = 2;
pub const FGA: usize = 3;
pub const F3M: usize = 4;
pub const F3A: usize = 5;
pub const FTM: usize = 6;
pub const FTA: usize = 7;
pub const OREB: usize = 8;
pub const DREB: usize = 9;
pub const AST: usize = 10;
pub const TOV: usize = 11;
pub const STL: usize = 12;
pub const BLK: usize = 13;
pub const STAT_KEYS: [&str; NST] = ["sec", "pts", "fgm", "fga", "fg3m", "fg3a", "ftm", "fta", "oreb", "dreb", "ast", "tov", "stl", "blk"];

#[derive(Clone, Copy, Serialize, Deserialize, Debug)]
pub struct Rate {
    pub mean: f64,
    pub n: f64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct League {
    pub season: i32,
    pub fg2: f64,
    pub fg3: f64,
    pub ft: f64,
    /// shares of possession-ending usage events
    pub tov_share: f64,
    pub trip_share: f64,
    pub three_share: f64, // of field-goal attempts
    pub ast_per_fgm: f64,
    pub stl_per_tov: f64,
    pub blk_per_miss: f64,
    pub oreb_pct: f64,
    pub pace: f64, // possessions per team per 48
    pub kappa: Kappas,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Kappas {
    pub fg2: f64,
    pub fg3: f64,
    pub ft: f64,
    pub tov: f64,
    pub trip: f64,
    pub three: f64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Player {
    pub id: String,
    pub name: String,
    pub pos: String,
    #[serde(default = "one")]
    pub p_active: f64,
    pub starter: bool,
    pub min_mean: f64,
    pub min_sd: f64,
    /// usage events per minute on the floor (FGA + TOV + FTA/2)
    pub usage: f64,
    pub tov: Rate,
    pub trip: Rate,
    pub three: Rate,
    pub fg2: Rate,
    pub fg3: Rate,
    pub ft: Rate,
    /// per-minute weights for credit sharing
    pub ast_w: f64,
    pub oreb_w: f64,
    pub dreb_w: f64,
    pub stl_w: f64,
    pub blk_w: f64,
    /// real shot locations (x, y feet; made)
    pub shots2: Vec<(f32, f32, bool)>,
    pub shots3: Vec<(f32, f32, bool)>,
}
fn one() -> f64 { 1.0 }

#[derive(Clone, Serialize, Deserialize)]
pub struct Team {
    pub abbr: String,
    pub players: Vec<Player>,
    pub pace: f64,
    /// defence: odds multipliers on opponents' 2P%, 3P%, turnover share; DREB odds multiplier
    pub def_fg2: f64,
    pub def_fg3: f64,
    pub def_tov: f64,
    pub dreb: f64,
    /// offensive rebounding odds multiplier
    pub oreb: f64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct GameInput {
    pub teams: [Team; 2], // 0 away, 1 home
    pub league: League,
    pub home_edge: f64,   // odds multiplier on home shooting
}

// ---------------------------------------------------------------------------

fn odds(p: f64) -> f64 { let p = p.clamp(1e-6, 1.0 - 1e-6); p / (1.0 - p) }
fn from_odds(o: f64) -> f64 { o / (1.0 + o) }
fn adj(p: f64, mult: f64) -> f64 { from_odds(odds(p) * mult) }
fn choose(rng: &mut Rng, w: &[f64]) -> usize {
    let tot: f64 = w.iter().sum();
    if tot <= 0.0 { return (rng.next_u64() % w.len().max(1) as u64) as usize; }
    let mut u = rng.f64() * tot;
    for (i, x) in w.iter().enumerate() { if u < *x { return i; } u -= x; }
    w.len() - 1
}
fn beta(rng: &mut Rng, r: Rate) -> f64 {
    let n = r.n.max(2.0);
    let a = (r.mean * n).max(0.05);
    let b = ((1.0 - r.mean) * n).max(0.05);
    let x = rng.gamma(a);
    let y = rng.gamma(b);
    if x + y <= 0.0 { r.mean } else { x / (x + y) }
}

/// One posterior draw of every rate + realised availability.
pub fn perturb(g: &GameInput, rng: &mut Rng) -> GameInput {
    let mut out = g.clone();
    for t in out.teams.iter_mut() {
        for p in t.players.iter_mut() {
            p.fg2.mean = beta(rng, p.fg2);
            p.fg3.mean = beta(rng, p.fg3);
            p.ft.mean = beta(rng, p.ft);
            p.tov.mean = beta(rng, p.tov);
            p.trip.mean = beta(rng, p.trip);
            p.three.mean = beta(rng, p.three);
            if p.p_active < 1.0 { p.p_active = if rng.f64() < p.p_active { 1.0 } else { 0.0 }; }
        }
    }
    out
}

#[derive(Clone, Serialize, Debug)]
pub struct Snapshot {
    pub period: i32,
    pub clock: f64,
    pub score: [i32; 2],
    pub possession: u8,
    /// player indices on the floor, [away, home]
    pub on_court: [[usize; 5]; 2],
}

#[derive(Clone, Serialize, Debug)]
pub struct Event {
    pub seq: u32,
    /// shot, free_throws, turnover, rebound, substitution, end_period, game_end
    pub kind: &'static str,
    pub period: i32,
    pub clock: f64,
    pub team: u8,
    pub player: Option<usize>,
    pub assist: Option<usize>,
    pub block: Option<usize>,
    pub steal: Option<usize>,
    pub value: i32,
    pub made: bool,
    pub offensive: bool,
    pub ft_made: i32,
    pub ft_att: i32,
    pub x: Option<f32>,
    pub y: Option<f32>,
    pub points: i32,
    pub subs_in: Vec<usize>,
    pub subs_out: Vec<usize>,
    pub state: Snapshot,
    /// [team][player index] stat lines (all players of both teams)
    pub box_score: [Vec<[i32; NST]>; 2],
}

pub struct GameResult {
    pub score: [i32; 2],
    pub stats: [Vec<[i32; NST]>; 2],
    pub active: [Vec<bool>; 2],
    pub overtime: bool,
    pub possessions: [i32; 2],
}

const SEG: f64 = 120.0; // stint length, seconds

/// Minutes for each player in this game, and the stint schedule.
fn rotation(t: &Team, rng: &mut Rng, segments: usize) -> Vec<[usize; 5]> {
    let n = t.players.len();
    let active: Vec<bool> = t.players.iter().map(|p| p.p_active > 0.5).collect();
    let mut mins: Vec<f64> = t.players.iter().enumerate().map(|(i, p)| {
        if !active[i] { return 0.0; }
        let z = { let u1 = rng.f64().max(1e-12); let u2 = rng.f64(); (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos() };
        (p.min_mean + p.min_sd * z).clamp(0.0, 46.0)
    }).collect();
    // priority allocation: the rotation gets its usual minutes in order, the
    // deep bench gets what is left of the 240 (scaled up if the pool is short)
    let target = 240.0;
    let tot: f64 = mins.iter().sum();
    if tot < target && tot > 0.0 {
        for m in mins.iter_mut() { *m *= target / tot; }
    } else {
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by(|a, b| mins[*b].total_cmp(&mins[*a]));
        let mut left = target;
        for i in order { let take = mins[i].min(left); mins[i] = take; left -= take; }
    }
    let mut remaining: Vec<f64> = mins.iter().map(|m| m.min(44.0)).collect();
    let starters: Vec<usize> = {
        let mut s: Vec<usize> = (0..n).filter(|&i| active[i] && t.players[i].starter).collect();
        if s.len() < 5 {
            let mut rest: Vec<usize> = (0..n).filter(|&i| active[i] && !s.contains(&i)).collect();
            rest.sort_by(|a, b| mins[*b].total_cmp(&mins[*a]));
            s.extend(rest.into_iter().take(5 - s.len()));
        }
        s.sort_by(|a, b| mins[*b].total_cmp(&mins[*a]));
        s.truncate(5);
        s
    };
    let mut sched = Vec::with_capacity(segments);
    let mut cur: Vec<usize> = Vec::new();
    for seg in 0..segments {
        let reg = seg < 24;
        let pick: Vec<usize> = if reg && (seg == 0 || seg == 12) && starters.len() == 5 {
            starters.clone()
        } else {
            let mut cand: Vec<(f64, usize)> = (0..n).filter(|&i| active[i]).map(|i| {
                let stay = if cur.contains(&i) { 1.6 } else { 0.0 };
                // overtime: the heaviest-minute players
                let score = if reg { remaining[i] + stay } else { mins[i] + stay };
                (score, i)
            }).collect();
            cand.sort_by(|a, b| b.0.total_cmp(&a.0));
            cand.into_iter().take(5).map(|x| x.1).collect()
        };
        let mut five = [0usize; 5];
        for (k, i) in pick.iter().enumerate().take(5) { five[k] = *i; remaining[*i] -= SEG / 60.0; }
        if pick.len() < 5 { for k in pick.len()..5 { five[k] = pick.first().copied().unwrap_or(0); } }
        cur = pick;
        sched.push(five);
    }
    sched
}

struct G<'a> {
    g: &'a GameInput,
    rng: &'a mut Rng,
    period: i32,
    clock: f64,
    score: [i32; 2],
    pos: u8,
    stats: [Vec<[i32; NST]>; 2],
    on: [[usize; 5]; 2],
    poss: [i32; 2],
    events: Option<&'a mut Vec<Event>>,
}

impl<'a> G<'a> {
    #[allow(clippy::too_many_arguments)]
    fn emit(&mut self, kind: &'static str, team: u8, player: Option<usize>, assist: Option<usize>, block: Option<usize>, steal: Option<usize>,
            value: i32, made: bool, offensive: bool, ft: (i32, i32), xy: Option<(f32, f32)>, points: i32, subs: (Vec<usize>, Vec<usize>)) {
        let Some(ev) = self.events.as_mut() else { return };
        let seq = ev.len() as u32 + 1;
        ev.push(Event {
            seq, kind, period: self.period, clock: (self.clock * 10.0).round() / 10.0, team, player, assist, block, steal, value, made, offensive,
            ft_made: ft.0, ft_att: ft.1, x: xy.map(|v| v.0), y: xy.map(|v| v.1), points, subs_in: subs.0, subs_out: subs.1,
            state: Snapshot { period: self.period, clock: (self.clock * 10.0).round() / 10.0, score: self.score, possession: self.pos, on_court: self.on },
            box_score: self.stats.clone(),
        });
    }
    fn p(&self, t: usize, i: usize) -> &'a Player { &self.g.teams[t].players[i] }

    /// One possession for team `o`; returns elapsed seconds.
    fn possession(&mut self, dur: f64) {
        let g = self.g;
        let lg = &g.league;
        let o = self.pos as usize;
        let d = 1 - o;
        let (ot, dt) = (&g.teams[o], &g.teams[d]);
        self.poss[o] += 1;
        // the possession's time runs off first, so every event shows the clock after it
        self.clock -= dur;
        let home_mult = if o == 1 { g.home_edge } else { 1.0 / g.home_edge };
        for _attempt in 0..6 {
            let five = self.on[o];
            let w: Vec<f64> = five.iter().map(|&i| ot.players[i].usage.max(0.01)).collect();
            let ui = five[choose(self.rng, &w)];
            let u = self.p(o, ui);
            // turnover
            let p_tov = adj(u.tov.mean, dt.def_tov);
            let r = self.rng.f64();
            if r < p_tov {
                self.stats[o][ui][TOV] += 1;
                let steal = if self.rng.f64() < lg.stl_per_tov {
                    let dw: Vec<f64> = self.on[d].iter().map(|&i| dt.players[i].stl_w.max(0.001)).collect();
                    let si = self.on[d][choose(self.rng, &dw)];
                    self.stats[d][si][STL] += 1;
                    Some(si)
                } else { None };
                self.emit("turnover", o as u8, Some(ui), None, None, steal, 0, false, false, (0, 0), None, 0, (vec![], vec![]));
                return;
            }
            // free-throw trip (shooting foul, two shots)
            if self.rng.f64() < u.trip.mean / (1.0 - u.tov.mean).max(0.05) {
                let mut m = 0;
                for _ in 0..2 { if self.rng.f64() < u.ft.mean { m += 1; } }
                self.stats[o][ui][FTA] += 2;
                self.stats[o][ui][FTM] += m;
                self.stats[o][ui][PTS] += m;
                self.score[o] += m;
                self.emit("free_throws", o as u8, Some(ui), None, None, None, 1, m > 0, false, (m, 2), None, m, (vec![], vec![]));
                if m == 2 || self.rng.f64() < 0.5 { return; } // a missed last FT: live rebound half the time
                if self.rebound(o, d, true) { continue; } else { return; }
            }
            // field-goal attempt
            let three = self.rng.f64() < u.three.mean;
            let (base, def) = if three { (u.fg3.mean, dt.def_fg3) } else { (u.fg2.mean, dt.def_fg2) };
            let p_make = adj(base, def * home_mult);
            let made = self.rng.f64() < p_make;
            let value = if three { 3 } else { 2 };
            let chart = if three { &u.shots3 } else { &u.shots2 };
            let xy = {
                let same: Vec<&(f32, f32, bool)> = chart.iter().filter(|s| s.2 == made).collect();
                let pool: Vec<&(f32, f32, bool)> = if same.is_empty() { chart.iter().collect() } else { same };
                if pool.is_empty() { None } else { let s = pool[(self.rng.next_u64() % pool.len() as u64) as usize]; Some((s.0, s.1)) }
            };
            self.stats[o][ui][FGA] += 1;
            if three { self.stats[o][ui][F3A] += 1; }
            if made {
                self.stats[o][ui][FGM] += 1;
                if three { self.stats[o][ui][F3M] += 1; }
                self.stats[o][ui][PTS] += value;
                self.score[o] += value;
                let assist = if self.rng.f64() < lg.ast_per_fgm {
                    let mates: Vec<usize> = self.on[o].iter().copied().filter(|&i| i != ui).collect();
                    let aw: Vec<f64> = mates.iter().map(|&i| ot.players[i].ast_w.max(0.001)).collect();
                    let ai = mates[choose(self.rng, &aw)];
                    self.stats[o][ai][AST] += 1;
                    Some(ai)
                } else { None };
                self.emit("shot", o as u8, Some(ui), assist, None, None, value, true, false, (0, 0), xy, value, (vec![], vec![]));
                return;
            }
            let block = if self.rng.f64() < lg.blk_per_miss {
                let bw: Vec<f64> = self.on[d].iter().map(|&i| dt.players[i].blk_w.max(0.001)).collect();
                let bi = self.on[d][choose(self.rng, &bw)];
                self.stats[d][bi][BLK] += 1;
                Some(bi)
            } else { None };
            self.emit("shot", o as u8, Some(ui), None, block, None, value, false, false, (0, 0), xy, 0, (vec![], vec![]));
            if self.rebound(o, d, false) {
                continue;
            }
            return;
        }
    }

    /// Returns true on an offensive rebound.
    fn rebound(&mut self, o: usize, d: usize, after_ft: bool) -> bool {
        let g = self.g;
        let (ot, dt) = (&g.teams[o], &g.teams[d]);
        let p_off = adj(g.league.oreb_pct, ot.oreb / dt.dreb) * if after_ft { 0.6 } else { 1.0 };
        let off = self.rng.f64() < p_off;
        let (t, team) = if off { (o, ot) } else { (d, dt) };
        let w: Vec<f64> = self.on[t].iter().map(|&i| if off { team.players[i].oreb_w } else { team.players[i].dreb_w }.max(0.001)).collect();
        let ri = self.on[t][choose(self.rng, &w)];
        self.stats[t][ri][if off { OREB } else { DREB }] += 1;
        self.emit("rebound", t as u8, Some(ri), None, None, None, 0, false, off, (0, 0), None, 0, (vec![], vec![]));
        off
    }
}

pub fn play_game(g: &GameInput, rng: &mut Rng, events: Option<&mut Vec<Event>>) -> GameResult {
    let n = [g.teams[0].players.len(), g.teams[1].players.len()];
    let max_segments = 24 + 3 * 6; // regulation + up to six overtimes (2.5 segments each, rounded up)
    let scheds = [rotation(&g.teams[0], rng, max_segments), rotation(&g.teams[1], rng, max_segments)];
    let pace = 0.5 * (g.teams[0].pace + g.teams[1].pace);
    let mean_dur = 2880.0 / (2.0 * pace);
    let tip = (rng.f64() < 0.5) as u8;
    let mut s = G {
        g, rng, period: 1, clock: 720.0, score: [0, 0], pos: tip, stats: [vec![[0; NST]; n[0]], vec![[0; NST]; n[1]]],
        on: [scheds[0][0], scheds[1][0]], poss: [0, 0], events,
    };
    let mut elapsed_total = 0.0;
    let mut seg_idx = 0usize;
    loop {
        // stint boundary -> substitutions
        let seg_now = ((elapsed_total / SEG) as usize).min(max_segments - 1);
        if seg_now != seg_idx {
            seg_idx = seg_now;
            for t in 0..2 {
                let next = scheds[t][seg_idx];
                let ins: Vec<usize> = next.iter().copied().filter(|i| !s.on[t].contains(i)).collect();
                let outs: Vec<usize> = s.on[t].iter().copied().filter(|i| !next.contains(i)).collect();
                s.on[t] = next;
                if !ins.is_empty() {
                    s.emit("substitution", t as u8, None, None, None, None, 0, false, false, (0, 0), None, 0, (ins, outs));
                }
            }
        }
        // possession length: gamma(shape 4) around the game pace, capped by the clock
        let dur = (s.rng.gamma(4.0) * mean_dur / 4.0).clamp(3.0, 24.0).min(s.clock.max(0.5));
        let before = s.clock;
        s.possession(dur);
        if s.clock < 0.0 { s.clock = 0.0; }
        let used = before - s.clock;
        elapsed_total += used;
        for t in 0..2 { for &i in &s.on[t] { s.stats[t][i][SEC] += used.round() as i32; } }
        s.pos = 1 - s.pos;
        if s.clock <= 0.0 {
            let tied = s.score[0] == s.score[1];
            if s.period >= 4 && !tied {
                break;
            }
            if s.period >= 10 { break; }
            s.emit("end_period", 0, None, None, None, None, 0, false, false, (0, 0), None, 0, (vec![], vec![]));
            s.period += 1;
            s.clock = if s.period <= 4 { 720.0 } else { 300.0 };
            // possession arrow: Q2/Q3 to the team that lost the tip, Q4 to the winner, OT jump ball
            s.pos = match s.period { 2 | 3 => 1 - tip, 4 => tip, _ => (s.rng.f64() < 0.5) as u8 };
            // regulation stints are time-based; OT continues the schedule
            elapsed_total = elapsed_total.max(((s.period - 1).min(4) as f64) * 720.0 + ((s.period - 5).max(0) as f64) * 300.0);
        }
    }
    s.clock = 0.0;
    s.emit("game_end", 0, None, None, None, None, 0, false, false, (0, 0), None, 0, (vec![], vec![]));
    let active = [0, 1].map(|t| g.teams[t].players.iter().map(|p| p.p_active > 0.5).collect::<Vec<_>>());
    GameResult { score: s.score, stats: s.stats, active, overtime: s.period > 4, possessions: s.poss }
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

#[derive(Clone)]
pub struct Hist { pub counts: Vec<u64>, pub n: u64, pub sum: f64 }
impl Hist {
    pub fn new() -> Self { Hist { counts: Vec::new(), n: 0, sum: 0.0 } }
    pub fn add(&mut self, v: i32) {
        let i = v.max(0) as usize;
        if i >= self.counts.len() { self.counts.resize(i + 1, 0); }
        self.counts[i] += 1; self.n += 1; self.sum += v as f64;
    }
    pub fn merge(&mut self, o: &Hist) {
        if o.counts.len() > self.counts.len() { self.counts.resize(o.counts.len(), 0); }
        for (i, c) in o.counts.iter().enumerate() { self.counts[i] += c; }
        self.n += o.n; self.sum += o.sum;
    }
    pub fn mean(&self) -> f64 { if self.n == 0 { 0.0 } else { self.sum / self.n as f64 } }
    pub fn quantile(&self, q: f64) -> i32 {
        let target = (q * self.n as f64).ceil().max(1.0) as u64;
        let mut acc = 0;
        for (i, c) in self.counts.iter().enumerate() { acc += c; if acc >= target { return i as i32; } }
        self.counts.len() as i32 - 1
    }
    /// (P(over), P(under), P(push), mcse)
    pub fn line(&self, line: f64) -> (f64, f64, f64, f64) {
        let (mut o, mut u, mut p) = (0u64, 0u64, 0u64);
        for (i, c) in self.counts.iter().enumerate() {
            let v = i as f64;
            if v > line { o += c } else if v < line { u += c } else { p += c }
        }
        let n = self.n.max(1) as f64;
        let po = o as f64 / n;
        (po, u as f64 / n, p as f64 / n, (po * (1.0 - po) / n).sqrt())
    }
    pub fn pmf(&self) -> Vec<f64> { let n = self.n.max(1) as f64; self.counts.iter().map(|c| *c as f64 / n).collect() }
}

/// Prop stats tracked per player.
pub const PROPS: [&str; 9] = ["pts", "reb", "ast", "fg3m", "stl", "blk", "tov", "pra", "min"];
fn prop_values(s: &[i32; NST]) -> [i32; 9] {
    let reb = s[OREB] + s[DREB];
    [s[PTS], reb, s[AST], s[F3M], s[STL], s[BLK], s[TOV], s[PTS] + reb + s[AST], (s[SEC] as f64 / 60.0).round() as i32]
}

pub struct Ensemble {
    pub n: u64,
    pub score: [Hist; 2],
    pub total: Hist,
    pub margin: Vec<u64>, // home - away, offset 80
    pub home_win: u64,
    pub overtime: u64,
    pub player: [Vec<Vec<Hist>>; 2],
    pub active_n: [Vec<u64>; 2],
    pub possessions: [f64; 2],
}
pub const MARGIN_OFFSET: i32 = 80;
impl Ensemble {
    fn new(n: [usize; 2]) -> Self {
        Ensemble { n: 0, score: [Hist::new(), Hist::new()], total: Hist::new(), margin: vec![0; 161], home_win: 0, overtime: 0,
            player: [0, 1].map(|t| (0..n[t]).map(|_| (0..PROPS.len()).map(|_| Hist::new()).collect()).collect()),
            active_n: [vec![0; n[0]], vec![0; n[1]]], possessions: [0.0, 0.0] }
    }
    fn add(&mut self, r: &GameResult) {
        self.n += 1;
        self.score[0].add(r.score[0]); self.score[1].add(r.score[1]);
        self.total.add(r.score[0] + r.score[1]);
        let m = (r.score[1] - r.score[0] + MARGIN_OFFSET).clamp(0, 160) as usize;
        self.margin[m] += 1;
        self.home_win += (r.score[1] > r.score[0]) as u64;
        self.overtime += r.overtime as u64;
        for t in 0..2 {
            self.possessions[t] += r.possessions[t] as f64;
            for (i, s) in r.stats[t].iter().enumerate() {
                if !r.active[t][i] { continue; }
                self.active_n[t][i] += 1;
                for (k, v) in prop_values(s).iter().enumerate() { self.player[t][i][k].add(*v); }
            }
        }
    }
    fn merge(&mut self, o: &Ensemble) {
        self.n += o.n;
        for t in 0..2 {
            self.score[t].merge(&o.score[t]);
            self.possessions[t] += o.possessions[t];
            for (a, b) in self.player[t].iter_mut().zip(&o.player[t]) { for (x, y) in a.iter_mut().zip(b) { x.merge(y); } }
            for (a, b) in self.active_n[t].iter_mut().zip(&o.active_n[t]) { *a += b; }
        }
        self.total.merge(&o.total);
        for (a, b) in self.margin.iter_mut().zip(&o.margin) { *a += b; }
        self.home_win += o.home_win;
        self.overtime += o.overtime;
    }
    /// P(home margin > line), P(< line)
    pub fn margin_line(&self, line: f64) -> (f64, f64) {
        let (mut o, mut u) = (0u64, 0u64);
        for (i, c) in self.margin.iter().enumerate() {
            let v = (i as i32 - MARGIN_OFFSET) as f64;
            if v > line { o += c } else if v < line { u += c }
        }
        let n = self.n.max(1) as f64;
        (o as f64 / n, u as f64 / n)
    }
    pub fn margin_mean(&self) -> f64 {
        let n = self.n.max(1) as f64;
        self.margin.iter().enumerate().map(|(i, c)| (i as i32 - MARGIN_OFFSET) as f64 * *c as f64).sum::<f64>() / n
    }
}

pub fn simulate(g: &GameInput, n: usize, draws: usize, seed: u64) -> Ensemble {
    let draws = draws.clamp(1, n.max(1));
    let threads = std::thread::available_parallelism().map(|x| x.get()).unwrap_or(2).clamp(1, 4);
    let per = n / draws;
    let extra = n % draws;
    let batches: Vec<(usize, usize)> = (0..draws).map(|b| (b, per + (b < extra) as usize)).collect();
    let np = [g.teams[0].players.len(), g.teams[1].players.len()];
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
pub mod tests {
    use super::*;

    pub fn toy() -> GameInput {
        let lg = League { season: 2026, fg2: 0.54, fg3: 0.36, ft: 0.78, tov_share: 0.12, trip_share: 0.10, three_share: 0.40,
            ast_per_fgm: 0.62, stl_per_tov: 0.5, blk_per_miss: 0.09, oreb_pct: 0.25, pace: 99.0,
            kappa: Kappas { fg2: 200.0, fg3: 200.0, ft: 50.0, tov: 100.0, trip: 100.0, three: 50.0 } };
        let player = |i: usize, starter: bool| Player {
            id: format!("p{i}"), name: format!("P{i}"), pos: "G".into(), p_active: 1.0, starter,
            min_mean: if starter { 32.0 } else { 16.0 }, min_sd: 4.0, usage: if starter { 0.45 } else { 0.35 },
            tov: Rate { mean: 0.12, n: 200.0 }, trip: Rate { mean: 0.10, n: 200.0 }, three: Rate { mean: 0.4, n: 200.0 },
            fg2: Rate { mean: 0.54, n: 300.0 }, fg3: Rate { mean: 0.36, n: 300.0 }, ft: Rate { mean: 0.78, n: 100.0 },
            ast_w: 0.1, oreb_w: 0.05, dreb_w: 0.15, stl_w: 0.03, blk_w: 0.02, shots2: vec![(25.0, 6.0, true), (20.0, 10.0, false)], shots3: vec![(3.0, 5.0, true), (25.0, 27.0, false)],
        };
        let team = |abbr: &str| Team { abbr: abbr.into(), players: (0..10).map(|i| player(i, i < 5)).collect(), pace: 99.0,
            def_fg2: 1.0, def_fg3: 1.0, def_tov: 1.0, dreb: 1.0, oreb: 1.0 };
        GameInput { teams: [team("AWY"), team("HOM")], league: lg, home_edge: 1.0 }
    }

    #[test]
    fn invariants() {
        let g = toy();
        for seed in 0..100u64 {
            let (r, ev) = single_game(&g, seed);
            assert_ne!(r.score[0], r.score[1], "no ties");
            for t in 0..2 {
                let pts: i32 = r.stats[t].iter().map(|s| s[PTS]).sum();
                assert_eq!(pts, r.score[t]);
                let mins: i32 = r.stats[t].iter().map(|s| s[SEC]).sum();
                assert!((mins as f64 - 5.0 * if r.overtime { 3000.0 } else { 2880.0 }).abs() < 5.0 * 300.0 * 6.0, "seconds {mins}");
                for s in &r.stats[t] {
                    assert!(s[FGM] <= s[FGA] && s[F3M] <= s[F3A] && s[F3A] <= s[FGA] && s[FTM] <= s[FTA]);
                    assert_eq!(s[PTS], 2 * (s[FGM] - s[F3M]) + 3 * s[F3M] + s[FTM]);
                }
            }
            let last = ev.last().unwrap();
            assert_eq!(last.kind, "game_end");
            assert_eq!(last.state.score, r.score);
            for (i, e) in ev.iter().enumerate() { assert_eq!(e.seq as usize, i + 1); }
        }
    }

    #[test]
    fn realistic_scoring_and_reproducible() {
        let g = toy();
        let e = simulate(&g, 4000, 20, 3);
        let tot = e.total.mean();
        assert!(tot > 190.0 && tot < 250.0, "total {tot}");
        let hw = e.home_win as f64 / e.n as f64;
        assert!((hw - 0.5).abs() < 0.04, "home {hw}");
        let e2 = simulate(&g, 4000, 20, 3);
        assert_eq!(e.total.sum, e2.total.sum);
    }
}
