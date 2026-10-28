import { useEffect, useState } from "react";
import { keepPreviousData, useQuery } from "@tanstack/react-query";
import clsx from "clsx";
import { api, type MarketGame, type MarketLines, type PlayerProjection, type SimHistogram } from "../lib/api";
import { americanToDecimal, americanToImpliedProb, decimalOf, formatAmerican, formatPct, fractionalOf, impliedOrNull, impliedPctText, noVig, parseOdds, stepAmerican } from "../lib/odds";
import { Card, Input, Msg, PageTitle, Pills, Stat, Table, Td, Th } from "../components/ui";

// AI Picks & Projections. Filter sport -> stat -> player, then see that player's
// model distribution (from projection-service's Bayesian posteriors, drawn by
// the simulation-engine at n=100k) against a draggable line. Every number is
// model-driven — the UI never invents a projection.

type StatDef = { key: string; label: string; model: string; unit: string };
type SportDef = { sport: string; label: string; stats: StatDef[] };

const nflStat = (key: string, label: string, unit: string): StatDef => ({ key, label, unit, model: `nfl-${key}-v1` });
const PROJ_SPORTS: SportDef[] = [
  {
    sport: "nfl",
    label: "NFL",
    stats: [
      nflStat("pass_yds", "Passing Yards", "yds"),
      nflStat("pass_td", "Passing TDs", "TDs"),
      nflStat("pass_att", "Pass Attempts", "att"),
      nflStat("completions", "Completions", "comp"),
      nflStat("interceptions", "Interceptions", "INT"),
      nflStat("rush_yds", "Rushing Yards", "yds"),
      nflStat("carries", "Carries", "att"),
      nflStat("rush_td", "Rushing TDs", "TDs"),
      nflStat("rec_yds", "Receiving Yards", "yds"),
      nflStat("receptions", "Receptions", "rec"),
      nflStat("targets", "Targets", "tgt"),
      nflStat("rec_td", "Receiving TDs", "TDs"),
      nflStat("rush_rec_yds", "Rush + Rec Yards", "yds"),
      nflStat("pass_rush_yds", "Pass + Rush Yards", "yds"),
      nflStat("tds", "Touchdowns", "TDs"),
    ],
  },
  {
    sport: "nhl",
    label: "NHL",
    stats: [
      { key: "shots", label: "Shots on Goal", unit: "SOG", model: "nhl-shots-v1" },
      { key: "points", label: "Points", unit: "P", model: "nhl-points-v1" },
      { key: "goals", label: "Goals", unit: "G", model: "nhl-goals-v1" },
      { key: "assists", label: "Assists", unit: "A", model: "nhl-assists-v1" },
      { key: "hits", label: "Hits", unit: "hits", model: "nhl-hits-v1" },
      { key: "pim", label: "Penalty Minutes", unit: "PIM", model: "nhl-pim-v1" },
      { key: "toi", label: "Time on Ice", unit: "min", model: "nhl-toi-v1" },
      { key: "saves", label: "Saves (G)", unit: "SV", model: "nhl-saves-v1" },
      { key: "shots_against", label: "Shots Against (G)", unit: "SA", model: "nhl-shots_against-v1" },
      { key: "goals_against", label: "Goals Against (G)", unit: "GA", model: "nhl-goals_against-v1" },
    ],
  },
  {
    sport: "nba",
    label: "NBA",
    stats: [
      { key: "points", label: "Points", unit: "PTS", model: "nba-points-v1" },
      { key: "rebounds", label: "Rebounds", unit: "REB", model: "nba-rebounds-v1" },
      { key: "assists", label: "Assists", unit: "AST", model: "nba-assists-v1" },
      { key: "pra", label: "Pts+Reb+Ast", unit: "PRA", model: "nba-pra-v1" },
      { key: "threes", label: "3-Pointers", unit: "3PM", model: "nba-threes-v1" },
      { key: "steals", label: "Steals", unit: "STL", model: "nba-steals-v1" },
      { key: "blocks", label: "Blocks", unit: "BLK", model: "nba-blocks-v1" },
      { key: "turnovers", label: "Turnovers", unit: "TOV", model: "nba-turnovers-v1" },
      { key: "minutes", label: "Minutes", unit: "MIN", model: "nba-minutes-v1" },
      { key: "pr", label: "Pts+Reb", unit: "P+R", model: "nba-pr-v1" },
      { key: "pa", label: "Pts+Ast", unit: "P+A", model: "nba-pa-v1" },
      { key: "ra", label: "Reb+Ast", unit: "R+A", model: "nba-ra-v1" },
      ...[
        ["stocks", "Stl+Blk", "S+B"], ["fgm", "FG Made", "FGM"], ["fga", "FG Attempts", "FGA"], ["fg3a", "3PT Attempts", "3PA"],
        ["ftm", "FT Made", "FTM"], ["fta", "FT Attempts", "FTA"], ["oreb", "Off Rebounds", "OREB"], ["dreb", "Def Rebounds", "DREB"],
        ["double_double", "Double-Double", "DD"], ["triple_double", "Triple-Double", "TD"],
      ].map(([key, label, unit]) => ({ key, label, unit, model: `nba-${key}-v1` })),
    ],
  },
  {
    sport: "mlb",
    label: "MLB",
    stats: [
      ["hits", "Hits", "H"], ["total_bases", "Total Bases", "TB"], ["home_runs", "Home Runs", "HR"], ["runs", "Runs", "R"],
      ["rbi", "RBI", "RBI"], ["hrr", "Hits+Runs+RBI", "H+R+RBI"], ["singles", "Singles", "1B"], ["doubles", "Doubles", "2B"],
      ["walks", "Walks", "BB"], ["batter_k", "Batter K", "K"], ["stolen_bases", "Stolen Bases", "SB"],
      ["pitcher_k", "Pitcher K (SP)", "K"], ["outs", "Outs (SP)", "outs"], ["hits_allowed", "Hits Allowed (SP)", "H"],
      ["earned_runs", "Earned Runs (SP)", "ER"], ["walks_allowed", "Walks Allowed (SP)", "BB"], ["batters_faced", "Batters Faced (SP)", "BF"],
      ["hr_allowed", "HR Allowed (SP)", "HR"], ["pitches", "Pitch Count (SP)", "P"],
    ].map(([key, label, unit]) => ({ key, label, unit, model: `mlb-${key}-v1` })),
  },
];

