import clsx from "clsx";
import { ChevronLeft, ChevronRight, Pause, Play, RotateCcw, SkipForward, Shuffle } from "lucide-react";
import type { SimEvent, SimPlayer, SimSnapshot } from "../../lib/sim";
import { downText, fmtClock, qtrLabel, spotText, teamAccent, teamColor } from "../../lib/sim";
import { SPEEDS } from "../../hooks/useSimulationPlayback";

// Broadcast chrome: scoreboard, team box scores, latest play, play-by-play,
// controls and timeline. Every number shown is read from the event log's
// authoritative state; nothing here computes an outcome.

export const PREGAME: SimSnapshot = {
  qtr: 1, clock: 900, score: [0, 0], possession: 0, down: 0, togo: 0, yardline_100: 65, ball_x: 50, first_down_x: null,
};

export function Scoreboard({ s, teams, live, final }: { s: SimSnapshot; teams: [string, string]; live: boolean; final: boolean }) {
  const side = (i: 0 | 1) => (
    <div className={clsx("flex items-center gap-2 md:gap-3", i === 1 && "flex-row-reverse")}>
      <div className="h-10 w-1.5 shrink-0 rounded" style={{ background: teamColor(teams[i]) }} />
      <div className={clsx("flex flex-col", i === 1 && "items-end")}>
        <span className="hidden sm:block text-xs tracking-[0.25em] text-gray-400">{i === 0 ? "AWAY" : "HOME"}</span>
        <span className="text-base md:text-xl font-bold tracking-wider" style={{ color: teamAccent(teams[i]) === "#000000" ? "#fff" : undefined }}>
          {teams[i]} {s.possession === i && !final && <span className="ml-1 inline-block h-2 w-2 rounded-full bg-amber align-middle" title="possession" />}
        </span>
      </div>
      <span className="font-mono text-2xl md:text-4xl font-bold tabular-nums md:w-14 text-center">{s.score[i]}</span>
    </div>
  );
  return (
    <div className="mx-auto flex w-full max-w-3xl items-center justify-between gap-2 md:gap-6 rounded-xl border border-white/10 bg-black/70 px-3 md:px-6 py-3 shadow-[0_0_40px_rgba(220,38,38,0.25)] backdrop-blur">
      {side(0)}
      <div className="flex flex-col items-center">
        <span className={clsx("text-xs font-semibold tracking-widest", final ? "text-amber" : "text-gray-400")}>
          {final ? "FINAL" : live ? qtrLabel(s.qtr) : "PREGAME"}
        </span>
        <span className="font-mono text-lg md:text-2xl tabular-nums">{final ? "" : fmtClock(s.clock)}</span>
        <span className="text-xs text-amber">{!final && s.down >= 1 ? downText(s) : " "}</span>
      </div>
      {side(1)}
    </div>
  );
}

type Line = { name: string; vals: string };
/** "Jayden Daniels" -> "J. Daniels" (box scores are narrow) */
export const shortName = (n: string) => {
  const parts = n.trim().split(/\s+/);
  return parts.length < 2 ? n : `${parts[0][0]}. ${parts.slice(1).join(" ")}`;
};
function teamBox(box: [number, number[]][], players: SimPlayer[], team: 0 | 1, keys: string[]) {
  const k = (n: string) => keys.indexOf(n);
  const rows = box.filter(([i]) => players[i]?.team === team).map(([i, s]) => ({ p: players[i], s }));
  const passing: Line[] = rows.filter((r) => r.s[k("pass_att")] > 0).sort((a, b) => b.s[k("pass_att")] - a.s[k("pass_att")]).slice(0, 1)
    .map((r) => ({ name: shortName(r.p.name), vals: `${r.s[k("completions")]}/${r.s[k("pass_att")]} · ${r.s[k("pass_yds")]} YDS${r.s[k("pass_td")] ? ` · ${r.s[k("pass_td")]} TD` : ""}${r.s[k("interceptions")] ? ` · ${r.s[k("interceptions")]} INT` : ""}` }));
  const rushing: Line[] = rows.filter((r) => r.s[k("carries")] > 0).sort((a, b) => b.s[k("rush_yds")] - a.s[k("rush_yds")]).slice(0, 3)
    .map((r) => ({ name: shortName(r.p.name), vals: `${r.s[k("carries")]}-${r.s[k("rush_yds")]}${r.s[k("rush_td")] ? ` · ${r.s[k("rush_td")]} TD` : ""}` }));
  const receiving: Line[] = rows.filter((r) => r.s[k("receptions")] > 0).sort((a, b) => b.s[k("rec_yds")] - a.s[k("rec_yds")]).slice(0, 4)
    .map((r) => ({ name: shortName(r.p.name), vals: `${r.s[k("receptions")]}-${r.s[k("rec_yds")]}${r.s[k("rec_td")] ? ` · ${r.s[k("rec_td")]} TD` : ""}` }));
  const kicking: Line[] = rows.filter((r) => r.s[k("fga")] > 0 || r.s[k("xpa")] > 0)
    .map((r) => ({ name: shortName(r.p.name), vals: `FG ${r.s[k("fgm")]}/${r.s[k("fga")]} · XP ${r.s[k("xpm")]}/${r.s[k("xpa")]}` }));
  return { passing, rushing, receiving, kicking };
}

