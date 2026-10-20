//! MLB plate-appearance game simulator (pure; no I/O).
//!
//! One simulated game walks the real structure of baseball: batting order,
//! half-innings, outs, who is on which base, starter -> bullpen hand-off, the
//! automatic runner in extra innings and walk-off endings. Every player stat
//! (hits, total bases, runs, RBI, strikeouts, pitcher outs, ...) is DERIVED from
//! those simulated plate appearances, so a batter's hits, total bases and runs,
//! the pitcher's strikeouts and the team's score all come from the same game.
//!
//! Model fidelity (documented, not overstated):
//! * plate-appearance level, not pitch level: each PA ends in one of
//!   K / BB / HBP / 1B / 2B / 3B / HR / ball-in-play out (errors, FC and
//!   sacrifices fold into "out", matching how the box score counts them);
//! * batter x pitcher via the multinomial odds-ratio ("log5") method against
//!   the league rates, times home/away and park environment factors;
//! * runner advancement, double plays, sac flies and RBI credit are sampled
//!   from an EMPIRICAL transition table (event, bases, outs) built from real
//!   MLB play-by-play; deterministic forced moves for BB/HBP/HR/K;
//! * starter workload = an empirical batters-faced draw from his own starts
//!   (partially pooled with the league); bullpen = team aggregate reliever;
//! * no pinch hitters, platoon/handedness splits, stolen-base attempts or
//!   weather yet.
//! Parameter (epistemic) uncertainty: each batch of games draws every player's
//! outcome rates from their Dirichlet posterior before simulating.

use std::collections::HashMap;

pub const NCAT: usize = 8;
pub const K: usize = 0;
pub const BB: usize = 1;
pub const HBP: usize = 2;
pub const S1: usize = 3;
pub const S2: usize = 4;
pub const S3: usize = 5;
pub const HR: usize = 6;
pub const OUT: usize = 7;
pub const CATS: [&str; NCAT] = ["k", "bb", "hbp", "1b", "2b", "3b", "hr", "out"];

// --------------------------------------------------------------------------
// RNG: xoshiro256** seeded through splitmix64 (deterministic, dependency-free)
// --------------------------------------------------------------------------

pub struct Rng {
    s: [u64; 4],
}

fn splitmix(x: &mut u64) -> u64 {
    *x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        let mut x = seed;
        Rng { s: [splitmix(&mut x), splitmix(&mut x), splitmix(&mut x), splitmix(&mut x)] }
    }
    pub fn next_u64(&mut self) -> u64 {
        let r = self.s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);
        r
    }
    /// Uniform in [0, 1).
    pub fn f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }
    fn normal(&mut self) -> f64 {
        let u1 = self.f64().max(1e-300);
        let u2 = self.f64();
        (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }
    /// Gamma(shape, 1) by Marsaglia-Tsang (boosted for shape < 1).
    pub fn gamma(&mut self, shape: f64) -> f64 {
        if shape < 1.0 {
            let u = self.f64().max(1e-300);
            return self.gamma(shape + 1.0) * u.powf(1.0 / shape);
        }
        let d = shape - 1.0 / 3.0;
        let c = 1.0 / (9.0 * d).sqrt();
        loop {
            let x = self.normal();
            let v = (1.0 + c * x).powi(3);
            if v <= 0.0 {
                continue;
            }
            let u = self.f64();
            if u < 1.0 - 0.0331 * x.powi(4) || u.ln() < 0.5 * x * x + d * (1.0 - v + v.ln()) {
                return d * v;
            }
        }
    }
    pub fn dirichlet(&mut self, alpha: &[f64; NCAT]) -> [f64; NCAT] {
        let mut out = [0.0; NCAT];
        let mut s = 0.0;
        for c in 0..NCAT {
            out[c] = if alpha[c] > 0.0 { self.gamma(alpha[c]) } else { 0.0 };
            s += out[c];
        }
        if s <= 0.0 {
            return *alpha;
        }
        for v in out.iter_mut() {
            *v /= s;
        }
        out
    }
}

/// Seed for one sub-stream, derived from the run seed and labels (FNV-1a).
pub fn derive_seed(base: u64, parts: &[&str]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ base;
    for p in parts {
        for b in p.bytes().chain(std::iter::once(0xff)) {
            h ^= b as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    let mut x = h;
    splitmix(&mut x)
}

// --------------------------------------------------------------------------
// Matchup
// --------------------------------------------------------------------------

/// Multinomial odds-ratio (generalised log5): q_c ∝ b_c * p_c / lg_c * env_c.
pub fn matchup(b: &[f64; NCAT], p: &[f64; NCAT], lg: &[f64; NCAT], env: &[f64; NCAT]) -> [f64; NCAT] {
    let mut q = [0.0; NCAT];
    let mut s = 0.0;
    for c in 0..NCAT {
        q[c] = if lg[c] > 0.0 { b[c] * p[c] / lg[c] * env[c] } else { 0.0 };
        s += q[c];
    }
    for v in q.iter_mut() {
        *v /= s;
    }
    q
}

fn cumulative(p: &[f64; NCAT]) -> [f64; NCAT] {
    let mut c = [0.0; NCAT];
    let mut acc = 0.0;
    for i in 0..NCAT {
        acc += p[i];
        c[i] = acc;
    }
    c[NCAT - 1] = f64::INFINITY; // guard against rounding
    c
}

fn draw_cat(cum: &[f64; NCAT], rng: &mut Rng) -> usize {
    let u = rng.f64();
    cum.iter().position(|&c| u < c).unwrap_or(NCAT - 1)
}

// --------------------------------------------------------------------------
// Baserunning transitions
// --------------------------------------------------------------------------

/// Destination codes for the batter (index 0) and the runners who started on
/// 1B/2B/3B (indices 1..3).
pub const D_OUT: u8 = 0;
pub const D_HOME: u8 = 4;
pub const D_NONE: u8 = 5;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, serde::Serialize, serde::Deserialize)]
pub struct Move {
    pub dest: [u8; 4],
    pub rbi: u8,
}