export function Projections() {
  const [sportKey, setSportKey] = useState("nfl");
  const [statKey, setStatKey] = useState("pass_td");
  const [tab, setTab] = useState<"player" | "game">("player");

  const sport = PROJ_SPORTS.find((s) => s.sport === sportKey) ?? PROJ_SPORTS[0];
  const stat = sport.stats.find((s) => s.key === statKey) ?? sport.stats[0];

  function pickSport(k: string) {
    setSportKey(k);
    const next = PROJ_SPORTS.find((s) => s.sport === k);
    setStatKey(next?.stats[0]?.key ?? "");
  }

  return (
    <div className="space-y-5">
      <PageTitle
        title="Projections"
        subtitle="model distribution vs the line · trained on decades of real games"
        right={
          <Pills
            options={[{ key: "player", label: "Player projections" }, { key: "game", label: "Game projections" }]}
            value={tab}
            onChange={(v) => setTab(v as "player" | "game")}
          />
        }
      />

      <div className="flex flex-wrap items-center gap-3">
        <span className="text-xs text-gray-500 uppercase tracking-wide">Sport</span>
        <Pills options={PROJ_SPORTS.map((s) => ({ key: s.sport, label: s.label }))} value={sportKey} onChange={pickSport} />
        {sport.stats.length > 0 && (
          <>
            <span className="text-xs text-gray-500 uppercase tracking-wide ml-2">Stat</span>
            <Pills options={sport.stats.map((s) => ({ key: s.key, label: s.label }))} value={statKey} onChange={setStatKey} />
          </>
        )}
      </div>

      {tab === "game" ? (
        sportKey === "nfl" || sportKey === "nhl" || sportKey === "nba" ? (
          <GameProjections key={sportKey} sport={sportKey} />
        ) : (
          <Msg>{sportKey === "mlb" ? <>MLB game projections come from the plate-appearance simulator — open <a href="/mlb-sim" className="text-violet-light underline">MLB Game Sim</a>.</> : <>{sport.label} team model is in the pipeline. NFL, NHL and NBA game projections are live now.</>}</Msg>
        )
      ) : sport.stats.length === 0 || !stat ? (
        <Msg>{sport.label} player models are in the pipeline. NFL passing TDs is live now — pick NFL above.</Msg>
      ) : (
        <PlayerProjections sport={sport.sport} stat={stat} />
      )}
    </div>
  );
}

function PlayerProjections({ sport, stat }: { sport: string; stat: StatDef }) {
  const [search, setSearch] = useState("");
  const [selected, setSelected] = useState<string | null>(null);
  const [gameId, setGameId] = useState("");

  const hasSchedule = sport === "nfl" || sport === "nhl" || sport === "nba";
  const games = useQuery({
    queryKey: ["games", sport, "scheduled"],
    queryFn: () => api.games(sport, { status: "scheduled", limit: 400 }),
    enabled: hasSchedule,
  });
  // upcoming games sorted by date, soonest first
  const upcoming = [...(games.data?.games ?? [])]
    .filter((g) => g.home && g.away)
    .sort((a, b) => (a.date ?? "").localeCompare(b.date ?? ""));
  const game = upcoming.find((g) => g.game_id === gameId);
  const teams = game ? `${game.away},${game.home}` : undefined;

  const list = useQuery({
    queryKey: ["projections", sport, stat.key, search, teams],
    queryFn: () =>
      api.playerProjections(sport, stat.key, {
        limit: 150,
        min_games: 16,
        q: search || undefined,
        teams, // when set, backend restricts to the current roster automatically
      }),
  });

  const rows = list.data?.projections ?? [];
  // default-select the top projection once data arrives
  useEffect(() => {
    if (rows.length && (!selected || !rows.some((r) => r.player_id === selected))) {
      setSelected(rows[0].player_id);
    }
  }, [rows, selected]);

  const current = rows.find((r) => r.player_id === selected) ?? null;

  return (
    <div className="grid lg:grid-cols-[minmax(0,360px)_1fr] gap-5 items-start">
      {/* player list */}
      <Card className="space-y-3">
        <div className="flex items-center gap-2">
          <h2 className="font-medium text-sm">{stat.label}</h2>
          <span className="text-xs text-gray-500 ml-auto">{list.data?.count ?? 0} players</span>
        </div>
        {hasSchedule && (
          <select
            value={gameId}
            onChange={(e) => setGameId(e.target.value)}
            className="w-full bg-surface border border-glass rounded-md px-2 py-1 text-sm text-gray-100 focus:outline-none focus:border-violet/50"
          >
            <option value="">All games · all players</option>
            {upcoming.map((g) => (
              <option key={g.game_id} value={g.game_id}>
                {g.away} @ {g.home}{sport === "nfl" && g.week ? ` · wk ${g.week}` : ""}{g.date ? ` · ${g.date}` : ""}
              </option>
            ))}
          </select>
        )}
        <Input value={search} onChange={setSearch} placeholder="Search player…" className="w-full" />
        {list.isLoading && <Msg>Loading projections…</Msg>}
        {list.error && <Msg tone="error">{(list.error as Error).message}</Msg>}
        {!list.isLoading && rows.length === 0 && <Msg>No players match.</Msg>}
        {rows.length > 0 && (
          <div className="max-h-[520px] overflow-auto -mx-2">
            <Table head={<tr><Th>Player</Th><Th className="text-right">Proj</Th><Th className="text-right">Recent</Th></tr>}>
              {rows.map((p) => (
                <tr
                  key={p.player_id}
                  onClick={() => setSelected(p.player_id)}
                  className={clsx(
                    "border-t border-glass cursor-pointer",
                    p.player_id === selected ? "bg-violet/15" : "hover:bg-white/[0.03]",
                  )}
                >
                  <Td>
                    <div className="font-medium leading-tight">{p.name}</div>
                    <div className="text-[10px] text-gray-500">{p.team ?? "—"} · {p.sample_games}g</div>
                  </Td>
                  <Td className="text-right font-mono text-violet-light">{p.proj_mean.toFixed(2)}</Td>
                  <Td className="text-right font-mono text-gray-400">{p.recent_mean?.toFixed(2) ?? "—"}</Td>
                </tr>
              ))}
            </Table>
          </div>
        )}
      </Card>

      {/* distribution for the selected player */}
      {current ? <Distribution sport={sport} stat={stat} player={current} /> : <Card><Msg>Select a player.</Msg></Card>}
    </div>
  );
}

