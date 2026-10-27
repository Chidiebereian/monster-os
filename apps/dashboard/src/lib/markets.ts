// Game markets shared by the Odds Screen, +EV Finder, hit rates and cheat
// sheets: the full-game three, both team totals, and the same five for the first
// segment (1st half; NHL 1st period; MLB first 5 innings).

/** projection-service sport for an Odds-API sport key ("basketball_nba" -> "nba"). */
export function sportOf(sportKey: string): string {
  if (sportKey.includes("nba")) return "nba";
  if (sportKey.includes("nhl")) return "nhl";
  if (sportKey.includes("nfl")) return "nfl";
  if (sportKey.includes("mlb")) return "mlb";
  return sportKey;
}

/** The first segment's name and short tag per sport. */
export function segmentOf(sport: string): { label: string; tag: string } {
  if (sport === "nhl") return { label: "1st period", tag: "1P" };
  if (sport === "mlb") return { label: "first 5 innings", tag: "F5" };
  return { label: "1st half", tag: "1H" };
}

export function gameMarkets(sport: string): { key: string; label: string }[] {
  const t = segmentOf(sport).tag;
  const spread = sport === "nhl" ? "Puck line" : sport === "mlb" ? "Run line" : "Spread";
  return [
    { key: "moneyline", label: "Moneyline" },
    { key: "spread", label: spread },
    { key: "total", label: "Total" },
    { key: "team_total_home", label: "Home team total" },
    { key: "team_total_away", label: "Away team total" },
    { key: "moneyline_1h", label: `${t} Moneyline` },
    { key: "spread_1h", label: `${t} ${spread}` },
    { key: "total_1h", label: `${t} Total` },
    { key: "team_total_home_1h", label: `${t} Home total` },
    { key: "team_total_away_1h", label: `${t} Away total` },
  ];
}