/// Ball-in-play event slots that use the empirical table.
pub fn bip_slot(cat: usize) -> Option<usize> {
    match cat {
        S1 => Some(0),
        S2 => Some(1),
        S3 => Some(2),
        OUT => Some(3),
        _ => None,
    }
}

fn cell_index(slot: usize, bases: u8, outs: u8) -> usize {
    (slot * 8 + bases as usize) * 3 + outs as usize
}

/// Empirical (event, bases, outs) -> distribution over moves.
#[derive(Clone, Default)]
pub struct Baserunning {
    cells: Vec<Vec<(f64, Move)>>, // cumulative probabilities
    pub counts: Vec<f64>,
}

/// One observed transition row, as stored in the database.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct TransitionObs {
    pub slot: u8,
    pub bases: u8,
    pub outs: u8,
    pub mv: Move,
    pub n: f64,
}

pub const MIN_CELL: f64 = 8.0;

impl Baserunning {
    pub fn from_obs(obs: &[TransitionObs]) -> Self {
        let mut raw: Vec<HashMap<Move, f64>> = vec![HashMap::new(); 4 * 8 * 3];
        for o in obs {
            if o.slot < 4 && o.bases < 8 && o.outs < 3 {
                *raw[cell_index(o.slot as usize, o.bases, o.outs)].entry(o.mv).or_insert(0.0) += o.n;
            }
        }
        let mut cells = Vec::with_capacity(raw.len());
        let mut counts = Vec::with_capacity(raw.len());
        for m in raw {
            let total: f64 = m.values().sum();
            counts.push(total);
            let mut v: Vec<(Move, f64)> = m.into_iter().collect();
            v.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.dest.cmp(&b.0.dest)));
            let mut acc = 0.0;
            cells.push(v.into_iter().map(|(mv, n)| {
                acc += n / total;
                (acc, mv)
            }).collect());
        }
        Baserunning { cells, counts }
    }

    pub fn sample(&self, cat: usize, bases: u8, outs: u8, rng: &mut Rng) -> Move {
        if let Some(slot) = bip_slot(cat) {
            let i = cell_index(slot, bases, outs);
            if let (Some(cell), Some(&n)) = (self.cells.get(i), self.counts.get(i)) {
                if n >= MIN_CELL && !cell.is_empty() {
                    let u = rng.f64();
                    return cell.iter().find(|(c, _)| u < *c).map(|(_, m)| *m).unwrap_or(cell[cell.len() - 1].1);
                }
            }
        }
        forced_move(cat, bases)
    }
}

/// Deterministic moves: forced advances for BB/HBP, everyone scores on HR, the
/// batter is out on K, and conservative station-to-station advancement when an
/// empirical cell is too thin (each runner moves as many bases as the batter).
pub fn forced_move(cat: usize, bases: u8) -> Move {
    let on = |b: u8| bases & (1 << (b - 1)) != 0;
    let mut dest = [D_NONE; 4];
    let mut rbi = 0u8;
    match cat {
        K => {
            dest[0] = D_OUT;
            for b in 1..=3u8 {
                if on(b) { dest[b as usize] = b; }
            }
        }
        BB | HBP => {
            dest[0] = 1;
            // force chain
            let f1 = on(1);
            let f2 = f1 && on(2);
            let f3 = f2 && on(3);
            if on(1) { dest[1] = 2; }
            if on(2) { dest[2] = if f1 { 3 } else { 2 }; }
            if on(3) { dest[3] = if f2 { D_HOME } else { 3 }; }
            if f3 { rbi = 1; }
        }
        HR => {
            dest[0] = D_HOME;
            rbi = 1;
            for b in 1..=3u8 {
                if on(b) { dest[b as usize] = D_HOME; rbi += 1; }
            }
        }
        OUT => {
            dest[0] = D_OUT;
            for b in 1..=3u8 {
                if on(b) { dest[b as usize] = b; }
            }
        }
        _ => {
            // hit for n bases: batter to n, runners advance n
            let n = match cat { S1 => 1, S2 => 2, _ => 3 };
            dest[0] = n;
            for b in 1..=3u8 {
                if on(b) {
                    let d = b + n;
                    dest[b as usize] = if d >= 4 { rbi += 1; D_HOME } else { d };
                }
            }
        }
    }
    Move { dest, rbi }
}

// --------------------------------------------------------------------------
// Inputs
// --------------------------------------------------------------------------

/// Posterior for one player's per-PA outcome rates: Dirichlet(conc * mean).
#[derive(Clone, Debug, serde::Serialize)]
pub struct RatePosterior {
    pub mean: [f64; NCAT],
    pub conc: f64,
}

impl RatePosterior {
    fn draw(&self, rng: &mut Rng) -> [f64; NCAT] {
        let mut a = [0.0; NCAT];
        for c in 0..NCAT {
            a[c] = (self.mean[c] * self.conc).max(1e-6);
        }
        rng.dirichlet(&a)
    }
}

#[derive(Clone)]
pub struct TeamInput {
    pub lineup: [RatePosterior; 9],
    pub starter: RatePosterior,
    pub bullpen: RatePosterior,
    /// Replacement hitter (pinch hitter / defensive sub): league bench rates.
    pub bench: RatePosterior,
    /// Empirical batters-faced counts from the starter's own starts.
    pub starter_bf: Vec<u32>,
    /// Batting / pitching environment for this side (home/away x park).
    pub bat_env: [f64; NCAT],
}