function Distribution({ sport, stat, player }: { sport: string; stat: StatDef; player: PlayerProjection }) {
  const mean = player.proj_mean;
  const [line, setLine] = useState(roundHalf(mean));
  const [marketOdds, setMarketOdds] = useState("-110");

  // reset the line when the player changes
  useEffect(() => setLine(roundHalf(mean)), [player.player_id, mean]);

  const base = {
    model_id: stat.model,
    parameter: `${stat.key}:${player.player_id}`,
    family: player.family ?? undefined,
    dispersion: player.dispersion ?? undefined,
    n: 100000,
  };

  // Stable histogram of the real simulated distribution (one draw per player).
  const hist = useQuery({
    queryKey: ["sim-hist", stat.model, player.player_id],
    queryFn: () => api.simulateModel({ ...base, hist: 64 }),
  });
  // Exact Over/Under/Push at the (rounded) line, from the backend draws.
  const qline = roundHalf(line);
  const prob = useQuery({
    queryKey: ["sim-prob", stat.model, player.player_id, qline],
    queryFn: () => api.simulateModel({ ...base, line: qline }),
    placeholderData: keepPreviousData,
  });
  const detail = useQuery({
    queryKey: ["proj-detail", sport, stat.key, player.player_id],
    queryFn: () => api.playerProjection(sport, stat.key, player.player_id),
  });
  // market line for this player+stat (recorded every 20 min from ESPN/DraftKings, The Odds API, SportsGameOdds), if available
  const marketQ = useQuery({
    queryKey: ["mkt-prop", sport, stat.key],
    queryFn: () => api.marketProps(sport, stat.key),
    retry: 0,
    staleTime: 60_000,
  });
  const prop = marketQ.data?.props.find((p) => p.player_id === player.player_id && p.line != null) ?? null;
  // when a market line exists, snap the slider to it so P(Over) is at the market line
  useEffect(() => {
    if (prop?.line != null) setLine(prop.line);
  }, [prop?.line]);
  // ...and the odds box to the market's Over price (back to -110 when there is none)
  useEffect(() => {
    setMarketOdds(prop?.over_odds != null ? String(prop.over_odds) : "-110");
  }, [player.player_id, prop?.over_odds]);

  const pOver = prob.data?.p_over ?? 0;
  const pUnder = prob.data?.p_under ?? 0;
  const pPush = prob.data?.p_push ?? 0;
  const marketImplied = americanToImpliedProb(Number(marketOdds) || 0);
  const edge = pOver - marketImplied;
  const s = hist.data;
  const sd = Math.max(player.proj_sd, 0.05);
  const evNum = (p: number, odds: number | null) => {
    if (odds == null) return 0;
    const dec = americanToDecimal(odds);
    return dec > 1 ? p * (dec - 1) - (1 - p) : 0;
  };
  const evStr = (p: number, odds: number | null) =>
    odds == null ? "—" : `${evNum(p, odds) >= 0 ? "+" : ""}${(evNum(p, odds) * 100).toFixed(1)}%`;

  return (
    <div className="space-y-4">
      <Card className="space-y-4">
        <div className="flex items-baseline gap-3 flex-wrap">
          <h2 className="font-medium">{player.name}</h2>
          <span className="text-xs text-gray-500">{player.team ?? "—"} · {stat.label} · {player.sample_games} games</span>
          <span className="ml-auto text-xs text-gray-500">
            career <span className="font-mono text-gray-300">{player.career_mean?.toFixed(2)}</span> ·
            recent <span className="font-mono text-gray-300"> {player.recent_mean?.toFixed(2)}</span>
            <span className="ml-2 px-1.5 py-0.5 rounded bg-white/5 text-[10px]">{s?.family ?? player.family}</span>
          </span>
        </div>

        <div className="grid md:grid-cols-4 gap-4 items-end">
          <Stat label="Projection (mean)" value={`${mean.toFixed(2)} ${stat.unit}`} />
          <label className="text-xs text-gray-500">Line ({stat.unit})<Input type="number" value={line} onChange={(v) => setLine(Number(v) || 0)} className="w-full mt-1" /></label>
          <OddsInput label={`Market odds (Over)${prop?.over_odds != null && String(prop.over_odds) === marketOdds ? ` · ${prop.book ?? "market"}` : ""}`} value={marketOdds} onChange={setMarketOdds} />
          <Stat label="Edge (Over)" value={`${edge >= 0 ? "+" : ""}${formatPct(edge)}`} tone={edge >= 0 ? "pos" : "neg"} />
        </div>

        <HistCurve hist={s?.hist} line={line} loading={hist.isLoading} />

        <div>
          <div className="flex justify-between text-xs text-gray-500 mb-1">
            <span>Line: <span className="text-gray-200 font-mono">{line.toFixed(1)}</span> {stat.unit}</span>
            <span>drag to recompute · exact from 100k draws</span>
          </div>
          <input
            type="range"
            min={Math.max(0, mean - 3 * sd)}
            max={mean + 3 * sd}
            step={(6 * sd) / 200}
            value={line}
            onChange={(e) => setLine(Number(e.target.value))}
            className="w-full accent-violet"
          />
        </div>

        <div className="grid grid-cols-2 md:grid-cols-4 gap-4 pt-3 border-t border-glass">
          <Stat label={`P(Over ${qline.toFixed(1)})`} value={formatPct(pOver)} tone="pos" />
          <Stat label="P(Under)" value={formatPct(pUnder)} tone="neg" />
          <Stat label="P(Push)" value={pPush > 0.0005 ? formatPct(pPush) : "—"} />
          <Stat label="Market implied" value={formatPct(marketImplied)} />
        </div>

        {prop && prop.line != null && (
          <div className="pt-3 border-t border-glass">
            <div className="flex items-center gap-2 mb-2 flex-wrap">
              <span className="font-medium text-sm text-emerald-light">Market EV · live</span>
              <span className="text-xs text-gray-500">
                {stat.label} {prop.line} · Over {formatAmerican(prop.over_odds)} <Implied odds={prop.over_odds} /> / Under {formatAmerican(prop.under_odds)} <Implied odds={prop.under_odds} />
                {noVig(prop.over_odds, prop.under_odds) != null && <> · no-vig Over {formatPct(noVig(prop.over_odds, prop.under_odds))}</>} ({prop.book ?? "SportsGameOdds"}{prop.books && prop.books > 1 ? `, ${prop.books} books` : ""})
              </span>
            </div>
            <div className="grid grid-cols-2 md:grid-cols-4 gap-4">
              <Stat label={`Over ${prop.line} EV/$1`} value={evStr(pOver, prop.over_odds)} tone={evNum(pOver, prop.over_odds) > 0 ? "pos" : "neg"} />
              <Stat label={`Under ${prop.line} EV/$1`} value={evStr(pUnder, prop.under_odds)} tone={evNum(pUnder, prop.under_odds) > 0 ? "pos" : "neg"} />
              <Stat label="Model P(Over)" value={formatPct(pOver)} />
              <Stat label="Mkt implied (Over)" value={formatPct(prop.over_odds != null ? americanToImpliedProb(prop.over_odds) : 0)} />
            </div>
          </div>
        )}
      </Card>

      <div className="grid md:grid-cols-2 gap-4">
        <Card>
          <h3 className="font-medium text-sm mb-2">Monte Carlo (n=100k, {s?.family ?? "model"} posterior)</h3>
          {hist.isLoading && <Msg>Simulating…</Msg>}
          {hist.error && <Msg tone="error">{(hist.error as Error).message}</Msg>}
          {s && (
            <div className="grid grid-cols-2 sm:grid-cols-4 gap-3">
              <Stat label="Mean" value={s.mean.toFixed(2)} />
              <Stat label="Median" value={s.median.toFixed(2)} />
              <Stat label="p10" value={s.p10.toFixed(2)} />
              <Stat label="p90" value={s.p90.toFixed(2)} />
            </div>
          )}
        </Card>
        <Card>
          <h3 className="font-medium text-sm mb-2">Recent form</h3>
          {detail.isLoading && <Msg>Loading…</Msg>}
          {detail.data && detail.data.recent_games.length > 0 ? (
            <div className="flex flex-wrap gap-1.5">
              {detail.data.recent_games.slice(0, 16).map((g, i) => {
                const v = Number(g.value ?? 0);
                const label = Number.isInteger(v) ? v : Math.round(v);
                return (
                  <div
                    key={i}
                    title={`${g.season} wk ${g.week} vs ${g.opponent}: ${v} ${stat.unit}`}
                    className={clsx(
                      "min-w-7 h-7 px-1.5 rounded flex items-center justify-center text-xs font-mono",
                      v >= mean ? "bg-emerald/25 text-emerald-light" : v > 0 ? "bg-white/5 text-gray-300" : "bg-crimson/15 text-crimson-light",
                    )}
                  >
                    {label}
                  </div>
                );
              })}
            </div>
          ) : (
            !detail.isLoading && <Msg>No recent games.</Msg>
          )}
        </Card>
      </div>
    </div>
  );
}

