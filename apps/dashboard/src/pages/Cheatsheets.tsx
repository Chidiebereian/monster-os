import { Fragment, useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import clsx from "clsx";
import { api, type GameRow, type PropRow } from "../lib/api";
import { formatAmerican, formatPct } from "../lib/odds";
import { Card, Input, Msg, PageTitle, Pills, Table, Td, Th } from "../components/ui";
import { PlayerHitDetail, RateCell, Streak, TeamHitDetail, fmtSpread, type Side } from "../components/HitRates";

const SPORTS = [
  { key: "nba", label: "NBA" },
  { key: "nfl", label: "NFL" },
  { key: "nhl", label: "NHL" },
];

// Cheat sheets: every live line graded by how often it has hit (L5/L10/L20,
// season, venue, head-to-head) next to the model's probability and edge vs the
// de-vigged market. Player props and game lines (win / cover / over).
export function Cheatsheets() {
  const [sport, setSport] = useState("nba");
  const [tab, setTab] = useState("props");
  return (
    <div className="space-y-4">
      <PageTitle
        title="Cheat Sheets"
        subtitle="live lines ranked by hit rate · model edge vs de-vigged market"
        right={<>
          <Pills options={[{ key: "props", label: "Player Props" }, { key: "games", label: "Game Lines" }]} value={tab} onChange={setTab} />
          <Pills options={SPORTS} value={sport} onChange={setSport} />
        </>}
      />
      {tab === "props" ? <PropsSheet key={sport} sport={sport} /> : <GamesSheet key={sport} sport={sport} />}
    </div>
  );
}

// --------------------------------------------------------------------------
// Player props
// --------------------------------------------------------------------------

const SORTS = [
  { key: "l5", label: "L5" }, { key: "l10", label: "L10" }, { key: "l20", label: "L20" },
  { key: "season", label: "Season" }, { key: "h2h", label: "H2H" }, { key: "edge", label: "Model edge" },
];

function PropsSheet({ sport }: { sport: string }) {
  const [stat, setStat] = useState("");
  const [sort, setSort] = useState("l10");
  const [side, setSide] = useState("best");
  const [source, setSource] = useState("market");
  const [search, setSearch] = useState("");
  const [minRate, setMinRate] = useState("");
  const [open, setOpen] = useState<string | null>(null);

  const stats = useQuery({ queryKey: ["stat-list", sport], queryFn: () => api.statList(sport) });
  const effectiveSource = source === "projection" && !stat ? "market" : source;
  const { data, error, isLoading, isFetching } = useQuery({
    queryKey: ["props-sheet", sport, stat, sort, side, effectiveSource],
    queryFn: () => api.propsCheatsheet(sport, { stat: stat || undefined, sort, side, source: effectiveSource, limit: 300 }),
    refetchInterval: 120_000,
  });

  const rows = useMemo(() => {
    const q = search.trim().toLowerCase();
    const floor = Number(minRate) / 100;
    return (data?.props ?? []).filter((p) =>
      (!q || p.player.toLowerCase().includes(q) || (p.team ?? "").toLowerCase().includes(q) || (p.opponent ?? "").toLowerCase().includes(q))
      && (!minRate || (p.side_rate ?? 0) >= floor));
  }, [data, search, minRate]);

  const statOptions = [{ key: "", label: "All props" }, ...(stats.data?.stats ?? []).map((s) => ({ key: s.key, label: s.label }))];

  return (
    <div className="space-y-3">
      <div className="flex items-center gap-2 flex-wrap text-xs text-gray-400">
        <select value={stat} onChange={(e) => setStat(e.target.value)}
          className="bg-surface border border-glass rounded-md px-2 py-1 text-sm text-gray-100">
          {statOptions.map((o) => <option key={o.key} value={o.key}>{o.label}</option>)}
        </select>
        rank by <Pills options={SORTS} value={sort} onChange={setSort} />
        <Pills options={[{ key: "best", label: "Best side" }, { key: "over", label: "Overs" }, { key: "under", label: "Unders" }]} value={side} onChange={setSide} />
        <Pills options={[{ key: "market", label: "Market lines" }, { key: "projection", label: "Model lines" }]} value={source} onChange={setSource} />
        <Input value={search} onChange={setSearch} placeholder="player / team" className="w-36" />
        min hit % <Input type="number" value={minRate} onChange={setMinRate} className="w-16" />
        {isFetching && <span className="text-violet">updating…</span>}
      </div>
      {source === "projection" && !stat && <div className="text-[11px] text-amber">Model lines need a single prop — pick one above (showing market lines).</div>}
      {isLoading && <Msg>Grading every live prop line…</Msg>}
      {error && <Msg tone="error">{(error as Error).message}</Msg>}
      {data && rows.length === 0 && <Msg>No props match. The market feed may not have posted lines for this sport yet.</Msg>}
      {rows.length > 0 && (
        <Table head={
          <tr>
            <Th>Player</Th><Th>Prop</Th><Th>Odds</Th><Th>L5</Th><Th>L10</Th><Th>L20</Th><Th>Season</Th>
            <Th>H2H</Th><Th>Venue</Th><Th>Streak</Th><Th>Avg L10</Th><Th>Model</Th><Th>Edge</Th>
          </tr>
        }>
          {rows.map((p) => {
            const key = `${p.player_id}-${p.stat}`;
            return (
              <Fragment key={key}>
                <PropLine p={p} active={open === key} onClick={() => setOpen(open === key ? null : key)} />
                {open === key && (
                  <tr><td colSpan={13} className="p-3 bg-obsidian/40">
                    <PlayerHitDetail sport={sport} playerId={p.player_id} stat={p.stat} line={p.line} opponent={p.opponent} initialSide={p.side} />
                  </td></tr>
                )}
              </Fragment>
            );
          })}
        </Table>
      )}
      {data && (
        <div className="text-[11px] text-gray-500">
          {rows.length} of {data.count} lines · hit % excludes pushes · Venue = tonight's home/away split (last 20) · H2H = last 10 vs tonight's opponent ·
          near-certain lines (priced under 15% / over 85%) are hidden · {data.note}.
        </div>
      )}
    </div>
  );
}

function PropLine({ p, active, onClick }: { p: PropRow; active: boolean; onClick: () => void }) {
  const side = p.side as Side;
  const h = p.hit_rates;
  const prob = p.model ? (side === "over" ? p.model.p_over : p.model.p_under) : null;
  const edge = p.model ? (side === "over" ? p.model.edge_over : p.model.edge_under) : null;
  const odds = side === "over" ? p.over_odds : p.under_odds;
  return (
    <tr onClick={onClick} className={clsx("border-t border-glass cursor-pointer hover:bg-white/[0.03]", active && "bg-violet/10")}>
      <Td>
        <div className="font-medium">{p.player}</div>
        <div className="text-[11px] text-gray-500">{p.team ?? ""}{p.opponent && ` ${p.home === false ? "@" : "vs"} ${p.opponent}`}</div>
      </Td>
      <Td className="whitespace-nowrap">
        <span className={clsx("font-mono font-semibold", side === "over" ? "text-emerald-light" : "text-crimson-light")}>{side === "over" ? "O" : "U"} {p.line}</span>
        <span className="text-gray-400 ml-1.5">{p.label}</span>
      </Td>
      <Td className="font-mono">{formatAmerican(odds)}</Td>
      <Td><RateCell s={h.l5} side={side} /></Td>
      <Td><RateCell s={h.l10} side={side} /></Td>
      <Td><RateCell s={h.l20} side={side} /></Td>
      <Td><RateCell s={h.season} side={side} /></Td>
      <Td><RateCell s={h.h2h} side={side} /></Td>
      <Td><RateCell s={h.venue} side={side} /></Td>
      <Td><Streak w={h} /></Td>
      <Td className="font-mono">{p.avg_l10 ?? "—"}</Td>
      <Td className="font-mono">{prob == null ? "—" : formatPct(prob, 0)}</Td>
      <Td className={clsx("font-mono", edge != null && edge > 0.03 && "text-emerald-light", edge != null && edge < -0.03 && "text-crimson-light")}>
        {edge == null ? "—" : `${edge > 0 ? "+" : ""}${(edge * 100).toFixed(1)}%`}
      </Td>
    </tr>
  );
}

// --------------------------------------------------------------------------
// Game lines
// --------------------------------------------------------------------------

function GamesSheet({ sport }: { sport: string }) {
  const { data, error, isLoading, isFetching } = useQuery({
    queryKey: ["games-sheet", sport],
    queryFn: () => api.gamesCheatsheet(sport),
    refetchInterval: 120_000,
  });
  const [open, setOpen] = useState<string | null>(null);
  const [search, setSearch] = useState("");
  const games = (data?.games ?? []).filter((g) => !search || `${g.home} ${g.away}`.toLowerCase().includes(search.toLowerCase()));
  return (
    <div className="space-y-3">
      <div className="flex items-center gap-2 text-xs text-gray-400">
        <Input value={search} onChange={setSearch} placeholder="team" className="w-28" />
        {data && <span>{data.count} games · lines from {data.source === "market" ? "live market" : "stored schedule"}</span>}
        {isFetching && <span className="text-violet">updating…</span>}
      </div>
      {isLoading && <Msg>Grading every upcoming game…</Msg>}
      {error && <Msg tone="error">{(error as Error).message}</Msg>}
      {data && games.length === 0 && <Msg>No upcoming games with lines.</Msg>}
      <div className="grid xl:grid-cols-2 gap-4 items-start">
        {games.map((g) => {
          const id = `${g.away}@${g.home}-${g.starts_at}`;
          return <GameCard key={id} g={g} sport={sport} open={open?.startsWith(id) ? open.slice(id.length + 1) : null}
            onOpen={(team) => setOpen(team && open !== `${id}:${team}` ? `${id}:${team}` : null)} />;
        })}
      </div>
      {data && <div className="text-[11px] text-gray-500">{data.note} Hit % excludes pushes. Click a team for its game-by-game chart.</div>}
    </div>
  );
}

function GameCard({ g, sport, open, onOpen }: { g: GameRow; sport: string; open: string | null; onOpen: (team: string | null) => void }) {
  const hasClosing = !!g.home_team.markets.ats_closing;
  const when = g.starts_at ? new Date(g.starts_at).toLocaleString(undefined, { weekday: "short", month: "short", day: "numeric", hour: "numeric", minute: "2-digit" }) : "";
  const sides = [
    { team: g.away, s: g.away_team, spread: g.spread_home == null ? null : -g.spread_home, ml: g.market?.ml_away_odds, opp: g.home },
    { team: g.home, s: g.home_team, spread: g.spread_home, ml: g.market?.ml_home_odds, opp: g.away },
  ];
  return (
    <Card className="space-y-3">
      <div className="flex items-baseline gap-2 flex-wrap">
        <div className="font-medium">{g.away} <span className="text-gray-500">@</span> {g.home}</div>
        <div className="text-xs text-gray-500">{when}</div>
        <div className="ml-auto text-xs text-gray-400 font-mono">
          {g.spread_home != null && <>{g.home} {fmtSpread(g.spread_home)} · </>}
          {g.total != null && <>O/U {g.total}</>}
        </div>
      </div>
      <div className="overflow-auto">
        <table className="w-full text-sm">
          <thead>
            <tr className="text-[11px] uppercase tracking-wide text-gray-500">
              <th className="text-left font-medium py-1">Team</th>
              <th className="text-left font-medium">ML</th>
              <th className="text-left font-medium">Win L10</th>
              <th className="text-left font-medium">Cover L10</th>
              <th className="text-left font-medium">Cover L20</th>
              <th className="text-left font-medium">Cover venue</th>
              <th className="text-left font-medium">Cover H2H</th>
              <th className="text-left font-medium">Over L10</th>
              <th className="text-left font-medium">Over szn</th>
              {hasClosing && <th className="text-left font-medium">ATS close L10</th>}
            </tr>
          </thead>
          <tbody>
            {sides.map(({ team, s, spread, ml }) => (
              <tr key={team} onClick={() => onOpen(team)} className={clsx("border-t border-glass cursor-pointer hover:bg-white/[0.03]", open === team && "bg-violet/10")}>
                <td className="py-1.5"><span className="font-medium">{team}</span> <span className="text-gray-500 font-mono text-xs">{fmtSpread(spread)}</span></td>
                <td className="font-mono text-xs">{formatAmerican(ml)}</td>
                <td><RateCell s={s.markets.win?.l10} side="over" compact /></td>
                <td><RateCell s={s.markets.cover?.l10} side="over" /></td>
                <td><RateCell s={s.markets.cover?.l20} side="over" compact /></td>
                <td><RateCell s={s.markets.cover?.venue} side="over" compact /></td>
                <td><RateCell s={s.markets.cover?.h2h} side="over" compact /></td>
                <td><RateCell s={s.markets.over?.l10} side="over" /></td>
                <td><RateCell s={s.markets.over?.season} side="over" compact /></td>
                {hasClosing && <td><RateCell s={s.markets.ats_closing?.l10} side="over" compact /></td>}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {open && (() => {
        const side = sides.find((x) => x.team === open);
        return side ? <TeamHitDetail key={open} sport={sport} team={side.team} spread={side.spread} total={g.total} opponent={side.opp} /> : null;
      })()}
    </Card>
  );
}
