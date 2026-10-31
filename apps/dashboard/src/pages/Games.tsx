// Games: every game on the slate (NFL by week, other sports by day) with the
// best lines across books, the champion model's score / line / total / win %,
// weather and a link into the full matchup page.
import { useEffect, useMemo, useRef, useState } from "react";
import { Link, useSearchParams } from "react-router-dom";
import { useQuery } from "@tanstack/react-query";
import clsx from "clsx";
import { ChevronLeft, ChevronRight, CloudSun, Home, Loader2, Plane, Search, Sparkles } from "lucide-react";
import { Msg } from "../components/ui";
import { useSettings } from "../store/settings";
import { HUB_SPORTS, hub, hubSportFromSettings, fmtOdds, fmtLine, bookName, type HubGame, type HubSport, type BestQuote } from "../lib/hub";

const dayLabel = (d: string) => new Date(`${d}T12:00:00`).toLocaleDateString(undefined, { weekday: "short", month: "short", day: "numeric" });
const timeLabel = (s: string | null) => (s ? new Date(s).toLocaleString(undefined, { weekday: "short", month: "short", day: "numeric", hour: "numeric", minute: "2-digit" }) : "TBD");

export function Games() {
  const settings = useSettings();
  const [params, setParams] = useSearchParams();
  const sport = (params.get("sport") as HubSport) || hubSportFromSettings(settings.sportKey);
  const week = params.get("week") ? Number(params.get("week")) : undefined;
  const date = params.get("date") ?? undefined;
  const [q, setQ] = useState("");
  const [day, setDay] = useState("all");
  const set = (o: Record<string, string | undefined>) => {
    const p = new URLSearchParams(params);
    for (const [k, v] of Object.entries(o)) { if (v == null) p.delete(k); else p.set(k, v); }
    setParams(p, { replace: true });
  };
  const sched = useQuery({ queryKey: ["hub-schedule", sport, week, date], queryFn: () => hub.schedule(sport, { week, date }),
    refetchInterval: (qq) => (qq.state.data?.games.some((g) => g.status.state === "in") ? 20_000 : 60_000) });
  const d = sched.data;
  useEffect(() => setDay("all"), [sport, week, date]);

  const days = useMemo(() => [...new Set((d?.games ?? []).map((g) => g.local_date).filter(Boolean) as string[])].sort(), [d]);
  const games = (d?.games ?? []).filter((g) => (day === "all" || g.local_date === day) &&
    (!q || [g.home.abbr, g.away.abbr, g.home.name, g.away.name].some((x) => x?.toLowerCase().includes(q.toLowerCase()))));

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center gap-3">
        <div>
          <h1 className="text-xl font-semibold">{HUB_SPORTS.find((s) => s.key === sport)?.label} Games</h1>
          <div className="text-sm text-gray-500">
            {sport === "nfl" ? "Browse the schedule by week." : "Browse the schedule by day."} Lines from every book we track, our model's score, line and win %, and a full matchup page for each game.
          </div>
        </div>
        <div className="ml-auto flex rounded-md border border-glass overflow-hidden">
          {HUB_SPORTS.map((s) => (
            <button key={s.key} onClick={() => { set({ sport: s.key, week: undefined, date: undefined }); settings.setSport(s.settingsKey); }}
              className={clsx("px-3 py-1.5 text-sm", sport === s.key ? "bg-violet/20 text-violet-light" : "text-gray-400 hover:bg-white/5")}>{s.label}</button>
          ))}
        </div>
      </div>

      {sport === "nfl" ? <WeekStrip weeks={d?.weeks ?? []} current={d?.week} onPick={(w) => set({ week: String(w) })} />
        : <DayStrip days={d?.days ?? []} current={d?.date} onPick={(x) => set({ date: x })} />}

      <div className="flex flex-wrap items-center gap-2">
        <div className="relative">
          <Search size={14} className="absolute left-2.5 top-2.5 text-gray-500" />
          <input value={q} onChange={(e) => setQ(e.target.value)} placeholder="Search teams..."
            className="w-64 bg-surface border border-glass rounded-md pl-8 pr-3 py-1.5 text-sm focus:outline-none focus:border-violet/50" />
        </div>
        {sport === "nfl" && days.length > 1 && (
          <div className="flex gap-1">
            {["all", ...days].map((x) => (
              <button key={x} onClick={() => setDay(x)}
                className={clsx("px-3 py-1.5 rounded-md border text-xs", day === x ? "border-violet/50 bg-violet/15 text-violet-light" : "border-glass text-gray-400 hover:bg-white/5")}>
                {x === "all" ? "All days" : dayLabel(x)}
              </button>
            ))}
          </div>
        )}
        {d?.byes && d.byes.length > 0 && <span className="text-xs text-gray-500">Bye: {d.byes.join(", ")}</span>}
        <span className="ml-auto text-xs text-gray-500">{sched.isFetching && <Loader2 size={12} className="inline animate-spin mr-1" />}{games.length} games</span>
      </div>

      {sched.error && <Msg tone="error">{(sched.error as Error).message}</Msg>}
      {sched.isLoading && <Msg><Loader2 size={14} className="inline animate-spin mr-2" />Loading schedule…</Msg>}
      {d && games.length === 0 && <Msg>No games {sport === "nfl" ? "this week" : "on this day"}.</Msg>}
      <div className="space-y-3">{games.map((g) => <GameCard key={g.id} sport={sport} g={g} />)}</div>
    </div>
  );
}