function roundHalf(x: number) {
  return Math.round(x * 2) / 2;
}

// Game projections from the team model (log-linear Poisson, 100k simulation):
// win probabilities, projected score, spread/puckline and total with distributions.
const GAME_DEFAULTS: Record<string, { spread: string; total: string; home: string; away: string; unit: string }> = {
  nfl: { spread: "-2.5", total: "47.5", home: "BUF", away: "KC", unit: "points" },
  nhl: { spread: "-1.5", total: "6.5", home: "EDM", away: "COL", unit: "goals" },
  nba: { spread: "-5.5", total: "225.5", home: "BOS", away: "LAL", unit: "points" },
};

// First-half lines as a share of the full-game market (regressions over 2019-2026
// games with closing lines: NBA margin 0.595 / total 0.505, NFL 0.57 / 0.51).
const H1_SCALE: Record<string, { margin: number; total: number }> = { nba: { margin: 0.595, total: 0.505 }, nfl: { margin: 0.57, total: 0.51 } };
const SEGMENT: Record<string, { label: string; tag: string }> = { nhl: { label: "1st period", tag: "1P" }, mlb: { label: "first 5 innings", tag: "F5" } };
const seg = (sport: string) => SEGMENT[sport] ?? { label: "1st half", tag: "1H" };
const halfLine = (x: number) => Math.round(x * 2) / 2;
const hook = (x: number) => Math.floor(x) + 0.5;
const signed = (x: number) => (x > 0 ? `+${x}` : `${x}`);

/** Lines the page derives when the user (or the market) has not set them. */
function derivedLines(sport: string, spread: number, total: number) {
  const homePts = (total - spread) / 2; // home spread is bettor convention: negative = home favoured
  const sc = H1_SCALE[sport];
  const h1Spread = sport === "nhl" ? -0.5 : halfLine((sc?.margin ?? 0.55) * spread);
  const h1Total = sport === "nhl" ? 1.5 : halfLine((sc?.total ?? 0.5) * total);
  const h1Home = sport === "nhl" ? 0.5 : hook((h1Total - h1Spread) / 2);
  const h1Away = sport === "nhl" ? 0.5 : hook((h1Total + h1Spread) / 2);
  return {
    homeTotal: hook(homePts), awayTotal: hook(total - homePts),
    h1Spread, h1Total, h1Home, h1Away,
  };
}

type LineKey = "homeTotal" | "awayTotal" | "h1Spread" | "h1Total" | "h1Home" | "h1Away";