#[derive(Clone)]
pub struct GameInput {
    pub away: TeamInput,
    pub home: TeamInput,
    pub league: [f64; NCAT],
    /// League batters-faced-per-start distribution (pooling target for hooks).
    pub league_bf: Vec<u32>,
    /// Weight (in starts) of the league hook distribution in the mix.
    pub hook_prior_starts: f64,
    pub baserunning: Baserunning,
    /// Earned share of runs charged to pitchers (from box scores).
    pub earned_share: f64,
    pub innings: u8,
    /// Automatic runner on 2B from the first extra inning (MLB 2020+).
    pub ghost_runner: bool,
    /// sub_hazard[n] = P(a starting batter is replaced before his next PA |
    /// he has taken n PAs and his slot is up again). Empty = never replaced.
    pub sub_hazard: Vec<f64>,
}

// --------------------------------------------------------------------------
// Per-game result
// --------------------------------------------------------------------------

pub const BAT_STATS: [&str; 13] = ["pa", "h", "1b", "2b", "3b", "hr", "tb", "bb", "k", "hbp", "r", "rbi", "hrr"];
pub const PIT_STATS: [&str; 8] = ["bf", "outs", "k", "bb", "h", "hr", "r", "er"];

#[derive(Clone, Copy, Default)]
pub struct BatLine {
    pub pa: u16, pub h: u16, pub s1: u16, pub s2: u16, pub s3: u16, pub hr: u16,
    pub bb: u16, pub k: u16, pub hbp: u16, pub r: u16, pub rbi: u16,
}
impl BatLine {
    pub fn tb(&self) -> u16 { self.s1 + 2 * self.s2 + 3 * self.s3 + 4 * self.hr }
    pub fn get(&self, stat: &str) -> u16 {
        match stat {
            "pa" => self.pa, "h" => self.h, "1b" => self.s1, "2b" => self.s2, "3b" => self.s3, "hr" => self.hr,
            "tb" => self.tb(), "bb" => self.bb, "k" => self.k, "hbp" => self.hbp, "r" => self.r, "rbi" => self.rbi,
            "hrr" => self.h + self.r + self.rbi,
            _ => 0,
        }
    }
}

#[derive(Clone, Copy, Default)]
pub struct PitLine {
    pub bf: u16, pub outs: u16, pub k: u16, pub bb: u16, pub h: u16, pub hr: u16, pub r: u16, pub er: u16,
}
impl PitLine {
    pub fn get(&self, stat: &str) -> u16 {
        match stat {
            "bf" => self.bf, "outs" => self.outs, "k" => self.k, "bb" => self.bb, "h" => self.h,
            "hr" => self.hr, "r" => self.r, "er" => self.er,
            _ => 0,
        }
    }
}

#[derive(Clone, Default)]
pub struct GameResult {
    /// [away, home]
    pub runs: [u16; 2],
    pub bat: [[BatLine; 9]; 2],
    /// replacement hitters in each slot (not credited to the starter)
    pub bench: [[BatLine; 9]; 2],
    /// starters, [away, home]
    pub starter: [PitLine; 2],
    pub bullpen: [PitLine; 2],
    pub innings: u8,
}

#[derive(Clone, Copy)]
struct Runner {
    slot: u8,
    bench: bool,
    pitcher: u8, // 0 = starter, 1 = bullpen (of the fielding team)
    earned: bool,
}

/// Per-batch matchup tables: [batting side][slot, 9 = bench][pitcher kind] cumulative probs.
pub struct Tables {
    cum: [[[[f64; NCAT]; 2]; 10]; 2],
}

pub(crate) fn build_tables(g: &GameInput, rng: &mut Rng) -> Tables {
    let draw_team = |t: &TeamInput, rng: &mut Rng| -> ([[f64; NCAT]; 10], [f64; NCAT], [f64; NCAT]) {
        let mut l = [[0.0; NCAT]; 10];
        for (i, p) in t.lineup.iter().enumerate() {
            l[i] = p.draw(rng);
        }
        l[9] = t.bench.draw(rng);
        (l, t.starter.draw(rng), t.bullpen.draw(rng))
    };
    let (away_bat, away_sp, away_bp) = draw_team(&g.away, rng);
    let (home_bat, home_sp, home_bp) = draw_team(&g.home, rng);
    let mut cum = [[[[0.0; NCAT]; 2]; 10]; 2];
    for slot in 0..10 {
        // away bats vs home pitchers
        cum[0][slot][0] = cumulative(&matchup(&away_bat[slot], &home_sp, &g.league, &g.away.bat_env));
        cum[0][slot][1] = cumulative(&matchup(&away_bat[slot], &home_bp, &g.league, &g.away.bat_env));
        cum[1][slot][0] = cumulative(&matchup(&home_bat[slot], &away_sp, &g.league, &g.home.bat_env));
        cum[1][slot][1] = cumulative(&matchup(&home_bat[slot], &away_bp, &g.league, &g.home.bat_env));
    }
    Tables { cum }
}

fn draw_hook(own: &[u32], league: &[u32], prior: f64, rng: &mut Rng) -> u32 {
    let n = own.len() as f64;
    let use_own = !own.is_empty() && (league.is_empty() || rng.f64() < n / (n + prior));
    let pool = if use_own { own } else { league };
    if pool.is_empty() {
        return 24;
    }
    pool[(rng.next_u64() % pool.len() as u64) as usize].max(1)
}

const MAX_INNINGS: u8 = 30;

// --------------------------------------------------------------------------
// Events (broadcast mode)
// --------------------------------------------------------------------------

/// Game state after an event; everything the broadcast shows comes from here.
#[derive(Clone, serde::Serialize, Debug)]
pub struct PaState {
    pub inning: u8,
    pub half: u8, // 0 top (away bats), 1 bottom
    pub outs: u8,
    /// batting-order slot (0-8) of the runner on 1B/2B/3B
    pub bases: [Option<u8>; 3],
    pub score: [u16; 2],
    /// runs by inning, [away, home]
    pub linescore: [Vec<u16>; 2],
    pub hits: [u16; 2],
    /// pitcher on the mound for each fielding side: 0 starter, 1 bullpen
    pub on_mound: [u8; 2],
    /// next batting-order slot due up for each team
    pub next_up: [u8; 2],
}

