// Shared hit-rate UI: colored rate cells, the L5/L10/L20/season/venue/H2H
// strip, and the player + team detail panels (editable line + bar chart).
// All rates come from projection-service; the UI only flips them to the side
// being viewed (under rate = 1 - over rate over decided games).

import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import clsx from "clsx";
import { api, type HitSummary, type HitWindows, type TeamHitRates } from "../lib/api";
import { formatPct } from "../lib/odds";
import { Card, Input, Msg, Pills, Stat } from "./ui";
import { HitRateChart, type HitBar } from "./HitRateChart";

export type Side = "over" | "under";

export function sideRate(s: HitSummary | null | undefined, side: Side): number | null {
  if (!s || s.rate == null) return null;
  return side === "over" ? s.rate : 1 - s.rate;
}

function tone(r: number | null) {
  if (r == null) return "text-gray-600";
  if (r >= 0.7) return "text-emerald-light font-semibold";
  if (r >= 0.55) return "text-emerald-light/80";
  if (r <= 0.3) return "text-crimson-light font-semibold";
  if (r <= 0.45) return "text-crimson-light/80";
  return "text-gray-300";
}

/** "70% 7/10" for one window, from the viewed side. */
export function RateCell({ s, side, compact }: { s: HitSummary | null | undefined; side: Side; compact?: boolean }) {
  const r = sideRate(s, side);
  if (!s || s.games === 0) return <span className="text-gray-600">—</span>;
  const hits = side === "over" ? s.over : s.under;
  return (
    <span className="font-mono whitespace-nowrap" title={`${s.over} over · ${s.under} under · ${s.push} push`}>
      <span className={tone(r)}>{r == null ? "—" : `${Math.round(r * 100)}%`}</span>
      {!compact && <span className="text-gray-500 text-[11px] ml-1">{hits}/{s.over + s.under}</span>}
    </span>
  );
}

export function Streak({ w }: { w: HitWindows }) {
  if (!w.streak.side || w.streak.games < 2) return <span className="text-gray-600">—</span>;
  return (
    <span className={clsx("font-mono text-xs", w.streak.side === "over" ? "text-emerald-light" : "text-crimson-light")}>
      {w.streak.games} {w.streak.side === "over" ? "O" : "U"}
    </span>
  );
}

/** Window strip used in both detail panels. */
export function WindowStrip({ w, side }: { w: HitWindows; side: Side }) {
  const cells: [string, HitSummary | null][] = [
    ["L5", w.l5], ["L10", w.l10], ["L20", w.l20], [`Season${w.season_label ? ` ${w.season_label}` : ""}`, w.season],
    ["Home (L20)", w.home], ["Away (L20)", w.away], ["H2H (L10)", w.h2h], ["Career", w.career],
  ];
  return (
    <div className="grid grid-cols-4 md:grid-cols-8 gap-3">
      {cells.map(([label, s]) => (
        <div key={label}>
          <div className="text-[11px] uppercase tracking-wide text-gray-500">{label}</div>
          <div className="text-base"><RateCell s={s} side={side} /></div>
        </div>
      ))}
    </div>
  );
}

// --------------------------------------------------------------------------
// Player detail
// --------------------------------------------------------------------------

