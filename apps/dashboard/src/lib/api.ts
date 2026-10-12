// Thin client for the MonsterOS api-gateway. Every call goes through /api,
// which the Vite dev server proxies to the gateway (default http://localhost:8090).

import type { LeaderboardRow, OddsScreenRow } from "../types/monster-os";

async function getJson<T>(path: string): Promise<T> {
  const res = await fetch(`/api${path}`, { headers: { accept: "application/json" } });
  if (!res.ok) throw new Error(await errText(res));
  return res.json() as Promise<T>;
}

async function postJson<T>(path: string, body: unknown): Promise<T> {
  const res = await fetch(`/api${path}`, {
    method: "POST",
    headers: { "content-type": "application/json", accept: "application/json" },
    body: JSON.stringify(body),
  });
  if (!res.ok) throw new Error(await errText(res));
  return res.json() as Promise<T>;
}

async function errText(res: Response): Promise<string> {
  const body = await res.text().catch(() => "");
  return `${res.status} ${res.statusText}${body ? `: ${body.slice(0, 200)}` : ""}`;
}

export const qs = (o: Record<string, string | number | undefined>) => {
  const p = new URLSearchParams();
  for (const [k, v] of Object.entries(o)) if (v !== undefined && v !== "") p.set(k, String(v));
  return p.toString();
};

export const api = {
  // --- odds-service ---
  oddsScreen: (sportKey: string) => getJson<OddsScreenRow[]>(`/odds-service/screen/${sportKey}`),
  oddsKeys: () => getJson<unknown[]>(`/odds-service/keys`),

  // --- pricing-engine ---
  implied: (odds: number) => getJson<{ odds: number; implied_probability: number }>(`/pricing-engine/implied?odds=${odds}`),
  novig: (oddsA: number, oddsB: number) =>
    getJson<NoVig>(`/pricing-engine/novig?${qs({ odds_a: oddsA, odds_b: oddsB })}`),

  // --- kelly-service (calculators) ---
  kelly: (probability: number, odds: number, fraction?: number) =>
    getJson<Kelly>(`/kelly-service/kelly?${qs({ probability, odds, fraction })}`),
  ev: (probability: number, odds: number, stake: number) =>
    getJson<Ev>(`/kelly-service/ev?${qs({ probability, odds, stake })}`),
  hedge: (oddsA: number, stakeA: number, oddsB: number) =>
    getJson<Hedge>(`/kelly-service/hedge?${qs({ odds_a: oddsA, stake_a: stakeA, odds_b: oddsB })}`),
  parlay: (odds: string) => getJson<Parlay>(`/kelly-service/parlay?${qs({ odds })}`),

  // --- arbitrage-engine ---
  scan: (sport: string, series: string, opts: { min_edge?: number; min_top?: number; min_depth?: number } = {}) =>
    getJson<ScanResult>(`/arbitrage-engine/scan?${qs({ sport, series, ...opts })}`),

  // --- sport-intelligence (Smart Signals) ---
  signals: (opts: { mode?: string; category?: string; min_score?: number; include_venue?: boolean; limit?: number } = {}) =>
    getJson<{ signals: Signal[]; count: number }>(
      `/sport-intelligence/signals?${qs({ ...opts, include_venue: opts.include_venue ? "true" : undefined })}`,
    ),
  consensus: (opts: { category?: string; min_wallets?: number } = {}) =>
    getJson<{ consensus: Consensus[]; count: number }>(`/sport-intelligence/consensus?${qs(opts)}`),
  bestVenue: (polymarket: string, opts: { side?: string; min_top?: number; min_depth?: number } = {}) =>
    getJson<BestVenue>(`/sport-intelligence/best-venue?${qs({ polymarket, ...opts })}`),
  signalsActive: () => getJson<PersistedSignal[]>(`/sport-intelligence/signals/active`),
  signalsHistory: () => getJson<PersistedSignal[]>(`/sport-intelligence/signals/history`),

  // --- copy-trading (Top Traders) ---
  leaderboard: (opts: { metric?: string; window?: string; category?: string; limit?: number } = {}) =>
    getJson<LeaderboardRow[]>(`/copy-trading/leaderboard?${qs({ limit: opts.limit ?? 25, ...opts })}`),
  trader: (wallet: string) => getJson<TraderDetail>(`/copy-trading/traders/${wallet}`),
  copySize: (body: CopySizeReq) => postJson<CopySize>(`/copy-trading/copy/size`, body),

  // --- whale-tracker ---
  exposure: (wallet: string, limit = 25) => getJson<Exposure[]>(`/whale-tracker/exposure?${qs({ wallet, limit })}`),
  walletTrades: (wallet: string, limit = 50) => getJson<WalletTrade[]>(`/whale-tracker/trades?${qs({ wallet, limit })}`),
  candles: (opts: { market_id?: string; wallet?: string; outcome_index?: number; interval?: number; limit?: number }) =>
    getJson<Candles>(`/whale-tracker/candles?${qs(opts)}`),

  // --- simulation-engine (projections) ---
  simulate: (opts: { mean: number; sd: number; n?: number; seed?: number }) =>
    getJson<Simulation>(`/simulation-engine/simulate?${qs({ n: 10000, ...opts })}`),
  simulateModel: (opts: { model_id: string; parameter: string; n?: number; seed?: number; family?: string; dispersion?: number; line?: number; hist?: number }) =>
    getJson<Simulation>(`/simulation-engine/simulate?${qs({ n: 100000, ...opts })}`),

  // --- projection-service (player/team models) ---
  playerProjections: (sport: string, stat: string, opts: { limit?: number; min_games?: number; q?: string; teams?: string; min_last_season?: number } = {}) =>
    getJson<ProjectionList>(`/projection-service/projections/${sport}/${stat}?${qs(opts)}`),
  games: (sport = "nfl", opts: { status?: string; season?: number; week?: string; limit?: number } = {}) =>
    getJson<GamesList>(`/projection-service/games/${sport}?${qs(opts)}`),
  playerProjection: (sport: string, stat: string, playerId: string) =>
    getJson<ProjectionDetail>(`/projection-service/projections/${sport}/${stat}/${playerId}`),
  teamRatings: (sport = "nfl") => getJson<TeamRatings>(`/projection-service/teams/${sport}`),
  matchup: (sport: string, home: string, away: string, opts: { spread?: number; total?: number; n?: number } = {}) =>
    getJson<Matchup>(`/projection-service/matchup/${sport}?${qs({ home, away, ...opts })}`),

  // --- monster-gpt ---
  ask: (question: string, context?: unknown) =>
    postJson<{ answer: string; model: string; retrieved: boolean }>(`/monster-gpt/ask`, { question, context }),
  retrieve: (q: string) => getJson<unknown>(`/monster-gpt/retrieve?${qs({ q })}`),
};

