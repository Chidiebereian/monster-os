// Broadcast simulation contract (projection-service /sim/sessions + /nfl/*).
// The backend produces every outcome; these types only describe what it sends.

export interface SimSnapshot {
  qtr: number;
  clock: number;
  score: [number, number]; // [away, home]
  possession: 0 | 1;
  down: number;
  togo: number;
  yardline_100: number;
  ball_x: number; // 0 = away goal line, 100 = home goal line
  first_down_x: number | null;
}

export interface SimEvent {
  seq: number;
  kind: string;
  qtr: number;
  clock_before: number;
  clock_after: number;
  offense: 0 | 1;
  down: number;
  togo: number;
  yardline_100: number;
  yards: number;
  passer: number | null;
  receiver: number | null;
  rusher: number | null;
  kicker: number | null;
  location: "left" | "middle" | "right" | null;
  x_from: number;
  x_to: number;
  first_down: boolean;
  touchdown: boolean;
  turnover: boolean;
  points: number;
  scoring_team: 0 | 1 | null;
  text: string;
  state: SimSnapshot;
  box_score: [number, number[]][];
}

export interface SimPlayer {
  idx: number;
  id: string;
  name: string;
  short: string;
  pos: string;
  team: 0 | 1;
}

export interface LineupEntry {
  id: string; name: string; pos: string; role: "starting QB" | "backup QB" | "QB" | "skill";
  target_share: number; carry_share: number; dropbacks: number; p_active: number;
  status: string; roster_status: string | null; report: string | null; practice: string | null; injury: string | null;
  scenario: "auto" | "in" | "out";
}

export interface SimTeamInputs {
  abbr: string;
  games_in_window: number;
  lineup: LineupEntry[];
  lineup_source: string;
  offense: {
    qb: string;
    qb_id: string;
    qb_p_active: number;
    backup_qb: string | null;
    qb_source: string;
    qb_dropbacks: number;
    sack_rate: number;
    int_rate: number;
    cmp_rate: number;
    pass_rate_over_expected_logit: number;
    receivers: { name: string; pos: string; target_share: number; catch_rate: number; yards_per_catch_vs_league: number }[];
    rushers: { name: string; pos: string; carry_share: number; yards_per_carry_vs_league: number }[];
    kicker: string | null;
  };
  defense: Record<string, number>;
}

export interface SimInputs {
  model_version: string;
  params: string;
  data_cutoff: string;
  data_window: string;
  players: SimPlayer[];
  teams: SimTeamInputs[];
  flags: string[];
  assumptions: string[];
  availability: { week: number; rates: { questionable: number; doubtful: number }; rates_source: string; scenario_out: string[]; scenario_in: string[] };
}

export interface SimSessionMeta {
  sport: string;
  game_id: string;
  date: string;
  season: number;
  postseason: boolean;
  away: string;
  home: string;
  stat_keys: string[];
  actual: { away_score: number | null; home_score: number | null };
  market: { spread_line: number | null; total_line: number | null };
  this_reality: { inactive: { id: string; name: string; p_active: number }[] };
  inputs: SimInputs;
}

/** An alternate reality: players ruled out / forced to play and chosen QBs (gsis ids). */
export interface Scenario { out: string[]; in: string[]; away_qb?: string; home_qb?: string }
export const EMPTY_SCENARIO: Scenario = { out: [], in: [] };
export const isEmptyScenario = (s: Scenario) => !s.out.length && !s.in.length && !s.away_qb && !s.home_qb;
const scenarioQs = (s: Scenario) => {
  const p = new URLSearchParams();
  if (s.out.length) p.set("out", s.out.join(","));
  if (s.in.length) p.set("in", s.in.join(","));
  if (s.away_qb) p.set("away_qb", s.away_qb);
  if (s.home_qb) p.set("home_qb", s.home_qb);
  return p.toString();
};

export interface SimSession {
  session_id: string;
  sport: string;
  game_id: string;
  seed: number;
  model_version: string;
  data_cutoff: string;
  events: number;
  meta: SimSessionMeta;
}

export interface SimEventsPage {
  session_id: string;
  total: number;
  after: number;
  next: number;
  done: boolean;
  events: SimEvent[];
}

export interface NflSimGame {
  game_id: string;
  season: number;
  week: string;
  date: string;
  away: string;
  home: string;
  away_score: number | null;
  home_score: number | null;
  spread_line: number | null;
  total_line: number | null;
  game_type: string | null;
  today: boolean;
}

export interface SimLine { line: number; over: number; under: number; push: number; mcse: number }
export interface SimDist { mean: number; q10: number; q50: number; q90: number; lines: SimLine[]; pmf_offset: number; pmf: number[] }

export interface NflEnsemblePlayer {
  idx: number;
  id: string;
  name: string;
  pos: string;
  role: "QB" | "K" | "skill";
  p_active: number;
  anytime_td: number;
  stats: Record<string, SimDist>;
}

export interface NflEnsemble {
  game_id: string;
  away: string;
  home: string;
  model_version: string;
  seed: number;
  n: number;
  sim_ms: number;
  win: { home: number; away: number; tie: number; overtime: number };
  score: { away: SimDist; home: SimDist };
  total: SimDist;
  margin_home: SimDist;
  plays: { away: number; home: number };
  market: { spread_line: number | null; total_line: number | null; p_home_cover: number | null; p_over_total: number | null };
  players: { away: NflEnsemblePlayer[]; home: NflEnsemblePlayer[] };
  inputs: SimInputs;
  note: string;
}