function GameProjections({ sport }: { sport: string }) {
  const nhl = sport === "nhl";
  const def = GAME_DEFAULTS[sport] ?? GAME_DEFAULTS.nfl;
  const scoreUnit = def.unit;
  const lineLabel = nhl ? "Home puckline" : "Home spread";
  const S = seg(sport);
  const ratings = useQuery({ queryKey: ["team-ratings", sport], queryFn: () => api.teamRatings(sport) });
  const teams = (ratings.data?.teams ?? []).map((t) => t.team).sort();
  const [home, setHome] = useState<string>("");
  const [away, setAway] = useState<string>("");
  const [spread, setSpread] = useState(def.spread);
  const [total, setTotal] = useState(def.total);
  // team totals and first-segment lines: blank = derived from the spread and total
  const [lineSet, setLineSet] = useState<Partial<Record<LineKey, string>>>({});
  // American odds per selection, full game ("ml_home") and first segment ("h1_ml_home")
  const [odds, setOdds] = useState<Record<string, string>>({});
  const oddsOf = (k: string) => odds[k] ?? "-110";
  const setOdd = (k: string) => (v: string) => setOdds((o) => ({ ...o, [k]: v }));

  useEffect(() => {
    if (teams.length && !home) {
      setHome(teams.includes(def.home) ? def.home : teams[0]);
      setAway(teams.includes(def.away) ? def.away : teams[1] ?? teams[0]);
    }
  }, [teams, home, def]);

  const derived = derivedLines(sport, Number(spread) || 0, Number(total) || 0);
  const lineOf = (k: LineKey) => { const v = lineSet[k]; return v != null && v.trim() !== "" && Number.isFinite(Number(v)) ? Number(v) : derived[k]; };
  const setLine = (k: LineKey) => (v: string) => setLineSet((l) => ({ ...l, [k]: v }));

  const m = useQuery({
    queryKey: ["matchup", sport, home, away, spread, total, lineOf("homeTotal"), lineOf("awayTotal")],
    queryFn: () =>
      api.matchup(sport, home, away, {
        [nhl ? "puckline" : "spread"]: Number(spread) || undefined,
        total: Number(total) || undefined,
        home_total: lineOf("homeTotal"), away_total: lineOf("awayTotal"),
        n: 100000,
      }),
    enabled: Boolean(home && away && home !== away),
    placeholderData: keepPreviousData,
  });
  const h1 = useQuery({
    queryKey: ["matchup-h1", sport, home, away, lineOf("h1Spread"), lineOf("h1Total"), lineOf("h1Home"), lineOf("h1Away")],
    queryFn: () => api.matchupH1(sport, home, away, { spread: lineOf("h1Spread"), total: lineOf("h1Total"), home_total: lineOf("h1Home"), away_total: lineOf("h1Away"), n: 100000 }),
    enabled: Boolean(home && away && home !== away),
    placeholderData: keepPreviousData,
  });
  const d = m.data;
  const h = h1.data;
  const homeScore = d ? d.proj_home_points ?? d.proj_home_goals ?? 0 : 0;
  const awayScore = d ? d.proj_away_points ?? d.proj_away_goals ?? 0 : 0;

  // upcoming games to prefill the matchup: the schedule, with recorded market
  // lines attached where the books have them (lines go stale after 6 h, the
  // schedule doesn't, so the picker never empties out)
  const [gameId, setGameId] = useState("");
  const market = useQuery({ queryKey: ["market-games", sport], queryFn: () => api.marketGames(sport) });
  const schedule = useQuery({
    queryKey: ["games", sport, "scheduled"],
    queryFn: () => api.games(sport, { status: "scheduled", limit: 400 }),
  });
  const lines = new Map((market.data?.games ?? []).map((g) => [`${g.away}@${g.home}`, g]));
  const seen = new Set<string>();
  const upcoming: { away: string; home: string; date: string | null; week: string | null; line: MarketGame | null }[] = [];
  for (const g of [...(schedule.data?.games ?? [])].filter((g) => g.home && g.away).sort((a, b) => (a.date ?? "").localeCompare(b.date ?? ""))) {
    const k = `${g.away}@${g.home}`;
    if (seen.has(k)) continue; // a team pair can recur later in the season; keep the soonest
    seen.add(k);
    upcoming.push({ away: g.away!, home: g.home!, date: g.date, week: g.week, line: lines.get(k) ?? null });
  }
  for (const [k, g] of lines) {
    if (!seen.has(k)) upcoming.unshift({ away: g.away, home: g.home, date: g.starts_at?.slice(0, 10) ?? null, week: null, line: g });
  }
  const withLines = upcoming.filter((g) => g.line).length;
  function pickGame(idx: string) {
    setGameId(idx);
    const u = upcoming[Number(idx)];
    if (!u) return;
    setHome(u.home);
    setAway(u.away);
    const g = u.line;
    if (!g) return; // schedule only: keep the lines the user has typed
    if (g.spread != null) setSpread(String(g.spread)); // home spread (negative = home favored) maps straight to our matchup spread
    if (g.total != null) setTotal(String(g.total));
    const nl: Partial<Record<LineKey, string>> = {};
    const no: Record<string, string> = {};
    const put = (k: string, v: number | null | undefined) => { if (v != null) no[k] = String(v); };
    const fill = (src: MarketLines | undefined, pre: string, keys: [LineKey | null, LineKey | null, LineKey | null, LineKey | null]) => {
      if (!src) return;
      const [spK, totK, htK, atK] = keys;
      put(`${pre}ml_home`, src.ml_home_odds); put(`${pre}ml_away`, src.ml_away_odds);
      put(`${pre}sp_home`, src.spread_home_odds); put(`${pre}sp_away`, src.spread_away_odds);
      put(`${pre}tot_over`, src.over_odds); put(`${pre}tot_under`, src.under_odds);
      put(`${pre}ht_over`, src.home_over_odds); put(`${pre}ht_under`, src.home_under_odds);
      put(`${pre}at_over`, src.away_over_odds); put(`${pre}at_under`, src.away_under_odds);
      if (spK && src.spread != null) nl[spK] = String(src.spread);
      if (totK && src.total != null) nl[totK] = String(src.total);
      if (htK && src.home_total != null) nl[htK] = String(src.home_total);
      if (atK && src.away_total != null) nl[atK] = String(src.away_total);
    };
    fill(g, "", [null, null, "homeTotal", "awayTotal"]);
    fill(g.h1, "h1_", ["h1Spread", "h1Total", "h1Home", "h1Away"]);
    setLineSet(nl);
    setOdds(no);
  }

  const teamSelect = (value: string, onChange: (v: string) => void, label: string) => (
    <label className="text-xs text-gray-500">
      {label}
      <select value={value} onChange={(e) => onChange(e.target.value)} className="w-full mt-1 bg-surface border border-glass rounded-md px-2 py-1 text-sm text-gray-100 focus:outline-none focus:border-violet/50">
        {teams.map((t) => <option key={t} value={t}>{t}</option>)}
      </select>
    </label>
  );

  // market rows: (label, line, P(win), P(loss), odds key); pushes are the rest
  type Row = { key: string; group: string; label: string; win: number; loss: number };
  const sp = Number(spread) || 0;
  const tot = Number(total) || 0;
  const full: Row[] = [];
  if (d) {
    const tie = d.tie_prob ?? 0;
    full.push({ key: "ml_home", group: "Moneyline", label: `${d.home} ML`, win: d.home_win_prob, loss: d.away_win_prob });
    full.push({ key: "ml_away", group: "Moneyline", label: `${d.away} ML`, win: d.away_win_prob, loss: d.home_win_prob });
    if (tie > 0.0005) full.forEach((r) => (r.label += " (tie pushes)"));
    if (d.cover_prob != null) {
      full.push({ key: "sp_home", group: nhl ? "Puck line" : "Spread", label: `${d.home} ${signed(sp)}`, win: d.cover_prob, loss: 1 - d.cover_prob });
      full.push({ key: "sp_away", group: nhl ? "Puck line" : "Spread", label: `${d.away} ${signed(-sp)}`, win: 1 - d.cover_prob, loss: d.cover_prob });
    }
    if (d.over_prob != null) {
      full.push({ key: "tot_over", group: "Total", label: `Over ${tot}`, win: d.over_prob, loss: 1 - d.over_prob });
      full.push({ key: "tot_under", group: "Total", label: `Under ${tot}`, win: 1 - d.over_prob, loss: d.over_prob });
    }
    const tt = (side: "home" | "away", pre: "ht" | "at") => {
      const a = d.team_totals?.[side]?.at_line;
      if (!a) return;
      const team = side === "home" ? d.home : d.away;
      full.push({ key: `${pre}_over`, group: `${team} team total`, label: `${team} Over ${a.line}`, win: a.over, loss: a.under });
      full.push({ key: `${pre}_under`, group: `${team} team total`, label: `${team} Under ${a.line}`, win: a.under, loss: a.over });
    };
    tt("home", "ht"); tt("away", "at");
  }
  const first: Row[] = [];
  if (h) {
    first.push({ key: "h1_ml_home", group: `${S.tag} Moneyline`, label: `${home} ${S.tag} ML`, win: h.win.home, loss: h.win.away });
    first.push({ key: "h1_ml_away", group: `${S.tag} Moneyline`, label: `${away} ${S.tag} ML`, win: h.win.away, loss: h.win.home });
    const a = h.at;
    if (a.spread) {
      first.push({ key: "h1_sp_home", group: `${S.tag} ${nhl ? "Puck line" : "Spread"}`, label: `${home} ${S.tag} ${signed(lineOf("h1Spread"))}`, win: a.spread.over, loss: a.spread.under });
      first.push({ key: "h1_sp_away", group: `${S.tag} ${nhl ? "Puck line" : "Spread"}`, label: `${away} ${S.tag} ${signed(-lineOf("h1Spread"))}`, win: a.spread.under, loss: a.spread.over });
    }
    if (a.total) {
      first.push({ key: "h1_tot_over", group: `${S.tag} Total`, label: `${S.tag} Over ${a.total.line}`, win: a.total.over, loss: a.total.under });
      first.push({ key: "h1_tot_under", group: `${S.tag} Total`, label: `${S.tag} Under ${a.total.line}`, win: a.total.under, loss: a.total.over });
    }
    for (const [side, pre, team] of [["home_total", "h1_ht", home], ["away_total", "h1_at", away]] as const) {
      const x = a[side];
      if (!x) continue;
      first.push({ key: `${pre}_over`, group: `${team} ${S.tag} team total`, label: `${team} ${S.tag} Over ${x.line}`, win: x.over, loss: x.under });
      first.push({ key: `${pre}_under`, group: `${team} ${S.tag} team total`, label: `${team} ${S.tag} Under ${x.line}`, win: x.under, loss: x.over });
    }
  }

  const lineInput = (k: LineKey, label: string) => (
    <label className="text-xs text-gray-500">{label}
      <Input type="number" value={lineSet[k] ?? ""} onChange={setLine(k)} placeholder={String(derived[k])} className="w-full mt-1" />
    </label>
  );

  return (
    <div className="space-y-4">
      <Card className="space-y-4">
        {upcoming.length > 0 && (
          <select
            value={gameId}
            onChange={(e) => pickGame(e.target.value)}
            className="w-full bg-surface border border-glass rounded-md px-2 py-1 text-sm text-gray-100 focus:outline-none focus:border-violet/50"
          >
            <option value="">
              Pick an upcoming game{withLines ? ` · ${withLines} with live lines (autofill every market + odds)` : " · no live lines right now, enter them yourself"}…
            </option>
            {upcoming.map((g, i) => (
              <option key={i} value={i}>
                {g.away} @ {g.home}{sport === "nfl" && g.week ? ` · wk ${g.week}` : ""}{g.date ? ` · ${g.date}` : ""}
                {g.line?.spread != null ? ` · ${g.home} ${g.line.spread > 0 ? "+" : ""}${g.line.spread}` : ""}{g.line?.total != null ? ` · O/U ${g.line.total}` : ""}
                {g.line?.h1 ? ` · ${S.tag} lines` : ""}
              </option>
            ))}
          </select>
        )}
        <div className="grid grid-cols-2 md:grid-cols-4 gap-4 items-end">
          {teamSelect(away, setAway, "Away")}
          {teamSelect(home, setHome, "Home")}
          <label className="text-xs text-gray-500">{lineLabel}<Input type="number" value={spread} onChange={setSpread} className="w-full mt-1" /></label>
          <label className="text-xs text-gray-500">Total<Input type="number" value={total} onChange={setTotal} className="w-full mt-1" /></label>
        </div>

        {ratings.isLoading && <Msg>Loading team ratings…</Msg>}
        {home === away && <Msg tone="error">Pick two different teams.</Msg>}
        {m.error && <Msg tone="error">{(m.error as Error).message}</Msg>}

        {d && (
          <>
            <div className="flex items-center gap-4 flex-wrap">
              <div className="text-lg font-semibold">
                {d.away} <span className="font-mono text-gray-300">{awayScore.toFixed(1)}</span>
                <span className="text-gray-600 mx-2">@</span>
                {d.home} <span className="font-mono text-gray-300">{homeScore.toFixed(1)}</span>
                <span className="text-xs text-gray-500 ml-2">{scoreUnit}</span>
              </div>
              {h && (
                <div className="text-sm text-gray-400">
                  {S.label}: <span className="font-mono text-gray-200">{h.away.mean.toFixed(1)}–{h.home.mean.toFixed(1)}</span>
                </div>
              )}
              <div className="ml-auto text-xs text-gray-500">100k sim · log-linear Poisson</div>
            </div>

            {/* win probability bar */}
            <div>
              <div className="flex justify-between text-xs mb-1">
                <span className="text-emerald-light">{d.away} {formatPct(d.away_win_prob)}</span>
                {(d.tie_prob ?? 0) > 0.0005 && <span className="text-gray-500">tie {formatPct(d.tie_prob ?? 0)}</span>}
                <span className="text-violet-light">{d.home} {formatPct(d.home_win_prob)}</span>
              </div>
              <div className="h-3 rounded overflow-hidden flex bg-surface border border-glass">
                <div className="bg-emerald/60" style={{ width: `${d.away_win_prob * 100}%` }} />
                <div className="bg-gray-600" style={{ width: `${(d.tie_prob ?? 0) * 100}%` }} />
                <div className="bg-violet/60" style={{ width: `${d.home_win_prob * 100}%` }} />
              </div>
            </div>

            <div className="grid grid-cols-2 md:grid-cols-4 gap-4 pt-2 border-t border-glass">
              <Stat label={`${d.home} win`} value={formatPct(d.home_win_prob)} tone="pos" />
              <Stat label="Median margin" value={d.median_margin.toFixed(0)} />
              <Stat label="Proj total" value={d.proj_total.toFixed(1)} />
              <Stat label="Cover / Over" value={`${d.cover_prob != null ? formatPct(d.cover_prob) : "—"} / ${d.over_prob != null ? formatPct(d.over_prob) : "—"}`} />
            </div>
          </>
        )}
      </Card>

      {d && (
        <Card className="space-y-3">
          <div className="flex items-center gap-3 flex-wrap">
            <h3 className="font-medium text-sm">Full game · edge &amp; EV vs market</h3>
            <span className="text-xs text-gray-500">model probability vs the price you pay · pushes refund</span>
          </div>
          <div className="grid grid-cols-2 gap-4 max-w-md">
            {lineInput("homeTotal", `${home} team total`)}
            {lineInput("awayTotal", `${away} team total`)}
          </div>
          <MarketTable rows={full} oddsOf={oddsOf} setOdd={setOdd} />
        </Card>
      )}

      {d && (
        <div className="grid md:grid-cols-2 gap-4">
          <Card>
            <h3 className="font-medium text-sm mb-1">Margin ({d.home} − {d.away})</h3>
            <div className="text-[11px] text-gray-500 mb-1">median {d.median_margin} · [{d.margin_p10}, {d.margin_p90}] · line at home spread</div>
            <HistCurve hist={d.margin_hist} line={-(Number(spread) || 0)} loading={false} />
          </Card>
          <Card>
            <h3 className="font-medium text-sm mb-1">Total points</h3>
            <div className="text-[11px] text-gray-500 mb-1">median {d.median_total} · [{d.total_p10}, {d.total_p90}] · line at total</div>
            <HistCurve hist={d.total_hist} line={Number(total) || 0} loading={false} />
          </Card>
        </div>
      )}
      <Card className="space-y-3">
        <div className="flex items-center gap-3 flex-wrap">
          <h3 className="font-medium text-sm">{S.label[0].toUpperCase() + S.label.slice(1)} · edge &amp; EV vs market</h3>
          <span className="text-xs text-gray-500">
            team model refit on {S.label} scores{h ? ` of ${h.games.toLocaleString()} games` : ""} · a tied {S.label} refunds the moneyline
          </span>
        </div>
        <div className="grid grid-cols-2 md:grid-cols-4 gap-4">
          {lineInput("h1Spread", `${S.tag} ${home} ${nhl ? "puck line" : "spread"}`)}
          {lineInput("h1Total", `${S.tag} total`)}
          {lineInput("h1Home", `${S.tag} ${home} team total`)}
          {lineInput("h1Away", `${S.tag} ${away} team total`)}
        </div>
        <div className="text-[11px] text-gray-500">Blank lines are derived from the full-game spread and total{nhl ? " (NHL: −0.5 / 1.5 / 0.5)" : ` (${S.tag} spread ≈ ${H1_SCALE[sport]?.margin ?? 0.55}× and total ≈ ${H1_SCALE[sport]?.total ?? 0.5}× the game's)`}; picking a game with {S.tag} lines fills the real ones.</div>
        {h1.error && <Msg tone="error">{(h1.error as Error).message}</Msg>}
        {h && (
          <>
            <div className="grid grid-cols-2 md:grid-cols-4 gap-4">
              <Stat label={`${home} leads after ${S.tag}`} value={formatPct(h.win.home)} tone="pos" />
              <Stat label={`Tied after ${S.tag}`} value={formatPct(h.win.tie)} />
              <Stat label={`${S.tag} proj total`} value={h.total.mean.toFixed(1)} />
              <Stat label={`${S.tag} margin (${home})`} value={`${h.margin_home.mean >= 0 ? "+" : ""}${h.margin_home.mean.toFixed(1)}`} />
            </div>
            <MarketTable rows={first} oddsOf={oddsOf} setOdd={setOdd} />
          </>
        )}
      </Card>
    </div>
  );
}

