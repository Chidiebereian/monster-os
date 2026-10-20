import { useEffect, useMemo, useState } from "react";
import { useMutation, useQuery } from "@tanstack/react-query";
import { useSearchParams } from "react-router-dom";
import clsx from "clsx";
import { formatPct } from "../../lib/odds";
import { Card, Msg } from "../ui";
import { useSimulationPlayback } from "../../hooks/useSimulationPlayback";
import { Controls, shortName } from "./Broadcast";
import { StartersCompare, type LineupMode } from "./StartersCompare";

// NBA Game Broadcast: one seeded game, possession by possession, from
// projection-service (nba_sim). Scores, clock, who is on the floor, box scores
// and shot locations (drawn from each shooter's real ESPN shot chart) all come
// from the event log; this file only draws them.

interface RosterP { idx: number; id: string; name: string; pos: string; starter: boolean; p_active: number; active_in_this_game: boolean }
interface InputP { id: string; name: string; pos: string; p_active: number; starter: boolean; status: string; games: number; min_mean: number; scenario: string }
interface InputTeam { abbr: string; pace: number; players: InputP[]; starters: { projected: string[]; actual: string[]; used: string[] } }
interface NbaMeta {
  sport: "nba"; game_id: string; date: string; away: string; home: string; stat_keys: string[];
  roster: [RosterP[], RosterP[]];
  actual: { away_score: number | null; home_score: number | null };
  market: { spread: number | null; over_under: number | null };
  inputs: { model_version: string; params: string; data_cutoff: string; lineup_mode: LineupMode; actual_available: boolean; teams: InputTeam[]; flags: string[]; assumptions: string[] };
}
interface NbaState { period: number; clock: number; score: [number, number]; possession: 0 | 1; on_court: [number[], number[]] }
interface NbaEvent {
  seq: number; kind: string; period: number; clock: number; team: 0 | 1; player: number | null; assist: number | null; block: number | null; steal: number | null;
  value: number; made: boolean; offensive: boolean; ft_made: number; ft_att: number; x: number | null; y: number | null; points: number;
  subs_in: number[]; subs_out: number[]; state: NbaState; box_score: [number[][], number[][]]; text: string;
}
interface NbaSession { session_id: string; seed: number; model_version: string; events: number; meta: NbaMeta }
interface NbaGame { game_id: string; date: string; away: string; home: string; away_score: number | null; home_score: number | null; completed: boolean; spread: number | null; over_under: number | null; today: boolean }
interface Dist { mean: number; q10: number; q50: number; q90: number }
interface EnsP { idx: number; id: string; name: string; starter: boolean; p_active: number; stats: Record<string, Dist> }
interface Ensemble { n: number; sim_ms: number; seed: number; model_version: string; win: { home: number; away: number }; total: Dist; margin_home: { mean: number };
  market: { spread: number | null; over_under: number | null; p_home_cover: number | null; p_over: number | null }; players: { away: EnsP[]; home: EnsP[] } }
interface Scenario { out: string[]; in: string[] }
const EMPTY: Scenario = { out: [], in: [] };

async function req<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(`/api/projection-service${path}`, { ...init, headers: { accept: "application/json", ...(init?.body ? { "content-type": "application/json" } : {}) } });
  if (!res.ok) { const t = await res.text().catch(() => ""); let m = t; try { m = JSON.parse(t).error ?? t; } catch { /* text */ } throw new Error(`${res.status}: ${m.slice(0, 300)}`); }
  return res.json() as Promise<T>;
}
const S = { SEC: 0, PTS: 1, FGM: 2, FGA: 3, F3M: 4, F3A: 5, FTM: 6, FTA: 7, OREB: 8, DREB: 9, AST: 10, TOV: 11, STL: 12, BLK: 13 };
const fmt = (c: number) => `${Math.floor(c / 60)}:${String(Math.floor(c % 60)).padStart(2, "0")}`;
const per = (p: number) => (p <= 4 ? `Q${p}` : `OT${p - 4}`);

/** ESPN shot (x across 0-50 ft, y from the baseline) -> full-court coords; away shoots at the right basket. */
function toCourt(team: 0 | 1, x: number, y: number) {
  return team === 0 ? { cx: (94 - y) * 10, cy: x * 10 } : { cx: y * 10, cy: (50 - x) * 10 };
}

