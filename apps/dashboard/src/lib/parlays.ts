// Parlay Builder API (projection-service /parlays*): joint probabilities from
// the champion simulators (legs of one game graded on the same simulated
// games), leg levels from the AI Picks blend, one-book prices, EV.

export interface Quote { book: string; book_title: string; price: number; p_novig: number | null; observed_at: string; one_sided: boolean }
export interface Leg {
  id: string; sport: string; game: string; game_label: string | null; starts_at: string | null;
  market: string; category: string; kind: "game" | "prop"; subject: string; subject_name: string | null; team: string | null;
  side: string; line: number | null; label: string; market_label: string;
  best: { book: string; book_title: string; price: number } | null; quotes: Quote[];
  p_market: number | null; p_sim: number | null; p_naive: number | null; p_blend: number | null; push: number; void: number; ev: number | null;
  tier: string; flags: string[]; eligible: boolean;
  /** on a ticket: the price at the ticket's book and the leg's tilted win/loss probabilities */
  price?: number; decimal?: number; p_win?: number; p_loss?: number;
}
export interface PairDiag { a: string; b: string; a_id: string; b_id: string; p_a: number; p_b: number; p_both: number; p_indep: number; lift: number; p_b_given_a: number }
export interface Ticket {
  key: string; sport: string; date: string; n_legs: number; categories: string[]; same_game: boolean; starts_at: string | null; quotes_at: string | null;
  book: string | null; book_title: string | null; price_decimal: number; price_american: number; price_product: number; sgp_adjust: number; price_verified: boolean; price_note: string | null;
  p_joint: number; p_dep: number; p_indep: number; p_raw: number; p_market: number; p_market_indep: number | null; p_cons: number; p_loss: number;
  lift: number; range: [number, number]; fair_decimal: number | null; fair_american: number | null;
  e_payout: number; ev: number; ev_cons: number; mc_se: number; ess: number; edge: number; edge_rel: number;
  dep_weight: number; model_version: string;
  legs: Leg[]; games: { game: string; label: string | null; legs: number; p_win: number; p_win_raw: number; p_win_market: number; p_indep: number; ess: number; sims: number | null }[];
  pairs: PairDiag[]; warnings: string[]; explain: string[];
}
export interface SlateGame {
  id: string; label: string; home: string; away: string; starts_at: string | null; status: "simulated" | "pending" | "excluded" | "error";
  detail: string | null; sims: number; sim_ms: number; simulated_at: string | null; engine: string; lineup: string; legs: number; eligible: number; missing_columns: number;
}
export interface Coverage {
  games: number; simulated: number; pending: number; excluded: number; errors: number; legs: number; eligible: number;
  by_category: Record<string, { legs: number; eligible: number }>; model_bias: { market: string; label: string; lines: number; mean_logit: number; recentred: boolean }[]; oldest_quote: string | null; newest_quote: string | null; max_age_minutes: number;
}
export interface Job { running: boolean; done: number; total: number; current: string | null; started: string | null; finished: string | null; errors: string[] }
export interface SlateResponse {
  sport: string; date: string; timezone: string; utc_offset: number; generated_at: string; model_version: string; dep_weight: number;
  coverage: Coverage; games: SlateGame[]; pending: string[]; job: Job;
  markets: { key: string; label: string; category: string; legs: number }[]; books: { key: string; title: string; legs: number }[]; live: boolean; note: string;
}
export interface GenerateResponse {
  sport: string; date: string; timezone: string; generated_at: string; legs: number; objective: string; category: string;
  coverage: Coverage; games: SlateGame[]; pending: string[]; job: Job; model_version: string; dep_weight: number;
  stats: { eligible_legs: number; pool: number; evaluated: number; ms: number }; notes: string[]; tickets: Ticket[];
}
export interface EvaluateResponse { ok: boolean; problems: string[]; pending: string[]; books: { book: string; book_title: string; price_decimal: number; price_american: number }[]; ticket: Ticket | null }
export interface StoredTicket {
  id: number; key: string; sport: string; source: "auto" | "custom"; category: string | null; objective: string | null; n_legs: number; same_game: boolean; date: string;
  starts_at: string | null; book: string | null; book_title: string | null; price_decimal: number; price_american: number; price_verified: boolean;
  p_joint: number; p_indep: number; p_market_indep: number | null; ev: number; ev_cons: number; mc_se: number; lift: number; dep_weight: number; model_version: string;
  legs: Leg[]; created_at: string; latest_price_decimal: number | null; latest_p_joint: number | null; latest_at: string | null; saved: boolean; note: string | null;
  status: "open" | "settled" | "void"; result: "win" | "loss" | "push" | "void" | null; payout: number | null; profit: number | null;
  legs_result: { id: string; label: string; result: string; actual: number | null; note: string }[] | null; settled_at: string | null;
  snapshot?: Ticket;
}
export interface Summary {
  key?: string; n: number; wins: number; losses: number; pushes: number; voids: number; roi: number; roi_se: number | null; units: number; slates: number;
  calibration_n: number; hit_rate: number | null; mean_p: number | null; brier: number; brier_indep: number; brier_vs_indep: number; brier_vs_indep_se: number | null;
  brier_vs_market: number | null; brier_vs_market_se: number | null; market_n: number; logloss: number; logloss_indep: number;
  close_n: number; price_vs_close: number | null; price_vs_close_se: number | null;
}
export interface RecordResponse {
  overall: Summary; by_sport: Summary[]; by_source: Summary[]; by_legs: Summary[]; by_category: Summary[]; by_odds: Summary[]; by_dependence: Summary[]; by_scope: Summary[];
  calibration: { lo: number; hi: number; n: number; predicted: number; observed: number; ci: [number, number] }[];
  open: number; void: number; since: string | null; stake: string; note: string;
}
export interface ValidationResponse {
  engine: string; champion: string; dep_weight: number; rule: string;
  preview: { candidate: number; current: number; train_n: number; test_n: number; train_brier: { a: number; brier: number }[]; test_diff: number; test_se: number | null; pass: boolean; reason: string };
  models: { version: string; dep_weight: number; status: string; parent: string | null; evidence: Record<string, unknown>; created_at: string; promoted_at: string | null; retired_at: string | null }[];
  runs: { sport: string; date: string; started_at: string; finished_at: string | null; ok: boolean | null; detail: Record<string, unknown> }[];
}

