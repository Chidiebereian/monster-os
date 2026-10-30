// Game hub API (projection-service /hub/*): the Games page schedule and the
// per-game matchup pages. ESPN supplies schedule/records/venue/weather/injuries;
// the model, lines, projections, picks and team rankings are ours.

export type HubSport = "nfl" | "nba" | "nhl" | "mlb";
export const HUB_SPORTS: { key: HubSport; label: string; settingsKey: string }[] = [
  { key: "nfl", label: "NFL", settingsKey: "americanfootball_nfl" },
  { key: "nba", label: "NBA", settingsKey: "basketball_nba" },
  { key: "nhl", label: "NHL", settingsKey: "icehockey_nhl" },
  { key: "mlb", label: "MLB", settingsKey: "baseball_mlb" },
];
export const hubSportFromSettings = (k: string): HubSport => HUB_SPORTS.find((s) => s.settingsKey === k)?.key ?? "nfl";

export interface HubTeam {
  id: string | null; abbr: string; espn_abbr: string; name: string | null; short: string | null; logo: string | null;
  color: string | null; record: string | null; score: number | null;
}
export interface BestQuote { line: number | null; price: number; book: string; link: string | null; books: number }
export interface BestLines {
  moneyline?: { home?: BestQuote; away?: BestQuote };
  spread?: { home?: BestQuote; away?: BestQuote };
  total?: { over?: BestQuote; under?: BestQuote };
}
export interface Model {
  ledger_id: number; game_id: string; engine: string; model_version: string;
  home_score: number | null; away_score: number | null; total: number | null; spread_home: number | null;
  win_home: number | null; win_away: number | null;
  home_range?: Range; away_range?: Range; total_range?: Range; overtime?: number | null;
  first_half?: { win: { home?: number; away?: number; tie?: number }; home: number | null; away: number | null; total: number | null };
}
interface Range { q10: number | null; q50: number | null; q90: number | null }
export interface HubGame {
  id: string; game_id: string | null; start: string | null; local_date: string | null; name: string | null; short: string | null;
  status: { state: "pre" | "in" | "post" | null; detail: string | null; completed: boolean };
  venue: { name: string | null; city: string | null; state: string | null; indoor: boolean | null; grass?: boolean | null; image?: string | null };
  weather: { text: string | null; temp: number | null; condition: string | null } | null;
  broadcast: string | null; neutral: boolean; note: string | null;
  home: HubTeam; away: HubTeam;
  espn_odds: { provider: string | null; details: string | null; spread_home: number | null; total: number | null; home_ml: number | null; away_ml: number | null } | null;
  model: Model | null; lines: BestLines | null; picks?: number;
}
export interface Schedule {
  sport: HubSport; games: HubGame[];
  season?: number; week?: number; byes?: string[];
  weeks?: { game_type: string; week: number; label: string; start: string; end: string; games: number }[];
  date?: string; days?: string[];
}

