import { useEffect, useMemo, useState } from "react";
import { useMutation, useQuery } from "@tanstack/react-query";
import { useSearchParams } from "react-router-dom";
import clsx from "clsx";
import { formatPct } from "../../lib/odds";
import { Card, Msg } from "../ui";
import { useSimulationPlayback } from "../../hooks/useSimulationPlayback";
import { Controls, shortName } from "./Broadcast";
import { StartersCompare, type LineupMode } from "./StartersCompare";

// NHL Game Broadcast: one seeded game, event by event, from projection-service
// (nhl_sim). Goals, shots, penalties, power plays, goalie pulls, clock and box
// scores come from the event log; shot locations are drawn by the engine from
// each shooter's real attempts. This file only draws them.

interface RosterP { idx: number; id: number; name: string; pos: string; p_active: number; active_in_this_game: boolean }
interface InputP { id: number; name: string; pos: string; p_active: number; status: string; games: number; toi_mean: number; finish: number; scenario: string }
interface InputTeam { abbr: string; off: number[]; def: number[]; goalie: { id: number; name: string; factor: number; shots_faced: number; source: string; projected: number | null; actual: number | null }; players: InputP[] }
interface NhlMeta {
  sport: "nhl"; game_id: string; date: string; away: string; home: string; roster: [RosterP[], RosterP[]]; goalies: [string, string];
  actual: { away_score: number | null; home_score: number | null; last_period: string | null };
  inputs: { model_version: string; params: string; data_cutoff: string; lineup_mode: LineupMode; actual_available: boolean; teams: InputTeam[]; flags: string[]; assumptions: string[] };
}
interface NhlState { period: number; clock: number; score: [number, number]; shots: [number, number]; skaters: [number, number]; pulled: [boolean, boolean] }
interface NhlEvent {
  seq: number; kind: string; period: number; clock: number; team: 0 | 1; player: number | null; assist1: number | null; assist2: number | null; blocker: number | null;
  result: string; strength: string; x: number | null; y: number | null; shot_type: string | null; xg: number; empty_net: boolean; minutes: number;
  state: NhlState; box_score: [number[][], number[][]]; saves: [number, number]; text: string;
}
interface NhlSession { session_id: string; seed: number; model_version: string; events: number; meta: NhlMeta }
interface NhlGame { game_id: string; date: string; away: string; home: string; away_score: number | null; home_score: number | null; completed: boolean; today: boolean }
interface Dist { mean: number; q10: number; q50: number; q90: number; lines: { line: number; over: number }[]; pmf: number[] }
interface Ens { n: number; sim_ms: number; seed: number; model_version: string; win: { home: number; away: number; overtime: number };
  puckline: { home_minus_1_5: number; away_minus_1_5: number }; total: Dist; goalies: { team: string; name: string; saves: Dist; shots_against: Dist }[];
  players: { away: { id: number; name: string; p_active: number; stats: Record<string, Dist> }[]; home: { id: number; name: string; p_active: number; stats: Record<string, Dist> }[] } }
interface Scenario { out: string[]; in: string[] }
const EMPTY: Scenario = { out: [], in: [] };

async function req<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(`/api/projection-service${path}`, { ...init, headers: { accept: "application/json", ...(init?.body ? { "content-type": "application/json" } : {}) } });
  if (!res.ok) { const t = await res.text().catch(() => ""); let m = t; try { m = JSON.parse(t).error ?? t; } catch { /* text */ } throw new Error(`${res.status}: ${m.slice(0, 300)}`); }
  return res.json() as Promise<T>;
}
const B = { SOG: 0, G: 1, A: 2, ATT: 3, BLK: 4, PIM: 5 };
const fmt = (c: number) => `${Math.floor(c / 60)}:${String(Math.floor(c % 60)).padStart(2, "0")}`;
const per = (p: number) => (p <= 3 ? `P${p}` : p === 4 ? "OT" : p === 5 ? "SO" : `${p - 3}OT`);
const COLOR = ["#38bdf8", "#f59e0b"];