export interface GenFilters {
  sport: string; date?: string; category?: string; markets?: string; props?: string; legs?: number; objective?: string; books?: string;
  games?: string; exclude_games?: string; teams?: string; exclude_teams?: string; players?: string; exclude_players?: string; exclude_legs?: string; locked?: string; side?: string;
  min_odds?: number; max_odds?: number; min_prob?: number; max_prob?: number; min_leg_prob?: number; min_ev?: number; min_ev_cons?: number;
  scope?: string; max_per_game?: number; max_per_player?: number; max_per_team?: number; min_lift?: number; max_lift?: number; max_range?: number;
  min_tier?: string; verified_only?: boolean; include_flagged?: boolean; max_age?: number; limit?: number; pool?: number; beam?: number;
}

async function req<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(`/api/projection-service${path}`, { ...init, headers: { accept: "application/json", ...(init?.body ? { "content-type": "application/json" } : {}) } });
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    let msg = body;
    try { msg = JSON.parse(body).error ?? body; } catch { /* plain text */ }
    throw new Error(`${res.status}: ${msg.slice(0, 400)}`);
  }
  return res.json() as Promise<T>;
}
const qs = (o: object) =>
  Object.entries(o).filter(([, v]) => v !== undefined && v !== "" && v !== false && v !== null && !(typeof v === "number" && Number.isNaN(v)))
    .map(([k, v]) => `${k}=${encodeURIComponent(String(v))}`).join("&");