export interface MetricVal {
  key: string; label: string; help: string; fmt: "pct" | "d1" | "d2" | "d3" | "epa";
  value: number | null; rank: number | null; of: number; vs_league?: number | null; usage?: boolean;
}
export interface Ranking { team: string; faces: string; games: number; rows: MetricVal[]; proe?: MetricVal; run_defense_rank?: number | null; pass_defense_rank?: number | null }
export interface TeamStatRow { key: string; label: string; help: string; home_off: MetricVal; home_def: MetricVal; away_off: MetricVal; away_def: MetricVal }
export interface HasBall {
  offense: string; defense: string;
  sections: { title: string; note: string; explore: string; rows: { key: string; label: string; help: string; offense: MetricVal; defense: MetricVal }[] }[];
}
export interface LogGame {
  game_id: string; season: number; week: number; game_type: string; date: string; home: boolean; opponent: string; score: string;
  result: "W" | "L" | "T"; line: number | null; ats: "W" | "L" | "P" | null; total_line: number | null; ou: "O" | "U" | "P" | null;
}
export interface PlayerMatchup {
  id: string; name: string; team: string; pos: string; games: number; opponent: string;
  projection: { name: string; pos: string; stats: Record<string, number | null> } | null;
  stats: Record<string, number>; opp_allows: MetricVal; opp_efficiency?: MetricVal; opp_target_share?: MetricVal; opp_sack_rate?: MetricVal;
}
export interface Analysis {
  error?: string; data_note?: string; season?: number; through_week?: number;
  rankings?: Ranking[]; team_stats?: { rows: TeamStatRow[] };
  stands_out?: { team: string; text: string; edge: number }[];
  has_ball?: HasBall[];
  situations?: { team: string; offense: MetricVal[]; defense: MetricVal[] }[];
  coaching?: { team: string; coach: string | null; record: string | null; offense: MetricVal[]; defense: MetricVal[] }[];
  logs?: { team: string; games: LogGame[]; su: string; ats: string; ou: string }[];
  season_records?: { team: string; su: string; ats: string; ou: string }[];
  game_info?: { referee: string | null; roof: string | null; surface: string | null; temp: number | null; wind: number | null; stadium: string | null;
    home_rest: number | null; away_rest: number | null; div_game: boolean | null; gametime: string | null } | null;
  matchups?: { receiving: PlayerMatchup[]; rushing: PlayerMatchup[]; qb: PlayerMatchup[]; touchdowns: PlayerMatchup[] };
  /** NBA/NHL/MLB: projected players vs how the opponent ranks at what it allows */
  player_matchups?: MatchupGroup[] | null;
  season_label?: string;
}
export interface MatchupCell { mean: number | null; line: number | null; price: number | null; book: string | null; p_over: number | null }
export interface MatchupGroup {
  key: string; label: string; cols: { key: string; label: string }[];
  rows: { id: string; name: string; pos: string; team: string; opponent: string; stats: Record<string, MatchupCell>; opp_allows: MetricVal | null; opp_side: "offense" | "defense" }[];
}
export interface GameReport { generated_at: string; sections: { title: string; paragraphs: string[] }[] }
export interface BookQuote { market: string; side: string; line: number | null; price: number; book: string; link: string | null; open_line: number | null; open_price: number | null }
export interface Edge { market: string; side: string; line: number | null; price: number; book: string; p_model: number; p_implied: number; edge: number; ev: number }
export interface ProjStat {
  mean: number | null; q10: number | null; q50: number | null; q90: number | null;
  books?: { side: string; line: number | null; price: number; book: string; link: string | null; p_model: number | null }[];
}
export interface ProjPlayer { id: string; name: string; pos: string | null; role: string | null; team: string; stats: Record<string, ProjStat> }
export interface HubPick {
  id: number; kind: string; market: string; subject: string; side: string; line: number | null; book: string; price: number;
  p_model: number; p_market: number; edge: number; ev: number; grade: string; tier: string; writeup: string; link: string | null;
  projection: number | null; published_at: string;
}
export interface GameDetail {
  sport: HubSport; event: HubGame; books: BookQuote[]; edges: Edge[];
  projections: { home: ProjPlayer[]; away: ProjPlayer[] } | null; picks: HubPick[];
  espn: {
    injuries: { team: string | null; logo: string | null; data: { name: string; pos: string | null; status: string | null; detail: string | null; side: string | null; return: string | null; headshot: string | null }[] }[];
    last_five: { team: string | null; logo: string | null; data: { date: string | null; opponent: string | null; at: string | null; result: string | null; score: string | null }[] }[];
    ats: { team: string | null; data: { type: string | null; summary: string | null }[] }[];
    leaders: { team: string | null; logo: string | null; data: { category: string | null; name: string | null; value: string | null; headshot: string | null }[] }[];
    predictor: { home: number | null; away: number | null } | null;
    standings: { header: string | null; teams: { team: string | null; stats: Record<string, string> }[] }[] | null;
  };
  analysis: Analysis | null;
  report?: GameReport;
}

async function get<T>(path: string): Promise<T> {
  const res = await fetch(`/api/projection-service${path}`, { headers: { accept: "application/json" } });
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    let msg = body;
    try { msg = JSON.parse(body).error ?? body; } catch { /* plain text */ }
    throw new Error(msg || `${res.status} ${res.statusText}`);
  }
  return res.json() as Promise<T>;
}

