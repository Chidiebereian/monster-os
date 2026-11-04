// Prediction-market arbitrage (arbitrage-engine /pm-arb): the same futures outcome on
// Kalshi and Polymarket where YES on one plus NO on the other costs under $1 after fees.

export interface PmLeg {
  venue: "kalshi" | "polymarket" | string;
  side: "yes" | "no";
  price: number | null;
  fee_per_contract: number | null;
  fee_known: boolean;
  market_id: string;
  event_title: string;
  outcome: string;
  rules: string;
  url: string;
  volume: number | null;
  resolves_at: string | null;
  top_size: number | null;
}
export interface PmStep { contracts: number; price_a: number; price_b: number; unit_cost: number }
export interface PmOpportunity {
  id: string;
  kind: "futures" | "game";
  league: string;
  family: string;
  team: string;
  entity_id: string;
  legs: [PmLeg, PmLeg];
  unit_cost: number;
  edge: number;
  roi: number;
  depth: { contracts: number; cost: number; fees: number; profit: number; steps: PmStep[] };
  resolves_at: string | null;
  days_to_resolve: number | null;
  first_seen: string;
  checks: { name: string; ok: boolean; detail: string }[];
}
export interface PmNear { family: string; team: string; kalshi: string; polymarket: string; unit_cost: number }
export interface PmSnapshot {
  scanned_at?: string;
  seconds?: number;
  markets?: Record<string, number>;
  pairs?: number;
  unmatched?: Record<string, number>;
  unresolved?: string[];
  opportunities: PmOpportunity[];
  closest?: PmNear[];
  errors?: string[];
  status?: string;
}

async function call(path: string, init?: RequestInit): Promise<PmSnapshot> {
  const res = await fetch(`/api/arbitrage-engine${path}`, { headers: { accept: "application/json" }, ...init });
  const body = await res.json().catch(() => ({}));
  if (!res.ok && res.status !== 202) throw new Error(body.error ?? `${res.status} ${res.statusText}`);
  return body as PmSnapshot;
}

export const pmArb = {
  get: () => call("/pm-arb"),
  refresh: () => call("/pm-arb/refresh", { method: "POST" }),
};

/** Fill a bankroll from the cheapest depth step up: contracts per side, each leg's
 *  stake, total cost (with fees), payout ($1 per pair) and profit. */
export function sizeFor(o: PmOpportunity, bankroll: number) {
  let left = bankroll;
  let contracts = 0, stakeA = 0, stakeB = 0, cost = 0;
  for (const s of o.depth.steps) {
    if (left <= 0) break;
    const n = Math.min(s.contracts, Math.floor((left / s.unit_cost) * 100) / 100);
    if (n <= 0) break;
    contracts += n;
    stakeA += n * s.price_a;
    stakeB += n * s.price_b;
    cost += n * s.unit_cost;
    left -= n * s.unit_cost;
  }
  return { contracts, stakeA, stakeB, fees: cost - stakeA - stakeB, cost, payout: contracts, profit: contracts - cost };
}

export const VENUE_LABEL: Record<string, string> = { kalshi: "Kalshi", polymarket: "Polymarket" };

export function ageLabel(iso: string): string {
  const m = Math.max(0, Math.round((Date.now() - new Date(iso).getTime()) / 60000));
  return m < 60 ? `${m}m` : m < 1440 ? `${Math.floor(m / 60)}h` : `${Math.floor(m / 1440)}d`;
}

export const cents = (p: number | null | undefined) => (p == null ? "—" : `${(p * 100).toFixed(1)}¢`);