/** One row per selection: model win probability (pushes excluded), the price,
 *  its implied probability, the edge and EV per $1 (pushes refund the stake). */
function MarketTable({ rows, oddsOf, setOdd }: { rows: { key: string; group: string; label: string; win: number; loss: number }[]; oddsOf: (k: string) => string; setOdd: (k: string) => (v: string) => void }) {
  if (rows.length === 0) return null;
  return (
    <Table head={<tr><Th>Bet</Th><Th className="text-right">Model</Th><Th className="text-right">Push</Th><Th>Odds</Th><Th className="text-right">Implied</Th><Th className="text-right">Edge</Th><Th className="text-right">EV / $1</Th></tr>}>
      {rows.map((r, i) => {
        const p = r.win / Math.max(1e-9, r.win + r.loss);
        const push = Math.max(0, 1 - r.win - r.loss);
        const o = oddsOf(r.key);
        const implied = americanToImpliedProb(Number(o) || 0);
        const dec = americanToDecimal(Number(o) || 0);
        const edge = p - implied;
        const e = dec > 1 ? r.win * (dec - 1) - r.loss : 0;
        const newGroup = i === 0 || rows[i - 1].group !== r.group;
        return (
          <tr key={r.key} className={clsx("border-t", newGroup ? "border-glass" : "border-transparent")}>
            <Td>{r.label}</Td>
            <Td className="text-right font-mono">{formatPct(p)}</Td>
            <Td className="text-right font-mono text-gray-500">{push >= 0.0005 ? formatPct(push) : "—"}</Td>
            <Td><OddsInput inline compact label="" value={o} onChange={setOdd(r.key)} /></Td>
            <Td className="text-right font-mono text-gray-400">{formatPct(implied)}</Td>
            <Td className={clsx("text-right font-mono", edge > 0 ? "text-emerald-light" : "text-gray-500")}>{edge >= 0 ? "+" : ""}{formatPct(edge)}</Td>
            <Td className={clsx("text-right font-mono", e > 0 ? "text-emerald-light" : "text-crimson-light")}>{e >= 0 ? "+" : ""}{(e * 100).toFixed(1)}%</Td>
          </tr>
        );
      })}
    </Table>
  );
}