export const hub = {
  schedule: (sport: HubSport, o: { season?: number; week?: number; date?: string } = {}) => {
    const p = new URLSearchParams();
    if (o.season) p.set("season", String(o.season));
    if (o.week) p.set("week", String(o.week));
    if (o.date) p.set("date", o.date);
    return get<Schedule>(`/hub/${sport}/schedule?${p}`);
  },
  game: (sport: HubSport, id: string) => get<GameDetail>(`/hub/${sport}/game/${id}`),
};

// ---- formatting ----
export const fmtOdds = (p: number | null | undefined) => (p == null ? "—" : p > 0 ? `+${p}` : `${p}`);
export const fmtLine = (l: number | null | undefined) => (l == null ? "" : l > 0 ? `+${l}` : `${l}`);
export function fmtMetric(m: Pick<MetricVal, "fmt" | "value">): string {
  const v = m.value;
  if (v == null) return "—";
  switch (m.fmt) {
    case "pct": return `${(v * 100).toFixed(1)}%`;
    case "epa": return `${v >= 0 ? "+" : ""}${v.toFixed(2)}`;
    case "d2": return v.toFixed(2);
    case "d3": return v.toFixed(3).replace(/^0\./, ".");
    default: return v.toFixed(1);
  }
}
export function fmtVsLeague(m: MetricVal): string {
  const d = m.vs_league;
  if (d == null) return "—";
  if (m.fmt === "epa" || m.key === "cpoe" || m.key === "proe") return `${d >= 0 ? "+" : ""}${d.toFixed(2)}`;
  return `${d >= 0 ? "+" : ""}${d.toFixed(1)}%`;
}

const BOOKS: Record<string, string> = {
  draftkings: "DraftKings", fanduel: "FanDuel", betmgm: "BetMGM", caesars: "Caesars", williamhill_us: "Caesars", pinnacle: "Pinnacle",
  betrivers: "BetRivers", bovada: "Bovada", betonlineag: "BetOnline", lowvig: "LowVig", mybookieag: "MyBookie", betus: "BetUS",
  espnbet: "ESPN BET", fanatics: "Fanatics", hardrockbet: "Hard Rock", polymarket: "Polymarket", kalshi: "Kalshi",
};
export const bookName = (b: string) => BOOKS[b] ?? b.replace(/(^|_)\w/g, (m) => m.replace("_", " ").toUpperCase());
/** Links some providers template with placeholders ({state}) are not clickable. */
export const usableLink = (l: string | null | undefined) => (l && !l.includes("{") && !l.includes("__s__") ? l : null);

export const STAT_LABELS: Record<string, string> = {
  pass_yds: "Pass yds", pass_td: "Pass TD", pass_att: "Pass att", completions: "Comp", interceptions: "INT", rush_yds: "Rush yds",
  carries: "Carries", rush_td: "Rush TD", rec_yds: "Rec yds", receptions: "Rec", targets: "Targets", rec_td: "Rec TD", tds: "TDs",
  longest_pass: "Long pass", longest_rush: "Long rush", longest_rec: "Long rec", rush_rec_yds: "Rush+Rec yds", pass_rush_yds: "Pass+Rush yds",
  kick_pts: "Kick pts", fgm: "FG made", pts: "PTS", reb: "REB", ast: "AST", fg3m: "3PM", stl: "STL", blk: "BLK", tov: "TOV",
  pra: "PRA", pr: "P+R", pa: "P+A", ra: "R+A", min: "MIN", stocks: "STL+BLK", goals: "Goals", assists: "Assists", points: "Points",
  shots: "SOG", sog: "SOG", hits: "Hits", blocks: "Blocks", saves: "Saves", h: "H", hr: "HR", r: "R", rbi: "RBI", tb: "TB", k: "K",
  bb: "BB", hrr: "H+R+RBI", sb: "SB", outs: "Outs", er: "ER",
};
export const statLabel = (k: string) => STAT_LABELS[k] ?? k.replace(/_/g, " ");