#[derive(Clone, serde::Serialize, Debug)]
pub struct PaEvent {
    pub seq: u32,
    /// "pa", "pitching_change", "inning_end", "game_end"
    pub kind: &'static str,
    pub inning: u8,
    pub half: u8,
    pub batting: u8,
    pub slot: u8,
    /// a replacement hitter batted in this slot
    pub bench: bool,
    pub pitcher: u8,
    /// k, bb, hbp, 1b, 2b, 3b, hr, out
    pub result: &'static str,
    pub outs_before: u8,
    pub bases_before: [Option<u8>; 3],
    pub outs_on_play: u8,
    pub runs: u16,
    /// slots of the runners who scored, in order
    pub scorers: Vec<u8>,
    pub rbi: u16,
    pub state: PaState,
    /// [side][slot] = [pa, h, hr, rbi, r, bb, k, tb] (starter in the slot)
    pub batting_box: [[[u16; 8]; 9]; 2],
    /// [side] = (starter, bullpen) each [bf, outs, k, bb, h, hr, r, er]
    pub pitching_box: [[[u16; 8]; 2]; 2],
}

fn bat_arr(b: &BatLine) -> [u16; 8] { [b.pa, b.h, b.hr, b.rbi, b.r, b.bb, b.k, b.tb()] }
fn pit_arr(p: &PitLine) -> [u16; 8] { [p.bf, p.outs, p.k, p.bb, p.h, p.hr, p.r, p.er] }

struct Recorder<'a> {
    ev: &'a mut Vec<PaEvent>,
    linescore: [Vec<u16>; 2],
    next_up: [u8; 2],
}
impl<'a> Recorder<'a> {
    #[allow(clippy::too_many_arguments)]
    fn push(&mut self, res: &GameResult, kind: &'static str, inning: u8, half: u8, slot: u8, bench: bool, pitcher: u8, result: &'static str,
            outs_before: u8, bases_before: [Option<u8>; 3], outs_on_play: u8, runs: u16, scorers: Vec<u8>, rbi: u16,
            outs: u8, bases: [Option<u8>; 3], on_mound: [u8; 2]) {
        let hits = [0, 1].map(|s: usize| res.bat[s].iter().chain(res.bench[s].iter()).map(|b| b.h).sum::<u16>());
        let seq = self.ev.len() as u32 + 1;
        self.ev.push(PaEvent {
            seq, kind, inning, half, batting: half, slot, bench, pitcher, result, outs_before, bases_before, outs_on_play, runs, scorers, rbi,
            state: PaState { inning, half, outs, bases, score: res.runs, linescore: self.linescore.clone(), hits, on_mound, next_up: self.next_up },
            batting_box: [0, 1].map(|s: usize| {
                let mut a = [[0u16; 8]; 9];
                for k in 0..9 { a[k] = bat_arr(&res.bat[s][k]); }
                a
            }),
            pitching_box: [0, 1].map(|s: usize| [pit_arr(&res.starter[s]), pit_arr(&res.bullpen[s])]),
        });
    }
}

fn slots(b: &[Option<Runner>; 3]) -> [Option<u8>; 3] { [b[0].map(|r| r.slot), b[1].map(|r| r.slot), b[2].map(|r| r.slot)] }

/// Simulate one game. Returns None if it hit the safety cap on innings
/// (counted as a rejected simulation by the caller).
pub fn sim_game(g: &GameInput, t: &Tables, rng: &mut Rng) -> Option<GameResult> {
    sim_game_rec(g, t, rng, None)
}

/// Broadcast mode: one game with its plate-appearance event log.
pub fn single_game(g: &GameInput, seed: u64) -> Option<(GameResult, Vec<PaEvent>)> {
    for attempt in 0..20u32 {
        let mut prng = Rng::new(derive_seed(seed, &["draw", "single", &attempt.to_string()]));
        let t = build_tables(g, &mut prng);
        let mut rng = Rng::new(derive_seed(seed, &["game", "single", &attempt.to_string()]));
        let mut ev = Vec::new();
        if let Some(r) = sim_game_rec(g, &t, &mut rng, Some(&mut ev)) {
            return Some((r, ev));
        }
    }
    None
}

