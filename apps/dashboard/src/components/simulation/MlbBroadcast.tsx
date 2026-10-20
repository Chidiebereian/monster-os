import { useEffect, useMemo, useState } from "react";
import { useMutation, useQuery } from "@tanstack/react-query";
import { useSearchParams } from "react-router-dom";
import clsx from "clsx";
import { api, type MlbSim } from "../../lib/api";
import { formatPct } from "../../lib/odds";
import { Card, Msg } from "../ui";
import { useSimulationPlayback } from "../../hooks/useSimulationPlayback";
import { Controls, shortName } from "./Broadcast";
import { StartersCompare, type LineupMode } from "./StartersCompare";

// MLB Game Broadcast: one seeded game, plate appearance by plate appearance,
// from projection-service (mlb_sim). Outcomes, bases, outs, score, line score
// and box scores all come from the event log; this file only draws them.

interface Person { id: string; name: string; pos: string }
interface MlbRosterSide {
  lineup: Person[]; lineup_source: string; starter: Person; starter_source: string;
  bench: Person[]; starter_options: Person[]; substitutions: { slot: number; out: string; in: string }[];
}
interface MlbView { lineup: Person[] | null; starter: Person | null }
interface MlbViews { projected: MlbView | null; confirmed: MlbView | null; actual: MlbView | null }
interface MlbMeta {
  sport: "mlb"; game_id: string; date: string; season: number; postseason: boolean; away: string; home: string; venue: string | null;
  status: string; bat_keys: string[]; pit_keys: string[];
  roster: { away: MlbRosterSide; home: MlbRosterSide; scenario_out: string[]; lineup_mode: LineupMode; views: [MlbViews, MlbViews] };
  actual: { away_score: number | null; home_score: number | null };
  inputs: { model_version: string; params: unknown; data_cutoff: string; flags: string[]; assumptions: string[] };
}
interface MlbState {
  inning: number; half: 0 | 1; outs: number; bases: (number | null)[]; score: [number, number];
  linescore: [number[], number[]]; hits: [number, number]; on_mound: [number, number]; next_up: [number, number];
}
interface MlbEvent {
  seq: number; kind: "pa" | "pitching_change" | "inning_end" | "game_end"; inning: number; half: 0 | 1; batting: 0 | 1; slot: number;
  bench: boolean; pitcher: number; result: string; outs_before: number; bases_before: (number | null)[]; outs_on_play: number;
  runs: number; scorers: number[]; rbi: number; text: string; state: MlbState;
  batting_box: number[][][]; pitching_box: number[][][];
}
interface MlbSession { session_id: string; seed: number; model_version: string; data_cutoff: string; events: number; meta: MlbMeta }
interface Scenario { out: string[]; away_sp?: string; home_sp?: string }
const EMPTY: Scenario = { out: [] };

async function req<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(`/api/projection-service${path}`, { ...init, headers: { accept: "application/json", ...(init?.body ? { "content-type": "application/json" } : {}) } });
  if (!res.ok) {
    const t = await res.text().catch(() => "");
    let m = t; try { m = JSON.parse(t).error ?? t; } catch { /* text */ }
    throw new Error(`${res.status}: ${m.slice(0, 300)}`);
  }
  return res.json() as Promise<T>;
}
async function loadEvents(id: string): Promise<MlbEvent[]> {
  const page = await req<{ events: MlbEvent[]; done: boolean }>(`/sim/sessions/${id}/events?after=0&limit=1000`);
  page.events.forEach((e, i) => { if (e.seq !== i + 1) throw new Error(`event stream gap at ${i + 1}`); });
  return page.events;
}

const ord = (n: number) => `${n}${n % 10 === 1 && n % 100 !== 11 ? "st" : n % 10 === 2 && n % 100 !== 12 ? "nd" : n % 10 === 3 && n % 100 !== 13 ? "rd" : "th"}`;
const RESULT_LABEL: Record<string, string> = { k: "STRIKEOUT", bb: "WALK", hbp: "HIT BY PITCH", "1b": "SINGLE", "2b": "DOUBLE", "3b": "TRIPLE", hr: "HOME RUN", out: "OUT" };
const PREGAME: MlbState = { inning: 1, half: 0, outs: 0, bases: [null, null, null], score: [0, 0], linescore: [[], []], hits: [0, 0], on_mound: [0, 0], next_up: [0, 0] };

