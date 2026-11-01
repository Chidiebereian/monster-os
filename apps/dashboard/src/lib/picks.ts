// AI Picks API (projection-service /picks*): model-backed plays priced from the
// champion engines' frozen forecasts, graded by edge and capped by track record.

export type Tier = "proven" | "baseline" | "unproven" | "losing";

export interface RiskFlag { code: string; text: string }

export interface Pick {
  id: number; sport: string; engine: string; model_version: string; ledger_id: number;
  game_id: string; game_label: string; home: string | null; away: string | null; starts_at: string | null;
  kind: "prop" | "game"; market: string; market_label: string; subject: string; subject_name: string; subject_team: string | null;
  headshot: string | null; side: string; label: string; line: number | null; book: string; book_title: string; price: number; link: string | null;
  p_model: number; p_market: number; edge: number; ev: number; projection: number | null; grade: string; tier: Tier;
  risk_flags: RiskFlag[]; writeup: string; writeup_source: string; published_at: string;
  latest: { line: number | null; price: number | null; book: string | null; p_market: number | null; edge: number | null; updated_at: string } | null;
  status: "open" | "started" | "settled" | "void"; result: "win" | "loss" | "push" | "void" | null; actual: number | null;
  profit: number | null; clv: number | null; close: { line: number | null; price: number | null }; settled_at: string | null;
  /** suggested stake, fraction of bankroll (fractional Kelly scaled by track record); null on picks published before stakes */
  stake: number | null;
  /** break-even price, and the worst price still worth playable_min_ev per unit (null on picks published before them) */
  fair_price: number | null; playable_price: number | null;
  /** latest price still at least the playable price at the published line; null = settled, or the line moved (re-check) */
  playable_now: boolean | null;
  /** validation engine: is the engine reliable on this market (A+..D, I = insufficient data)? */
  model_quality?: string;
  /** validation engine: is the current price worth it (EV at the latest price, A+..F)? */
  opportunity_grade?: string;
  ev_now?: number;
  validation_status?: MarketStatus;
  qualification?: Qualification;
  feed_quarantined?: boolean;
}

export type MarketStatus = "verified" | "model_only" | "no_signal" | "losing" | "insufficient_data";
export type Qualification = "verified_edge" | "watchlist" | "high_uncertainty" | "no_value" | "insufficient_data" | "suspended";

export const QUALIFICATIONS: { key: Qualification; label: string; text: string }[] = [
  { key: "verified_edge", label: "Verified edge", text: "The engine has beaten the market on this market, and the current price has positive EV" },
  { key: "watchlist", label: "Watchlist", text: "The engine beats its naive baseline but has not beaten the market yet" },
  { key: "high_uncertainty", label: "High uncertainty", text: "A large disagreement from an engine without a market track record" },
  { key: "no_value", label: "No value", text: "The current price no longer has positive EV" },
  { key: "insufficient_data", label: "Insufficient data", text: "Too few settled lines and no backtest for this market" },
  { key: "suspended", label: "Suspended", text: "Started, price gone, or the odds feed is quarantined" },
];

export interface Calib { n: number; brier?: number; logloss?: number; ece?: number; mean_p?: number; hit_rate?: number; reliability?: { predicted: number; observed: number; n: number }[] }
export interface Interval { mean: number; lo: number; hi: number }
export interface ScoreMarket {
  sport: string; engine: string; version: string | null; market: string; kind: "prop" | "game"; tier: Tier; tier_source: string | null;
  model_quality: string; status: MarketStatus; excluded_picks: number; reasons: string[];
  backtest: { comparison: string; n: number; seasons: number; better: number; worse: number; w: number | null } | null;
  lines: (Calib & { brier_model?: number; brier_market?: number; brier_vs_market?: Interval | null }) | null;
  picks: { n: number; units: number; roi: number; roi_lo: number; roi_hi: number; hit_rate: number; max_drawdown: number; profit_factor: number | null;
    sharpe: number; sortino: number; clv_mean: number | null; clv_positive: number | null } | null;
}
export interface ExperimentModel {
  model: string; brier: number; ece: number; brier_vs_market?: { mean: number; lo95: number; hi95: number; lo99: number; hi99: number } | null;
  folds_better?: number; bets?: number | null; roi?: { mean: number; lo95: number; hi95: number } | null; clv_mean?: number | null;
}
export interface Experiment {
  id: number; created_at: string; name: string; sport: string; markets: string[]; source: "live" | "backtest"; target: "outcome" | "movement";
  dataset_hash: string; n: number; n_test: number; folds: number; best_model: string | null;
  verdict: "shadow_candidate" | "inconclusive" | "rejected" | "insufficient_data"; gate: { rule: string; why: string; best_model: string | null };
  models: ExperimentModel[]; code_version: string;
}
export interface Scorecard {
  generated_at: string; min_n: number;
  summary: Record<"markets" | MarketStatus, number>;
  markets: ScoreMarket[];
  forecasts: (Calib & { sport: string; engine: string; version: string; market: string; champion: boolean; brier_vs_baseline: Interval | null })[];
  quarantined_feeds: { sport: string; market: string; book: string }[];
  experiments?: Experiment[];
  dataset?: { sport: string; source: string; examples: number; games: number; from: string | null; to: string | null }[];
  feeds: { sport: string; market: string; book: string; side: string; n: number; implied: number; hit_rate: number; z: number; suspect: boolean }[];
}

