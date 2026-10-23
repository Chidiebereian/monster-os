// Polymarket research client (whale-tracker /pm/*): Top Traders leaderboard,
// wallet profiles and the Discover feeds. All data is Polymarket's public API.
import { qs } from "./api";

async function getJson<T>(path: string): Promise<T> {
  const res = await fetch(`/api/whale-tracker${path}`, { headers: { accept: "application/json" } });
  if (!res.ok) throw new Error(`${res.status} ${res.statusText}: ${(await res.text().catch(() => "")).slice(0, 200)}`);
  return res.json() as Promise<T>;
}

export const PM_CATEGORIES = [
  { key: "overall", label: "All" },
  { key: "politics", label: "Politics" },
  { key: "sports", label: "Sports" },
  { key: "crypto", label: "Crypto" },
  { key: "culture", label: "Culture" },
  { key: "mentions", label: "Mentions" },
  { key: "weather", label: "Weather" },
  { key: "economics", label: "Economics" },
  { key: "tech", label: "Tech" },
  { key: "finance", label: "Finance" },
  { key: "esports", label: "Esports" },
];
export const PM_WINDOWS = [
  { key: "day", label: "Day" },
  { key: "week", label: "Week" },
  { key: "month", label: "Month" },
  { key: "all", label: "All" },
];

export interface CategoryStanding { category: string; rank: number; pnl: number; vol: number }

export interface WalletStats {
  wallet: string;
  name: string | null;
  pseudonym: string | null;
  profile_image: string | null;
  x_username: string | null;
  verified: boolean | null;
  joined_at: string | null;
  age_days: number | null;
  positions_value: number | null;
  markets_traded: number | null;
  all_pnl: number | null;
  all_vol: number | null;
  all_rank: number | null;
  active_positions: number | null;
  closed_counted: number | null;
  closed_capped: boolean | null;
  wins: number | null;
  losses: number | null;
  win_rate: number | null;
  win_amount: number | null;
  loss_amount: number | null;
  biggest_win: number | null;
  biggest_loss: number | null;
  categories: CategoryStanding[] | null;
  full_at: string | null;
}

export interface BoardRow {
  rank: number;
  rank_change: number | null;
  wallet: string;
  name: string | null;
  profile_image: string | null;
  verified: boolean;
  pnl: number | null;
  vol: number | null;
  stats: WalletStats | null;
}
export interface Board { category: string; window: string; order: string; rows: BoardRow[]; refreshing: number }

export interface Position {
  title: string | null;
  slug: string | null;
  event_slug: string | null;
  icon: string | null;
  outcome: string | null;
  size: number | null;
  avg_price: number | null;
  cur_price: number | null;
  initial_value: number | null;
  current_value: number | null;
  cash_pnl: number | null;
  percent_pnl: number | null;
  total_bought: number | null;
  realized_pnl: number | null;
  redeemable: boolean | null;
  end_date: string | null;
  closed_at: number | null;
}

export interface Activity {
  ts: number; type: string | null; side: string | null; title: string | null; slug: string | null;
  icon: string | null; outcome: string | null; price: number | null; size: number | null; usdc: number | null; tx: string | null;
}

export interface PnlSeries {
  points: { t: number; pnl: number }[];
  change: number | null;
  latest: { realized_pnl: number | null; unrealized_pnl: number | null; volume: number | null; trade_count: number | null; fees_paid: number | null };
}

export interface TradeCard {
  wallet: string; name: string | null; ts: number; side: string | null; title: string | null; slug: string | null;
  icon: string | null; outcome: string | null; price: number | null; size: number | null; usdc: number | null;
  stats: WalletStats | null;
}
export interface Mover {
  wallet: string; name: string | null; profile_image: string | null; rank: number; all_rank: number | null;
  climb: number | null; pnl: number | null; vol: number | null; all_pnl: number | null; stats: WalletStats | null;
}