function WeekStrip({ weeks, current, onPick }: { weeks: NonNullable<import("../lib/hub").Schedule["weeks"]>; current?: number; onPick: (w: number) => void }) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => { ref.current?.querySelector("[data-current='1']")?.scrollIntoView({ inline: "center", block: "nearest" }); }, [current, weeks.length]);
  const idx = weeks.findIndex((w) => w.week === current);
  const today = new Date().toISOString().slice(0, 10);
  const thisWeek = weeks.find((w) => w.end >= today)?.week;
  return (
    <div className="flex items-center gap-2">
      <button disabled={idx <= 0} onClick={() => onPick(weeks[idx - 1].week)} className="p-2 rounded-md border border-glass text-gray-400 hover:bg-white/5 disabled:opacity-30"><ChevronLeft size={14} /></button>
      <div ref={ref} className="flex-1 flex gap-2 overflow-x-auto pb-1">
        {weeks.map((w) => (
          <button key={`${w.game_type}${w.week}`} data-current={w.week === current ? "1" : "0"} onClick={() => onPick(w.week)}
            className={clsx("shrink-0 rounded-md border px-4 py-2 text-center", w.week === current ? "border-violet/60 bg-violet/15" : "border-glass hover:bg-white/5")}>
            <div className="text-[9px] uppercase tracking-wider text-gray-500">{w.game_type === "REG" ? "Regular season" : "Postseason"}</div>
            <div className={clsx("text-sm font-medium", w.week === current ? "text-violet-light" : "text-gray-300")}>{w.label}</div>
          </button>
        ))}
      </div>
      <button disabled={idx < 0 || idx >= weeks.length - 1} onClick={() => onPick(weeks[idx + 1].week)} className="p-2 rounded-md border border-glass text-gray-400 hover:bg-white/5 disabled:opacity-30"><ChevronRight size={14} /></button>
      {thisWeek != null && thisWeek !== current && (
        <button onClick={() => onPick(thisWeek)} className="px-3 py-2 rounded-md border border-glass text-xs text-gray-300 hover:bg-white/5">Current week</button>
      )}
    </div>
  );
}

function DayStrip({ days, current, onPick }: { days: string[]; current?: string; onPick: (d: string) => void }) {
  const today = new Date(Date.now() - 4 * 3600_000).toISOString().slice(0, 10);
  return (
    <div className="flex items-center gap-2 overflow-x-auto pb-1">
      {days.map((x) => (
        <button key={x} onClick={() => onPick(x)}
          className={clsx("shrink-0 rounded-md border px-4 py-2 text-center", x === current ? "border-violet/60 bg-violet/15" : "border-glass hover:bg-white/5")}>
          <div className="text-[9px] uppercase tracking-wider text-gray-500">{x === today ? "Today" : new Date(`${x}T12:00:00`).toLocaleDateString(undefined, { weekday: "short" })}</div>
          <div className={clsx("text-sm font-medium", x === current ? "text-violet-light" : "text-gray-300")}>{new Date(`${x}T12:00:00`).toLocaleDateString(undefined, { month: "short", day: "numeric" })}</div>
        </button>
      ))}
    </div>
  );
}