/** Split a writeup into the case for the pick and the case against it. */
export function splitWriteup(w: string): { case_for: string; against: string | null } {
  const i = w.indexOf("What could make it wrong:");
  if (i < 0) return { case_for: w, against: null };
  return { case_for: w.slice(0, i).trim(), against: w.slice(i + "What could make it wrong:".length).trim() };
}

export interface PickFacts {
  q10?: number; q50?: number; q90?: number; p_active?: number; p_push?: number; books_quoting?: number; lineup_mode?: string;
  l10_avg?: number; l10_hits?: number; l10_games?: number; l20_avg?: number; l20_hits?: number; l20_games?: number;
  l5_minutes?: number; proj_minutes?: number; open_line?: number | null; evidence_text?: string;
  /** engine's own probability before blending; naive baseline (props); blend weights (0 = anchor, 1 = raw model) */
  p_raw?: number; p_naive?: number | null; w_market?: number; w_naive?: number | null; blend_source?: string; blend_text?: string;
  /** the sharp check: Pinnacle (or the other books' consensus) de-vigged, and this price's EV against it */
  p_sharp?: number; sharp_ev?: number; sharp_source?: string; sharp_text?: string; stake?: number;
  fair_price?: number | null; playable_price?: number | null; playable_min_ev?: number;
  game_log?: { opp: string; value: number | null }[];
  home_form?: { games: number; w: number; l: number; pf: number; pa: number };
  away_form?: { games: number; w: number; l: number; pf: number; pa: number };
  evidence?: { source: string; comparison: string; n: number; seasons: number; better: number; worse: number };
  [k: string]: unknown;
}

export interface PickDetail extends Pick {
  facts: PickFacts;
  books?: { book: string; book_title: string; side: string; line: number | null; price: number; link: string | null; observed_at: string }[];
}

export interface PickMeta {
  markets: { id: string; label: string }[];
  games: { id: string; label: string }[];
  books: { id: string; label: string }[];
  usage_today: { provider: string; requests: number; units: number }[];
  lines: { source: string; sport: string; last: string; rows: number }[];
  counts: { hidden: number; total: number } | null;
  llm_writeups: { configured: boolean; reachable: boolean; model: string | null };
  settings: { min_edge: number; oddsapi_daily_credits: number; sgo_daily_entities: number };
}

export interface RecordRow {
  sport: string | null; kind: string | null; market: string | null; tier: string | null; grade: string | null;
  graded: number; wins: number; losses: number; pushes: number; units: number; roi: number | null; win_rate: number | null;
  clv_mean: number | null; clv_n: number; brier_vs_market: number | null; kelly_roi: number | null;
}
export interface EvidenceRow {
  engine: string; market: string; source: string; comparison: string; n: number; seasons: number; better: number; worse: number;
  metric: number | null; se: number | null; detail: Record<string, unknown>; updated_at: string;
}
export interface PickRecord { days: number; rows: RecordRow[]; evidence: EvidenceRow[]; tiers: { engine: string; market: string; tier: Tier; cap: string }[] }

/** A stale line: a book's price that beats the sharp fair price (no model involved). */
export interface Edge {
  id: number; sport: string; game_id: string; game_label: string; home: string | null; away: string | null; starts_at: string | null;
  kind: "prop" | "game"; market: string; market_label: string; subject: string; subject_name: string; side: string; label: string; line: number | null;
  book: string; book_title: string; price: number; link: string | null; fair_source: "pinnacle" | "consensus"; fair_books: number; fair_text: string;
  p_fair: number; p_book: number; ev: number; stake: number; observed_at: string; published_at: string;
  latest: { price: number | null; ev: number | null; updated_at: string };
  status: "open" | "started" | "settled" | "void"; close_p_fair: number | null; close_ev: number | null;
  actual: number | null; result: "win" | "loss" | "push" | "void" | null; profit: number | null; settled_at: string | null;
}
export interface EdgeRecord {
  fair_source: string | null; graded: number; wins: number; losses: number; pushes: number; units: number; roi: number | null;
  kelly_roi: number | null; close_ev_mean: number | null; close_ev_n: number; ev_mean: number | null;
}
export interface EdgesResponse {
  count: number; edges: Edge[]; record: EdgeRecord[]; days: number;
  settings: { min_ev: number; max_ev: number; consensus_books: number; kelly_fraction: number; kelly_cap: number };
}

