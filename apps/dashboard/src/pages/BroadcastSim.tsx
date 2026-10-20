import { useEffect, useMemo, useState } from "react";
import { useMutation, useQuery } from "@tanstack/react-query";
import { useSearchParams } from "react-router-dom";
import clsx from "clsx";
import { FootballField } from "../components/simulation/FootballField";
import { BoxScorePanel, Controls, LatestPlay, PlayByPlay, PREGAME, Scoreboard, Timeline } from "../components/simulation/Broadcast";
import { useSimulationPlayback } from "../hooks/useSimulationPlayback";
import { EMPTY_SCENARIO, isEmptyScenario, loadAllEvents, simApi, teamColor, type NflEnsemble, type NflSimGame, type Scenario, type SimEvent, type SimSession } from "../lib/sim";
import { LineupPanel, ScenarioImpact } from "../components/simulation/LineupPanel";
import { MlbBroadcast } from "../components/simulation/MlbBroadcast";
import { StartersCompare, type LineupMode } from "../components/simulation/StartersCompare";
import { formatPct } from "../lib/odds";
import { Card, Msg } from "../components/ui";

// Game Broadcast: one seeded simulated game played back like a TV broadcast.
// projection-service generates the whole authoritative event log (every play,
// clock, score and box score); this page only paces and draws it. The
// prediction panel is the separate 100k-game ensemble for the same matchup.

const SPORTS = [
  { key: "nfl", label: "NFL", ready: true },
  { key: "mlb", label: "MLB", ready: true },
  { key: "nba", label: "NBA", ready: false },
  { key: "nhl", label: "NHL", ready: false },
];

export function BroadcastSim() {
  const [params, setParams] = useSearchParams();
  const sport = params.get("sport") === "mlb" ? "mlb" : "nfl";
  return (
    <div className="space-y-4 min-w-0">
      <div className="flex flex-wrap items-center gap-3">
        <h1 className="text-lg font-semibold">Game Broadcast</h1>
        <span className="text-sm text-gray-500">one simulated game, play by play · every outcome from the simulation engine</span>
        <div className="ml-auto flex overflow-hidden rounded-md border border-glass">
          {SPORTS.map((s) => (
            <button key={s.key} disabled={!s.ready} onClick={() => setParams(s.key === "nfl" ? {} : { sport: s.key })} title={s.ready ? "" : "broadcast engine not built yet"}
              className={clsx("px-3 py-1 text-xs", sport === s.key ? "bg-amber/20 text-amber" : "text-gray-400", !s.ready && "opacity-40 cursor-not-allowed")}>
              {s.label}
            </button>
          ))}
        </div>
      </div>
      {sport === "mlb" ? <MlbBroadcast /> : <NflBroadcast />}
    </div>
  );
}

function GamePicker({ games, value, onPick }: { games: NflSimGame[]; value: string | null; onPick: (g: string) => void }) {
  return (
    <div className="flex gap-2 overflow-x-auto pb-1">
      {games.map((g) => (
        <button key={g.game_id} onClick={() => onPick(g.game_id)}
          className={clsx("shrink-0 rounded-lg border px-3 py-2 text-left text-xs",
            g.game_id === value ? "border-amber/60 bg-amber/10" : "border-glass hover:bg-white/5")}>
          <div className="flex items-center gap-1.5 text-sm font-medium">
            <span className="h-2.5 w-2.5 rounded-sm" style={{ background: teamColor(g.away) }} />{g.away}
            <span className="text-gray-500">@</span>
            <span className="h-2.5 w-2.5 rounded-sm" style={{ background: teamColor(g.home) }} />{g.home}
            {g.today && <span className="ml-1 rounded bg-crimson/30 px-1 text-[10px] text-crimson-light">TODAY</span>}
          </div>
          <div className="text-gray-500">Wk {g.week} · {g.date}{g.home_score != null ? ` · final ${g.away_score}-${g.home_score}` : ""}</div>
          {g.spread_line != null && <div className="text-gray-400">{g.home} {g.spread_line > 0 ? `-${g.spread_line}` : `+${-g.spread_line}`} · O/U {g.total_line}</div>}
        </button>
      ))}
    </div>
  );
}

