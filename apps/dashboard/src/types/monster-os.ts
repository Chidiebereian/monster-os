// Data contracts shared across the dashboard. These mirror the MonsterOS
// backend service responses (served through apps/api-gateway).

export type Sport = "nba" | "wnba" | "mlb" | "nfl" | "nhl" | "soccer" | "prediction-markets";

/** The Odds-API sport key the backend expects, with a friendly label. */
export interface SportKey {
  key: string; // e.g. "basketball_nba"
  series: string; // Kalshi game series, e.g. "KXNBAGAME"
  label: string; // e.g. "NBA"
}

/** One row of odds-service /screen/{sport} — the line-shopping board. */
export interface OddsScreenRow {
  game: string;
  commence_time: string;
  outcome: string;
  best_price: number; // American odds
  best_book: string;
  books: Record<string, number>; // bookmaker -> American price
  fair_probability: number | null; // no-vig consensus
  fair_odds: number | null; // fair American odds
  best_implied_probability: number | null;
  edge: number | null; // fair_prob - best_implied (positive = +EV number available)
}

/** One trader row from copy-trading /leaderboard. */
export interface LeaderboardRow {
  wallet: string;
  venue: string;
  label: string | null;
  category: string | null;
  window: string;
  as_of: string;
  win_rate: number | null;
  edge: number | null; // reconstructed ROI
  pnl: number | null;
  volume: number | null;
  risk: number | null;
  trades: number | null;
}