function Diamond({ e, s, lineups, starters, teams }: {
  e?: MlbEvent; s: MlbState; lineups: [Person[], Person[]]; starters: [Person, Person]; teams: [string, string];
}) {
  const batting = s.half;
  const fielding = (1 - batting) as 0 | 1;
  // the batter due up (from the engine's state), unless the half-inning just ended
  const batter = s.outs < 3 && (!e || e.kind !== "game_end") ? lineups[batting][s.next_up[batting]]?.name : undefined;
  const pitcher = s.on_mound[fielding] === 0 ? starters[fielding]?.name : `${teams[fielding]} bullpen`;
  const base = [{ x: 330, y: 250 }, { x: 250, y: 170 }, { x: 170, y: 250 }];
  const runnerName = (i: number) => {
    const slot = s.bases[i];
    return slot == null ? null : shortName(lineups[batting][slot]?.name ?? "");
  };
  const label = e?.kind === "pa" ? RESULT_LABEL[e.result] : e?.kind === "pitching_change" ? "PITCHING CHANGE" : null;
  return (
    <div className="relative w-full" style={{ perspective: "1400px" }}>
      <div style={{ transform: "rotateX(28deg)", transformOrigin: "50% 70%" }}>
        <svg viewBox="0 0 500 380" className="w-full drop-shadow-[0_30px_40px_rgba(0,0,0,0.6)]">
          <defs>
            <radialGradient id="grass" cx="50%" cy="85%" r="85%"><stop offset="0" stopColor="#2f8a4b" /><stop offset="1" stopColor="#1b5c32" /></radialGradient>
          </defs>
          <path d="M250 360 L10 120 Q250 -60 490 120 Z" fill="url(#grass)" stroke="#0e3d20" strokeWidth={4} />
          {Array.from({ length: 9 }, (_, i) => <path key={i} d={`M250 360 L${10 + i * 60} ${120 - Math.sin((i / 8) * Math.PI) * 130}`} stroke="#ffffff" strokeOpacity={0.04} strokeWidth={26} />)}
          <path d="M250 340 L360 230 Q250 110 140 230 Z" fill="#b5835a" />
          <polygon points="250,330 330,250 250,170 170,250" fill="#3a8f53" stroke="white" strokeOpacity={0.7} strokeWidth={2} />
          <line x1={250} y1={330} x2={10} y2={90} stroke="white" strokeOpacity={0.6} strokeWidth={2} />
          <line x1={250} y1={330} x2={490} y2={90} stroke="white" strokeOpacity={0.6} strokeWidth={2} />
          <circle cx={250} cy={252} r={14} fill="#b5835a" />
          <rect x={246} y={249} width={8} height={3} fill="white" />
          {/* bases */}
          {base.map((b, i) => {
            const on = s.bases[i] != null;
            return (
              <g key={i}>
                <rect x={b.x - 9} y={b.y - 9} width={18} height={18} transform={`rotate(45 ${b.x} ${b.y})`}
                  fill={on ? "#FBBF24" : "white"} stroke={on ? "#F59E0B" : "#ddd"} strokeWidth={2} style={on ? { filter: "drop-shadow(0 0 8px #FBBF24)" } : undefined} />
                {on && <g transform={`translate(${b.x} ${b.y - 24})`}><rect x={-46} y={-13} width={92} height={18} rx={4} fill="rgba(0,0,0,0.7)" />
                  <text y={0} fill="white" fontSize={11} fontWeight={600} textAnchor="middle">{runnerName(i)}</text></g>}
              </g>
            );
          })}
          <polygon points="250,336 258,330 258,324 242,324 242,330" fill="white" />
          {batter && <g transform="translate(250 360)"><rect x={-70} y={-12} width={140} height={18} rx={4} fill="rgba(0,0,0,0.7)" />
            <text y={1} fill="white" fontSize={11} fontWeight={600} textAnchor="middle">Up: {batter}</text></g>}
          <g transform="translate(250 278)"><rect x={-66} y={-10} width={132} height={16} rx={4} fill="rgba(0,0,0,0.55)" />
            <text y={2} fill="#cbd5e1" fontSize={10} textAnchor="middle">P: {pitcher}</text></g>
        </svg>
      </div>
      {label && (
        <div key={e?.seq} className={clsx("pointer-events-none absolute left-1/2 top-[18%] -translate-x-1/2 rounded-lg px-4 py-1.5 text-2xl font-black tracking-widest animate-[pulse_1s_ease-out_1]",
          e?.result === "hr" ? "bg-amber/90 text-black" : e && e.runs > 0 ? "bg-emerald/80 text-black" : "bg-black/70 text-white")}>
          {label}{e && e.runs > 0 ? ` · +${e.runs}` : ""}
        </div>
      )}
      <div className="absolute right-3 top-3 flex items-center gap-1.5 rounded-md bg-black/60 px-2 py-1 text-xs">
        <span className="text-gray-400">OUTS</span>
        {[0, 1, 2].map((i) => <span key={i} className={clsx("h-2.5 w-2.5 rounded-full", i < s.outs ? "bg-amber" : "bg-white/15")} />)}
      </div>
    </div>
  );
}