export interface PickFilters {
  sport?: string; kind?: string; side?: string; grades?: string; market?: string; game?: string; book?: string;
  min_edge?: number; min_prob?: number; sort?: string; q?: string; include_hidden?: boolean; status?: string; limit?: number; qualification?: string;
}

async function req<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(`/api/projection-service${path}`, { ...init, headers: { accept: "application/json" } });
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    let msg = body;
    try { msg = JSON.parse(body).error ?? body; } catch { /* plain text */ }
    throw new Error(`${res.status}: ${msg.slice(0, 400)}`);
  }
  return res.json() as Promise<T>;
}

const qs = (o: Record<string, string | number | boolean | undefined>) =>
  Object.entries(o).filter(([, v]) => v !== undefined && v !== "" && v !== false).map(([k, v]) => `${k}=${encodeURIComponent(String(v))}`).join("&");

export const picksApi = {
  list: (f: PickFilters) => req<{ count: number; picks: Pick[] }>(`/picks?${qs({ ...f })}`),
  get: (id: number) => req<PickDetail>(`/picks/${id}`),
  meta: (sport?: string) => req<PickMeta>(`/picks/meta?${qs({ sport })}`),
  record: (sport?: string, days = 90) => req<PickRecord>(`/picks/record?${qs({ sport, days })}`),
  edges: (sport?: string, status = "open", days = 90) => req<EdgesResponse>(`/picks/edges?${qs({ sport, status, days })}`),
  run: (sport?: string, force = false) => req<{ runs: unknown[] }>(`/picks/run?${qs({ sport, force })}`, { method: "POST" }),
  scorecard: (sport?: string, kind?: string) => req<Scorecard>(`/validation/scorecard?${qs({ sport, kind })}`),
};

/** The bankroll stakes are shown against (per browser). */
export const BANKROLL_KEY = "aiPicks.bankroll";
export const readBankroll = () => { try { const v = Number(localStorage.getItem(BANKROLL_KEY)); return v > 0 ? v : 1000; } catch { return 1000; } };
export const money = (x: number) => `$${x >= 100 ? Math.round(x).toLocaleString() : x.toFixed(2)}`;

export const GRADES = ["A+", "A", "A-", "B+", "B", "B-", "C+", "C"];

export const odds = (p: number | null | undefined) => (p == null ? "—" : p > 0 ? `+${p}` : `${p}`);
export const pct = (x: number | null | undefined, d = 0) => (x == null ? "—" : `${(x * 100).toFixed(d)}%`);
export const signedPct = (x: number | null | undefined, d = 1) => (x == null ? "—" : `${x >= 0 ? "+" : ""}${(x * 100).toFixed(d)}%`);
export const num = (x: number | null | undefined, d = 1) => (x == null ? "—" : Number.isInteger(x) ? `${x}` : x.toFixed(d));

export const TIER_TEXT: Record<Tier, string> = {
  proven: "Blended model beat real closing lines",
  baseline: "Beat a naive model in backtests (capped B+)",
  unproven: "No track record yet (capped C+)",
  losing: "Lost to the comparison in backtests (capped C)",
};

const ESPN_ABBR: Record<string, Record<string, string>> = {
  nhl: { TBL: "tb", NJD: "nj", LAK: "la", SJS: "sj", VGK: "vgk", WSH: "wsh", UTA: "utah" },
  nfl: { LA: "lar", WAS: "wsh", JAX: "jax", LV: "lv" },
  nba: { NOP: "no", NYK: "ny", SAS: "sa", GSW: "gs", UTA: "utah", WAS: "wsh" },
};
export const teamLogo = (sport: string, abbr: string | null | undefined) =>
  abbr ? `https://a.espncdn.com/i/teamlogos/${sport}/500/${(ESPN_ABBR[sport]?.[abbr] ?? abbr).toLowerCase()}.png` : null;

export const startLabel = (iso: string | null) => {
  if (!iso) return "";
  const d = new Date(iso);
  return `${d.getMonth() + 1}/${d.getDate()} @ ${d.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })}`;
};
