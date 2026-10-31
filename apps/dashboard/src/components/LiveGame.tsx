// Live scoreboard for one game: period-by-period score, clock and situation,
// ESPN's live win probability next to our pregame number, a live tracker of
// every prop we priced (current stat vs the line), play-by-play and the box score.
import { useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import clsx from "clsx";
import { Activity, Loader2, Radio } from "lucide-react";
import { Msg } from "./ui";
import { hub, bookName, fmtOdds, statLabel, nameKey, type GameDetail, type HubSport, type LiveGame as Live, type LivePlayer } from "../lib/hub";

const periodName = (sport: string, i: number) =>
  sport === "nba" || sport === "nfl" ? (i < 4 ? `Q${i + 1}` : i === 4 ? "OT" : `OT${i - 3}`)
    : sport === "nhl" ? (i < 3 ? `P${i + 1}` : i === 3 ? "OT" : "SO")
      : `${i + 1}`;

export function LiveGame({ d }: { d: GameDetail }) {
  const e = d.event;
  const state = e.status.state;
  const q = useQuery({
    queryKey: ["hub-live", d.sport, e.id],
    queryFn: () => hub.live(d.sport as HubSport, e.id),
    refetchInterval: (qq) => ((qq.state.data?.status.state ?? state) === "in" ? 15_000 : false),
  });
  const l = q.data;
  if (q.isLoading) return <Msg><Loader2 size={14} className="inline animate-spin mr-2" />Loading live data…</Msg>;
  if (q.error || !l) return <Msg tone="error">{(q.error as Error)?.message ?? "No live data"}</Msg>;
  if (l.status.state === "pre") return <Msg>The game hasn't started. The scoreboard, prop tracker and play-by-play switch on at tip-off / puck drop / first pitch / kickoff.</Msg>;
  return (
    <div className="space-y-4">
      <Scoreboard d={d} l={l} />
      <PropTracker d={d} l={l} />
      <div className="grid xl:grid-cols-[1fr_1.4fr] gap-4">
        <Plays d={d} l={l} />
        <BoxScore l={l} />
      </div>
    </div>
  );
}

function Scoreboard({ d, l }: { d: GameDetail; l: Live }) {
  const e = d.event;
  const away = l.teams.find((t) => t.side === "away");
  const home = l.teams.find((t) => t.side === "home");
  const n = Math.max(away?.linescores.length ?? 0, home?.linescores.length ?? 0, d.sport === "mlb" ? 9 : d.sport === "nhl" ? 3 : 4);
  const live = l.status.state === "in";
  const wpHome = l.win_prob?.home ?? null;
  const ours = e.model?.win_home ?? null;
  const s = l.situation;
  return (
    <div className="rounded-xl border border-glass bg-surface/60 overflow-hidden">
      <div className="flex items-center gap-2 px-4 py-2 border-b border-glass text-sm">
        {live ? <span className="inline-flex items-center gap-1.5 text-crimson-light font-medium"><Radio size={13} className="animate-pulse" />LIVE</span>
          : <span className="text-gray-400 font-medium">FINAL</span>}
        <span className="text-gray-300">{l.status.detail}</span>
        {s?.down_distance && <span className="text-gray-400">· {s.down_distance}{s.possession ? ` · ball: ${s.possession}` : ""}</span>}
        {d.sport === "mlb" && s && s.outs != null && live && (
          <span className="text-gray-400 inline-flex items-center gap-2">· {s.balls ?? 0}-{s.strikes ?? 0}, {s.outs} out
            <Bases first={s.on_first} second={s.on_second} third={s.on_third} /></span>)}
        <span className="ml-auto text-[11px] text-gray-500">{live ? "updates every 15 s" : ""} · {new Date(l.fetched_at).toLocaleTimeString([], { hour: "numeric", minute: "2-digit", second: "2-digit" })}</span>
      </div>
      <div className="overflow-x-auto">
        <table className="w-full text-sm">
          <thead className="text-[10px] uppercase tracking-wider text-gray-500"><tr>
            <th className="text-left px-4 py-2">Team</th>
            {Array.from({ length: n }, (_, i) => <th key={i} className="px-2 text-center w-10">{periodName(d.sport, i)}</th>)}
            <th className="px-4 text-right">{d.sport === "mlb" ? "R" : "T"}</th></tr></thead>
          <tbody>{[away, home].map((t) => t && (
            <tr key={t.side} className="border-t border-glass">
              <td className="px-4 py-2"><div className="flex items-center gap-2">{t.logo && <img src={t.logo} className="w-5 h-5" alt="" />}<span className="font-semibold">{t.abbr}</span>
                {t.possession && <span className="w-1.5 h-1.5 rounded-full bg-amber-400" title="possession" />}</div></td>
              {Array.from({ length: n }, (_, i) => <td key={i} className="px-2 text-center font-mono text-gray-300">{t.linescores[i] ?? ""}</td>)}
              <td className="px-4 text-right font-mono text-xl font-semibold">{t.score ?? 0}</td>
            </tr>))}</tbody>
        </table>
      </div>
      {(wpHome != null || ours != null) && (
        <div className="px-4 py-3 border-t border-glass grid md:grid-cols-[1fr_auto] gap-3 items-center">
          <div>
            <div className="flex justify-between text-xs mb-1">
              <span>{e.away.abbr} {wpHome != null ? `${((1 - wpHome) * 100).toFixed(1)}%` : "—"}</span>
              <span className="text-gray-500">ESPN live win probability</span>
              <span>{e.home.abbr} {wpHome != null ? `${(wpHome * 100).toFixed(1)}%` : "—"}</span>
            </div>
            {wpHome != null ? <WinChart series={l.win_prob!.series} /> : <div className="text-xs text-gray-500">ESPN doesn't publish live win probability for this sport.</div>}
          </div>
          {ours != null && <div className="text-xs text-gray-400 md:text-right">Our pregame model<br /><span className="font-mono text-gray-200">{e.home.abbr} {(ours * 100).toFixed(1)}%</span></div>}
        </div>)}
    </div>
  );
}

function Bases({ first, second, third }: { first: boolean; second: boolean; third: boolean }) {
  const b = (on: boolean, x: number, y: number) => <rect x={x} y={y} width="6" height="6" transform={`rotate(45 ${x + 3} ${y + 3})`} className={on ? "fill-amber-400" : "fill-white/15"} />;
  return <svg width="26" height="18" viewBox="0 0 26 18">{b(second, 10, 1)}{b(third, 3, 8)}{b(first, 17, 8)}</svg>;
}

function WinChart({ series }: { series: number[] }) {
  const w = 600, h = 60;
  if (series.length < 2) return null;
  const pts = series.map((v, i) => `${(i / (series.length - 1)) * w},${h - v * h}`).join(" ");
  return (
    <svg viewBox={`0 0 ${w} ${h}`} preserveAspectRatio="none" className="w-full h-14">
      <line x1="0" x2={w} y1={h / 2} y2={h / 2} stroke="rgba(255,255,255,0.15)" strokeDasharray="4 4" vectorEffect="non-scaling-stroke" />
      <polyline points={pts} fill="none" stroke="#A78BFA" strokeWidth="2" vectorEffect="non-scaling-stroke" />
    </svg>
  );
}

// ---------------------------------------------------------------------------

interface Tracked { name: string; team: string; stat: string; current: number; line: number; side: "over" | "under"; price: number | null; book: string; proj: number | null; p: number | null; minutes: number | null }

function PropTracker({ d, l }: { d: GameDetail; l: Live }) {
  const [filter, setFilter] = useState<"all" | "on" | "off">("all");
  const rows = useMemo(() => {
    const byId = new Map<string, LivePlayer[]>();
    const byName = new Map<string, LivePlayer[]>();
    for (const p of l.players) {
      if (p.id) byId.set(p.id, [...(byId.get(p.id) ?? []), p]);
      byName.set(p.key, [...(byName.get(p.key) ?? []), p]);
    }
    const out: Tracked[] = [];
    for (const side of ["away", "home"] as const) {
      for (const p of d.projections?.[side] ?? []) {
        let live = byId.get(p.id) ?? byName.get(nameKey(p.name));
        if (!live) continue;
        // MLB: starters track their pitching line, everyone else batting
        if (d.sport === "mlb") live = live.filter((x) => (p.role === "starter") === (x.group === "pitching"));
        const stats = Object.assign({}, ...live.map((x) => x.stats)) as Record<string, number>;
        for (const [k, s] of Object.entries(p.stats)) {
          const bs = (s.books ?? []).filter((b) => b.line != null && b.flag !== "implausible");
          if (!bs.length || stats[k] == null) continue;
          // the side the model leans to at the main line
          const best = bs.sort((a, b) => (b.p_model ?? 0) - (a.p_model ?? 0))[0];
          // only one side quoted and the model is below 50%: it leans the other way
          const flip = best.p_model != null && best.p_model < 0.5;
          const side = (flip ? (best.side === "over" ? "under" : "over") : best.side) as "over" | "under";
          const opp = flip ? bs.find((b) => b.side === side && b.line === best.line) : best;
          out.push({ name: p.name, team: p.team, stat: k, current: stats[k], line: best.line!, side, price: opp?.price ?? null,
            book: opp?.book ?? best.book, proj: s.mean, p: best.p_model == null ? null : flip ? 1 - best.p_model : best.p_model, minutes: stats.min ?? null });
        }
      }
    }
    return out;
  }, [d, l]);
  if (!rows.length) return null;
  const done = l.status.state === "post";
  const status = (r: Tracked) => {
    const over = r.current > r.line;
    if (r.side === "over") return over ? "hit" : done ? "miss" : "live";
    return over ? "miss" : done ? "hit" : "live";
  };
  const shown = rows.filter((r) => filter === "all" || (filter === "on" ? status(r) === "hit" : status(r) === "miss"))
    .sort((a, b) => Math.abs(a.current - a.line) / a.line - Math.abs(b.current - b.line) / b.line);
  const hits = rows.filter((r) => status(r) === "hit").length, misses = rows.filter((r) => status(r) === "miss").length;
  return (
    <div className="rounded-xl border border-glass bg-surface/60">
      <div className="flex items-center gap-2 px-4 py-3 border-b border-glass">
        <Activity size={15} className="text-violet-light" /><span className="font-semibold text-sm">Prop tracker</span>
        <span className="text-xs text-gray-500">the side our model leans at each main line · {hits} cashed · {misses} lost · {rows.length - hits - misses} live</span>
        <div className="ml-auto flex gap-1">{(["all", "on", "off"] as const).map((f) => (
          <button key={f} onClick={() => setFilter(f)} className={clsx("px-2.5 py-1 rounded-md text-xs", filter === f ? "bg-white/10 text-gray-100" : "text-gray-400 hover:bg-white/5")}>
            {f === "all" ? "All" : f === "on" ? "Cashed" : "Lost"}</button>))}</div>
      </div>
      <div className="grid sm:grid-cols-2 lg:grid-cols-3 gap-2 p-3 max-h-[420px] overflow-y-auto">
        {shown.map((r) => {
          const st = status(r);
          const pctDone = Math.min(100, (r.current / Math.max(r.line, 0.5)) * 100);
          return (
            <div key={`${r.name}-${r.stat}`} className={clsx("rounded-lg border p-3 text-xs", st === "hit" ? "border-emerald/40 bg-emerald/5" : st === "miss" ? "border-crimson/30 bg-crimson/5" : "border-glass")}>
              <div className="flex items-center gap-2">
                <span className="font-medium text-sm truncate">{r.name}</span><span className="text-gray-500">{r.team}</span>
                <span className={clsx("ml-auto font-medium", st === "hit" ? "text-emerald-light" : st === "miss" ? "text-crimson-light" : "text-gray-400")}>
                  {st === "hit" ? "Cashed" : st === "miss" ? "Lost" : r.side === "over" ? `needs ${+(Math.floor(r.line) + 1 - r.current).toFixed(1)}` : `${+(r.line - r.current).toFixed(1)} to spare`}</span>
              </div>
              <div className="flex items-baseline gap-2 mt-1">
                <span className="font-mono text-lg">{r.current}</span>
                <span className="text-gray-400">{r.side === "over" ? "O" : "U"} {r.line} {statLabel(r.stat)}</span>
                <span className="ml-auto text-gray-500">{r.price != null ? `${bookName(r.book)} ${fmtOdds(r.price)}` : "model side"}</span>
              </div>
              <div className="h-1.5 rounded bg-white/10 mt-2 overflow-hidden">
                <div className={clsx("h-full", st === "hit" ? "bg-emerald-light" : st === "miss" ? "bg-crimson-light" : r.side === "over" ? "bg-violet" : pctDone >= 80 ? "bg-amber-400" : "bg-emerald-light/60")} style={{ width: `${pctDone}%` }} />
              </div>
              <div className="flex justify-between text-[10px] text-gray-500 mt-1">
                <span>projection {r.proj?.toFixed(1) ?? "—"}{r.minutes != null ? ` · ${r.minutes} min` : ""}</span>
                <span>pregame {r.p != null ? `${(r.p * 100).toFixed(0)}%` : "—"}</span>
              </div>
            </div>);
        })}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------

function Plays({ d, l }: { d: GameDetail; l: Live }) {
  const [scoring, setScoring] = useState(false);
  const plays = l.plays.filter((p) => !scoring || p.scoring);
  const teamAbbr = (id: string | null) => l.teams.find((t) => t.id === id)?.abbr;
  return (
    <div className="rounded-xl border border-glass bg-surface/60">
      <div className="flex items-center gap-2 px-4 py-3 border-b border-glass">
        <span className="font-semibold text-sm">Play-by-play</span>
        <label className="ml-auto flex items-center gap-1.5 text-xs text-gray-400"><input type="checkbox" checked={scoring} onChange={(x) => setScoring(x.target.checked)} />Scoring only</label>
      </div>
      <div className="max-h-[560px] overflow-y-auto divide-y divide-glass/60">
        {plays.length === 0 && <div className="px-4 py-6 text-sm text-gray-500 text-center">No plays yet.</div>}
        {plays.map((p, i) => (
          <div key={`${p.id}-${i}`} className={clsx("px-4 py-2 text-sm flex gap-3", p.scoring && "bg-violet/5")}>
            <div className="w-16 shrink-0 text-[11px] text-gray-500">{p.period_label ?? (p.period != null ? periodName(d.sport, p.period - 1) : "")}<br />{p.clock ?? ""}</div>
            <div className="flex-1 min-w-0">{teamAbbr(p.team_id) && <span className="text-[10px] text-gray-500 mr-1.5">{teamAbbr(p.team_id)}</span>}
              <span className={clsx(p.scoring ? "text-gray-100 font-medium" : "text-gray-300")}>{p.text}</span></div>
            {p.away != null && <div className="shrink-0 font-mono text-xs text-gray-400">{p.away}-{p.home}</div>}
          </div>))}
      </div>
    </div>
  );
}

function BoxScore({ l }: { l: Live }) {
  const [ti, setTi] = useState(0);
  const t = l.box[ti];
  if (!t) return <Msg>No box score yet.</Msg>;
  return (
    <div className="rounded-xl border border-glass bg-surface/60 overflow-hidden">
      <div className="flex items-center gap-1 px-3 py-2 border-b border-glass">
        <span className="font-semibold text-sm mr-2">Box score</span>
        {l.box.map((b, i) => (
          <button key={b.team} onClick={() => setTi(i)} className={clsx("inline-flex items-center gap-1.5 px-2.5 py-1 rounded-md text-xs", i === ti ? "bg-white/10 text-gray-100" : "text-gray-400 hover:bg-white/5")}>
            {b.logo && <img src={b.logo} className="w-4 h-4" alt="" />}{b.team}</button>))}
      </div>
      <div className="max-h-[560px] overflow-auto">
        {t.groups.map((g) => (
          <table key={g.name} className="w-full text-xs">
            <thead className="text-[10px] uppercase tracking-wider text-gray-500 bg-surface sticky top-0"><tr>
              <th className="text-left px-3 py-1.5 min-w-[140px]">{g.title ?? g.name}</th>{g.labels.map((x, i) => <th key={i} className="px-1.5 text-right">{x}</th>)}</tr></thead>
            <tbody>
              {g.rows.map((r, i) => (
                <tr key={`${r.id}-${i}`} className={clsx("border-t border-glass/50", r.dnp && "text-gray-600")}>
                  <td className="px-3 py-1"><span className={clsx(r.starter && "font-medium")}>{r.short ?? r.name}</span> <span className="text-gray-500">{r.pos}</span></td>
                  {r.dnp ? <td colSpan={g.labels.length} className="px-1.5 text-gray-600">{r.reason ?? "DNP"}</td>
                    : r.stats.map((v, j) => <td key={j} className="px-1.5 text-right font-mono">{v}</td>)}
                </tr>))}
              {g.totals && g.totals.some((x) => x) && (
                <tr className="border-t border-glass font-medium"><td className="px-3 py-1">Team</td>{g.totals.map((v, j) => <td key={j} className="px-1.5 text-right font-mono">{v}</td>)}</tr>)}
            </tbody>
          </table>))}
      </div>
    </div>
  );
}