// Draws the real simulated predictive distribution (histogram density from the
// backend's 100k draws). Bins right of the line are shaded as the Over region.
function HistCurve({ hist, line, loading }: { hist?: SimHistogram; line: number; loading: boolean }) {
  const W = 640;
  const H = 180;
  if (!hist) {
    return <div className="h-44 flex items-center justify-center text-xs text-gray-600">{loading ? "Simulating…" : "No distribution"}</div>;
  }
  const bins = hist.density.length;
  const maxD = Math.max(...hist.density, 1e-9);
  const barW = W / bins;
  const span = hist.hi - hist.lo || 1;
  const xAt = (v: number) => ((v - hist.lo) / span) * W;
  const lineX = Math.max(0, Math.min(W, xAt(line)));

  return (
    <svg viewBox={`0 0 ${W} ${H}`} className="w-full h-44">
      {hist.density.map((d, i) => {
        const h = (d / maxD) * (H - 18);
        const center = hist.lo + (i + 0.5) * hist.bin_width;
        const isOver = center >= line;
        return (
          <rect
            key={i}
            x={i * barW + 0.5}
            y={H - h}
            width={Math.max(0.5, barW - 1)}
            height={h}
            fill={isOver ? "rgba(16,185,129,0.55)" : "rgba(139,92,246,0.5)"}
          />
        );
      })}
      <line x1={lineX} y1={2} x2={lineX} y2={H} stroke="#F59E0B" strokeWidth={1.5} strokeDasharray="4 3" />
      <text x={Math.min(lineX, W - 14)} y={10} fill="#F59E0B" fontSize="9" textAnchor="middle">line</text>
    </svg>
  );
}