export function PlayerHitDetail({ sport, playerId, stat, line: initialLine, opponent, initialSide = "over" }: {
  sport: string; playerId: string; stat: string; line?: number; opponent?: string | null; initialSide?: Side;
}) {
  const [lineText, setLineText] = useState(initialLine != null ? String(initialLine) : "");
  const [side, setSide] = useState<Side>(initialSide);
  const [games, setGames] = useState("20");
  const line = lineText === "" ? undefined : Number(lineText);
  const { data, error, isLoading } = useQuery({
    queryKey: ["hit-player", sport, playerId, stat, line, opponent, games],
    queryFn: () => api.playerHitRates(sport, playerId, { stat, line, opponent: opponent ?? undefined, games: Number(games) || 20 }),
  });
  if (isLoading) return <Msg>Loading hit rates…</Msg>;
  if (error) return <Msg tone="error">{(error as Error).message}</Msg>;
  if (!data) return null;
  const bars: HitBar[] = [...data.log].reverse().map((g, i) => ({
    key: `${g.season}-${g.week}-${i}`, value: g.value, result: g.result,
    label: `${g.home === false ? "@" : ""}${g.opponent}`,
    title: `${g.date ?? `${g.season} wk ${g.week}`} ${g.home === false ? "@" : "vs"} ${g.opponent}: ${g.value} ${data.unit} (${g.result})`,
  }));
  const m = data.model;
  return (
    <Card className="space-y-4">
      <div className="flex items-center gap-3 flex-wrap">
        <div>
          <div className="font-medium">{data.player} <span className="text-gray-500 text-sm">{data.team} {data.position}</span></div>
          <div className="text-xs text-gray-500">{data.label} · line {data.line}{data.line_source !== "query" && ` (from ${data.line_source.replace("_", " ")})`}{data.opponent && ` · H2H vs ${data.opponent}`}</div>
        </div>
        <div className="ml-auto flex items-center gap-2 text-xs text-gray-400">
          line <Input type="number" value={lineText} onChange={setLineText} placeholder={String(data.line)} className="w-20" />
          games <Input type="number" value={games} onChange={setGames} className="w-16" />
          <Pills options={[{ key: "over", label: "Over" }, { key: "under", label: "Under" }]} value={side} onChange={(v) => setSide(v as Side)} />
        </div>
      </div>
      <WindowStrip w={data.hit_rates} side={side} />
      <HitRateChart bars={bars} line={data.line} unit={data.unit} />
      <div className="grid grid-cols-3 md:grid-cols-6 gap-4">
        <Stat label="Avg L5" value={data.averages.l5 ?? "—"} />
        <Stat label="Avg L10" value={data.averages.l10 ?? "—"} />
        <Stat label="Avg season" value={data.averages.season ?? "—"} />
        <Stat label="Streak" value={<Streak w={data.hit_rates} />} />
        <Stat label="Model mean" value={m ? m.mean : "—"} />
        <Stat label={`Model P(${side})`} value={m ? formatPct(side === "over" ? m.p_over : m.p_under) : "—"} />
      </div>
      {m && m.participation < 0.95 && (
        <div className="text-[11px] text-gray-500">
          Had the opportunity in {formatPct(m.participation, 0)} of the last 20 games; the model mixes in zeros for the rest
          (conditional mean {m.conditional_mean}).
        </div>
      )}
    </Card>
  );
}

// --------------------------------------------------------------------------
// Team detail (game lines)
// --------------------------------------------------------------------------

type TeamMarket = "cover" | "over" | "win" | "team_total_over" | "ats_closing";