function Validation() {
  const bt = useQuery({ queryKey: ["nfl-backtests"], queryFn: simApi.nflBacktests, staleTime: 30 * 60_000 });
  const runs = bt.data?.backtests ?? [];
  if (!runs.length) return <div className="text-[11px] text-amber">Experimental: no stored walk-forward backtest yet.</div>;
  const verdict = (b?: { brier_diff: number; brier_diff_se: number }) =>
    !b ? "—" : b.brier_diff < -2 * b.brier_diff_se ? "beats" : b.brier_diff > 2 * b.brier_diff_se ? "worse than" : "≈";
  return (
    <div className="rounded-md border border-amber/30 bg-amber/5 p-2 text-[11px] text-gray-300 space-y-0.5">
      <div className="font-semibold text-amber">Experimental model — walk-forward results (data strictly before each game)</div>
      {runs.map((r) => (
        <div key={r.season}>
          {r.season} ({r.games_simulated} games, params {r.params}): home win {verdict(r.binary.home_win)} closing spread
          (Brier {r.binary.home_win?.brier_model} vs {r.binary.home_win?.brier_baseline}); totals {verdict(r.binary.total_over)} a coin flip at the closing total,
          scoring bias {r.numeric.total_points?.bias > 0 ? "+" : ""}{r.numeric.total_points?.bias} pts; player props {verdict(r.binary.rec_yds_over_avg)} players' own history.
        </div>
      ))}
      <div className="text-gray-500">Not betting advice; no historical prop prices are stored, so this is not an ROI claim.</div>
    </div>
  );
}

function Prediction({ q, teams }: { q: { data?: NflEnsemble; isLoading: boolean; error: unknown }; teams: [string, string] }) {
  if (q.isLoading) return <Msg>Running 100,000 simulated games…</Msg>;
  if (q.error) return <Msg tone="error">{(q.error as Error).message}</Msg>;
  const d = q.data;
  if (!d) return null;
  const top = (side: "away" | "home") => {
    const ps = d.players[side];
    const qb = ps.find((p) => p.role === "QB");
    const rec = ps.filter((p) => p.role === "skill" && p.stats.rec_yds).sort((a, b) => b.stats.rec_yds.mean - a.stats.rec_yds.mean).slice(0, 3);
    const rush = ps.filter((p) => p.role === "skill" && p.stats.rush_yds).sort((a, b) => b.stats.rush_yds.mean - a.stats.rush_yds.mean).slice(0, 2);
    return { qb, rec, rush, td: ps.filter((p) => p.role !== "K").sort((a, b) => b.anytime_td * b.p_active - a.anytime_td * a.p_active).slice(0, 3) };
  };
  const row = (label: string, s?: { q10: number; q50: number; q90: number; mean: number }, pa = 1) => s && (
    <div className="flex justify-between gap-2 text-[13px]"><span className="truncate text-gray-300">{label}{pa < 0.999 && <span className="ml-1 text-[11px] text-amber">plays {formatPct(pa, 0)}</span>}</span>
      <span className="font-mono text-gray-400">{s.q50} <span className="text-gray-600">({s.q10}–{s.q90})</span></span></div>
  );
  return (
    <div className="space-y-4">
      <div className="grid [&>*]:min-w-0 grid-cols-3 gap-3 text-center">
        <div><div className="text-[11px] text-gray-500">{teams[0]} win</div><div className="font-mono text-xl">{formatPct(d.win.away)}</div></div>
        <div><div className="text-[11px] text-gray-500">Median score</div><div className="font-mono text-xl">{d.score.away.q50}–{d.score.home.q50}</div></div>
        <div><div className="text-[11px] text-gray-500">{teams[1]} win</div><div className="font-mono text-xl">{formatPct(d.win.home)}</div></div>
      </div>
      <div className="grid [&>*]:min-w-0 grid-cols-2 gap-3 text-[13px]">
        <div className="rounded-md border border-glass p-2">
          <div className="text-gray-500 text-[11px]">Total points</div>
          <div className="font-mono">{d.total.mean.toFixed(1)} <span className="text-gray-500">(80%: {d.total.q10}–{d.total.q90})</span></div>
          {d.market.total_line != null && <div className="text-gray-400">P(over {d.market.total_line}) {formatPct(d.market.p_over_total ?? 0)}</div>}
        </div>
        <div className="rounded-md border border-glass p-2">
          <div className="text-gray-500 text-[11px]">{teams[1]} margin</div>
          <div className="font-mono">{d.margin_home.mean > 0 ? "+" : ""}{d.margin_home.mean.toFixed(1)} <span className="text-gray-500">(80%: {d.margin_home.q10}–{d.margin_home.q90})</span></div>
          {d.market.spread_line != null && <div className="text-gray-400">P({teams[1]} covers {d.market.spread_line > 0 ? `-${d.market.spread_line}` : `+${-d.market.spread_line}`}) {formatPct(d.market.p_home_cover ?? 0)}</div>}
        </div>
      </div>
      <div className="grid [&>*]:min-w-0 grid-cols-2 gap-4">
        {(["away", "home"] as const).map((side, i) => {
          const t = top(side);
          return (
            <div key={side} className="space-y-1">
              <div className="text-[11px] font-semibold tracking-widest" style={{ color: teamColor(teams[i]) === "#000000" ? "#ddd" : undefined }}>{teams[i]} · median (80%)</div>
              {t.qb && row(`${t.qb.name} pass yds`, t.qb.stats.pass_yds, t.qb.p_active)}
              {t.rush.map((p) => <div key={`r${p.idx}`}>{row(`${p.name} rush yds`, p.stats.rush_yds, p.p_active)}</div>)}
              {t.rec.map((p) => <div key={`c${p.idx}`}>{row(`${p.name} rec yds`, p.stats.rec_yds, p.p_active)}</div>)}
              <div className="pt-1 text-[11px] text-gray-500">Anytime TD: {t.td.map((p) => `${p.name.split(" ").slice(-1)[0]} ${formatPct(p.anytime_td)}`).join(" · ")}</div>
            </div>
          );
        })}
      </div>
      <div className="text-[11px] text-gray-500">n = {d.n.toLocaleString()} games in {(d.sim_ms / 1000).toFixed(1)}s · seed {d.seed} · {d.model_version}. {d.note}</div>
      <Validation />
    </div>
  );
}