/** engine coords (attacking the net at x = +89) -> rink px; away attacks right, home attacks left. */
function toRink(team: 0 | 1, x: number, y: number) {
  const s = 4;
  return team === 0 ? { cx: (100 + x) * s, cy: (42.5 - y) * s } : { cx: (100 - x) * s, cy: (42.5 + y) * s };
}

function Rink({ e, history, teams }: { e?: NhlEvent; history: NhlEvent[]; teams: [string, string] }) {
  const shots = history.filter((h) => (h.kind === "attempt" || h.kind === "goal") && h.x != null && h.y != null).slice(-14);
  const W = 800, H = 340;
  return (
    <div className="relative w-full" style={{ perspective: "1500px" }}>
      <div style={{ transform: "rotateX(32deg)", transformOrigin: "50% 65%" }}>
        <svg viewBox={`-16 -16 ${W + 32} ${H + 32}`} className="w-full drop-shadow-[0_30px_40px_rgba(0,0,0,0.6)]">
          <rect x={-16} y={-16} width={W + 32} height={H + 32} rx={40} fill="#1f2937" />
          <rect x={0} y={0} width={W} height={H} rx={112} fill="#eef6fb" stroke="#94a3b8" strokeWidth={4} />
          <line x1={400} y1={0} x2={400} y2={H} stroke="#dc2626" strokeWidth={5} />
          <line x1={300} y1={0} x2={300} y2={H} stroke="#2563eb" strokeWidth={6} /><line x1={500} y1={0} x2={500} y2={H} stroke="#2563eb" strokeWidth={6} />
          <line x1={44} y1={14} x2={44} y2={H - 14} stroke="#dc2626" strokeWidth={2} /><line x1={756} y1={14} x2={756} y2={H - 14} stroke="#dc2626" strokeWidth={2} />
          <circle cx={400} cy={170} r={60} fill="none" stroke="#2563eb" strokeWidth={2} />
          {[[124, 82], [124, 258], [676, 82], [676, 258]].map(([x, y]) => <circle key={`${x}${y}`} cx={x} cy={y} r={60} fill="none" stroke="#dc2626" strokeWidth={2} opacity={0.7} />)}
          <path d="M44 146 A24 24 0 0 1 44 194" fill="#bfdbfe" opacity={0.7} /><path d="M756 146 A24 24 0 0 0 756 194" fill="#bfdbfe" opacity={0.7} />
          <rect x={36} y={158} width={8} height={24} fill="none" stroke="#dc2626" strokeWidth={3} /><rect x={756} y={158} width={8} height={24} fill="none" stroke="#dc2626" strokeWidth={3} />
          <text x={200} y={178} fill="#0f172a" opacity={0.12} fontSize={56} fontWeight={900} textAnchor="middle">{teams[1]}</text>
          <text x={600} y={178} fill="#0f172a" opacity={0.12} fontSize={56} fontWeight={900} textAnchor="middle">{teams[0]}</text>
          {shots.map((h) => {
            const { cx, cy } = toRink(h.team, h.x!, h.y!);
            const cur = e && h.seq === e.seq;
            const c = COLOR[h.team];
            const o = cur ? 1 : 0.4;
            if (h.kind === "goal") return <g key={h.seq} opacity={o}><circle cx={cx} cy={cy} r={cur ? 12 : 8} fill={c} stroke="#111" strokeWidth={2} /></g>;
            if (h.result === "sog") return <circle key={h.seq} cx={cx} cy={cy} r={cur ? 9 : 6} fill="none" stroke={c} strokeWidth={3} opacity={o} />;
            if (h.result === "blocked") return <rect key={h.seq} x={cx - 5} y={cy - 5} width={10} height={10} fill={c} opacity={o * 0.7} />;
            return <g key={h.seq} stroke={c} strokeWidth={3} opacity={o}><line x1={cx - 6} y1={cy - 6} x2={cx + 6} y2={cy + 6} /><line x1={cx + 6} y1={cy - 6} x2={cx - 6} y2={cy + 6} /></g>;
          })}
          {e && (e.kind === "attempt" || e.kind === "goal") && e.x != null && e.y != null && (() => {
            const { cx, cy } = toRink(e.team, e.x, e.y);
            const net = e.team === 0 ? { x: 756, y: 170 } : { x: 44, y: 170 };
            return <line x1={cx} y1={cy} x2={net.x} y2={net.y} stroke={e.kind === "goal" ? "#dc2626" : "#334155"} strokeWidth={e.kind === "goal" ? 4 : 2} strokeDasharray={e.kind === "goal" ? "" : "8 6"} opacity={0.8} />;
          })()}
        </svg>
      </div>
    </div>
  );
}