export function TeamHitDetail({ sport, team, spread: s0, total: t0, opponent }: {
  sport: string; team: string; spread?: number | null; total?: number | null; opponent?: string | null;
}) {
  const [spread, setSpread] = useState(s0 != null ? String(s0) : "");
  const [total, setTotal] = useState(t0 != null ? String(t0) : "");
  const [teamTotal, setTeamTotal] = useState("");
  const [market, setMarket] = useState<TeamMarket>(s0 != null ? "cover" : "win");
  const num = (s: string) => (s === "" || Number.isNaN(Number(s)) ? undefined : Number(s));
  const { data, error, isLoading } = useQuery({
    queryKey: ["hit-team", sport, team, spread, total, teamTotal, opponent],
    queryFn: () => api.teamHitRates(sport, team, { spread: num(spread), total: num(total), team_total: num(teamTotal), opponent: opponent ?? undefined, games: 20 }),
  });
  if (isLoading) return <Msg>Loading team hit rates…</Msg>;
  if (error) return <Msg tone="error">{(error as Error).message}</Msg>;
  if (!data) return null;
  const options = [
    { key: "win", label: "Win" },
    ...(data.lines.spread != null ? [{ key: "cover", label: `Cover ${fmtSpread(data.lines.spread)}` }] : []),
    ...(data.lines.total != null ? [{ key: "over", label: `Over ${data.lines.total}` }] : []),
    ...(data.lines.team_total != null ? [{ key: "team_total_over", label: `TT o${data.lines.team_total}` }] : []),
    ...(data.markets.ats_closing ? [{ key: "ats_closing", label: "ATS (closing)" }] : []),
  ];
  const active = options.some((o) => o.key === market) ? market : "win";
  const w = data.markets[active];
  return (
    <Card className="space-y-4">
      <div className="flex items-center gap-3 flex-wrap">
        <div>
          <div className="font-medium">{data.team}{data.opponent && <span className="text-gray-500 text-sm"> vs {data.opponent}</span>}</div>
          <div className="text-xs text-gray-500">last 20 games · lines applied to past results</div>
        </div>
        <div className="ml-auto flex items-center gap-2 text-xs text-gray-400 flex-wrap">
          spread <Input type="number" value={spread} onChange={setSpread} className="w-16" />
          total <Input type="number" value={total} onChange={setTotal} className="w-20" />
          team total <Input type="number" value={teamTotal} onChange={setTeamTotal} className="w-16" />
        </div>
      </div>
      <Pills options={options} value={active} onChange={(v) => setMarket(v as TeamMarket)} />
      {w ? <WindowStrip w={w} side="over" /> : <Msg>No data for this market.</Msg>}
      <HitRateChart {...teamChart(data, active)} signed={active !== "over" && active !== "team_total_over"} />
      <div className="grid grid-cols-2 md:grid-cols-4 gap-4">
        <Stat label="Pts for L10" value={data.averages.points_for.l10 ?? "—"} />
        <Stat label="Pts against L10" value={data.averages.points_against.l10 ?? "—"} />
        <Stat label="Margin L10" value={data.averages.margin.l10 ?? "—"} />
        <Stat label="Total L10" value={data.averages.total.l10 ?? "—"} />
      </div>
      <div className="text-[11px] text-gray-500">{data.note}</div>
    </Card>
  );
}

/** Bars for the selected market: margin vs the spread, total vs the total, etc. */
function teamChart(d: TeamHitRates, market: TeamMarket): { bars: HitBar[]; line: number; unit?: string } {
  const games = [...d.log].reverse();
  const res = (v: number, line: number): HitBar["result"] => (v > line ? "over" : v < line ? "under" : "push");
  const tag = (g: TeamHitRates["log"][number]) => `${g.home ? "" : "@"}${g.opponent}`;
  const title = (g: TeamHitRates["log"][number], extra: string) =>
    `${g.date ?? g.season} ${g.home ? "vs" : "@"} ${g.opponent}: ${g.points_for}-${g.points_against}${extra}`;
  if (market === "over" && d.lines.total != null) {
    const t = d.lines.total;
    return { line: t, unit: "total", bars: games.map((g, i) => ({ key: String(i), value: g.total, result: res(g.total, t), label: tag(g), title: title(g, ` (total ${g.total})`) })) };
  }
  if (market === "team_total_over" && d.lines.team_total != null) {
    const t = d.lines.team_total;
    return { line: t, unit: "pts", bars: games.map((g, i) => ({ key: String(i), value: g.points_for, result: res(g.points_for, t), label: tag(g), title: title(g, "") })) };
  }
  if (market === "ats_closing") {
    // margin + closing spread vs 0 (bars show cover margin)
    return {
      line: 0, unit: "vs close", bars: games.filter((g) => g.closing_spread != null).map((g, i) => {
        const v = g.margin + (g.closing_spread as number);
        return { key: String(i), value: v, result: res(v, 0), label: tag(g), title: title(g, ` (closing ${fmtSpread(g.closing_spread as number)})`) };
      }),
    };
  }
  if (market === "cover" && d.lines.spread != null) {
    // cover iff margin > -spread
    const need = -d.lines.spread;
    return { line: need, unit: "margin", bars: games.map((g, i) => ({ key: String(i), value: g.margin, result: res(g.margin, need), label: tag(g), title: title(g, ` (margin ${g.margin})`) })) };
  }
  return { line: 0, unit: "margin", bars: games.map((g, i) => ({ key: String(i), value: g.margin, result: res(g.margin, 0), label: tag(g), title: title(g, "") })) };
}

export function fmtSpread(s: number | null | undefined) {
  if (s == null) return "—";
  return s > 0 ? `+${s}` : String(s);
}