/** "(53.1%)": the implied probability of an American price, or nothing when it is not one. */
function Implied({ odds }: { odds: number | string | null | undefined }) {
  const p = impliedOrNull(odds);
  return p == null ? null : <span className="text-gray-400 font-mono">({formatPct(p)})</span>;
}

/** Odds input: type American (-162.5, +150), decimal (1.62) or fractional (8/13, evens);
 *  arrows / arrow keys step through real American prices (-101 -> +100, never -99).
 *  Shows the implied probability, decimal and fractional odds as you type. The
 *  value handed back is always American. */
function OddsInput({ label, value, onChange, inline = false, compact = false }: { label: string; value: string; onChange: (v: string) => void; inline?: boolean; compact?: boolean }) {
  const [text, setText] = useState(value);
  // follow outside changes (market price filled in), not our own normalisation
  useEffect(() => { if (parseOdds(text) !== parseOdds(value)) setText(value); }, [value]); // eslint-disable-line react-hooks/exhaustive-deps
  const a = parseOdds(text);
  const fmt = (x: number) => (x > 0 ? `+${+x.toFixed(1)}` : `${+x.toFixed(1)}`);
  const set = (t: string) => { setText(t); const x = parseOdds(t); if (x != null) onChange(String(+x.toFixed(1))); };
  const step = (dir: 1 | -1) => set(fmt(stepAmerican(a ?? -110, dir)));
  const frac = a == null ? null : fractionalOf(a);
  const hint = (
    <span className="font-mono text-[11px] text-gray-300 whitespace-nowrap" title="Implied probability (the break-even win rate at this price, vig included) · decimal · fractional">
      {a == null ? "—" : <>{impliedPctText(a)} implied<span className="text-gray-500"> · {decimalOf(a)}{frac ? ` · ${frac}` : ""}</span></>}
    </span>
  );
  const box = (
    <span className={clsx("relative inline-flex", inline ? "w-24 ml-1" : "w-full mt-1")}>
      <input value={text} inputMode="decimal" spellCheck={false}
        onChange={(e) => set(e.target.value)}
        onBlur={() => { if (a != null) setText(fmt(a)); }}
        onKeyDown={(e) => { if (e.key === "ArrowUp" || e.key === "ArrowDown") { e.preventDefault(); step(e.key === "ArrowUp" ? 1 : -1); } }}
        className={clsx("w-full rounded-md border bg-surface/60 px-3 py-2 pr-6 text-sm font-mono outline-none focus:border-violet/60", a == null && text.trim() !== "" ? "border-amber/60" : "border-glass")} />
      <span className="absolute right-1 inset-y-0 flex flex-col justify-center text-[9px] leading-none text-gray-500">
        <button type="button" tabIndex={-1} onClick={() => step(1)} className="hover:text-gray-200 px-0.5" aria-label="Odds up">▲</button>
        <button type="button" tabIndex={-1} onClick={() => step(-1)} className="hover:text-gray-200 px-0.5" aria-label="Odds down">▼</button>
      </span>
    </span>
  );
  if (inline) return <label className="text-xs text-gray-500 flex items-center gap-1">{label}{box}{!compact && hint}</label>;
  return (
    <label className="text-xs text-gray-500">
      <span className="flex justify-between gap-2"><span>{label}</span>{hint}</span>
      {box}
    </label>
  );
}