export function BoxScorePanel({ team, abbr, box, players, keys }: {
  team: 0 | 1; abbr: string; box: [number, number[]][]; players: SimPlayer[]; keys: string[];
}) {
  const b = teamBox(box, players, team, keys);
  const section = (label: string, lines: Line[]) => (
    <div className="space-y-1">
      <div className="text-[11px] font-semibold tracking-[0.2em] text-amber">{label}</div>
      {lines.length === 0 && <div className="text-xs text-gray-600">—</div>}
      {lines.map((l) => (
        <div key={l.name} className="flex flex-wrap items-baseline justify-between gap-x-2 border-b border-white/5 pb-1 text-[13px]">
          <span className="min-w-0 truncate text-gray-200">{l.name}</span>
          <span className="font-mono text-gray-300">{l.vals}</span>
        </div>
      ))}
    </div>
  );
  return (
    <div className="rounded-xl border border-white/10 bg-black/55 p-4 backdrop-blur space-y-4" style={{ borderTop: `3px solid ${teamColor(abbr)}` }}>
      <div className="flex items-center justify-between">
        <span className="text-lg font-bold tracking-wider">{abbr}</span>
        <span className="text-[10px] tracking-[0.2em] text-gray-500">BOX SCORE</span>
      </div>
      {section("PASSING", b.passing)}
      {section("RUSHING", b.rushing)}
      {section("RECEIVING", b.receiving)}
      {b.kicking.length > 0 && section("KICKING", b.kicking)}
    </div>
  );
}

export function LatestPlay({ e, teams }: { e?: SimEvent; teams: [string, string] }) {
  if (!e) return (
    <div className="rounded-xl border border-white/10 bg-black/60 px-5 py-3 text-sm text-gray-400">Press play to start the simulated game.</div>
  );
  const pre = { down: e.down, togo: e.togo, yardline_100: e.yardline_100, possession: e.offense };
  const scoring = e.points > 0;
  return (
    <div className={clsx("rounded-xl border bg-black/65 px-5 py-3 backdrop-blur", scoring ? "border-amber/60" : "border-white/10")}>
      <div className="flex items-center gap-3 text-[11px] font-semibold tracking-widest text-amber">
        <span>{qtrLabel(e.qtr)} {fmtClock(e.clock_before)}</span>
        {e.down >= 1 && <span>{downText(pre)} · {spotText(pre, teams)}</span>}
        <span className="text-gray-500">{teams[e.offense]} ball</span>
        {scoring && e.scoring_team !== null && <span className="ml-auto rounded bg-amber/20 px-2 py-0.5 text-amber">+{e.points} {teams[e.scoring_team]}</span>}
      </div>
      <div className="mt-1 text-[17px] font-medium text-gray-100">{e.text}</div>
    </div>
  );
}

