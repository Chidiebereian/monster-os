// Copy-trading bots (whale-tracker /bots/*): named strategies that each follow
// one Polymarket wallet. Paper only — copies are simulated at the live CLOB
// price when the leader's trade is seen; no order is ever placed.
import { qs } from "./api";

export type BuyMode = "percentage" | "range" | "fixed" | "portfolio";
export type SellMode = "same" | "fixed" | "custom" | "manual";

export interface BotConfig {
  buy_mode: BuyMode;
  buy_pct: number;
  buy_fixed: number;
  range_min: number | null;
  range_max: number | null;
  bankroll: number;
  sell_mode: SellMode;
  sell_pct: number;
  sell_fixed: number;
  category_mode: "all" | "include" | "exclude";
  categories: string[];
  budget: number | null;
  max_per_trade: number | null;
  max_per_market: number | null;
  /** probability, 0.05 = 5¢ */
  min_price: number | null;
  max_price: number | null;
  /** price units, 0.03 = 3¢ */
  max_slippage: number | null;
  min_trigger: number | null;
  min_volume: number | null;
  min_liquidity: number | null;
  max_resolution_days: number | null;
  stop_loss_pct: number | null;
  take_profit_pct: number | null;
  expires_at: string | null;
}

export const DEFAULT_CONFIG: BotConfig = {
  buy_mode: "percentage", buy_pct: 10, buy_fixed: 10, range_min: null, range_max: null, bankroll: 1000,
  sell_mode: "same", sell_pct: 100, sell_fixed: 10, category_mode: "all", categories: [],
  budget: null, max_per_trade: null, max_per_market: null, min_price: null, max_price: null, max_slippage: null,
  min_trigger: null, min_volume: null, min_liquidity: null, max_resolution_days: null,
  stop_loss_pct: null, take_profit_pct: null, expires_at: null,
};

export interface BotSummary {
  positions_open: number; positions_redeemable: number; positions_closed: number;
  buys: number; sells: number; spent: number; open_cost: number; open_value: number;
  realized_pnl: number; unrealized_pnl: number; total_pnl: number; roi: number | null;
  wins: number; losses: number; win_rate: number | null;
  leader_trades?: number; skipped?: number; unseen?: number;
}

export interface Bot {
  id: number;
  strategy: string;
  wallet: string;
  trader_name: string | null;
  trader_image: string | null;
  status: "active" | "paused" | "expired";
  config: BotConfig;
  created_at: string;
  updated_at: string;
  last_checked_at: string | null;
  last_trade_at: string | null;
  last_error: string | null;
  summary: BotSummary;
}

export interface BotPosition {
  asset: string; condition_id: string | null; title: string | null; slug: string | null; event_slug: string | null;
  icon: string | null; outcome: string | null; category: string | null;
  shares: number; cost: number; bought_usdc: number; sold_usdc: number; realized: number;
  buys: number; sells: number; opened_at: string | null; last_at: string | null;
  status: "open" | "redeemable" | "closed"; price: number | null; value: number | null; pnl: number | null; pnl_pct: number | null;
}

export interface BotEvent {
  id: number; bot_id: number; ts: string; tx: string; asset: string; side: "BUY" | "SELL";
  title: string | null; slug: string | null; event_slug: string | null; icon: string | null; outcome: string | null;
  category: string | null; trader_price: number | null; trader_shares: number | null; trader_usdc: number | null;
  action: "copied" | "skipped"; copy_price: number | null; copy_shares: number | null; copy_usdc: number | null;
  reason: string | null; seen: boolean;
}
export interface BotAlert extends BotEvent {
  strategy: string; wallet: string; trader_name: string | null; trader_image: string | null;
}

export interface BotDetail extends Bot { positions: BotPosition[]; events: BotEvent[] }

async function call<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(`/api/whale-tracker${path}`, {
    ...init,
    headers: { accept: "application/json", ...(init?.body ? { "content-type": "application/json" } : {}) },
  });
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    let msg = body;
    try { msg = JSON.parse(body).error ?? body; } catch { /* plain text */ }
    throw new Error(msg || `${res.status} ${res.statusText}`);
  }
  return res.status === 204 ? (undefined as T) : (res.json() as Promise<T>);
}

export const bots = {
  list: () => call<{ rows: Bot[] }>("/bots"),
  get: (id: number) => call<BotDetail>(`/bots/${id}`),
  create: (b: { strategy: string; wallet: string; config: BotConfig }) =>
    call<Bot>("/bots", { method: "POST", body: JSON.stringify(b) }),
  update: (id: number, b: { strategy?: string; status?: "active" | "paused"; config?: BotConfig }) =>
    call<Bot>(`/bots/${id}`, { method: "PATCH", body: JSON.stringify(b) }),
  remove: (id: number) => call<void>(`/bots/${id}`, { method: "DELETE" }),
  alerts: (o: { unseen?: boolean; after_id?: number; limit?: number } = {}) =>
    call<{ rows: BotAlert[]; unseen: number }>(`/bots/alerts?${qs({ ...o, unseen: o.unseen == null ? undefined : String(o.unseen) })}`),
  markSeen: (bot_id?: number) => call<{ marked: number }>("/bots/alerts/seen", { method: "POST", body: JSON.stringify({ bot_id }) }),
  run: () => call<unknown>("/bots/run", { method: "POST" }),
};

/** "Copied $5.00 · BUY Up @ 84¢" style one-liner for an event. */
export function describeEvent(e: BotEvent): string {
  const leader = `${e.side} ${e.outcome ?? ""}`.trim();
  if (e.action === "copied") {
    const usd = e.copy_usdc != null ? `$${e.copy_usdc.toFixed(2)}` : "";
    const px = e.copy_price != null ? ` @ ${(e.copy_price * 100).toFixed(1)}¢` : "";
    return `Copied ${leader} ${usd}${px}`;
  }
  return `Skipped ${leader}: ${e.reason ?? "filtered"}`;
}
