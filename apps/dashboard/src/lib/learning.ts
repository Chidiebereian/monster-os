// Adaptive learning loop API (projection-service /learning/*): frozen pregame
// predictions, postgame scores, the model registry and its audit trail.

export interface MarketScore {
  n: number;
  baseline: string;
  brier_model: number;
  brier_baseline: number;
  brier_diff: number;
  brier_diff_se: number;
  log_loss_model?: number;
  log_loss_baseline?: number;
  ece_model?: number;
}

export interface LedgerRow {
  id: number; sport: string; game_id: string; game_date: string; game_start: string | null; frozen_at: string;
  model_version: string; lineup: string; n_sims: number; sha256: string; away: string | null; home: string | null;
  forecasts: number; scored: number; resolved: number; brier_model: number | null; brier_baseline: number | null;
}

export interface LedgerForecast {
  market: string; subject: string; name: string; stat: string; line: number; p_model: number; p_base: number; baseline: string;
  actual: number | null; y: number | null; void_reason: string | null; scored_at: string | null;
}

export interface LedgerEntry extends Omit<LedgerRow, "forecasts" | "scored" | "resolved" | "brier_model" | "brier_baseline" | "away" | "home"> {
  engine: string; seed: number; data_cutoff: string; hash_verified: boolean; forecasts: LedgerForecast[];
  payload: { away?: string; home?: string; win?: { home: number }; [k: string]: unknown };
}

export interface Summary {
  live: { sport: string; model_version: string; markets: Record<string, MarketScore> }[];
  counts: { sport: string; predictions: number; forecasts: number; resolved: number; void: number; first_game: string | null; last_game: string | null }[];
  backtests: { key: string; run_at: string; season: number; games: number | null; lineup: string | null; markets: Record<string, MarketScore> | null; model_version: string | null }[];
  note: string;
}

export interface Gate { rule: string; improves: string[]; degrades: string[]; pass: boolean; stage?: "search" | "holdout" }
export type Paired = Record<string, { n: number; brier_diff_candidate_minus_champion: number; se: number; z: number }>;
/** One paired evaluation: champion and candidate on the same games and seeds. */
export interface EvalReport {
  champion?: string; candidate?: string; season: number; games: number; sims_per_game: number; secs: number; evaluated_at: string;
  paired: Paired; gate: Gate;
}
/** Evidence behind a status: a manual evaluation (an EvalReport), or the
 *  autopilot's search + holdout reports and, once live, the shadow comparison. */
export interface Evidence extends Partial<EvalReport> {
  champion: string; gate: Gate; change?: string;
  search?: EvalReport; holdout?: EvalReport;
  live?: { games: number; markets: Paired; shadow_better: string[]; shadow_worse: string[] };
}
export interface RegistryRow {
  sport: string; version: string; engine: string; config: Record<string, number>; status: "candidate" | "shadow" | "champion" | "retired" | "rejected";
  parent: string | null; evidence: Evidence | null; notes: string | null; created_at: string; promoted_at: string | null; retired_at: string | null;
  current_engine: string;
}
export type Tunables = Record<string, Record<string, { default: number; min: number; max: number; note: string }>>;

export interface AuditRow { id: number; at: string; sport: string; action: string; version: string | null; detail: Record<string, unknown> }

export interface AutopilotJob { job: string; last_run: string; ok: boolean; detail: Record<string, unknown> }
export interface ShadowProgress {
  engine: string; version: string; role: string; notes: string | null; champion: string; live_games: number; needed: number;
  markets: Paired; shadow_better: string[]; shadow_worse: string[];
}
export interface Autopilot { enabled: boolean; paused: boolean; now: string; jobs: AutopilotJob[]; shadows: ShadowProgress[]; schedule: Record<string, string | number> }

/** Registry key -> label: "nba" -> "NBA", "nba_team" -> "NBA team model". */
export const engineLabel = (e: string) => (e.endsWith("_team") ? `${e.slice(0, -5).toUpperCase()} team model` : e.toUpperCase());

async function req<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(`/api/projection-service${path}`, {
    ...init,
    headers: { accept: "application/json", ...(init?.body ? { "content-type": "application/json" } : {}) },
  });
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    let msg = body;
    try { msg = JSON.parse(body).error ?? body; } catch { /* plain text */ }
    throw new Error(`${res.status}: ${msg.slice(0, 400)}`);
  }
  return res.json() as Promise<T>;
}

const qs = (o: Record<string, string | number | undefined>) =>
  Object.entries(o).filter(([, v]) => v !== undefined && v !== "").map(([k, v]) => `${k}=${encodeURIComponent(String(v))}`).join("&");

export const learningApi = {
  summary: (sport?: string) => req<Summary>(`/learning/summary?${qs({ sport })}`),
  ledger: (sport?: string, limit = 100) => req<{ ledger: LedgerRow[] }>(`/learning/ledger?${qs({ sport, limit })}`),
  entry: (id: number) => req<LedgerEntry>(`/learning/ledger/${id}`),
  registry: (sport?: string) => req<{ registry: RegistryRow[]; tunables: Tunables }>(`/learning/registry?${qs({ sport })}`),
  audit: (sport?: string, limit = 100) => req<{ audit: AuditRow[] }>(`/learning/audit?${qs({ sport, limit })}`),
  freeze: (sport: string, date?: string) => req<{ frozen: unknown[]; skipped: { game_id: string; reason: string }[]; model_version: string }>(
    `/learning/${sport}/freeze?${qs({ date })}`, { method: "POST" }),
  score: (sport: string) => req<{ games_scored: number; forecasts_scored: number; forecasts_void: number; games_pending: number }>(
    `/learning/${sport}/score`, { method: "POST" }),
  register: (sport: string, config: Record<string, number>, notes?: string) =>
    req<{ version: string }>(`/learning/${sport}/candidates`, { method: "POST", body: JSON.stringify({ config, notes }) }),
  evaluate: (sport: string, version: string, games = 300) =>
    req<EvalReport>(`/learning/${sport}/evaluate?${qs({ version, games })}`, { method: "POST" }),
  promote: (sport: string, version: string, reason?: string) =>
    req<{ champion: string }>(`/learning/${sport}/promote?${qs({ version, reason })}`, { method: "POST" }),
  autopilot: () => req<Autopilot>(`/learning/autopilot`),
  setPaused: (paused: boolean) => req<Autopilot>(`/learning/autopilot?paused=${paused}`, { method: "POST" }),
  runTick: () => req<{ frozen: number; forecasts_scored: number; errors: unknown[] }>(`/learning/autopilot/run?job=tick`, { method: "POST" }),
  rollback: (sport: string, reason?: string) =>
    req<{ champion: string }>(`/learning/${sport}/rollback?${qs({ reason })}`, { method: "POST" }),
};

/** "beats" / "worse" only when the paired difference is at least two standard errors. */
export function verdict(diff: number, se: number): "beats" | "worse" | "even" {
  if (diff < -2 * se) return "beats";
  if (diff > 2 * se) return "worse";
  return "even";
}
