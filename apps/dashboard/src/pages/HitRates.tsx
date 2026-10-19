import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import clsx from "clsx";
import { api } from "../lib/api";
import { Card, Input, Msg, PageTitle, Pills } from "../components/ui";
import { PlayerHitDetail, TeamHitDetail } from "../components/HitRates";

const SPORTS = [
  { key: "nba", label: "NBA" },
  { key: "nfl", label: "NFL" },
  { key: "nhl", label: "NHL" },
];

// Hit Rates: look up any player prop or team line and see how often it has
// cleared, window by window, with the game-by-game chart.
export function HitRates() {
  const [sport, setSport] = useState("nba");
  const [mode, setMode] = useState("player");
  return (
    <div className="space-y-4">
      <PageTitle
        title="Hit Rates"
        subtitle="how often a line has cleared · L5 / L10 / L20 / season / venue / head-to-head"
        right={<>
          <Pills options={[{ key: "player", label: "Player props" }, { key: "team", label: "Game lines" }]} value={mode} onChange={setMode} />
          <Pills options={SPORTS} value={sport} onChange={setSport} />
        </>}
      />
      {mode === "player" ? <PlayerLookup key={sport} sport={sport} /> : <TeamLookup key={sport} sport={sport} />}
    </div>
  );
}

function PlayerLookup({ sport }: { sport: string }) {
  const stats = useQuery({ queryKey: ["stat-list", sport], queryFn: () => api.statList(sport) });
  const [stat, setStat] = useState("");
  const [q, setQ] = useState("");
  const [opponent, setOpponent] = useState("");
  const [picked, setPicked] = useState<{ id: string; name: string } | null>(null);
  const activeStat = stat || stats.data?.stats[0]?.key || "";
  const list = useQuery({
    queryKey: ["hit-search", sport, activeStat, q],
    queryFn: () => api.playerProjections(sport, activeStat, { q, limit: 12, min_games: 5 }),
    enabled: !!activeStat && q.trim().length >= 2,
  });
  return (
    <div className="space-y-3">
      <div className="flex items-center gap-2 flex-wrap text-xs text-gray-400">
        <select value={activeStat} onChange={(e) => setStat(e.target.value)}
          className="bg-surface border border-glass rounded-md px-2 py-1 text-sm text-gray-100">
          {(stats.data?.stats ?? []).map((s) => <option key={s.key} value={s.key}>{s.label}</option>)}
        </select>
        <Input value={q} onChange={(v) => { setQ(v); setPicked(null); }} placeholder="search player (2+ letters)" className="w-56" />
        H2H vs <Input value={opponent} onChange={(v) => setOpponent(v.toUpperCase())} placeholder="opp" className="w-16" />
      </div>
      {!picked && list.data && (
        <Card className="p-2">
          {list.data.projections.length === 0 && <div className="text-sm text-gray-500 p-2">No players found.</div>}
          {list.data.projections.map((p) => (
            <button key={p.player_id} onClick={() => setPicked({ id: p.player_id, name: p.name })}
              className="w-full text-left px-2 py-1.5 rounded hover:bg-white/5 text-sm flex gap-2">
              <span className="font-medium">{p.name}</span>
              <span className="text-gray-500">{p.team} · last {p.last_season}</span>
              <span className="ml-auto font-mono text-gray-400">proj {p.proj_mean.toFixed(1)}</span>
            </button>
          ))}
        </Card>
      )}
      {list.error && <Msg tone="error">{(list.error as Error).message}</Msg>}
      {picked
        ? <PlayerHitDetail key={`${picked.id}-${activeStat}`} sport={sport} playerId={picked.id} stat={activeStat} opponent={opponent || null} />
        : !list.data && <Msg>Pick a prop and search a player to see their hit rates.</Msg>}
    </div>
  );
}

function TeamLookup({ sport }: { sport: string }) {
  const teams = useQuery({ queryKey: ["team-ratings", sport], queryFn: () => api.teamRatings(sport) });
  const [team, setTeam] = useState<string | null>(null);
  const [opponent, setOpponent] = useState("");
  const list = [...(teams.data?.teams ?? [])].sort((a, b) => a.team.localeCompare(b.team));
  return (
    <div className="space-y-3">
      {teams.error && <Msg tone="error">{(teams.error as Error).message}</Msg>}
      <div className="flex flex-wrap gap-1.5">
        {list.map((t) => (
          <button key={t.team} onClick={() => setTeam(t.team)}
            className={clsx("px-2 py-1 rounded-md border border-glass text-xs font-mono", team === t.team ? "bg-violet/20 text-violet" : "text-gray-300 hover:bg-white/5")}>
            {t.team}
          </button>
        ))}
      </div>
      <div className="flex items-center gap-2 text-xs text-gray-400">
        H2H vs <Input value={opponent} onChange={(v) => setOpponent(v.toUpperCase())} placeholder="opp" className="w-16" />
      </div>
      {team
        ? <TeamHitDetail key={`${team}-${opponent}`} sport={sport} team={team} opponent={opponent || null} />
        : <Msg>Pick a team, then enter a spread / total to see how often they would have cleared it.</Msg>}
    </div>
  );
}
