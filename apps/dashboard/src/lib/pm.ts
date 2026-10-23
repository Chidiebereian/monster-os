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