export interface NflBinary { n: number; baseline: string; brier_model: number; brier_baseline: number; brier_diff: number; brier_diff_se: number }
export interface NflBacktest {
  model_version: string; season: number; games_simulated: number; sims_per_game: number; params: string;
  binary: Record<string, NflBinary>;
  numeric: Record<string, { n: number; baseline: string; mae_model: number; mae_baseline: number; bias: number; coverage_80: number }>;
}

async function req<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(`/api/projection-service${path}`, {
    ...init,
    headers: { accept: "application/json", ...(init?.body ? { "content-type": "application/json" } : {}) },
  });
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    let msg = body;
    try { msg = JSON.parse(body).error ?? body; } catch { /* plain text */ }
    throw new Error(`${res.status}: ${msg.slice(0, 300)}`);
  }
  return res.json() as Promise<T>;
}

export const simApi = {
  nflGames: () => req<{ today: string; plays_through: string | null; games: NflSimGame[] }>(`/nfl/sim/games?days=10&back=7`),
  createSession: (body: { sport: string; game_id: string; seed?: number } & Partial<Scenario>) =>
    req<SimSession>(`/sim/sessions`, { method: "POST", body: JSON.stringify(body) }),
  events: (id: string, after = 0, limit = 1000) => req<SimEventsPage>(`/sim/sessions/${id}/events?after=${after}&limit=${limit}`),
  // production predictions: always 100,000 simulated games (the backend enforces the floor too)
  nflEnsemble: (gameId: string, scenario: Scenario = EMPTY_SCENARIO) => {
    const extra = scenarioQs(scenario);
    return req<NflEnsemble>(`/nfl/simulate/${gameId}?n=100000${extra ? `&${extra}` : ""}`);
  },
  nflBacktests: () => req<{ backtests: NflBacktest[] }>(`/nfl/backtests`),
};

/** Fetch the complete, ordered event log, verifying sequence continuity. */
export async function loadAllEvents(id: string): Promise<SimEvent[]> {
  const out: SimEvent[] = [];
  let after = 0;
  for (let guard = 0; guard < 50; guard++) {
    const page = await simApi.events(id, after, 1000);
    for (const e of page.events) {
      if (e.seq !== out.length + 1) throw new Error(`event stream gap: expected seq ${out.length + 1}, got ${e.seq}`);
      out.push(e);
    }
    if (page.done || page.events.length === 0) break;
    after = page.next;
  }
  return out;
}

// Team colours (primary, secondary) — factual brand colours, no logos.
export const NFL_COLORS: Record<string, [string, string]> = {
  ARI: ["#97233F", "#FFB612"], ATL: ["#A71930", "#000000"], BAL: ["#241773", "#9E7C0C"], BUF: ["#00338D", "#C60C30"],
  CAR: ["#0085CA", "#101820"], CHI: ["#0B162A", "#C83803"], CIN: ["#FB4F14", "#000000"], CLE: ["#311D00", "#FF3C00"],
  DAL: ["#003594", "#869397"], DEN: ["#FB4F14", "#002244"], DET: ["#0076B6", "#B0B7BC"], GB: ["#203731", "#FFB612"],
  HOU: ["#03202F", "#A71930"], IND: ["#002C5F", "#A2AAAD"], JAX: ["#006778", "#D7A22A"], KC: ["#E31837", "#FFB81C"],
  LV: ["#000000", "#A5ACAF"], LAC: ["#0080C6", "#FFC20E"], LA: ["#003594", "#FFA300"], MIA: ["#008E97", "#FC4C02"],
  MIN: ["#4F2683", "#FFC62F"], NE: ["#002244", "#C60C30"], NO: ["#D3BC8D", "#101820"], NYG: ["#0B2265", "#A71930"],
  NYJ: ["#125740", "#FFFFFF"], PHI: ["#004C54", "#A5ACAF"], PIT: ["#FFB612", "#101820"], SF: ["#AA0000", "#B3995D"],
  SEA: ["#002244", "#69BE28"], TB: ["#D50A0A", "#34302B"], TEN: ["#0C2340", "#4B92DB"], WAS: ["#5A1414", "#FFB612"],
};
export const teamColor = (abbr: string) => NFL_COLORS[abbr]?.[0] ?? "#475569";
export const teamAccent = (abbr: string) => NFL_COLORS[abbr]?.[1] ?? "#94A3B8";

export const fmtClock = (s: number) => `${Math.floor(Math.max(0, s) / 60)}:${String(Math.max(0, s) % 60).padStart(2, "0")}`;
export const qtrLabel = (q: number) => (q <= 4 ? `Q${q}` : q === 5 ? "OT" : `${q - 4}OT`);
const ORD = ["", "1st", "2nd", "3rd", "4th"];
export function downText(s: { down: number; togo: number; yardline_100: number }) {
  if (s.down < 1 || s.down > 4) return "";
  return `${ORD[s.down]} & ${s.togo >= s.yardline_100 ? "Goal" : s.togo}`;
}
export function spotText(s: { yardline_100: number; possession: 0 | 1 }, teams: [string, string]) {
  const yl = s.yardline_100;
  if (yl === 50) return "50";
  const own = teams[s.possession];
  const opp = teams[1 - s.possession];
  return yl > 50 ? `${own} ${100 - yl}` : `${opp} ${yl}`;
}
