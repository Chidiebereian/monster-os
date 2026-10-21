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
}

export interface PickFacts {
  q10?: number; q50?: number; q90?: number; p_active?: number; p_push?: number; books_quoting?: number; lineup_mode?: string;
  l10_avg?: number; l10_hits?: number; l10_games?: number; l20_avg?: number; l20_hits?: number; l20_games?: number;
  l5_minutes?: number; proj_minutes?: number; open_line?: number | null; evidence_text?: string;
  /** engine's own probability before blending; naive baseline (props); blend weights (0 = anchor, 1 = raw model) */
  p_raw?: number; p_naive?: number | null; w_market?: number; w_naive?: number | null; blend_source?: string; blend_text?: string;
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
  clv_mean: number | null; clv_n: number; brier_vs_market: number | null;
}
export interface EvidenceRow {
  engine: string; market: string; source: string; comparison: string; n: number; seasons: number; better: number; worse: number;
  metric: number | null; se: number | null; detail: Record<string, unknown>; updated_at: string;
}
export interface PickRecord { days: number; rows: RecordRow[]; evidence: EvidenceRow[]; tiers: { engine: string; market: string; tier: Tier; cap: string }[] }

export interface PickFilters {
  sport?: string; kind?: string; side?: string; grades?: string; market?: string; game?: string; book?: string;
  min_edge?: number; min_prob?: number; sort?: string; q?: string; include_hidden?: boolean; status?: string;
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
  run: (sport?: string, force = false) => req<{ runs: unknown[] }>(`/picks/run?${qs({ sport, force })}`, { method: "POST" }),
};

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