export const pm = {
  leaderboard: (o: { category?: string; window?: string; order?: string; limit?: number; offset?: number; q?: string }) =>
    getJson<Board>(`/pm/leaderboard?${qs(o)}`),
  wallet: (w: string) => getJson<WalletStats>(`/pm/wallet/${w}`),
  positions: (w: string, o: { status: "active" | "resolved" | "closed"; sort?: string; dir?: string; limit?: number; offset?: number; q?: string }) =>
    getJson<{ rows: Position[] }>(`/pm/wallet/${w}/positions?${qs(o)}`),
  activity: (w: string, o: { type?: string; limit?: number; offset?: number } = {}) =>
    getJson<{ rows: Activity[] }>(`/pm/wallet/${w}/activity?${qs(o)}`),
  pnl: (w: string, range: string) => getJson<PnlSeries>(`/pm/wallet/${w}/pnl?${qs({ range })}`),
  whales: (o: { min_trade?: number; min_value?: number; limit?: number } = {}) =>
    getJson<{ rows: TradeCard[] }>(`/pm/discover/whales?${qs(o)}`),
  insiders: (o: { min_trade?: number; max_age_days?: number; limit?: number } = {}) =>
    getJson<{ rows: TradeCard[] }>(`/pm/discover/insiders?${qs(o)}`),
  movers: (o: { category?: string; window?: string; limit?: number }) =>
    getJson<{ rows: Mover[] }>(`/pm/discover/movers?${qs(o)}`),
};

export const shortAddr = (w: string) => `${w.slice(0, 6)}…${w.slice(-4)}`;
export const displayName = (name: string | null | undefined, w: string) =>
  name && !name.startsWith("0x") ? name : shortAddr(w);

export function walletAge(days: number | null | undefined): string {
  if (days == null) return "—";
  if (days < 1) return "today";
  if (days < 31) return `${days} day${days === 1 ? "" : "s"}`;
  if (days < 365) return `${Math.floor(days / 30.4)} mo`;
  const y = days / 365;
  return `${y < 2 ? y.toFixed(1) : Math.floor(y)} yr`;
}

export const cents = (p: number | null | undefined) =>
  p == null ? "—" : `${(p * 100).toFixed(p * 100 < 10 && p > 0 ? 1 : p * 100 % 1 === 0 ? 0 : 1)}¢`;

export function timeAgo(ts: number): string {
  const s = Math.max(0, Date.now() / 1000 - ts);
  if (s < 60) return `${Math.floor(s)}s ago`;
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
}

// ---------- Smart Signals ----------

export interface PmSignal {
  id: number; wallet: string; asset: string; category: string | null; title: string | null; slug: string | null;
  event_slug: string | null; icon: string | null; outcome: string | null;
  entry_price: number | null; entry_usdc: number | null; entry_shares: number | null;
  first_trade_at: string | null; last_trade_at: string | null; current_price: number | null; price_at_signal: number | null;
  slippage: number | null; depth_2c: number | null; rel_size: number | null; wallet_score: number | null;
  score: number | null; best_score: number | null; precision: boolean | null; flow: boolean | null;
  parts: Record<string, number> | null; flags: string[] | null; status: string; close_reason: string | null;
  first_seen: string; last_seen: string; closed_at: string | null; result: string | null; final_price: number | null;
  name: string | null; profile_image: string | null; normal_bet: number | null;
  cat_rank: number | null; cat_pnl: number | null; cat_vol: number | null; cat_roi: number | null;
  overall_rank: number | null; overall_pnl: number | null;
}
export interface SignalStatus { state: string; since?: string; last?: { started: string; seconds: number; wallets: number; candidates: number; active: number; retired: number; resolved: number; new_trades: number }; error?: string }
export interface Consensus {
  asset: string; title: string | null; slug: string | null; event_slug: string | null; icon: string | null; outcome: string | null;
  category: string | null; wallets: number; usdc: number | null; avg_entry: number | null; current_price: number | null;
  avg_wallet_score: number; top_signal_score: number | null; depth_2c: number | null; score: number;
  members: { wallet: string; name: string | null; entry: number; usdc: number; wallet_score: number }[] | null;
  opposed: number;
}
export interface SignalRecord { modes: { mode: string; resolved: number; won: number; hit_rate: number | null; roi: number | null }[]; unresolved: number }

export const pmSignals = {
  list: (o: { mode: string; category?: string; min_score?: number; limit?: number }) =>
    getJson<{ rows: PmSignal[]; status: SignalStatus }>(`/pm/signals?${qs(o)}`),
  consensus: (o: { category?: string; min_wallets?: number } = {}) => getJson<{ rows: Consensus[] }>(`/pm/signals/consensus?${qs(o)}`),
  history: (o: { category?: string; limit?: number } = {}) => getJson<{ rows: PmSignal[] }>(`/pm/signals/history?${qs(o)}`),
  record: () => getJson<SignalRecord>(`/pm/signals/record`),
  chart: (asset: string, range = "1w") => getJson<{ points: { t: number; p: number }[] }>(`/pm/signals/chart?${qs({ asset, range })}`),
  run: () => fetch(`/api/whale-tracker/pm/signals/run`, { method: "POST" }).then((r) => r.json()),
};