function Box({ t, abbr, e, roster, goalie }: { t: 0 | 1; abbr: string; e?: NhlEvent; roster: RosterP[]; goalie: string }) {
  const rows = roster.map((p, i) => ({ p, s: e?.box_score[t][i] ?? [0, 0, 0, 0, 0, 0] })).filter((r) => r.p.active_in_this_game)
    .sort((a, b) => (b.s[B.G] + b.s[B.A]) * 10 + b.s[B.SOG] - ((a.s[B.G] + a.s[B.A]) * 10 + a.s[B.SOG])).slice(0, 12);
  const sa = e ? e.state.shots[1 - t] : 0;
  const sv = e ? e.saves[t] : 0;
  return (
    <div className="rounded-xl border border-white/10 bg-black/55 p-3 backdrop-blur">
      <div className="mb-1 flex justify-between"><span className="text-lg font-bold">{abbr}</span><span className="text-[10px] tracking-[0.2em] text-gray-500">BOX SCORE</span></div>
      <table className="w-full text-[11px]">
        <thead><tr className="text-[10px] text-amber"><th className="text-left">SKATER</th><th>G</th><th>A</th><th>P</th><th>SOG</th><th>BLK</th><th>PIM</th></tr></thead>
        <tbody>{rows.map(({ p, s }) => (
          <tr key={p.id} className="border-t border-white/5">
            <td className="max-w-[96px] truncate">{shortName(p.name)} <span className="text-[9px] text-gray-500">{p.pos}</span></td>
            {[B.G, B.A].map((k) => <td key={k} className="text-center font-mono">{s[k]}</td>)}
            <td className="text-center font-mono font-semibold">{s[B.G] + s[B.A]}</td>
            {[B.SOG, B.BLK, B.PIM].map((k) => <td key={k} className="text-center font-mono text-gray-400">{s[k]}</td>)}
          </tr>))}
        </tbody>
      </table>
      <div className="mt-2 border-t border-white/10 pt-1 text-[11px]"><span className="text-amber">GOALIE</span> {goalie} · {sv} saves / {sa} SA{sa ? ` · ${(sv / sa).toFixed(3)}` : ""}</div>
    </div>
  );
}