export const parlaysApi = {
  slate: (sport: string, date?: string, max_age?: number) => req<SlateResponse>(`/parlays/slate?${qs({ sport, date, max_age })}`),
  legs: (sport: string, o: { date?: string; game?: string; q?: string; market?: string; category?: string; max_age?: number }) =>
    req<{ total: number; legs: Leg[] }>(`/parlays/legs?${qs({ sport, ...o })}`),
  generate: (f: GenFilters) => req<GenerateResponse>(`/parlays/generate?${qs(f)}`),
  evaluate: (sport: string, legs: string[], date?: string, book?: string) =>
    req<EvaluateResponse>(`/parlays/evaluate`, { method: "POST", body: JSON.stringify({ sport, legs, date, book }) }),
  save: (sport: string, legs: string[], date?: string, book?: string, note?: string) =>
    req<{ id: number; ticket: Ticket }>(`/parlays/tickets`, { method: "POST", body: JSON.stringify({ sport, legs, date, book, note }) }),
  tickets: (o: { sport?: string; source?: string; status?: string; saved?: boolean; date?: string; legs?: number; category?: string; limit?: number }) =>
    req<{ tickets: StoredTicket[] }>(`/parlays/tickets?${qs(o)}`),
  ticket: (id: number) => req<StoredTicket>(`/parlays/tickets/${id}`),
  unsave: (id: number) => req<{ updated: number }>(`/parlays/tickets/${id}/unsave`, { method: "POST" }),
  record: (o: { sport?: string; days?: number; source?: string }) => req<RecordResponse>(`/parlays/record?${qs(o)}`),
  validation: () => req<ValidationResponse>(`/parlays/validation`),
  cancel: (sport: string) => req<{ cancelled: boolean }>(`/parlays/cancel?${qs({ sport })}`, { method: "POST" }),
};

// --- odds formats ---
export type OddsFormat = "american" | "decimal" | "fractional";
export const ODDS_KEY = "parlays.oddsFormat";
export const readOddsFormat = (): OddsFormat => { try { return (localStorage.getItem(ODDS_KEY) as OddsFormat) || "american"; } catch { return "american"; } };

export const americanToDecimal = (a: number) => (a > 0 ? 1 + a / 100 : 1 + 100 / -a);
export const decimalToAmerican = (d: number) => (d >= 2 ? Math.round((d - 1) * 100) : d > 1 ? Math.round(-100 / (d - 1)) : 0);
function gcd(a: number, b: number): number { return b ? gcd(b, a % b) : a; }
/** Fractional odds from decimal, to the nearest 1/100. */
export const decimalToFractional = (d: number) => {
  const num = Math.round((d - 1) * 100);
  const g = gcd(num, 100) || 1;
  return `${num / g}/${100 / g}`;
};
export const fmtOdds = (dec: number | null | undefined, f: OddsFormat) => {
  if (dec == null || !Number.isFinite(dec) || dec <= 1) return "—";
  if (f === "decimal") return dec.toFixed(2);
  if (f === "fractional") return decimalToFractional(dec);
  const a = decimalToAmerican(dec);
  return a > 0 ? `+${a}` : `${a}`;
};
export const fmtAmerican = (a: number | null | undefined, f: OddsFormat) => (a == null ? "—" : fmtOdds(americanToDecimal(a), f));

export const CATEGORIES = [
  { key: "moneyline", label: "Moneylines" }, { key: "spread", label: "Spreads" }, { key: "total", label: "Game totals" },
  { key: "team_total", label: "Team totals" }, { key: "props", label: "Player props" }, { key: "mixed", label: "Mixed" },
];
export const OBJECTIVES = [
  { key: "ev_cons", label: "Conservative EV" }, { key: "ev", label: "Highest EV" }, { key: "prob", label: "Highest probability" },
  { key: "odds_high", label: "Longest odds" }, { key: "odds_low", label: "Shortest odds" }, { key: "uncertainty", label: "Lowest uncertainty" },
];
export const OBJECTIVE_TEXT: Record<string, string> = {
  ev_cons: "EV with every leg's probability halfway between the model and the de-vigged market, minus two simulation standard errors. A screen that favours tickets whose edge survives doubt — not a guarantee.",
  ev: "Expected profit per unit at the offered price. Rewards long odds when the model thinks they are underpriced; the most sensitive to model error.",
  prob: "The likeliest tickets to cash. Usually short prices with little or negative EV: probability is not value.",
  odds_high: "The biggest payouts the filters allow. Long tickets rarely win and errors in each leg compound.",
  odds_low: "The shortest prices: frequent small wins, usually at a loss to the vig.",
  uncertainty: "Tickets whose probability changes least between the simulator, the blend and the market, with the smallest simulation error.",
};
export const SPORTS = [{ key: "nba", label: "NBA" }, { key: "nfl", label: "NFL" }, { key: "nhl", label: "NHL" }, { key: "mlb", label: "MLB" }];