export function Controls({ playing, toggle, step, restart, jumpEnd, speed, setSpeed, onNew, seq, total, busy }: {
  playing: boolean; toggle: () => void; step: (d: number) => void; restart: () => void; jumpEnd: () => void;
  speed: number; setSpeed: (s: number) => void; onNew: () => void; seq: number; total: number; busy: boolean;
}) {
  const btn = "flex items-center gap-1.5 rounded-md border border-white/10 bg-white/5 px-3 py-1.5 text-sm hover:bg-white/10 disabled:opacity-40";
  return (
    <div className="flex flex-wrap items-center gap-2 min-w-0">
      <button className={clsx(btn, "bg-amber/20 border-amber/40 text-amber")} onClick={toggle} disabled={!total}>
        {playing ? <Pause size={15} /> : <Play size={15} />}{playing ? "Pause" : seq >= total && total ? "Replay" : "Play"}
      </button>
      <button className={btn} onClick={() => step(-1)} disabled={seq <= 0} title="Previous play"><ChevronLeft size={15} /></button>
      <button className={btn} onClick={() => step(1)} disabled={seq >= total} title="Next play"><ChevronRight size={15} />Next play</button>
      <button className={btn} onClick={restart} disabled={!total} title="Replay from kickoff"><RotateCcw size={15} />Replay</button>
      <button className={btn} onClick={jumpEnd} disabled={!total} title="Skip to final"><SkipForward size={15} /></button>
      <div className="flex overflow-hidden rounded-md border border-white/10">
        {SPEEDS.map((s) => (
          <button key={s} onClick={() => setSpeed(s)} className={clsx("px-2 py-1.5 text-xs", s === speed ? "bg-amber/25 text-amber" : "text-gray-400 hover:bg-white/5")}>{s}x</button>
        ))}
      </div>
      <button className={clsx(btn, "ml-auto")} onClick={onNew} disabled={busy} title="Simulate this game again with a new seed">
        <Shuffle size={15} />{busy ? "Simulating…" : "New simulation"}
      </button>
    </div>
  );
}

export function Timeline({ events, seq, jump, teams }: { events: SimEvent[]; seq: number; jump: (s: number) => void; teams: [string, string] }) {
  const n = events.length;
  if (!n) return null;
  return (
    <div className="space-y-1">
      <div className="relative h-5">
        {/* only plays already shown: no spoilers */}
        {events.slice(0, seq).map((e) => (e.points > 0 && e.scoring_team !== null) && (
          <button key={e.seq} title={e.text} onClick={() => jump(e.seq)}
            className="absolute top-0 h-3 w-1.5 -translate-x-1/2 rounded-sm"
            style={{ left: `${(e.seq / n) * 100}%`, background: teamColor(teams[e.scoring_team]), boxShadow: `0 0 0 1px ${teamAccent(teams[e.scoring_team])}` }} />
        ))}
        {events.slice(0, seq).filter((e) => e.kind === "end_period").map((e) => (
          <div key={`q${e.seq}`} className="absolute top-0 h-5 w-px bg-white/25" style={{ left: `${(e.seq / n) * 100}%` }} />
        ))}
      </div>
      <input type="range" min={0} max={n} value={seq} onChange={(ev) => jump(Number(ev.target.value))} className="w-full accent-amber" />
      <div className="flex justify-between text-[11px] text-gray-500"><span>Kickoff</span><span>Play {seq} / {n}</span><span>Final</span></div>
    </div>
  );
}

export function PlayByPlay({ events, seq, jump, teams }: { events: SimEvent[]; seq: number; jump: (s: number) => void; teams: [string, string] }) {
  const shown = events.slice(0, seq).reverse();
  return (
    <div className="max-h-[420px] overflow-y-auto pr-1 space-y-1">
      {shown.length === 0 && <div className="text-sm text-gray-500">No plays yet.</div>}
      {shown.map((e) => (
        <button key={e.seq} onClick={() => jump(e.seq)}
          className={clsx("w-full text-left rounded-md px-3 py-2 text-[13px] border",
            e.seq === seq ? "border-amber/50 bg-amber/10" : "border-transparent hover:bg-white/5",
            e.points > 0 && "font-medium")}>
          <div className="flex items-center gap-2 text-[11px] text-gray-500">
            <span className="font-mono">{qtrLabel(e.qtr)} {fmtClock(e.clock_before)}</span>
            {e.down >= 1 && <span>{downText({ down: e.down, togo: e.togo, yardline_100: e.yardline_100 })} · {spotText({ yardline_100: e.yardline_100, possession: e.offense }, teams)}</span>}
            <span className="ml-auto inline-block h-2 w-2 rounded-full" style={{ background: teamColor(teams[e.offense]) }} />
          </div>
          <div className={clsx(e.points > 0 ? "text-amber" : e.turnover ? "text-crimson-light" : "text-gray-200")}>{e.text}</div>
          {e.points > 0 && <div className="text-[11px] text-gray-400 font-mono">{teams[0]} {e.state.score[0]} — {teams[1]} {e.state.score[1]}</div>}
        </button>
      ))}
    </div>
  );
}