export function TeamLine({ t, winner, big, score = true }: { t: HubGame["home"]; winner?: boolean; big?: boolean; score?: boolean }) {
  return (
    <div className="flex items-center gap-3">
      {t.logo ? <img src={t.logo} alt="" className={big ? "w-9 h-9" : "w-7 h-7"} /> : <div className="w-7 h-7 rounded-full bg-white/10" />}
      <div className="min-w-0">
        <div className="flex items-baseline gap-2">
          <span className={clsx("font-semibold", winner && "text-emerald-light")}>{t.abbr}</span>
          {t.record && <span className="text-xs text-gray-500">{t.record}</span>}
        </div>
        <div className="text-[11px] text-gray-500 truncate">{t.name}</div>
      </div>
      {score && t.score != null && <span className={clsx("ml-auto font-mono text-lg", winner ? "text-gray-100" : "text-gray-400")}>{t.score}</span>}
    </div>
  );
}

export function Quote({ label, q, fallback }: { label: string; q?: BestQuote; fallback?: { line?: number | null; price?: number | null } }) {
  const price = q?.price ?? fallback?.price ?? null;
  const line = q?.line ?? fallback?.line ?? null;
  return (
    <div className="flex items-center gap-2 rounded-md border border-glass bg-obsidian/40 px-2.5 py-2 min-w-0" title={q ? `Best of ${q.books} books: ${bookName(q.book)}` : fallback ? "ESPN (DraftKings)" : ""}>
      <span className="text-xs text-gray-300 truncate">{label}{line != null && ` ${label.match(/^[OU] ?$/) ? Math.abs(line) : fmtLine(line)}`}</span>
      <span className="ml-auto flex items-center gap-1.5 shrink-0">
        {q && <span className="text-[9px] px-1 rounded bg-white/5 text-gray-400">{bookName(q.book)}</span>}
        <span className={clsx("font-mono text-xs", price == null ? "text-gray-600" : price > 0 ? "text-violet-light" : "text-gray-200")}>{price == null ? "—" : fmtOdds(price)}</span>
      </span>
    </div>
  );
}

function weatherChip(g: HubGame) {
  if (g.venue.indoor) return <span className="inline-flex items-center gap-1 rounded-md border border-glass px-2 py-1 text-[11px] text-gray-400"><Home size={11} />Indoor</span>;
  if (g.weather?.temp != null) return <span className="inline-flex items-center gap-1 rounded-md border border-glass px-2 py-1 text-[11px] text-gray-300" title={g.weather.text ?? ""}><CloudSun size={11} />{Math.round(g.weather.temp)}°F</span>;
  return null;
}