function Linescore({ s, teams, final }: { s: MlbState; teams: [string, string]; final: boolean }) {
  const n = Math.max(9, s.linescore[0].length, s.linescore[1].length);
  return (
    <div className="mx-auto w-full max-w-3xl overflow-x-auto rounded-xl border border-white/10 bg-black/70 px-4 py-2 shadow-[0_0_40px_rgba(37,99,235,0.25)]">
      <table className="w-full text-center font-mono text-sm">
        <thead><tr className="text-[11px] text-gray-500">
          <th className="text-left font-sans">{final ? <span className="text-amber">FINAL</span> : `${s.half === 0 ? "▲" : "▼"} ${ord(s.inning)}`}</th>
          {Array.from({ length: n }, (_, i) => <th key={i} className="w-7">{i + 1}</th>)}<th className="w-8 text-gray-300">R</th><th className="w-8">H</th>
        </tr></thead>
        <tbody>
          {[0, 1].map((t) => (
            <tr key={t} className="border-t border-white/5">
              <td className="text-left font-sans font-bold tracking-wider">{teams[t]}{!final && s.half === t && <span className="ml-1 text-amber">•</span>}</td>
              {Array.from({ length: n }, (_, i) => <td key={i} className="text-gray-300">{s.linescore[t][i] ?? ""}</td>)}
              <td className="text-lg font-bold">{s.score[t]}</td><td className="text-gray-400">{s.hits[t]}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function BoxPanel({ side, abbr, e, roster }: { side: 0 | 1; abbr: string; e?: MlbEvent; roster: MlbRosterSide }) {
  const bat = e?.batting_box[side] ?? [];
  const pit = e?.pitching_box[side] ?? [];
  return (
    <div className="rounded-xl border border-white/10 bg-black/55 p-3 backdrop-blur space-y-3">
      <div className="flex justify-between"><span className="text-lg font-bold">{abbr}</span><span className="text-[10px] tracking-[0.2em] text-gray-500">BOX SCORE</span></div>
      <table className="w-full text-[12px]">
        <thead><tr className="text-[10px] text-amber"><th className="text-left">BATTING</th><th>PA</th><th>H</th><th>HR</th><th>RBI</th><th>R</th><th>BB</th><th>K</th></tr></thead>
        <tbody>
          {roster.lineup.map((p, i) => {
            const r = bat[i] ?? [0, 0, 0, 0, 0, 0, 0, 0];
            return (
              <tr key={p.id} className="border-t border-white/5">
                <td className="max-w-[90px] truncate text-gray-200">{i + 1}. {shortName(p.name)}</td>
                {[0, 1, 2, 3, 4, 5, 6].map((k) => <td key={k} className={clsx("text-center font-mono", r[k] > 0 && k > 0 ? "text-gray-100" : "text-gray-500")}>{r[k]}</td>)}
              </tr>
            );
          })}
        </tbody>
      </table>
      <table className="w-full text-[12px]">
        <thead><tr className="text-[10px] text-amber"><th className="text-left">PITCHING</th><th>IP</th><th>H</th><th>R</th><th>ER</th><th>BB</th><th>K</th></tr></thead>
        <tbody>
          {[{ n: shortName(roster.starter.name), r: pit[0] }, { n: "Bullpen", r: pit[1] }].filter((x) => x.r && x.r[0] > 0).map((x) => (
            <tr key={x.n} className="border-t border-white/5">
              <td className="text-gray-200">{x.n}</td>
              <td className="text-center font-mono">{Math.floor(x.r[1] / 3)}.{x.r[1] % 3}</td>
              {[4, 6, 7, 3, 2].map((k) => <td key={k} className="text-center font-mono text-gray-300">{x.r[k]}</td>)}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function LineupRealities({ meta, draft, setDraft, applied, onRun, running }: {
  meta: MlbMeta; draft: Scenario; setDraft: (s: Scenario) => void; applied: Scenario; onRun: (s: Scenario) => void; running: boolean;
}) {
  const dirty = JSON.stringify(draft) !== JSON.stringify(applied);
  const sides: ["away", "home"] = ["away", "home"];
  return (
    <div className="space-y-3">
      <div className="text-[12px] text-gray-400">
        Lineups are MLB's official posted batting orders when available, otherwise each team's previous batting order; starters are the
        announced probables (or the team's most recent starter, flagged). Rule a batter out and the team's next most-used starter takes his
        slot; pick a different starting pitcher. Each reality re-runs 100,000 games and the broadcast.
      </div>
      <div className="grid gap-4 md:grid-cols-2 [&>*]:min-w-0">
        {sides.map((side) => {
          const r = meta.roster[side];
          const key = side === "away" ? "away_sp" : "home_sp";
          return (
            <div key={side} className="space-y-2 min-w-0">
              <div className="flex items-center gap-2"><span className="font-semibold">{meta[side]}</span><span className="truncate text-[11px] text-gray-500">{r.lineup_source}</span></div>
              <label className="flex items-center gap-2 text-[12px] text-gray-400">Starting pitcher
                <select value={draft[key] ?? ""} onChange={(ev) => setDraft({ ...draft, [key]: ev.target.value || undefined })}
                  className="min-w-0 flex-1 rounded border border-glass bg-surface px-2 py-1 text-[12px] text-gray-100">
                  <option value="">Auto — {r.starter.name} ({r.starter_source})</option>
                  {r.starter_options.filter((p) => p.id !== r.starter.id).map((p) => <option key={p.id} value={p.id}>{p.name}</option>)}
                </select>
              </label>
              <div className="rounded-md border border-glass">
                {r.lineup.map((p, i) => {
                  const out = draft.out.includes(p.id);
                  return (
                    <div key={p.id} className={clsx("flex items-center gap-2 border-t border-white/5 px-2 py-1 text-[12px] first:border-t-0", out && "opacity-50")}>
                      <span className="w-4 text-gray-500">{i + 1}</span>
                      <span className="min-w-0 flex-1 truncate text-gray-200">{p.name} <span className="text-[10px] text-gray-500">{p.pos}</span></span>
                      <button onClick={() => setDraft({ ...draft, out: out ? draft.out.filter((x) => x !== p.id) : [...draft.out, p.id] })}
                        className={clsx("rounded border px-2 py-0.5 text-[10px]", out ? "border-crimson/50 bg-crimson/25 text-crimson-light" : "border-glass text-gray-400 hover:bg-white/5")}>
                        {out ? "Out" : "Plays"}
                      </button>
                    </div>
                  );
                })}
              </div>
              {r.substitutions.length > 0 && <div className="text-[11px] text-amber">Substitutions: {r.substitutions.map((s) => `#${s.slot}: ${meta.roster[side].lineup[s.slot - 1]?.name ?? s.in} in`).join(", ")}</div>}
              <div className="text-[11px] text-gray-500">Bench (next up): {r.bench.slice(0, 5).map((p) => p.name).join(", ") || "—"}</div>
            </div>
          );
        })}
      </div>
      <div className="flex flex-wrap gap-2">
        <button onClick={() => onRun(draft)} disabled={!dirty || running} className="rounded-md border border-amber/40 bg-amber/20 px-3 py-1.5 text-sm text-amber disabled:opacity-40">
          {running ? "Simulating 100,000 games…" : "Run this reality"}
        </button>
        <button onClick={() => { setDraft(EMPTY); onRun(EMPTY); }} disabled={running || (!draft.out.length && !draft.away_sp && !draft.home_sp && !applied.out.length && !applied.away_sp && !applied.home_sp)}
          className="rounded-md border border-glass px-3 py-1.5 text-sm text-gray-300 disabled:opacity-40">Back to confirmed lineup</button>
      </div>
    </div>
  );
}

function MlbPrediction({ d }: { d?: MlbSim }) {
  if (!d) return null;
  const all = [...d.away.batters.map((b) => ({ ...b, team: d.game.away })), ...d.home.batters.map((b) => ({ ...b, team: d.game.home }))];
  const p = (b: (typeof all)[number], stat: string, line: number) => b.stats[stat]?.lines.find((l) => l.line === line)?.over ?? 0;
  const hr = [...all].sort((a, b) => p(b, "hr", 0.5) - p(a, "hr", 0.5)).slice(0, 5);
  const hits = [...all].sort((a, b) => p(b, "h", 0.5) - p(a, "h", 0.5)).slice(0, 5);
  const o85 = d.team.totals.find((l) => l.line === 8.5);
  return (
    <div className="space-y-3 text-[13px]">
      <div className="grid grid-cols-3 gap-3 text-center">
        <div><div className="text-[11px] text-gray-500">{d.game.away} win</div><div className="font-mono text-xl">{formatPct(d.team.away_win)}</div></div>
        <div><div className="text-[11px] text-gray-500">Total runs</div><div className="font-mono text-xl">{d.team.total.mean.toFixed(1)}</div>
          {o85 && <div className="text-[11px] text-gray-400">P(over 8.5) {formatPct(o85.over)}</div>}</div>
        <div><div className="text-[11px] text-gray-500">{d.game.home} win</div><div className="font-mono text-xl">{formatPct(d.team.home_win)}</div></div>
      </div>
      <div className="grid grid-cols-2 gap-4">
        <div><div className="text-[11px] font-semibold tracking-widest text-gray-400">HOME RUN</div>
          {hr.map((b) => <div key={b.player_id} className="flex justify-between"><span className="truncate">{b.name} <span className="text-gray-500 text-[11px]">{b.team}</span></span><span className="font-mono">{formatPct(p(b, "hr", 0.5))}</span></div>)}</div>
        <div><div className="text-[11px] font-semibold tracking-widest text-gray-400">1+ HIT</div>
          {hits.map((b) => <div key={b.player_id} className="flex justify-between"><span className="truncate">{b.name} <span className="text-gray-500 text-[11px]">{b.team}</span></span><span className="font-mono">{formatPct(p(b, "h", 0.5))}</span></div>)}</div>
      </div>
      <div className="text-[11px] text-gray-500">{d.simulation.valid.toLocaleString()} valid games in {(d.simulation.sim_ms / 1000).toFixed(1)}s · seed {d.simulation.seed} · {d.simulation.model_version} · validated walk-forward (see MLB Game Sim page).</div>
    </div>
  );
}

export function MlbBroadcast() {
  // deep links: ?sport=mlb&game=<gamePk>&seq=<event>&seed=<n>
  const [params] = useSearchParams();
  // the last 3 days (completed games: actual lineups) through the next week
  const slate = useQuery({ queryKey: ["mlb-slate-broadcast"], queryFn: () => {
    const from = new Date(Date.now() - 3 * 86400_000).toISOString().slice(0, 10);
    return req<Awaited<ReturnType<typeof api.mlbSlate>>>(`/mlb/slate?date=${from}&days=10`);
  } });
  const games = slate.data?.games ?? [];
  const [pk, setPk] = useState<string | null>(params.get("game"));
  useEffect(() => {
    if (pk || !games.length) return;
    const upcoming = games.find((g) => g.status !== "Final" && g.status !== "Game Over") ?? games[games.length - 1];
    setPk(String(upcoming.game_pk));
  }, [games, pk]);
  const [draft, setDraft] = useState<Scenario>(EMPTY);
  const [applied, setApplied] = useState<Scenario>(EMPTY);
  const [lineupMode, setLineupMode] = useState<LineupMode>("confirmed");
  const [session, setSession] = useState<MlbSession | null>(null);
  const [events, setEvents] = useState<MlbEvent[]>([]);
  const create = useMutation({
    mutationFn: async ({ gid, sc, lineup = "confirmed" }: { gid: string; sc: Scenario; lineup?: LineupMode }) => {
      const s = await req<MlbSession>(`/sim/sessions`, { method: "POST", body: JSON.stringify({ sport: "mlb", game_id: gid, lineup, seed: params.get("seed") ? Number(params.get("seed")) : undefined, out: sc.out, away_sp: sc.away_sp, home_sp: sc.home_sp }) });
      return { s, ev: await loadEvents(s.session_id) };
    },
    onSuccess: ({ s, ev }) => { setSession(s); setEvents(ev); },
  });
  useEffect(() => {
    if (pk) { setSession(null); setEvents([]); setDraft(EMPTY); setApplied(EMPTY); setLineupMode("confirmed"); create.mutate({ gid: pk, sc: EMPTY }); }
  }, [pk]); // eslint-disable-line react-hooks/exhaustive-deps
  const sim = useQuery({
    queryKey: ["mlb-broadcast-ensemble", pk, applied, lineupMode],
    queryFn: () => api.mlbSimulate(pk!, { n: 100000, lineup: lineupMode, out: applied.out.join(",") || undefined, away_sp: applied.away_sp, home_sp: applied.home_sp }),
    enabled: !!pk, staleTime: 10 * 60_000,
  });
  const run = (sc: Scenario) => { if (!pk) return; setApplied(sc); create.mutate({ gid: pk, sc, lineup: lineupMode }); };
  const changeLineupMode = (m: LineupMode) => { if (!pk) return; setLineupMode(m); create.mutate({ gid: pk, sc: applied, lineup: m }); };
  const pb = useSimulationPlayback(events);
  useEffect(() => {
    const q = Number(params.get("seq"));
    if (events.length && q > 0) pb.jump(q);
  }, [events]); // eslint-disable-line react-hooks/exhaustive-deps
  const meta = session?.meta;
  const teams: [string, string] = meta ? [meta.away, meta.home] : ["AWAY", "HOME"];
  const cur = pb.current;
  const st = cur?.state ?? PREGAME;
  const lineups: [Person[], Person[]] = meta ? [meta.roster.away.lineup, meta.roster.home.lineup] : [[], []];
  const starters: [Person, Person] = meta ? [meta.roster.away.starter, meta.roster.home.starter] : [{ id: "", name: "", pos: "" }, { id: "", name: "", pos: "" }];
  const final = pb.done && events.length > 0;
  const scoring = useMemo(() => events.filter((e) => e.runs > 0), [events]);

  return (
    <div className="space-y-4">
      {slate.error && <Msg tone="error">{(slate.error as Error).message}</Msg>}
      {games.length === 0 && !slate.isLoading && <Msg>No MLB games in the next 7 days.</Msg>}
      <div className="flex gap-2 overflow-x-auto pb-1">
        {games.map((g) => (
          <button key={g.game_pk} onClick={() => setPk(String(g.game_pk))}
            className={clsx("shrink-0 rounded-lg border px-3 py-2 text-left text-xs", String(g.game_pk) === pk ? "border-amber/60 bg-amber/10" : "border-glass hover:bg-white/5")}>
            <div className="text-sm font-medium">{g.away || "TBD"} @ {g.home || "TBD"}</div>
            <div className="text-gray-500">{g.date} · {g.game_type === "POST" ? "postseason" : g.status}</div>
            <div className="text-gray-400">{g.away_probable?.name ?? "TBD"} vs {g.home_probable?.name ?? "TBD"}</div>
          </button>
        ))}
      </div>
      <div className="relative overflow-hidden rounded-2xl border border-white/10 p-4 md:p-6"
        style={{ background: "radial-gradient(ellipse at 50% 0%, rgba(29,78,216,0.45), rgba(10,10,14,0.96) 55%), #07090E" }}>
        {create.isPending && <Msg>Simulating the game…</Msg>}
        {create.error && <Msg tone="error">{(create.error as Error).message}</Msg>}
        <Linescore s={st} teams={teams} final={final} />
        {meta && (
          <div className="mt-4 grid gap-4 lg:grid-cols-[260px_minmax(0,1fr)_260px] [&>*]:min-w-0">
            <div className="order-2 lg:order-1"><BoxPanel side={0} abbr={teams[0]} e={cur} roster={meta.roster.away} /></div>
            <div className="order-1 lg:order-2 space-y-3">
              <Diamond e={cur} s={st} lineups={lineups} starters={starters} teams={teams} />
              <div className={clsx("rounded-xl border bg-black/65 px-5 py-3", cur && cur.runs > 0 ? "border-amber/60" : "border-white/10")}>
                {cur ? (<>
                  <div className="text-[11px] font-semibold tracking-widest text-amber">{cur.half === 0 ? "TOP" : "BOTTOM"} {ord(cur.inning)} · {cur.outs_before} out{cur.outs_before === 1 ? "" : "s"} · {teams[cur.batting]} batting</div>
                  <div className="mt-1 text-[17px] font-medium">{cur.text}</div>
                </>) : <div className="text-sm text-gray-400">Press play to start the simulated game.</div>}
              </div>
            </div>
            <div className="order-3"><BoxPanel side={1} abbr={teams[1]} e={cur} roster={meta.roster.home} /></div>
          </div>
        )}
        <div className="mt-4 space-y-3">
          <Controls playing={pb.playing} toggle={pb.toggle} step={pb.step} restart={pb.restart} jumpEnd={() => pb.jump(pb.total)}
            speed={pb.speed} setSpeed={pb.setSpeed} onNew={() => pk && create.mutate({ gid: pk, sc: applied, lineup: lineupMode })} seq={pb.seq} total={pb.total} busy={create.isPending} />
          {events.length > 0 && (
            <div>
              <div className="relative h-4">
                {scoring.filter((e) => e.seq <= pb.seq).map((e) => (
                  <button key={e.seq} title={e.text} onClick={() => pb.jump(e.seq)} className={clsx("absolute h-3 w-1.5 -translate-x-1/2 rounded-sm", e.batting === 0 ? "bg-sky-400" : "bg-amber")}
                    style={{ left: `${(e.seq / events.length) * 100}%` }} />
                ))}
              </div>
              <input type="range" min={0} max={events.length} value={pb.seq} onChange={(ev) => pb.jump(Number(ev.target.value))} className="w-full accent-amber" />
              <div className="flex justify-between text-[11px] text-gray-500"><span>First pitch</span><span>Event {pb.seq} / {events.length}</span><span>Final</span></div>
            </div>
          )}
        </div>
        {final && meta?.actual.home_score != null && <div className="mt-3 text-sm text-gray-400">Real result: {teams[0]} {meta.actual.away_score} — {teams[1]} {meta.actual.home_score}.</div>}
      </div>

      {meta && (
        <Card>
          <div className="mb-2 text-sm font-semibold">Starters: projected vs confirmed vs actual</div>
          <StartersCompare mode={lineupMode} onMode={changeLineupMode} busy={create.isPending}
            available={{ projected: true, confirmed: true, actual: meta.roster.views.every((v) => !!v.actual?.lineup) }}
            teams={[0, 1].map((si) => {
              const v = meta.roster.views[si];
              const row = (label: string, pick: (x: MlbView) => Person | null | undefined) => ({
                label, projected: v.projected ? pick(v.projected) ?? null : null,
                confirmed: v.confirmed ? pick(v.confirmed) ?? null : null, actual: v.actual ? pick(v.actual) ?? null : undefined,
              });
              return { abbr: teams[si], rows: [row("SP", (x) => x.starter), ...Array.from({ length: 9 }, (_, i) => row(String(i + 1), (x) => x.lineup?.[i]))] };
            })} />
        </Card>
      )}

      {meta && (
        <Card>
          <div className="mb-2 text-sm font-semibold">Lineups &amp; alternate realities</div>
          <LineupRealities meta={meta} draft={draft} setDraft={setDraft} applied={applied} onRun={run} running={create.isPending || sim.isFetching} />
        </Card>
      )}

      <div className="grid gap-4 lg:grid-cols-2 [&>*]:min-w-0">
        <Card>
          <div className="mb-2 text-sm font-semibold">Play-by-play</div>
          <div className="max-h-[420px] overflow-y-auto space-y-1">
            {events.slice(0, pb.seq).reverse().map((e) => (
              <button key={e.seq} onClick={() => pb.jump(e.seq)} className={clsx("w-full rounded-md border px-3 py-1.5 text-left text-[13px]", e.seq === pb.seq ? "border-amber/50 bg-amber/10" : "border-transparent hover:bg-white/5")}>
                <div className="text-[11px] text-gray-500">{e.half === 0 ? "▲" : "▼"}{e.inning} · {teams[e.batting]} · {e.outs_before} out</div>
                <div className={clsx(e.runs > 0 ? "text-amber" : e.kind !== "pa" ? "text-gray-400" : "text-gray-200")}>{e.text}</div>
                {e.runs > 0 && <div className="font-mono text-[11px] text-gray-400">{teams[0]} {e.state.score[0]} — {teams[1]} {e.state.score[1]}</div>}
              </button>
            ))}
            {pb.seq === 0 && <div className="text-sm text-gray-500">No plate appearances yet.</div>}
          </div>
        </Card>
        <div className="space-y-4">
          <Card>
            <div className="mb-2 text-sm font-semibold">{applied.out.length || applied.away_sp || applied.home_sp ? "Alternate reality" : "Pregame prediction"} · 100,000-game ensemble</div>
            {sim.isLoading || sim.isFetching ? <Msg>Running 100,000 simulated games…</Msg> : sim.error ? <Msg tone="error">{(sim.error as Error).message}</Msg> : <MlbPrediction d={sim.data} />}
          </Card>
          {session && meta && (
            <Card>
              <div className="mb-2 text-sm font-semibold">This simulation</div>
              <div className="grid grid-cols-2 gap-x-4 gap-y-1 text-[13px] text-gray-300 [&>*]:min-w-0">
                <span className="text-gray-500">Session</span><span className="truncate font-mono">{session.session_id}</span>
                <span className="text-gray-500">Seed</span><span className="font-mono">{session.seed}</span>
                <span className="text-gray-500">Model</span><span className="font-mono">{session.model_version}</span>
                <span className="text-gray-500">Data cutoff</span><span className="font-mono">{meta.inputs.data_cutoff}</span>
                <span className="text-gray-500">Starters</span><span>{meta.roster.away.starter.name} vs {meta.roster.home.starter.name}</span>
              </div>
              <ul className="mt-3 list-disc pl-5 text-[11px] text-gray-500 space-y-0.5">
                {[...meta.inputs.flags, ...meta.inputs.assumptions].map((a) => <li key={a}>{a}</li>)}
              </ul>
            </Card>
          )}
        </div>
      </div>
    </div>
  );
}