// --- response shapes ---
export interface NoVig {
  odds_a: number; odds_b: number; overround: number; hold: number;
  fair_probability_a: number; fair_probability_b: number; fair_odds_a: number; fair_odds_b: number;
}
export interface Kelly { full_kelly: number; stake_fraction: number; edge_per_unit: number; }
export interface Ev { ev: number; roi: number; profit_if_win: number; loss_if_lose: number; }
export interface Hedge { hedge_stake: number; total_outlay: number; guaranteed_return: number; guaranteed_profit: number; }
export interface Parlay { combined_decimal: number; combined_odds: number | null; combined_probability: number; legs: number[]; }
export interface ScanOpportunity {
  market_id: string; outcome: string; game: string; side: string; price: number;
  edge: number; est_fee: number; net_edge: number; net_roi: number; in_play: boolean;
  top_size?: number | null; exec_depth_2c?: number | null; executable?: boolean;
}
export interface ScanResult { kalshi_markets: number; sportsbook_games: number; opportunities: ScanOpportunity[]; unmatched: unknown[]; }
export interface Signal {
  score: number; wallet: string; wallet_score: number; category: string; category_win_rate: number | null;
  market_id: string; side: string; entry_price: number; current_price: number | null; slippage: number | null;
  trade_size: number; position_value: number; relative_bet_size: number; tailable: boolean; ts: string;
  components: Record<string, number>; venue?: VenueBlock;
}
export interface VenueBlock { best_venue: string; kalshi_ticker: string | null; polymarket_cost: number | null; kalshi_cost: number | null; entry_saving: number | null; }
export interface Consensus {
  score: number; market_id: string; side: string; category: string; num_wallets: number;
  wallets: string[]; avg_wallet_score: number; avg_entry: number; current_price: number | null; tailable: boolean;
}
export interface BestVenue {
  side: string; matched_kalshi: string | null; best_venue: string | null; entry_saving: number | null;
  best_venue_executable?: boolean | null;
  polymarket: VenueSide; kalshi: VenueSide;
}
export interface VenueSide {
  available: boolean; entry_cost?: number | null; yes_bid?: number | null; yes_ask?: number | null;
  spread?: number | null; ask_depth_2c?: number | null; top_size?: number | null; executable?: boolean; reason?: string;
}
export interface PersistedSignal {
  id: number; wallet: string; market_id: string; side: string; category: string | null;
  entry_price: number | null; current_price: number | null; score: number | null; status: string;
  close_reason: string | null; first_seen: string; last_seen: string; closed_at: string | null;
}
export interface TraderDetail {
  profile: { wallet: string; venue: string; label: string | null; category: string | null; first_seen: string };
  performance: Array<{ as_of: string; win_rate: number | null; edge: number | null; pnl: number | null; risk: number | null; trades: number | null }>;
  category_performance: Array<{ category: string; win_rate: number | null; roi: number | null; wins: number | null; resolved: number | null }>;
}
export interface CopySizeReq {
  bankroll: number; budget?: number; max_position_fraction: number; slippage_tolerance: number;
  reference_price: number; current_price: number; copy_ratio?: number; trader_size?: number; trader_bankroll?: number;
}
export interface CopySize { accepted: boolean; recommended_size: number; raw_size: number; slippage: number; caps_applied: string[]; reason: string | null; }
export interface Exposure { market_id: string; outcome: string; net_shares: number; cost_basis: number; avg_entry: number | null; }
export interface WalletTrade { ts: string; venue: string; wallet: string; market_id: string; side: string; price: number; size: number; }
export interface SimHistogram { lo: number; hi: number; bin_width: number; density: number[]; }
export interface Simulation {
  mean: number; median: number; p10: number; p90: number; seed: number; n: number;
  family?: string;
  p_over?: number; p_under?: number; p_push?: number;
  hist?: SimHistogram;
  posterior?: { parameter: string; mean: number; sd: number; q05?: number | null; q95?: number | null; rhat?: number | null };
}
export interface PlayerProjection {
  player_id: string; name: string; team: string | null; last_season: number | null;
  proj_mean: number; proj_sd: number; dispersion: number | null; sample_games: number | null;
  career_mean: number | null; recent_mean: number | null;
  market_line: number | null; market_implied: number | null; model_id: string | null;
  family: string | null; unit: string | null;
}
export interface ProjectionList { sport: string; stat: string; model_id: string; count: number; projections: PlayerProjection[]; }
export interface ProjectionDetail {
  stat: string;
  unit: string;
  projection: PlayerProjection;
  recent_games: { season: number; week: string; opponent: string; value: number | string | null }[];
}
export interface Game {
  game_id: string; season: number | null; week: string | null; game_type: string | null;
  date: string | null; home: string | null; away: string | null;
  home_score: number | null; away_score: number | null;
  spread_line: number | null; total_line: number | null; status: string | null;
}
export interface GamesList { count: number; games: Game[]; }
export interface TeamRating { team: string; off: number; def: number; net: number; }
export interface TeamRatings { model_id: string; count: number; teams: TeamRating[]; }
export interface Matchup {
  home: string; away: string; mu_home: number; mu_away: number; n: number;
  proj_home_points: number; proj_away_points: number;
  home_win_prob: number; away_win_prob: number; tie_prob: number;
  proj_spread: number; median_margin: number; margin_p10: number; margin_p90: number;
  proj_total: number; median_total: number; total_p10: number; total_p90: number;
  cover_prob: number | null; over_prob: number | null;
  margin_hist: SimHistogram; total_hist: SimHistogram;
}
export interface Candle { t: number; o: number; h: number; l: number; c: number; v: number; n: number; }
export interface Candles { market_id: string | null; outcome_index: number | null; interval_secs: number; candles: Candle[]; }