function NflBroadcast() {
  // deep links: ?game=<game_id>&seq=<play number> (seed=<n> replays a specific simulation)
  const [params] = useSearchParams();
  const games = useQuery({ queryKey: ["nfl-sim-games"], queryFn: simApi.nflGames });
  const [gameId, setGameId] = useState<string | null>(params.get("game"));
  const list = games.data?.games ?? [];
  useEffect(() => {
    if (gameId || !list.length) return;
    const pick = list.find((g) => g.today && g.home_score == null) ?? list.find((g) => g.home_score == null) ?? list[list.length - 1];
    setGameId(pick.game_id);
  }, [list, gameId]);

  const [session, setSession] = useState<SimSession | null>(null);
  // alternate reality: `draft` is being edited, `applied` drives the broadcast + prediction
  const [draft, setDraft] = useState<Scenario>(EMPTY_SCENARIO);
  const [applied, setApplied] = useState<Scenario>(EMPTY_SCENARIO);
  const [lineupMode, setLineupMode] = useState<LineupMode>("confirmed");
  const [events, setEvents] = useState<SimEvent[]>([]);
  const create = useMutation({
    mutationFn: async ({ gid, scenario, lineup = "confirmed" }: { gid: string; scenario: Scenario; lineup?: LineupMode }) => {
      const seedParam = params.get("seed");
      const s = await simApi.createSession({ sport: "nfl", game_id: gid, seed: seedParam ? Number(seedParam) : undefined, lineup, ...scenario });
      const ev = await loadAllEvents(s.session_id);
      return { s, ev };
    },
    onSuccess: ({ s, ev }) => { setSession(s); setEvents(ev); },
  });
  useEffect(() => {
    if (gameId) {
      setSession(null); setEvents([]); setDraft(EMPTY_SCENARIO); setApplied(EMPTY_SCENARIO); setLineupMode("confirmed");
      create.mutate({ gid: gameId, scenario: EMPTY_SCENARIO });
    }
  }, [gameId]); // eslint-disable-line react-hooks/exhaustive-deps

  const ensemble = useQuery({ queryKey: ["nfl-ensemble", gameId, lineupMode], queryFn: () => simApi.nflEnsemble(gameId!, EMPTY_SCENARIO, lineupMode), enabled: !!gameId, staleTime: 10 * 60_000 });
  const scenarioOn = !isEmptyScenario(applied);
  const altEnsemble = useQuery({ queryKey: ["nfl-ensemble", gameId, lineupMode, applied], queryFn: () => simApi.nflEnsemble(gameId!, applied, lineupMode),
    enabled: !!gameId && scenarioOn, staleTime: 10 * 60_000 });
  const runReality = (s: Scenario) => {
    if (!gameId) return;
    setApplied(s);
    create.mutate({ gid: gameId, scenario: s, lineup: lineupMode });
  };
  const changeLineupMode = (m: LineupMode) => {
    if (!gameId) return;
    setLineupMode(m);
    create.mutate({ gid: gameId, scenario: applied, lineup: m });
  };
  const starterTeams = (session?.meta.inputs.starters ?? []).map((t) => ({
    abbr: t.team,
    rows: t.projected.map((r, i) => ({ label: r.role, projected: r.player, confirmed: t.confirmed[i]?.player ?? null, actual: t.actual ? t.actual[i]?.player ?? null : undefined })),
  }));
  const lineupTeams = ensemble.data?.inputs.teams ?? session?.meta.inputs.teams ?? [];
  const nameOf = (id: string) => lineupTeams.flatMap((t) => t.lineup).find((e) => e.id === id)?.name ?? id;

  const pb = useSimulationPlayback(events);
  useEffect(() => {
    const q = Number(params.get("seq"));
    if (events.length && q > 0) pb.jump(q);
  }, [events]); // eslint-disable-line react-hooks/exhaustive-deps
  const teams: [string, string] = session ? [session.meta.away, session.meta.home] : ["AWAY", "HOME"];
  const players = session?.meta.inputs.players ?? [];
  const keys = session?.meta.stat_keys ?? [];
  const cur = pb.current;
  const state = cur?.state ?? PREGAME;
  const box = cur?.box_score ?? [];
  const carrierIdx = cur ? (cur.receiver ?? cur.rusher ?? null) : null;
  const carrier = carrierIdx != null && cur && !["pass_incomplete", "interception"].includes(cur.kind) ? players[carrierIdx]?.name : undefined;
  const final = pb.done && events.length > 0;
  const actual = session?.meta.actual;
  const inputs = session?.meta.inputs;
  const flags = useMemo(() => inputs?.flags ?? [], [inputs]);

  return (
    <div className="space-y-4 min-w-0">
      {games.error && <Msg tone="error">{(games.error as Error).message}</Msg>}
      {games.isLoading && <Msg>Loading games…</Msg>}
      {list.length > 0 && <GamePicker games={list} value={gameId} onPick={setGameId} />}
      {games.data?.plays_through && <div className="text-[11px] text-gray-500">Play-by-play data through {games.data.plays_through}; each game uses only plays before its date.</div>}

      <div className="relative overflow-hidden rounded-2xl border border-white/10 p-4 md:p-6"
        style={{ background: "radial-gradient(ellipse at 50% 0%, rgba(185,28,28,0.45), rgba(10,10,14,0.96) 55%), radial-gradient(ellipse at 50% 100%, rgba(127,29,29,0.35), transparent 60%), #07090E" }}>
        {create.isPending && <Msg>Simulating the game…</Msg>}
        {create.error && <Msg tone="error">{(create.error as Error).message}</Msg>}
        <Scoreboard s={state} teams={teams} live={!!cur} final={final} />
        <div className="mt-4 grid gap-4 lg:grid-cols-[230px_minmax(0,1fr)_230px]">
          <div className="order-2 lg:order-1"><BoxScorePanel team={0} abbr={teams[0]} box={box} players={players} keys={keys} /></div>
          <div className="order-1 lg:order-2 space-y-3">
            <FootballField event={cur} teams={teams} speed={pb.speed} carrier={carrier} />
            <LatestPlay e={cur} teams={teams} />
          </div>
          <div className="order-3"><BoxScorePanel team={1} abbr={teams[1]} box={box} players={players} keys={keys} /></div>
        </div>
        <div className="mt-4 space-y-3">
          <Controls playing={pb.playing} toggle={pb.toggle} step={pb.step} restart={pb.restart} jumpEnd={() => pb.jump(pb.total)}
            speed={pb.speed} setSpeed={pb.setSpeed} onNew={() => gameId && create.mutate({ gid: gameId, scenario: applied, lineup: lineupMode })} seq={pb.seq} total={pb.total} busy={create.isPending} />
          {scenarioOn && <div className="text-[12px] text-amber">Alternate reality: {[
            ...applied.out.map((id) => `${nameOf(id)} out`),
            ...applied.in.map((id) => `${nameOf(id)} plays`),
            ...[applied.away_qb, applied.home_qb].filter((x): x is string => !!x).map((id) => `${nameOf(id)} starts at QB`),
          ].join(" · ")}</div>}
          <Timeline events={events} seq={pb.seq} jump={pb.jump} teams={teams} />
        </div>
        {final && actual?.home_score != null && (
          <div className="mt-3 text-sm text-gray-400">Real result of this game: {teams[0]} {actual.away_score} — {teams[1]} {actual.home_score} (the simulation used only data from before kickoff).</div>
        )}
      </div>

      {starterTeams.length > 0 && (
        <Card>
          <div className="mb-2 text-sm font-semibold">Starters: projected vs confirmed vs actual</div>
          <StartersCompare teams={starterTeams} mode={lineupMode} onMode={changeLineupMode} busy={create.isPending}
            available={{ projected: true, confirmed: true, actual: !!session?.meta.inputs.actual_available }} />
        </Card>
      )}

      {lineupTeams.length > 0 && (
        <Card>
          <div className="mb-2 text-sm font-semibold">Lineups &amp; alternate realities</div>
          <LineupPanel teams={lineupTeams} draft={draft} setDraft={setDraft} applied={applied} running={create.isPending || altEnsemble.isFetching}
            onRun={() => runReality(draft)} onReset={() => { setDraft(EMPTY_SCENARIO); runReality(EMPTY_SCENARIO); }}
            rates={ensemble.data ? { ...ensemble.data.inputs.availability.rates, source: ensemble.data.inputs.availability.rates_source } : undefined} />
        </Card>
      )}

      <div className="grid [&>*]:min-w-0 gap-4 lg:grid-cols-2">
        <Card>
          <div className="mb-2 text-sm font-semibold">Play-by-play</div>
          <PlayByPlay events={events} seq={pb.seq} jump={pb.jump} teams={teams} />
        </Card>
        <div className="space-y-4">
          <Card>
            <div className="mb-2 text-sm font-semibold">{scenarioOn ? "Alternate reality" : "Confirmed lineup"} · 100,000-game ensemble</div>
            {scenarioOn && <div className="mb-3"><ScenarioImpact base={ensemble.data} alt={altEnsemble.data} teams={teams} /></div>}
            <Prediction q={scenarioOn ? altEnsemble : ensemble} teams={teams} />
          </Card>
          {session && inputs && (
            <Card>
              <div className="mb-2 text-sm font-semibold">This simulation</div>
              <div className="grid [&>*]:min-w-0 grid-cols-2 gap-x-4 gap-y-1 text-[13px] text-gray-300">
                <span className="text-gray-500">Session</span><span className="font-mono truncate min-w-0">{session.session_id}</span>
                <span className="text-gray-500">Seed</span><span className="font-mono">{session.seed}</span>
                <span className="text-gray-500">Model</span><span className="font-mono">{session.model_version}</span>
                <span className="text-gray-500">Parameters</span><span className="font-mono">{inputs.params}</span>
                <span className="text-gray-500">Data cutoff</span><span className="font-mono">{inputs.data_cutoff}</span>
                <span className="text-gray-500">Events</span><span className="font-mono">{session.events}</span>
              </div>
              <div className="mt-3 grid grid-cols-2 gap-3 text-[12px]">
                {inputs.teams.map((t) => (
                  <div key={t.abbr} className="space-y-0.5">
                    <div className="font-semibold">{t.abbr} · QB {t.offense.qb}</div>
                    <div className="text-gray-400">Targets: {t.offense.receivers.slice(0, 4).map((r) => `${r.name.split(" ").slice(-1)[0]} ${(r.target_share * 100).toFixed(0)}%`).join(", ")}</div>
                    <div className="text-gray-400">Carries: {t.offense.rushers.slice(0, 3).map((r) => `${r.name.split(" ").slice(-1)[0]} ${(r.carry_share * 100).toFixed(0)}%`).join(", ")}</div>
                  </div>
                ))}
              </div>
              {(session.meta.this_reality?.inactive?.length ?? 0) > 0 && (
                <div className="mt-3 text-[12px] text-amber">In this simulated game: {session.meta.this_reality.inactive.map((p) => `${p.name} (Questionable/Doubtful) sat out`).join(" · ")}</div>
              )}
              <ul className="mt-3 list-disc pl-5 text-[11px] text-gray-500 space-y-0.5">
                {[...flags, ...inputs.assumptions].map((a) => <li key={a}>{a}</li>)}
              </ul>
            </Card>
          )}
        </div>
      </div>
    </div>
  );
}