function Court({ e, teams, roster, history }: { e?: NbaEvent; teams: [string, string]; roster: [RosterP[], RosterP[]]; history: NbaEvent[] }) {
  const recent = history.filter((h) => h.kind === "shot" && h.x != null && h.y != null).slice(-12);
  const s = e?.state;
  const line = "#e5e7eb";
  return (
    <div className="relative w-full" style={{ perspective: "1500px" }}>
      <div style={{ transform: "rotateX(34deg)", transformOrigin: "50% 65%" }}>
        <svg viewBox="-20 -20 980 540" className="w-full drop-shadow-[0_30px_40px_rgba(0,0,0,0.6)]">
          <rect x={-20} y={-20} width={980} height={540} rx={8} fill="#2a1a0e" />
          <rect x={0} y={0} width={940} height={500} fill="#c8955b" />
          {Array.from({ length: 24 }, (_, i) => <rect key={i} x={0} y={i * 21} width={940} height={10} fill="#b98448" opacity={0.25} />)}
          <rect x={0} y={170} width={190} height={160} fill="#7c2d12" opacity={0.55} />
          <rect x={750} y={170} width={190} height={160} fill="#1e3a8a" opacity={0.55} />
          <g fill="none" stroke={line} strokeWidth={3}>
            <rect x={0} y={0} width={940} height={500} />
            <line x1={470} y1={0} x2={470} y2={500} />
            <circle cx={470} cy={250} r={60} />
            <rect x={0} y={170} width={190} height={160} /><rect x={750} y={170} width={190} height={160} />
            <circle cx={190} cy={250} r={60} /><circle cx={750} cy={250} r={60} />
            <path d="M0 30 L140 30 A237.5 237.5 0 0 1 140 470 L0 470" /><path d="M940 30 L800 30 A237.5 237.5 0 0 0 800 470 L940 470" />
            <circle cx={52} cy={250} r={8} stroke="#f97316" /><circle cx={888} cy={250} r={8} stroke="#f97316" />
            <line x1={40} y1={220} x2={40} y2={280} strokeWidth={4} /><line x1={900} y1={220} x2={900} y2={280} strokeWidth={4} />
          </g>
          <text x={95} y={255} fill="white" opacity={0.18} fontSize={48} fontWeight={900} textAnchor="middle" transform="rotate(-90 95 255)">{teams[1]}</text>
          <text x={845} y={255} fill="white" opacity={0.18} fontSize={48} fontWeight={900} textAnchor="middle" transform="rotate(90 845 255)">{teams[0]}</text>
          {/* recent shots, faded; the current one bright */}
          {recent.map((h) => {
            const { cx, cy } = toCourt(h.team, h.x!, h.y!);
            const cur = e && h.seq === e.seq;
            return h.made
              ? <circle key={h.seq} cx={cx} cy={cy} r={cur ? 14 : 8} fill={h.team === 0 ? "#38bdf8" : "#f59e0b"} opacity={cur ? 1 : 0.35} stroke="white" strokeWidth={cur ? 3 : 1} />
              : <g key={h.seq} opacity={cur ? 1 : 0.35} stroke={h.team === 0 ? "#38bdf8" : "#f59e0b"} strokeWidth={cur ? 5 : 3}>
                  <line x1={cx - 9} y1={cy - 9} x2={cx + 9} y2={cy + 9} /><line x1={cx + 9} y1={cy - 9} x2={cx - 9} y2={cy + 9} /></g>;
          })}
          {e?.kind === "shot" && e.x != null && e.y != null && (() => {
            const { cx, cy } = toCourt(e.team, e.x, e.y);
            const hoop = e.team === 0 ? { x: 888, y: 250 } : { x: 52, y: 250 };
            return <path d={`M${cx} ${cy} Q ${(cx + hoop.x) / 2} ${Math.min(cy, hoop.y) - 120} ${hoop.x} ${hoop.y}`} fill="none" stroke={e.made ? "#FBBF24" : "#e5e7eb"} strokeWidth={4} strokeDasharray={e.made ? "" : "10 8"} opacity={0.85} />;
          })()}
        </svg>
      </div>
      {s && (
        <div className="absolute inset-x-2 bottom-1 grid grid-cols-2 gap-2 text-[11px]">
          {[0, 1].map((t) => (
            <div key={t} className={clsx("rounded bg-black/60 px-2 py-1", t === 1 && "text-right")}>
              <span className="font-semibold" style={{ color: t === 0 ? "#38bdf8" : "#f59e0b" }}>{teams[t]} on court: </span>
              {s.on_court[t].map((i) => shortName(roster[t][i]?.name ?? "")).join(" · ")}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

function Box({ t, abbr, e, roster }: { t: 0 | 1; abbr: string; e?: NbaEvent; roster: RosterP[] }) {
  const rows = roster.map((p, i) => ({ p, s: e?.box_score[t][i] ?? new Array(14).fill(0) }))
    .filter((r) => r.p.active_in_this_game).sort((a, b) => b.s[S.SEC] - a.s[S.SEC]).slice(0, 11);
  return (
    <div className="rounded-xl border border-white/10 bg-black/55 p-3 backdrop-blur">
      <div className="mb-1 flex justify-between"><span className="text-lg font-bold">{abbr}</span><span className="text-[10px] tracking-[0.2em] text-gray-500">BOX SCORE</span></div>
      <table className="w-full text-[11px]">
        <thead><tr className="text-[10px] text-amber"><th className="text-left">PLAYER</th><th>MIN</th><th>PTS</th><th>REB</th><th>AST</th><th>3PM</th><th>FG</th></tr></thead>
        <tbody>{rows.map(({ p, s }) => (
          <tr key={p.id} className={clsx("border-t border-white/5", p.starter && "text-gray-100")}>
            <td className="max-w-[92px] truncate text-left">{shortName(p.name)}{p.starter ? "" : ""}</td>
            <td className="text-center font-mono text-gray-400">{Math.round(s[S.SEC] / 60)}</td>
            <td className="text-center font-mono font-semibold">{s[S.PTS]}</td>
            <td className="text-center font-mono">{s[S.OREB] + s[S.DREB]}</td>
            <td className="text-center font-mono">{s[S.AST]}</td>
            <td className="text-center font-mono">{s[S.F3M]}</td>
            <td className="text-center font-mono text-gray-400">{s[S.FGM]}-{s[S.FGA]}</td>
          </tr>))}
        </tbody>
      </table>
    </div>
  );
}

export function NbaBroadcast() {
  const [params] = useSearchParams();
  const gamesQ = useQuery({ queryKey: ["nba-sim-games"], queryFn: () => req<{ today: string; games: NbaGame[] }>(`/nba/sim/games`) });
  const games = gamesQ.data?.games ?? [];
  const [gid, setGid] = useState<string | null>(params.get("game"));
  useEffect(() => {
    if (gid || !games.length) return;
    setGid((games.find((g) => !g.completed) ?? games[games.length - 1]).game_id);
  }, [games, gid]);
  const [mode, setMode] = useState<LineupMode>("confirmed");
  const [draft, setDraft] = useState<Scenario>(EMPTY);
  const [applied, setApplied] = useState<Scenario>(EMPTY);
  const [session, setSession] = useState<NbaSession | null>(null);
  const [events, setEvents] = useState<NbaEvent[]>([]);
  const create = useMutation({
    mutationFn: async ({ id, sc, lineup }: { id: string; sc: Scenario; lineup: LineupMode }) => {
      const s = await req<NbaSession>(`/sim/sessions`, { method: "POST", body: JSON.stringify({ sport: "nba", game_id: id, lineup, out: sc.out, in: sc.in,
        seed: params.get("seed") ? Number(params.get("seed")) : undefined }) });
      const page = await req<{ events: NbaEvent[] }>(`/sim/sessions/${s.session_id}/events?after=0&limit=1000`);
      page.events.forEach((e, i) => { if (e.seq !== i + 1) throw new Error(`event stream gap at ${i + 1}`); });
      return { s, ev: page.events };
    },
    onSuccess: ({ s, ev }) => { setSession(s); setEvents(ev); },
  });
  useEffect(() => {
    if (gid) { setSession(null); setEvents([]); setMode("confirmed"); setDraft(EMPTY); setApplied(EMPTY); create.mutate({ id: gid, sc: EMPTY, lineup: "confirmed" }); }
  }, [gid]); // eslint-disable-line react-hooks/exhaustive-deps
  const ens = useQuery({
    queryKey: ["nba-ens", gid, mode, applied],
    queryFn: () => req<Ensemble>(`/nba/simulate/${gid}?n=100000&lineup=${mode}${applied.out.length ? `&out=${applied.out.join(",")}` : ""}${applied.in.length ? `&in=${applied.in.join(",")}` : ""}`),
    enabled: !!gid, staleTime: 10 * 60_000,
  });
  const pb = useSimulationPlayback(events);
  useEffect(() => { const q = Number(params.get("seq")); if (events.length && q > 0) pb.jump(q); }, [events]); // eslint-disable-line react-hooks/exhaustive-deps
  const meta = session?.meta;
  const teams: [string, string] = meta ? [meta.away, meta.home] : ["AWAY", "HOME"];
  const roster: [RosterP[], RosterP[]] = meta ? meta.roster : [[], []];
  const cur = pb.current;
  const st: NbaState = cur?.state ?? { period: 1, clock: 720, score: [0, 0], possession: 0, on_court: [[], []] };
  const final = pb.done && events.length > 0;
  const shown = useMemo(() => events.slice(0, pb.seq), [events, pb.seq]);
  const rerun = (sc: Scenario, lineup: LineupMode) => { if (!gid) return; setApplied(sc); setMode(lineup); create.mutate({ id: gid, sc, lineup }); };
  const nameById = (id: string) => meta?.inputs.teams.flatMap((t) => t.players).find((p) => p.id === id)?.name ?? id;

  return (
    <div className="space-y-4">
      {gamesQ.error && <Msg tone="error">{(gamesQ.error as Error).message}</Msg>}
      <div className="flex gap-2 overflow-x-auto pb-1">
        {games.map((g) => (
          <button key={g.game_id} onClick={() => setGid(g.game_id)}
            className={clsx("shrink-0 rounded-lg border px-3 py-2 text-left text-xs", g.game_id === gid ? "border-amber/60 bg-amber/10" : "border-glass hover:bg-white/5")}>
            <div className="text-sm font-medium">{g.away} @ {g.home}{g.today && <span className="ml-1 rounded bg-crimson/30 px-1 text-[10px] text-crimson-light">TODAY</span>}</div>
            <div className="text-gray-500">{g.date}{g.completed ? ` · final ${g.away_score}-${g.home_score}` : ""}</div>
            {g.spread != null && <div className="text-gray-400">{g.home} {g.spread > 0 ? `+${g.spread}` : g.spread} · O/U {g.over_under}</div>}
          </button>
        ))}
      </div>
      <div className="relative overflow-hidden rounded-2xl border border-white/10 p-4 md:p-6"
        style={{ background: "radial-gradient(ellipse at 50% 0%, rgba(234,88,12,0.4), rgba(10,10,14,0.96) 55%), #07090E" }}>
        {create.isPending && <Msg>Simulating the game…</Msg>}
        {create.error && <Msg tone="error">{(create.error as Error).message}</Msg>}
        <div className="mx-auto flex w-full max-w-3xl items-center justify-between gap-3 rounded-xl border border-white/10 bg-black/70 px-4 py-3 md:px-6">
          {[0, 1].map((t) => (
            <div key={t} className={clsx("flex items-center gap-3", t === 1 && "order-3 flex-row-reverse")}>
              <span className="text-lg font-bold tracking-wider md:text-xl">{teams[t]}{!final && cur && st.possession === t && <span className="ml-1 text-amber">•</span>}</span>
              <span className="font-mono text-3xl font-bold md:text-4xl">{st.score[t]}</span>
            </div>
          ))}
          <div className="order-2 flex flex-col items-center">
            <span className={clsx("text-xs font-semibold tracking-widest", final ? "text-amber" : "text-gray-400")}>{final ? "FINAL" : cur ? per(st.period) : "TIP-OFF"}</span>
            <span className="font-mono text-xl md:text-2xl">{final ? "" : fmt(st.clock)}</span>
          </div>
        </div>
        {meta && (
          <div className="mt-4 grid gap-4 lg:grid-cols-[250px_minmax(0,1fr)_250px] [&>*]:min-w-0">
            <div className="order-2 lg:order-1"><Box t={0} abbr={teams[0]} e={cur} roster={roster[0]} /></div>
            <div className="order-1 space-y-3 lg:order-2">
              <Court e={cur} teams={teams} roster={roster} history={shown} />
              <div className={clsx("rounded-xl border bg-black/65 px-5 py-3", cur && cur.points > 0 ? "border-amber/60" : "border-white/10")}>
                {cur ? (<>
                  <div className="text-[11px] font-semibold tracking-widest text-amber">{per(cur.period)} {fmt(cur.clock)} · {teams[cur.team]}{cur.points > 0 ? ` · +${cur.points}` : ""}</div>
                  <div className="mt-1 text-[17px] font-medium">{cur.text}</div>
                </>) : <div className="text-sm text-gray-400">Press play for tip-off.</div>}
              </div>
            </div>
            <div className="order-3"><Box t={1} abbr={teams[1]} e={cur} roster={roster[1]} /></div>
          </div>
        )}
        <div className="mt-4 space-y-2">
          <Controls playing={pb.playing} toggle={pb.toggle} step={pb.step} restart={pb.restart} jumpEnd={() => pb.jump(pb.total)}
            speed={pb.speed} setSpeed={pb.setSpeed} onNew={() => gid && create.mutate({ id: gid, sc: applied, lineup: mode })} seq={pb.seq} total={pb.total} busy={create.isPending} />
          {events.length > 0 && <>
            <input type="range" min={0} max={events.length} value={pb.seq} onChange={(ev) => pb.jump(Number(ev.target.value))} className="w-full accent-amber" />
            <div className="flex justify-between text-[11px] text-gray-500"><span>Tip-off</span><span>Event {pb.seq} / {events.length}</span><span>Final</span></div>
          </>}
        </div>
        {final && meta?.actual.home_score != null && <div className="mt-3 text-sm text-gray-400">Real result: {teams[0]} {meta.actual.away_score} — {teams[1]} {meta.actual.home_score}.</div>}
      </div>

      {meta && (
        <Card>
          <div className="mb-2 text-sm font-semibold">Starters: projected vs confirmed vs actual</div>
          <StartersCompare mode={mode} onMode={(m) => rerun(applied, m)} busy={create.isPending}
            available={{ projected: true, confirmed: true, actual: meta.inputs.actual_available }}
            teams={meta.inputs.teams.map((t) => {
              const person = (id?: string) => id ? { id, name: nameById(id) } : null;
              const proj = t.starters.projected; const used = t.starters.used; const act = t.starters.actual;
              return { abbr: t.abbr, rows: Array.from({ length: 5 }, (_, i) => ({
                label: `S${i + 1}`, projected: person(proj[i]), confirmed: mode === "confirmed" ? person(used[i]) : null,
                actual: meta.inputs.actual_available ? person(act[i]) : undefined })) };
            })} />
        </Card>
      )}

      {meta && (
        <Card>
          <div className="mb-2 text-sm font-semibold">Lineups &amp; alternate realities</div>
          <div className="mb-2 text-[12px] text-gray-400">Availability from the team's recent games and ESPN's injury report (upcoming games). Out / Doubtful / Questionable / Day-To-Day map to
            0 / 15% / 50% / 60% chances to play (documented priors, not fitted). Override anyone; each reality re-runs 100,000 games and the broadcast.</div>
          <div className="grid gap-4 md:grid-cols-2 [&>*]:min-w-0">
            {meta.inputs.teams.map((t) => (
              <div key={t.abbr} className="rounded-md border border-glass">
                <div className="px-2 py-1 text-[12px] font-semibold">{t.abbr} <span className="font-normal text-gray-500">pace {t.pace.toFixed(1)}</span></div>
                {[...t.players].sort((a, b) => b.min_mean - a.min_mean).map((p) => {
                  const c = draft.out.includes(p.id) ? "out" : draft.in.includes(p.id) ? "in" : "auto";
                  const set = (k: string) => setDraft({ out: [...draft.out.filter((x) => x !== p.id), ...(k === "out" ? [p.id] : [])], in: [...draft.in.filter((x) => x !== p.id), ...(k === "in" ? [p.id] : [])] });
                  return (
                    <div key={p.id} className={clsx("flex items-center gap-2 border-t border-white/5 px-2 py-1 text-[12px]", p.p_active === 0 && c === "auto" && "opacity-50")}>
                      <span className="min-w-0 flex-1 truncate" title={p.status}>{p.name} <span className="text-[10px] text-gray-500">{p.pos}{p.starter ? " · starter" : ""}</span></span>
                      <span className="w-10 text-right font-mono text-gray-500">{p.min_mean.toFixed(0)}m</span>
                      <span className={clsx("w-10 text-right font-mono", p.p_active === 0 ? "text-crimson-light" : p.p_active < 1 ? "text-amber" : "text-gray-300")}>{formatPct(p.p_active, 0)}</span>
                      <div className="flex overflow-hidden rounded border border-glass">
                        {["auto", "in", "out"].map((k) => <button key={k} onClick={() => set(k)} className={clsx("px-1.5 py-0.5 text-[10px]", c === k ? "bg-white/10 text-gray-100" : "text-gray-500")}>{k === "auto" ? "Auto" : k === "in" ? "Plays" : "Out"}</button>)}
                      </div>
                    </div>
                  );
                })}
              </div>
            ))}
          </div>
          <div className="mt-3 flex gap-2">
            <button onClick={() => rerun(draft, mode)} disabled={create.isPending || JSON.stringify(draft) === JSON.stringify(applied)}
              className="rounded-md border border-amber/40 bg-amber/20 px-3 py-1.5 text-sm text-amber disabled:opacity-40">{create.isPending ? "Simulating…" : "Run this reality"}</button>
            <button onClick={() => { setDraft(EMPTY); rerun(EMPTY, mode); }} className="rounded-md border border-glass px-3 py-1.5 text-sm text-gray-300">Back to the lineup</button>
          </div>
        </Card>
      )}

      <div className="grid gap-4 lg:grid-cols-2 [&>*]:min-w-0">
        <Card>
          <div className="mb-2 text-sm font-semibold">Play-by-play</div>
          <div className="max-h-[420px] space-y-1 overflow-y-auto">
            {shown.slice().reverse().filter((e) => e.kind !== "substitution" || e.seq === pb.seq).map((e) => (
              <button key={e.seq} onClick={() => pb.jump(e.seq)} className={clsx("w-full rounded-md border px-3 py-1.5 text-left text-[13px]", e.seq === pb.seq ? "border-amber/50 bg-amber/10" : "border-transparent hover:bg-white/5")}>
                <div className="text-[11px] text-gray-500">{per(e.period)} {fmt(e.clock)} · {teams[e.team]}</div>
                <div className={clsx(e.points > 0 ? "text-amber" : "text-gray-200")}>{e.text}</div>
                {e.points > 0 && <div className="font-mono text-[11px] text-gray-400">{teams[0]} {e.state.score[0]} — {teams[1]} {e.state.score[1]}</div>}
              </button>
            ))}
            {pb.seq === 0 && <div className="text-sm text-gray-500">No possessions yet.</div>}
          </div>
        </Card>
        <Card>
          <div className="mb-2 text-sm font-semibold">{applied.out.length || applied.in.length ? "Alternate reality" : "Prediction"} · 100,000-game ensemble ({mode} lineup)</div>
          {ens.isFetching ? <Msg>Running 100,000 simulated games…</Msg> : ens.error ? <Msg tone="error">{(ens.error as Error).message}</Msg> : ens.data && (
            <div className="space-y-3 text-[13px]">
              <div className="grid grid-cols-3 gap-3 text-center">
                <div><div className="text-[11px] text-gray-500">{teams[0]} win</div><div className="font-mono text-xl">{formatPct(ens.data.win.away)}</div></div>
                <div><div className="text-[11px] text-gray-500">Total</div><div className="font-mono text-xl">{ens.data.total.mean.toFixed(1)}</div>
                  {ens.data.market.over_under != null && <div className="text-[11px] text-gray-400">P(over {ens.data.market.over_under}) {formatPct(ens.data.market.p_over ?? 0)}</div>}</div>
                <div><div className="text-[11px] text-gray-500">{teams[1]} win</div><div className="font-mono text-xl">{formatPct(ens.data.win.home)}</div></div>
              </div>
              <div className="grid grid-cols-2 gap-4">
                {(["away", "home"] as const).map((side, t) => (
                  <div key={side}>
                    <div className="text-[11px] font-semibold tracking-widest text-gray-400">{teams[t]} · median pts / reb / ast</div>
                    {[...ens.data!.players[side]].sort((a, b) => b.stats.pts.mean - a.stats.pts.mean).slice(0, 6).map((p) => (
                      <div key={p.id} className="flex justify-between gap-2">
                        <span className="truncate">{p.name}{p.p_active < 0.999 && <span className="ml-1 text-[11px] text-amber">plays {formatPct(p.p_active, 0)}</span>}</span>
                        <span className="font-mono text-gray-400">{p.stats.pts.q50}/{p.stats.reb.q50}/{p.stats.ast.q50}</span>
                      </div>
                    ))}
                  </div>
                ))}
              </div>
              <div className="rounded-md border border-amber/30 bg-amber/5 p-2 text-[11px] text-gray-300">
                <span className="font-semibold text-amber">Experimental.</span> Walk-forward 2025-26 (149 games): the closing spread and total are better than the model; player props with projected
                lineups ≈ players' own history; with the actual (hindsight) lineup, points/assists/threes props beat it clearly — most of the edge is knowing who plays.
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