fn sim_game_rec(g: &GameInput, t: &Tables, rng: &mut Rng, events: Option<&mut Vec<PaEvent>>) -> Option<GameResult> {
    let mut rec = events.map(|ev| Recorder { ev, linescore: [Vec::new(), Vec::new()], next_up: [0, 0] });
    let mut res = GameResult::default();
    let mut next_slot = [0usize; 2];
    let hooks = [
        draw_hook(&g.away.starter_bf, &g.league_bf, g.hook_prior_starts, rng),
        draw_hook(&g.home.starter_bf, &g.league_bf, g.hook_prior_starts, rng),
    ];
    // pitcher currently on the mound for each FIELDING side: 0 starter, 1 bullpen
    let mut on_mound = [0u8; 2];
    // starting batter replaced (pinch hitter / defensive sub) in this slot?
    let mut subbed = [[false; 9]; 2];
    let mut inning: u8 = 1;
    loop {
        for bat in 0..2usize {
            let field = 1 - bat;
            // bottom of the last regulation inning (or later) isn't played if home leads
            if bat == 1 && inning >= g.innings && res.runs[1] > res.runs[0] {
                res.innings = inning;
                if let Some(r) = rec.as_mut() {
                    r.push(&res, "game_end", inning, 1, 0, false, 0, "", 0, [None; 3], 0, 0, vec![], 0, 0, [None; 3], on_mound);
                }
                return Some(res);
            }
            if let Some(r) = rec.as_mut() { r.linescore[bat].push(0); }
            let mut outs: u8 = 0;
            let mut bases: [Option<Runner>; 3] = [None, None, None];
            if g.ghost_runner && inning > g.innings {
                let prev = (next_slot[bat] + 8) % 9;
                bases[1] = Some(Runner { slot: prev as u8, bench: subbed[bat][prev], pitcher: on_mound[field], earned: false });
            }
            while outs < 3 {
                let slot = next_slot[bat];
                next_slot[bat] = (slot + 1) % 9;
                if !subbed[bat][slot] {
                    let taken = res.bat[bat][slot].pa as usize;
                    if taken > 0 && taken < g.sub_hazard.len() && rng.f64() < g.sub_hazard[taken] {
                        subbed[bat][slot] = true;
                    }
                }
                let bench = subbed[bat][slot];
                let kind = on_mound[field] as usize;
                let cat = draw_cat(&t.cum[bat][if bench { 9 } else { slot }][kind], rng);
                let mask: u8 = (0..3).filter(|&i| bases[i].is_some()).fold(0, |m, i| m | (1 << i));
                let (outs_before, bases_before) = (outs, slots(&bases));
                let mv = g.baserunning.sample(cat, mask, outs, rng);

                // pitcher + batter line for the event itself
                let pl = if kind == 0 { &mut res.starter[field] } else { &mut res.bullpen[field] };
                pl.bf += 1;
                let bl = if bench { &mut res.bench[bat][slot] } else { &mut res.bat[bat][slot] };
                bl.pa += 1;
                match cat {
                    K => { bl.k += 1; pl.k += 1; }
                    BB => { bl.bb += 1; pl.bb += 1; }
                    HBP => { bl.hbp += 1; }
                    S1 => { bl.h += 1; bl.s1 += 1; pl.h += 1; }
                    S2 => { bl.h += 1; bl.s2 += 1; pl.h += 1; }
                    S3 => { bl.h += 1; bl.s3 += 1; pl.h += 1; }
                    HR => { bl.h += 1; bl.hr += 1; pl.h += 1; pl.hr += 1; }
                    _ => {}
                }

                // apply the move: lead runners first so the scoring order is real
                let batter = Runner { slot: slot as u8, bench, pitcher: kind as u8, earned: true };
                let mut new_bases: [Option<Runner>; 3] = [None, None, None];
                let mut scorers: Vec<Runner> = Vec::with_capacity(4);
                let mut outs_on_play = 0u8;
                let order = [(3usize, bases[2]), (2, bases[1]), (1, bases[0]), (0, Some(batter))];
                for (idx, who) in order {
                    let Some(r) = who else { continue };
                    let mut d = mv.dest[idx];
                    if d == D_NONE {
                        d = if idx == 0 { 1 } else { idx as u8 }; // inconsistent cell: hold
                    }
                    match d {
                        D_OUT => outs_on_play += 1,
                        D_HOME => scorers.push(r),
                        b => {
                            // never stack two runners on one base: push the trailer up
                            let mut b = b as usize;
                            while b <= 3 && new_bases[b - 1].is_some() { b += 1; }
                            if b > 3 { scorers.push(r) } else { new_bases[b - 1] = Some(r) }
                        }
                    }
                }
                let outs_after = (outs + outs_on_play).min(3);
                let real_outs = outs_after - outs;
                // runs on a play that ends the inning on a force/batter out don't count
                let third_out_force = outs_after == 3 && (mv.dest[0] == D_OUT || cat == K);
                if third_out_force {
                    scorers.clear();
                }
                // walk-off: only the runs needed count, unless it's a home run
                if bat == 1 && inning >= g.innings && !scorers.is_empty() {
                    let need = (res.runs[0] + 1).saturating_sub(res.runs[1]) as usize;
                    if res.runs[1] + scorers.len() as u16 > res.runs[0] && cat != HR && need > 0 {
                        scorers.truncate(need);
                    }
                }
                let runs = scorers.len() as u16;
                let rbi = (mv.rbi as u16).min(runs);
                if bench { res.bench[bat][slot].rbi += rbi } else { res.bat[bat][slot].rbi += rbi }
                for r in &scorers {
                    if r.bench { res.bench[bat][r.slot as usize].r += 1 } else { res.bat[bat][r.slot as usize].r += 1 }
                    let charged = if r.pitcher == 0 { &mut res.starter[field] } else { &mut res.bullpen[field] };
                    charged.r += 1;
                    if r.earned && rng.f64() < g.earned_share {
                        charged.er += 1;
                    }
                }
                res.runs[bat] += runs;
                let pl = if kind == 0 { &mut res.starter[field] } else { &mut res.bullpen[field] };
                pl.outs += real_outs as u16;
                outs = outs_after;
                bases = new_bases;
                if let Some(r) = rec.as_mut() {
                    r.next_up = [next_slot[0] as u8, next_slot[1] as u8];
                    if let Some(last) = r.linescore[bat].last_mut() { *last += runs; }
                    let sc: Vec<u8> = scorers.iter().map(|x| x.slot).collect();
                    r.push(&res, "pa", inning, bat as u8, slot as u8, bench, kind as u8, CATS[cat], outs_before, bases_before,
                        real_outs, runs, sc, rbi, outs, if outs >= 3 { [None; 3] } else { slots(&bases) }, on_mound);
                }

                // starter hook once he reaches his batters-faced draw
                if kind == 0 && res.starter[field].bf as u32 >= hooks[field] {
                    on_mound[field] = 1;
                    if let Some(r) = rec.as_mut() {
                        if outs < 3 && !(bat == 1 && inning >= g.innings && res.runs[1] > res.runs[0]) {
                            r.push(&res, "pitching_change", inning, bat as u8, 0, false, 1, "", outs, slots(&bases), 0, 0, vec![], 0,
                                outs, slots(&bases), on_mound);
                        }
                    }
                }
                // walk-off ends the game immediately
                if bat == 1 && inning >= g.innings && res.runs[1] > res.runs[0] {
                    res.innings = inning;
                    if let Some(r) = rec.as_mut() {
                        r.push(&res, "game_end", inning, 1, 0, false, 0, "", outs, [None; 3], 0, 0, vec![], 0, outs, [None; 3], on_mound);
                    }
                    return Some(res);
                }
            }
            if let Some(r) = rec.as_mut() {
                r.push(&res, "inning_end", inning, bat as u8, 0, false, 0, "", 3, [None; 3], 0, 0, vec![], 0, 3, [None; 3], on_mound);
            }
        }
        if inning >= g.innings && res.runs[0] != res.runs[1] {
            res.innings = inning;
            if let Some(r) = rec.as_mut() {
                r.push(&res, "game_end", inning, 1, 0, false, 0, "", 3, [None; 3], 0, 0, vec![], 0, 3, [None; 3], on_mound);
            }
            return Some(res);
        }
        inning += 1;
        if inning > MAX_INNINGS {
            return None;
        }
    }
}