export function NhlBroadcast() {
  const [params] = useSearchParams();
  const gamesQ = useQuery({ queryKey: ["nhl-sim-games"], queryFn: () => req<{ games: NhlGame[] }>(`/nhl/sim/games`) });
  const games = gamesQ.data?.games ?? [];
  const [gid, setGid] = useState<string | null>(params.get("game"));
  useEffect(() => { if (!gid && games.length) setGid((games.find((g) => !g.completed) ?? games[games.length - 1]).game_id); }, [games, gid]);
  const [mode, setMode] = useState<LineupMode>("confirmed");
  const [draft, setDraft] = useState<Scenario>(EMPTY);
  const [applied, setApplied] = useState<Scenario>(EMPTY);
  const [session, setSession] = useState<NhlSession | null>(null);
  const [events, setEvents] = useState<NhlEvent[]>([]);
  const create = useMutation({
    mutationFn: async ({ id, sc, lineup }: { id: string; sc: Scenario; lineup: LineupMode }) => {
      const s = await req<NhlSession>(`/sim/sessions`, { method: "POST", body: JSON.stringify({ sport: "nhl", game_id: id, lineup, out: sc.out, in: sc.in,
        seed: params.get("seed") ? Number(params.get("seed")) : undefined }) });
      const page = await req<{ events: NhlEvent[] }>(`/sim/sessions/${s.session_id}/events?after=0&limit=1000`);
      page.events.forEach((e, i) => { if (e.seq !== i + 1) throw new Error(`event stream gap at ${i + 1}`); });
      return { s, ev: page.events };
    },
    onSuccess: ({ s, ev }) => { setSession(s); setEvents(ev); },
  });
  useEffect(() => {
    if (gid) { setSession(null); setEvents([]); setMode("confirmed"); setDraft(EMPTY); setApplied(EMPTY); create.mutate({ id: gid, sc: EMPTY, lineup: "confirmed" }); }
  }, [gid]); // eslint-disable-line react-hooks/exhaustive-deps
  const ens = useQuery({
    queryKey: ["nhl-ens", gid, mode, applied],
    queryFn: () => req<Ens>(`/nhl/simulate/${gid}?n=100000&lineup=${mode}${applied.out.length ? `&out=${applied.out.join(",")}` : ""}${applied.in.length ? `&in=${applied.in.join(",")}` : ""}`),
    enabled: !!gid, staleTime: 10 * 60_000,
  });
  // goals get the longer "score" hold in playback
  const paced = useMemo(() => events.map((e) => ({ ...e, points: e.kind === "goal" ? 6 : 0 })), [events]);
  const pb = useSimulationPlayback(paced);
  useEffect(() => { const q = Number(params.get("seq")); if (events.length && q > 0) pb.jump(q); }, [events]); // eslint-disable-line react-hooks/exhaustive-deps
  const meta = session?.meta;
  const teams: [string, string] = meta ? [meta.away, meta.home] : ["AWAY", "HOME"];
  const cur = pb.seq > 0 ? events[pb.seq - 1] : undefined;
  const st: NhlState = cur?.state ?? { period: 1, clock: 1200, score: [0, 0], shots: [0, 0], skaters: [5, 5], pulled: [false, false] };
  const final = pb.done && events.length > 0;
  const shown = useMemo(() => events.slice(0, pb.seq), [events, pb.seq]);
  const rerun = (sc: Scenario, lineup: LineupMode) => { if (!gid) return; setApplied(sc); setMode(lineup); create.mutate({ id: gid, sc, lineup }); };
  const strengthLabel = (t: 0 | 1) => st.pulled[t] ? "EXTRA ATTACKER" : st.skaters[t] > st.skaters[1 - t] ? "POWER PLAY" : "";

  return (
    <div className="space-y-4">
      {gamesQ.error && <Msg tone="error">{(gamesQ.error as Error).message}</Msg>}
      <div className="flex gap-2 overflow-x-auto pb-1">
        {games.map((g) => (
          <button key={g.game_id} onClick={() => setGid(g.game_id)} className={clsx("shrink-0 rounded-lg border px-3 py-2 text-left text-xs", g.game_id === gid ? "border-amber/60 bg-amber/10" : "border-glass hover:bg-white/5")}>
            <div className="text-sm font-medium">{g.away} @ {g.home}{g.today && <span className="ml-1 rounded bg-crimson/30 px-1 text-[10px] text-crimson-light">TODAY</span>}</div>
            <div className="text-gray-500">{g.date}{g.completed ? ` · final ${g.away_score}-${g.home_score}` : ""}</div>
          </button>
        ))}
      </div>
      <div className="relative overflow-hidden rounded-2xl border border-white/10 p-4 md:p-6" style={{ background: "radial-gradient(ellipse at 50% 0%, rgba(14,165,233,0.35), rgba(10,10,14,0.96) 55%), #07090E" }}>
        {create.isPending && <Msg>Simulating the game…</Msg>}
        {create.error && <Msg tone="error">{(create.error as Error).message}</Msg>}
        <div className="mx-auto flex w-full max-w-3xl items-center justify-between gap-3 rounded-xl border border-white/10 bg-black/70 px-4 py-3 md:px-6">
          {[0, 1].map((t) => (
            <div key={t} className={clsx("flex items-center gap-3", t === 1 && "order-3 flex-row-reverse")}>
              <div className={clsx("flex flex-col", t === 1 && "items-end")}>
                <span className="text-lg font-bold tracking-wider md:text-xl" style={{ color: COLOR[t] }}>{teams[t]}</span>
                <span className="text-[10px] text-gray-500">SOG {st.shots[t]}{strengthLabel(t as 0 | 1) && <span className="ml-1 text-amber">{strengthLabel(t as 0 | 1)}</span>}</span>
              </div>
              <span className="font-mono text-3xl font-bold md:text-4xl">{st.score[t]}</span>
            </div>
          ))}
          <div className="order-2 flex flex-col items-center">
            <span className={clsx("text-xs font-semibold tracking-widest", final ? "text-amber" : "text-gray-400")}>{final ? `FINAL${meta && events[events.length - 1] && st.period >= 4 ? ` (${per(Math.min(st.period, 5))})` : ""}` : cur ? per(st.period) : "PUCK DROP"}</span>
            <span className="font-mono text-xl md:text-2xl">{final ? "" : fmt(st.clock)}</span>
            {!final && cur && <span className="text-[10px] text-gray-400">{st.skaters[0]}v{st.skaters[1]}</span>}
          </div>
        </div>
        {meta && (
          <div className="mt-4 grid gap-4 lg:grid-cols-[250px_minmax(0,1fr)_250px] [&>*]:min-w-0">
            <div className="order-2 lg:order-1"><Box t={0} abbr={teams[0]} e={cur} roster={meta.roster[0]} goalie={meta.goalies[0]} /></div>
            <div className="order-1 space-y-3 lg:order-2">
              <Rink e={cur} history={shown} teams={teams} />
              <div className={clsx("rounded-xl border bg-black/65 px-5 py-3", cur?.kind === "goal" ? "border-amber/60" : "border-white/10")}>
                {cur ? (<>
                  <div className="text-[11px] font-semibold tracking-widest text-amber">{per(cur.period)} {fmt(cur.clock)} · {teams[cur.team]}{cur.strength && cur.kind !== "end_period" ? ` · ${cur.strength}` : ""}{cur.xg > 0 && cur.kind !== "penalty" ? ` · xG ${cur.xg.toFixed(2)}` : ""}</div>
                  <div className="mt-1 text-[17px] font-medium">{cur.text}</div>
                </>) : <div className="text-sm text-gray-400">Press play for puck drop.</div>}
              </div>
            </div>
            <div className="order-3"><Box t={1} abbr={teams[1]} e={cur} roster={meta.roster[1]} goalie={meta.goalies[1]} /></div>
          </div>
        )}
        <div className="mt-4 space-y-2">
          <Controls playing={pb.playing} toggle={pb.toggle} step={pb.step} restart={pb.restart} jumpEnd={() => pb.jump(pb.total)}
            speed={pb.speed} setSpeed={pb.setSpeed} onNew={() => gid && create.mutate({ id: gid, sc: applied, lineup: mode })} seq={pb.seq} total={pb.total} busy={create.isPending} />
          {events.length > 0 && <>
            <div className="relative h-4">{shown.filter((e) => e.kind === "goal").map((e) => <div key={e.seq} className="absolute h-3 w-1.5 -translate-x-1/2 rounded-sm" style={{ left: `${(e.seq / events.length) * 100}%`, background: COLOR[e.team] }} />)}</div>
            <input type="range" min={0} max={events.length} value={pb.seq} onChange={(ev) => pb.jump(Number(ev.target.value))} className="w-full accent-amber" />
            <div className="flex justify-between text-[11px] text-gray-500"><span>Puck drop</span><span>Event {pb.seq} / {events.length}</span><span>Final</span></div>
          </>}
        </div>
        {final && meta?.actual.home_score != null && <div className="mt-3 text-sm text-gray-400">Real result: {teams[0]} {meta.actual.away_score} — {teams[1]} {meta.actual.home_score}{meta.actual.last_period && meta.actual.last_period !== "REG" ? ` (${meta.actual.last_period})` : ""}.</div>}
      </div>

      {meta && (
        <Card>
          <div className="mb-2 text-sm font-semibold">Goalies &amp; lineup: projected vs confirmed vs actual</div>
          <StartersCompare mode={mode} onMode={(m) => rerun(applied, m)} busy={create.isPending} available={{ projected: true, confirmed: true, actual: meta.inputs.actual_available }}
            teams={meta.inputs.teams.map((t) => {
              const nm = (id: number | null) => id == null ? null : { id: String(id), name: id === t.goalie.id ? t.goalie.name : String(id) };
              return { abbr: t.abbr, rows: [{ label: "G", projected: nm(t.goalie.projected), confirmed: mode === "confirmed" ? { id: String(t.goalie.id), name: t.goalie.name } : null,
                actual: meta.inputs.actual_available ? nm(t.goalie.actual) : undefined }] };
            })} />
          <div className="mt-3 grid gap-4 md:grid-cols-2 [&>*]:min-w-0">
            {meta.inputs.teams.map((t) => (
              <div key={t.abbr} className="rounded-md border border-glass">
                <div className="px-2 py-1 text-[12px] font-semibold">{t.abbr} <span className="font-normal text-gray-500">goalie {t.goalie.name} · goals vs expected ×{t.goalie.factor.toFixed(2)} ({t.goalie.shots_faced} shots) · {t.goalie.source}</span></div>
                {t.players.slice(0, 22).map((p) => {
                  const k = String(p.id);
                  const c = draft.out.includes(k) ? "out" : draft.in.includes(k) ? "in" : "auto";
                  const set = (v: string) => setDraft({ out: [...draft.out.filter((x) => x !== k), ...(v === "out" ? [k] : [])], in: [...draft.in.filter((x) => x !== k), ...(v === "in" ? [k] : [])] });
                  return (
                    <div key={p.id} className={clsx("flex items-center gap-2 border-t border-white/5 px-2 py-1 text-[12px]", p.p_active === 0 && c === "auto" && "opacity-50")}>
                      <span className="min-w-0 flex-1 truncate" title={p.status}>{p.name} <span className="text-[10px] text-gray-500">{p.pos}</span></span>
                      <span className="w-12 text-right font-mono text-gray-500">{p.toi_mean.toFixed(1)}m</span>
                      <span className={clsx("w-10 text-right font-mono", p.p_active === 0 ? "text-crimson-light" : p.p_active < 1 ? "text-amber" : "text-gray-300")}>{formatPct(p.p_active, 0)}</span>
                      <div className="flex overflow-hidden rounded border border-glass">
                        {["auto", "in", "out"].map((v) => <button key={v} onClick={() => set(v)} className={clsx("px-1.5 py-0.5 text-[10px]", c === v ? "bg-white/10 text-gray-100" : "text-gray-500")}>{v === "auto" ? "Auto" : v === "in" ? "Plays" : "Out"}</button>)}
                      </div>
                    </div>
                  );
                })}
              </div>
            ))}
          </div>
          <div className="mt-3 flex gap-2">
            <button onClick={() => rerun(draft, mode)} disabled={create.isPending || JSON.stringify(draft) === JSON.stringify(applied)} className="rounded-md border border-amber/40 bg-amber/20 px-3 py-1.5 text-sm text-amber disabled:opacity-40">{create.isPending ? "Simulating…" : "Run this reality"}</button>
            <button onClick={() => { setDraft(EMPTY); rerun(EMPTY, mode); }} className="rounded-md border border-glass px-3 py-1.5 text-sm text-gray-300">Back to the lineup</button>
          </div>
        </Card>
      )}

      <div className="grid gap-4 lg:grid-cols-2 [&>*]:min-w-0">
        <Card>
          <div className="mb-2 text-sm font-semibold">Play-by-play</div>
          <div className="max-h-[420px] space-y-1 overflow-y-auto">
            {shown.slice().reverse().filter((e) => e.kind !== "attempt" || e.result === "sog" || e.seq === pb.seq).map((e) => (
              <button key={e.seq} onClick={() => pb.jump(e.seq)} className={clsx("w-full rounded-md border px-3 py-1.5 text-left text-[13px]", e.seq === pb.seq ? "border-amber/50 bg-amber/10" : "border-transparent hover:bg-white/5")}>
                <div className="text-[11px] text-gray-500">{per(e.period)} {fmt(e.clock)} · {teams[e.team]}</div>
                <div className={clsx(e.kind === "goal" ? "font-semibold text-amber" : e.kind === "penalty" ? "text-crimson-light" : "text-gray-200")}>{e.text}</div>
                {e.kind === "goal" && <div className="font-mono text-[11px] text-gray-400">{teams[0]} {e.state.score[0]} — {teams[1]} {e.state.score[1]}</div>}
              </button>
            ))}
            {pb.seq === 0 && <div className="text-sm text-gray-500">No events yet.</div>}
          </div>
        </Card>
        <Card>
          <div className="mb-2 text-sm font-semibold">{applied.out.length || applied.in.length ? "Alternate reality" : "Prediction"} · 100,000-game ensemble ({mode} lineup)</div>
          {ens.isFetching ? <Msg>Running 100,000 simulated games…</Msg> : ens.error ? <Msg tone="error">{(ens.error as Error).message}</Msg> : ens.data && (
            <div className="space-y-3 text-[13px]">
              <div className="grid grid-cols-3 gap-3 text-center">
                <div><div className="text-[11px] text-gray-500">{teams[0]} win</div><div className="font-mono text-xl">{formatPct(ens.data.win.away)}</div></div>
                <div><div className="text-[11px] text-gray-500">Total goals</div><div className="font-mono text-xl">{ens.data.total.mean.toFixed(2)}</div>
                  <div className="text-[11px] text-gray-400">P(over 5.5) {formatPct(ens.data.total.lines.find((l) => l.line === 5.5)?.over ?? 0)} · OT {formatPct(ens.data.win.overtime)}</div></div>
                <div><div className="text-[11px] text-gray-500">{teams[1]} win</div><div className="font-mono text-xl">{formatPct(ens.data.win.home)}</div></div>
              </div>
              <div className="text-[12px] text-gray-400">{ens.data.goalies.map((g) => `${g.name} (${g.team}) saves ${g.saves.q50} (80%: ${g.saves.q10}–${g.saves.q90})`).join(" · ")}</div>
              <div className="grid grid-cols-2 gap-4">
                {(["away", "home"] as const).map((side, t) => (
                  <div key={side}>
                    <div className="text-[11px] font-semibold tracking-widest text-gray-400">{teams[t]} · P(1+ point) · SOG median</div>
                    {[...ens.data!.players[side]].sort((a, b) => b.stats.points.mean - a.stats.points.mean).slice(0, 6).map((p) => (
                      <div key={p.id} className="flex justify-between gap-2">
                        <span className="truncate">{p.name}{p.p_active < 0.999 && <span className="ml-1 text-[11px] text-amber">plays {formatPct(p.p_active, 0)}</span>}</span>
                        <span className="font-mono text-gray-400">{formatPct(1 - (p.stats.points.pmf[0] ?? 1))} · {p.stats.sog.q50}</span>
                      </div>
                    ))}
                  </div>
                ))}
              </div>
              <div className="rounded-md border border-amber/30 bg-amber/5 p-2 text-[11px] text-gray-300">
                <span className="font-semibold text-amber">Experimental.</span> Walk-forward 2025-26 (200 games): home win beats the league-rate baseline (−0.013 ± 0.006 Brier);
                goalie saves slightly better than goalies' own history; skater shots / points ≈ players' own history; scoring runs ~0.45 goals/game low (shooting % rose vs the training season).
              </div>
              <div className="text-[11px] text-gray-500">n = {ens.data.n.toLocaleString()} in {(ens.data.sim_ms / 1000).toFixed(1)}s · seed {ens.data.seed} · {ens.data.model_version}; props conditional on playing.</div>
            </div>
          )}
          {meta && <ul className="mt-3 list-disc space-y-0.5 pl-5 text-[11px] text-gray-500">{[...meta.inputs.flags, ...meta.inputs.assumptions].map((a) => <li key={a}>{a}</li>)}</ul>}
        </Card>
      </div>
    </div>
  );
}