function GameCard({ sport, g }: { sport: HubSport; g: HubGame }) {
  const m = g.model;
  const L = g.lines ?? {};
  const eo = g.espn_odds;
  const done = g.status.state === "post";
  const live = g.status.state === "in";
  const fav = m && m.win_home != null && m.win_away != null ? (m.win_home >= m.win_away ? { t: g.home.abbr, p: m.win_home } : { t: g.away.abbr, p: m.win_away }) : null;
  const winH = done && (g.home.score ?? 0) > (g.away.score ?? 0);
  const winA = done && (g.away.score ?? 0) > (g.home.score ?? 0);
  const spreadLabel = sport === "nhl" ? "Puck line" : sport === "mlb" ? "Run line" : "Spread";
  return (
    <div className="rounded-lg border border-glass bg-surface/60 overflow-hidden hover:border-violet/40 transition-colors">
      <div className="grid md:grid-cols-[minmax(0,1fr)_minmax(0,1.35fr)]">
        <div className="p-4 space-y-3 md:border-r border-glass">
          <div className="flex items-start gap-2">
            <div className="min-w-0">
              <div className="text-xs text-gray-400">{timeLabel(g.start)}{g.broadcast && ` · ${g.broadcast}`}</div>
              <div className="text-[11px] text-gray-500 truncate">{g.venue.name}{g.note && ` · ${g.note}`}</div>
            </div>
            <div className="ml-auto flex items-center gap-1.5">
              {g.neutral && <span className="inline-flex items-center gap-1 rounded-md border border-glass px-2 py-1 text-[11px] text-gray-400"><Plane size={11} />Neutral</span>}
              {live && <span className="rounded-md bg-crimson/20 text-crimson-light px-2 py-1 text-[11px]">{g.status.detail}</span>}
              {done && <span className="rounded-md bg-white/5 text-gray-400 px-2 py-1 text-[11px]">{g.status.detail}</span>}
              {!done && weatherChip(g)}
            </div>
          </div>
          <TeamLine t={g.away} winner={winA} score={g.status.state !== "pre"} />
          <TeamLine t={g.home} winner={winH} score={g.status.state !== "pre"} />
        </div>
        <div className="p-4 grid grid-cols-3 gap-2 content-start">
          <div className="text-[10px] uppercase tracking-wider text-gray-500 text-center">Moneyline</div>
          <div className="text-[10px] uppercase tracking-wider text-gray-500 text-center">{spreadLabel}</div>
          <div className="text-[10px] uppercase tracking-wider text-gray-500 text-center">Total</div>
          <Quote label={g.away.abbr} q={L.moneyline?.away} fallback={eo ? { price: eo.away_ml } : undefined} />
          <Quote label={g.away.abbr} q={L.spread?.away} fallback={eo?.spread_home != null ? { line: -eo.spread_home } : undefined} />
          <Quote label="O" q={L.total?.over} fallback={eo?.total != null ? { line: eo.total } : undefined} />
          <Quote label={g.home.abbr} q={L.moneyline?.home} fallback={eo ? { price: eo.home_ml } : undefined} />
          <Quote label={g.home.abbr} q={L.spread?.home} fallback={eo?.spread_home != null ? { line: eo.spread_home } : undefined} />
          <Quote label="U" q={L.total?.under} fallback={eo?.total != null ? { line: eo.total } : undefined} />
        </div>
      </div>
      {m && (
        <div className="grid grid-cols-2 md:grid-cols-4 border-t border-glass text-sm">
          <div className="px-4 py-3 border-r border-glass">
            <div className="text-[10px] uppercase tracking-wider text-gray-500">Model score</div>
            <div className="font-mono mt-0.5">{g.away.abbr} {m.away_score ?? "—"} <span className="text-gray-600">/</span> {g.home.abbr} {m.home_score ?? "—"}</div>
          </div>
          <div className="px-4 py-3 md:border-r border-glass">
            <div className="text-[10px] uppercase tracking-wider text-gray-500">Model line</div>
            <div className="font-mono mt-0.5">{m.spread_home == null ? "—" : m.spread_home <= 0 ? `${g.home.abbr} ${fmtLine(m.spread_home)}` : `${g.away.abbr} ${fmtLine(-m.spread_home)}`}</div>
          </div>
          <div className="px-4 py-3 border-r border-glass border-t md:border-t-0">
            <div className="text-[10px] uppercase tracking-wider text-gray-500">Model total</div>
            <div className="font-mono mt-0.5">{m.total ?? "—"}</div>
          </div>
          <div className="px-4 py-3 border-t md:border-t-0">
            <div className="text-[10px] uppercase tracking-wider text-gray-500">Projected winner</div>
            <div className="font-mono mt-0.5">{fav ? <>{fav.t} <span className="text-violet-light">{(fav.p * 100).toFixed(1)}%</span></> : "—"}</div>
          </div>
          {fav && m.win_away != null && (
            <div className="col-span-full px-4 pb-3">
              <div className="flex justify-between text-[10px] text-gray-500 mb-1"><span>{g.away.abbr} {(m.win_away * 100).toFixed(0)}%</span><span>Win probability</span><span>{g.home.abbr} {((m.win_home ?? 0) * 100).toFixed(0)}%</span></div>
              <div className="h-1.5 rounded-full bg-white/5 overflow-hidden flex">
                <div className="bg-violet/70" style={{ width: `${m.win_away * 100}%` }} />
                <div className="bg-sky-500/60 flex-1" />
              </div>
            </div>
          )}
        </div>
      )}
      <div className="flex items-center gap-3 px-4 py-2 border-t border-glass text-xs">
        <Link to={`/games/${sport}/${g.id}`} className="text-violet-light hover:underline font-medium">Full matchup</Link>
        {(g.picks ?? 0) > 0 && <span className="inline-flex items-center gap-1 text-amber"><Sparkles size={11} />{g.picks} AI pick{g.picks === 1 ? "" : "s"}</span>}
        {!m && <span className="text-gray-500">Model forecast is frozen closer to kickoff.</span>}
        {!g.lines && eo && <span className="ml-auto text-gray-600">Lines: ESPN ({eo.provider}) until we record books for this game</span>}
      </div>
    </div>
  );
}