// --------------------------------------------------------------------------
// Monte Carlo driver + aggregation
// --------------------------------------------------------------------------

pub const HIST_CAP: usize = 40;

/// Value histogram for one integer stat (values >= HIST_CAP land in the last bin).
#[derive(Clone)]
pub struct Hist {
    pub counts: Vec<u32>,
    pub n: u32,
    pub sum: f64,
    pub sum_sq: f64,
}
impl Hist {
    pub fn new() -> Self {
        Hist { counts: vec![0; HIST_CAP + 1], n: 0, sum: 0.0, sum_sq: 0.0 }
    }
    pub fn add(&mut self, v: u16) {
        let i = (v as usize).min(HIST_CAP);
        self.counts[i] += 1;
        self.n += 1;
        self.sum += v as f64;
        self.sum_sq += (v as f64).powi(2);
    }
    pub fn mean(&self) -> f64 {
        if self.n == 0 { 0.0 } else { self.sum / self.n as f64 }
    }
    pub fn sd(&self) -> f64 {
        if self.n < 2 { return 0.0; }
        let m = self.mean();
        ((self.sum_sq / self.n as f64 - m * m).max(0.0) * self.n as f64 / (self.n as f64 - 1.0)).sqrt()
    }
    /// Smallest value v with P(X <= v) >= q.
    pub fn quantile(&self, q: f64) -> u32 {
        let target = q * self.n as f64;
        let mut acc = 0.0;
        for (v, &c) in self.counts.iter().enumerate() {
            acc += c as f64;
            if acc >= target && acc > 0.0 {
                return v as u32;
            }
        }
        HIST_CAP as u32
    }
    /// Exact P(over), P(under), P(push) for a line, plus the Monte Carlo
    /// standard error of P(over).
    pub fn line(&self, line: f64) -> LineProbs {
        let n = self.n.max(1) as f64;
        let (mut o, mut u, mut p) = (0.0, 0.0, 0.0);
        for (v, &c) in self.counts.iter().enumerate() {
            let v = v as f64;
            if v > line { o += c as f64 } else if v < line { u += c as f64 } else { p += c as f64 }
        }
        let (o, u, p) = (o / n, u / n, p / n);
        LineProbs { over: o, under: u, push: p, mcse_over: (o * (1.0 - o) / n).sqrt() }
    }
    pub fn pmf(&self) -> Vec<f64> {
        let n = self.n.max(1) as f64;
        let last = self.counts.iter().rposition(|&c| c > 0).unwrap_or(0);
        self.counts[..=last].iter().map(|&c| c as f64 / n).collect()
    }
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct LineProbs {
    pub over: f64,
    pub under: f64,
    pub push: f64,
    pub mcse_over: f64,
}

pub struct SimOutput {
    pub requested: usize,
    pub valid: usize,
    pub rejected: usize,
    pub draws: usize,
    /// [side][slot][stat index in BAT_STATS]
    pub bat: Vec<Vec<Vec<Hist>>>,
    /// [side][stat index in PIT_STATS] for the starters
    pub starter: Vec<Vec<Hist>>,
    pub team_runs: [Hist; 2],
    pub total_runs: Hist,
    /// home margin histogram offset by MARGIN_OFFSET
    pub margin_counts: Vec<u32>,
    pub extra_innings: u32,
}
pub const MARGIN_OFFSET: i32 = 40;

impl SimOutput {
    pub fn home_win(&self) -> f64 {
        let n = self.valid.max(1) as f64;
        self.margin_counts.iter().enumerate().filter(|(i, _)| *i as i32 > MARGIN_OFFSET).map(|(_, &c)| c as f64).sum::<f64>() / n
    }
    /// P(home margin > -spread), i.e. the home side covers `spread` (bettor convention).
    pub fn home_cover(&self, spread: f64) -> f64 {
        let n = self.valid.max(1) as f64;
        self.margin_counts.iter().enumerate()
            .filter(|(i, _)| (*i as i32 - MARGIN_OFFSET) as f64 + spread > 0.0)
            .map(|(_, &c)| c as f64).sum::<f64>() / n
    }
}

/// Run `n` simulations in `draws` parameter batches. Each batch redraws every
/// player's outcome rates from their posterior (parameter uncertainty), builds
/// the matchup tables, then simulates its share of games.
pub fn simulate(g: &GameInput, n: usize, draws: usize, seed: u64) -> SimOutput {
    let draws = draws.clamp(1, n.max(1));
    let mut out = SimOutput {
        requested: n,
        valid: 0,
        rejected: 0,
        draws,
        bat: (0..2).map(|_| (0..9).map(|_| (0..BAT_STATS.len()).map(|_| Hist::new()).collect()).collect()).collect(),
        starter: (0..2).map(|_| (0..PIT_STATS.len()).map(|_| Hist::new()).collect()).collect(),
        team_runs: [Hist::new(), Hist::new()],
        total_runs: Hist::new(),
        margin_counts: vec![0; (2 * MARGIN_OFFSET + 1) as usize],
        extra_innings: 0,
    };
    for d in 0..draws {
        let mut rng = Rng::new(derive_seed(seed, &["draw", &d.to_string()]));
        let tables = build_tables(g, &mut rng);
        let share = n / draws + usize::from(d < n % draws);
        for _ in 0..share {
            let Some(r) = sim_game(g, &tables, &mut rng) else {
                out.rejected += 1;
                continue;
            };
            out.valid += 1;
            if r.innings > g.innings {
                out.extra_innings += 1;
            }
            for side in 0..2 {
                for slot in 0..9 {
                    for (si, s) in BAT_STATS.iter().enumerate() {
                        out.bat[side][slot][si].add(r.bat[side][slot].get(s));
                    }
                }
                for (si, s) in PIT_STATS.iter().enumerate() {
                    out.starter[side][si].add(r.starter[side].get(s));
                }
                out.team_runs[side].add(r.runs[side]);
            }
            out.total_runs.add(r.runs[0] + r.runs[1]);
            let m = (r.runs[1] as i32 - r.runs[0] as i32).clamp(-MARGIN_OFFSET, MARGIN_OFFSET);
            out.margin_counts[(m + MARGIN_OFFSET) as usize] += 1;
        }
    }
    out
}

// --------------------------------------------------------------------------
// Empirical-Bayes rate estimation (partial pooling)
// --------------------------------------------------------------------------

/// Beta-binomial method-of-moments prior strength for one outcome category
/// across units (players): returns kappa such that a unit's rate is shrunk as
/// (x + kappa * p_league) / (n + kappa). `units` = (successes, trials).
pub fn mom_kappa(units: &[(f64, f64)], min_trials: f64) -> Option<f64> {
    let u: Vec<&(f64, f64)> = units.iter().filter(|(_, n)| *n >= min_trials).collect();
    if u.len() < 10 {
        return None;
    }
    let tot_x: f64 = u.iter().map(|(x, _)| x).sum();
    let tot_n: f64 = u.iter().map(|(_, n)| n).sum();
    let p = tot_x / tot_n;
    if p <= 0.0 || p >= 1.0 {
        return None;
    }
    // trial-weighted variance of observed rates around p
    let var_obs: f64 = u.iter().map(|(x, n)| n * (x / n - p).powi(2)).sum::<f64>() / tot_n;
    // expected binomial (sampling) part of that variance
    let k = u.len() as f64;
    let samp = p * (1.0 - p) * k / tot_n;
    let tau2 = var_obs - samp;
    if tau2 <= 1e-9 {
        return Some(1e4); // no detectable between-player spread: pool almost fully
    }
    Some((p * (1.0 - p) / tau2 - 1.0).clamp(5.0, 1e4))
}

/// Shrink a player's (recency-weighted) outcome counts toward the league with
/// per-category strengths `kappa`, renormalise, and attach a Dirichlet
/// concentration for parameter uncertainty (effective PA + typical kappa).
pub fn shrink(counts: &[f64; NCAT], league: &[f64; NCAT], kappa: &[f64; NCAT]) -> RatePosterior {
    let n: f64 = counts.iter().sum();
    let mut m = [0.0; NCAT];
    let mut s = 0.0;
    for c in 0..NCAT {
        m[c] = (counts[c] + kappa[c] * league[c]) / (n + kappa[c]);
        s += m[c];
    }
    for v in m.iter_mut() {
        *v /= s;
    }
    let mut ks: Vec<f64> = kappa.to_vec();
    ks.sort_by(|a, b| a.total_cmp(b));
    let k_med = ks[NCAT / 2];
    RatePosterior { mean: m, conc: n + k_med }
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod event_tests {
    use super::*;
    #[test]
    fn events_are_consistent_with_the_result() {
        let g = tests::game();
        for seed in 0..200u64 {
            let (r, ev) = single_game(&g, seed).expect("game");
            assert_eq!(ev.last().unwrap().kind, "game_end");
            assert_eq!(ev.last().unwrap().state.score, r.runs);
            let runs: [u16; 2] = [0, 1].map(|s| ev.iter().filter(|e| e.kind == "pa" && e.batting == s).map(|e| e.runs).sum());
            assert_eq!(runs, r.runs, "seed {seed}");
            let ls: [u16; 2] = [0, 1].map(|s| ev.last().unwrap().state.linescore[s].iter().sum());
            assert_eq!(ls, r.runs);
            for (i, e) in ev.iter().enumerate() {
                assert_eq!(e.seq as usize, i + 1);
                assert!(e.state.outs <= 3);
                if e.kind == "pa" { assert_eq!(e.scorers.len() as u16, e.runs); }
            }
        }
        let (a, ea) = single_game(&g, 7).unwrap();
        let (b, eb) = single_game(&g, 7).unwrap();
        assert_eq!(a.runs, b.runs);
        assert_eq!(ea.len(), eb.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn league() -> [f64; NCAT] {
        // a plausible league split used ONLY as a synthetic fixture
        [0.225, 0.085, 0.011, 0.140, 0.043, 0.004, 0.030, 0.462]
    }
    fn flat_team() -> TeamInput {
        let p = RatePosterior { mean: league(), conc: 1e6 };
        TeamInput {
            lineup: std::array::from_fn(|_| p.clone()),
            starter: p.clone(),
            bullpen: p.clone(),
            bench: p.clone(),
            starter_bf: vec![24],
            bat_env: [1.0; NCAT],
        }
    }
    pub(super) fn game() -> GameInput {
        GameInput {
            away: flat_team(),
            home: flat_team(),
            league: league(),
            league_bf: vec![24],
            hook_prior_starts: 10.0,
            baserunning: Baserunning::default(), // forced moves only
            earned_share: 0.92,
            innings: 9,
            ghost_runner: true,
            sub_hazard: vec![0.0, 0.01, 0.02, 0.06, 0.15],
        }
    }

    #[test]
    fn matchup_is_identity_for_league_average_players() {
        let lg = league();
        let q = matchup(&lg, &lg, &lg, &[1.0; NCAT]);
        for c in 0..NCAT {
            assert!((q[c] - lg[c]).abs() < 1e-12);
        }
        let s: f64 = matchup(&[0.3, 0.1, 0.01, 0.15, 0.05, 0.005, 0.05, 0.335], &lg, &lg, &[1.0; NCAT]).iter().sum();
        assert!((s - 1.0).abs() < 1e-12);
    }

    #[test]
    fn forced_moves_follow_the_rules() {
        // bases loaded walk forces in one run
        let m = forced_move(BB, 0b111);
        assert_eq!(m.dest, [1, 2, 3, D_HOME]);
        assert_eq!(m.rbi, 1);
        // runner on 2B only: walk does not force him
        let m = forced_move(BB, 0b010);
        assert_eq!(m.dest, [1, D_NONE, 2, D_NONE]);
        assert_eq!(m.rbi, 0);
        // grand slam
        let m = forced_move(HR, 0b111);
        assert_eq!(m.rbi, 4);
        assert!(m.dest.iter().all(|&d| d == D_HOME));
    }

    #[test]
    fn game_invariants_hold() {
        let g = game();
        let mut rng = Rng::new(7);
        let t = build_tables(&g, &mut rng);
        for _ in 0..3000 {
            let r = sim_game(&g, &t, &mut rng).expect("game finished");
            assert!(r.innings >= 9);
            assert_ne!(r.runs[0], r.runs[1], "no ties");
            for side in 0..2 {
                let lines = &r.bat[side];
                let bench = &r.bench[side];
                let runs: u16 = lines.iter().chain(bench.iter()).map(|b| b.r).sum();
                assert_eq!(runs, r.runs[side], "batter runs reconcile with team runs");
                let rbi: u16 = lines.iter().chain(bench.iter()).map(|b| b.rbi).sum();
                assert!(rbi <= runs);
                for b in lines.iter().chain(bench.iter()) {
                    assert_eq!(b.h, b.s1 + b.s2 + b.s3 + b.hr);
                    assert_eq!(b.tb(), b.s1 + 2 * b.s2 + 3 * b.s3 + 4 * b.hr);
                    assert!(b.pa >= b.h + b.bb + b.k + b.hbp);
                }
                // batting order: slot PAs (starter + replacement) differ by at most one, in order
                let slot_pa = |s: usize| lines[s].pa + bench[s].pa;
                for s in 1..9 {
                    assert!(slot_pa(s - 1) >= slot_pa(s) && slot_pa(s - 1) - slot_pa(s) <= 1);
                    assert!(lines[s].pa >= 1, "every starter bats at least once");
                }
                // runs allowed by the fielding side's pitchers = runs scored against
                let field = 1 - side;
                assert_eq!(r.starter[field].r + r.bullpen[field].r, r.runs[side]);
                assert!(r.starter[field].er + r.bullpen[field].er <= r.runs[side]);
                // every PA faced by a pitcher
                let pa: u16 = lines.iter().chain(bench.iter()).map(|b| b.pa).sum();
                assert_eq!(r.starter[field].bf + r.bullpen[field].bf, pa);
            }
            // the away side always bats a full 3 outs per inning
            assert_eq!(r.starter[1].outs + r.bullpen[1].outs, 3 * r.innings as u16);
        }
    }

    #[test]
    fn same_seed_same_result_different_seed_differs() {
        let g = game();
        let a = simulate(&g, 2000, 4, 42);
        let b = simulate(&g, 2000, 4, 42);
        let c = simulate(&g, 2000, 4, 43);
        assert_eq!(a.total_runs.counts, b.total_runs.counts);
        assert_ne!(a.total_runs.counts, c.total_runs.counts);
        assert_eq!(a.valid + a.rejected, 2000);
    }

    #[test]
    fn symmetric_teams_give_a_plausible_game() {
        let out = simulate(&game(), 20_000, 10, 1);
        let hw = out.home_win();
        // identical teams, no home edge in the fixture: close to a coin flip
        assert!((hw - 0.5).abs() < 0.03, "home win {hw}");
        let lp = out.total_runs.line(8.5);
        assert!((lp.over + lp.under + lp.push - 1.0).abs() < 1e-9);
        assert_eq!(lp.push, 0.0, "no push on a half line");
        let integer = out.total_runs.line(8.0);
        assert!(integer.push > 0.0);
        // leadoff gets more PAs than the 9-hole
        let pa = BAT_STATS.iter().position(|&s| s == "pa").unwrap();
        assert!(out.bat[0][0][pa].mean() > out.bat[0][8][pa].mean());
    }

    #[test]
    fn mom_kappa_recovers_spread() {
        // synthetic: true rates spread around 0.2 with Beta(40, 160) -> kappa ~ 200
        let mut rng = Rng::new(3);
        let units: Vec<(f64, f64)> = (0..400).map(|_| {
            let a = rng.gamma(40.0);
            let b = rng.gamma(160.0);
            let p = a / (a + b);
            let n = 600.0;
            let x = (0..600).filter(|_| rng.f64() < p).count() as f64;
            (x, n)
        }).collect();
        let k = mom_kappa(&units, 100.0).unwrap();
        assert!(k > 120.0 && k < 330.0, "kappa {k}");
    }

    #[test]
    fn shrinkage_pulls_small_samples_to_league() {
        let lg = league();
        let kappa = [60.0; NCAT];
        // 10 PA, 5 HR: a tiny-sample outlier
        let mut c = [0.0; NCAT];
        c[HR] = 5.0;
        c[OUT] = 5.0;
        let small = shrink(&c, &lg, &kappa);
        assert!(small.mean[HR] < 0.12, "{:?}", small.mean);
        // same rate over 1000 PA is believed far more
        let mut c2 = [0.0; NCAT];
        c2[HR] = 500.0;
        c2[OUT] = 500.0;
        let big = shrink(&c2, &lg, &kappa);
        assert!(big.mean[HR] > 0.4);
        assert!((small.mean.iter().sum::<f64>() - 1.0).abs() < 1e-12);
    }
}
