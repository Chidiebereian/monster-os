// Full matchup page for one game: header + best lines, then Overview, Rankings,
// Projections, Picks, Matchups (NFL), Lines, Lineups & injuries, Coaching,
// Situations, Team stats and Weather tabs. All numbers come from
// projection-service /hub/{sport}/game/{id}.
import { Fragment, useMemo, useState, type ReactNode } from "react";
import { Link, useNavigate, useParams, useSearchParams } from "react-router-dom";
import { useQuery } from "@tanstack/react-query";
import clsx from "clsx";
import { ArrowLeft, Bot, CloudSun, ExternalLink, Home, Info, Loader2, Search, Wind } from "lucide-react";
import { Msg } from "../components/ui";
import { Quote } from "./Games";
import {
  hub, fmtOdds, fmtLine, fmtMetric, fmtVsLeague, bookName, usableLink, statLabel,
  type GameDetail, type HubSport, type MetricVal, type HasBall, type PlayerMatchup, type ProjPlayer, type Ranking, type MatchupGroup,
} from "../lib/hub";

const pct = (x: number | null | undefined, d = 1) => (x == null ? "—" : `${(x * 100).toFixed(d)}%`);
const signed = (x: number | null | undefined, d = 1) => (x == null ? "—" : `${x >= 0 ? "+" : ""}${(x * 100).toFixed(d)}%`);

export function GameMatchup() {
  const { sport = "nfl", id = "" } = useParams<{ sport: HubSport; id: string }>();
  const [params, setParams] = useSearchParams();
  const nav = useNavigate();
  const q = useQuery({ queryKey: ["hub-game", sport, id], queryFn: () => hub.game(sport as HubSport, id), refetchInterval: 120_000 });
  const d = q.data;
  const nfl = sport === "nfl";
  const tabs = useMemo(() => [
    { key: "overview", label: "Overview" },
    { key: "rankings", label: "Rankings" },
    { key: "projections", label: "Projections" },
    { key: "picks", label: `Picks${d?.picks.length ? ` ${d.picks.length}` : ""}` },
    ...(nfl || d?.analysis?.player_matchups?.length ? [{ key: "matchups", label: "Matchups" }] : []),
    { key: "report", label: nfl ? "Weekly report" : "Game report" },
    { key: "lines", label: "Lines" },
    { key: "injuries", label: "Lineups & injuries" },
    ...(nfl ? [{ key: "coaching", label: "Coaching" }, { key: "situations", label: "Situations" }] : []),
    { key: "teamstats", label: "Team stats" },
    { key: "weather", label: "Weather" },
  ], [nfl, d?.picks.length, d?.analysis?.player_matchups?.length]);
  const tab = params.get("tab") ?? "overview";
  const setTab = (t: string, extra: Record<string, string> = {}) => { const p = new URLSearchParams(params); p.set("tab", t); for (const [k, v] of Object.entries(extra)) p.set(k, v); setParams(p, { replace: true }); };

  if (q.isLoading) return <Msg><Loader2 size={14} className="inline animate-spin mr-2" />Loading matchup…</Msg>;
  if (q.error || !d) return <Msg tone="error">{(q.error as Error)?.message ?? "Not found"}</Msg>;
  const e = d.event;
  const a = d.analysis ?? {};
  const ask = `Break down ${e.away.name} at ${e.home.name}${e.start ? ` on ${new Date(e.start).toLocaleDateString()}` : ""}: model edge, key matchups, injuries and the best bets.`;

  return (
    <div className="space-y-4">
      <div className="flex items-center gap-2">
        <button onClick={() => nav(-1)} className="inline-flex items-center gap-1.5 rounded-md border border-glass px-3 py-1.5 text-sm text-gray-300 hover:bg-white/5"><ArrowLeft size={14} />Games</button>
        <Link to={`/monster-gpt?${new URLSearchParams({ q: ask, sport, game: id })}`} className="ml-auto inline-flex items-center gap-1.5 rounded-md bg-violet/20 px-3 py-1.5 text-sm text-violet-light hover:bg-violet/30"><Bot size={14} />Ask MonsterGPT</Link>
      </div>
      <Header d={d} sport={sport as HubSport} />
      <div className="flex gap-1 overflow-x-auto border-b border-glass">
        {tabs.map((t) => (
          <button key={t.key} onClick={() => setTab(t.key)}
            className={clsx("shrink-0 px-3 py-2 text-sm border-b-2 -mb-px", tab === t.key ? "border-violet text-violet-light" : "border-transparent text-gray-400 hover:text-gray-200")}>{t.label}</button>
        ))}
      </div>
      {a.error && <Msg tone="error">Analytics: {a.error}</Msg>}
      {tab === "overview" && <Overview d={d} onExplore={(k) => setTab("matchups", { m: k })} />}
      {tab === "rankings" && <Rankings d={d} />}
      {tab === "projections" && <Projections d={d} />}
      {tab === "picks" && <Picks d={d} />}
      {tab === "matchups" && (nfl ? <Matchups d={d} initial={params.get("m") ?? "receiving"} />
        : <GroupMatchups d={d} initial={params.get("m") ?? d.analysis?.player_matchups?.[0]?.key ?? ""} />)}
      {tab === "report" && <Report d={d} />}
      {tab === "lines" && <Lines d={d} />}
      {tab === "injuries" && <Injuries d={d} />}
      {tab === "coaching" && <Coaching d={d} />}
      {tab === "situations" && <Situations d={d} />}
      {tab === "teamstats" && <TeamStats d={d} />}
      {tab === "weather" && <Weather d={d} />}
      {a.data_note && <div className="text-[11px] text-gray-500 flex gap-1.5"><Info size={12} className="shrink-0 mt-0.5" />{a.data_note}</div>}
    </div>
  );
}

// ---------------------------------------------------------------------------

function Header({ d, sport }: { d: GameDetail; sport: HubSport }) {
  const e = d.event;
  const L = e.lines ?? {};
  const done = e.status.state === "post";
  const ref = L.spread?.home?.line ?? e.espn_odds?.spread_home;
  const tot = L.total?.over?.line ?? e.espn_odds?.total;
  return (
    <div className="rounded-lg border border-glass bg-surface/60 overflow-hidden">
      <div className="grid grid-cols-[1fr_auto_1fr] items-center gap-4 px-6 py-6">
        <TeamBig t={e.away} />
        <div className="text-center">
          {e.status.state === "pre" ? <div className="text-2xl text-gray-500">@</div>
            : <div className="font-mono text-3xl">{e.away.score ?? 0} <span className="text-gray-600">-</span> {e.home.score ?? 0}</div>}
          <div className="text-xs text-gray-400 mt-1">{e.status.state === "pre" && e.start ? new Date(e.start).toLocaleString(undefined, { weekday: "short", month: "short", day: "numeric", hour: "numeric", minute: "2-digit" }) : e.status.detail}</div>
          {done && <div className="text-[11px] text-gray-500">Final</div>}
        </div>
        <TeamBig t={e.home} />
      </div>
      <div className="flex flex-wrap items-center gap-x-4 gap-y-1 px-6 py-2 border-t border-glass text-xs text-gray-400">
        <span>{[e.venue.name, e.venue.city && `${e.venue.city}${e.venue.state ? `, ${e.venue.state}` : ""}`, e.broadcast].filter(Boolean).join(" · ")}</span>
        {ref != null && <span className="ml-auto">Matchup reference: {ref <= 0 ? `${e.home.abbr} ${fmtLine(ref)}` : `${e.away.abbr} ${fmtLine(-ref)}`}{tot != null && ` · Total ${tot}`}</span>}
      </div>
      <div className="grid md:grid-cols-3 gap-3 p-4 border-t border-glass">
        {[["Moneyline", "moneyline"], [sport === "nhl" ? "Puck line" : sport === "mlb" ? "Run line" : "Spread", "spread"], ["Total", "total"]].map(([label, k]) => (
          <div key={k} className="space-y-2">
            <div className="text-[10px] uppercase tracking-wider text-gray-500 text-center">{label}</div>
            {k === "moneyline" && <><Quote label={e.away.abbr} q={L.moneyline?.away} fallback={{ price: e.espn_odds?.away_ml }} /><Quote label={e.home.abbr} q={L.moneyline?.home} fallback={{ price: e.espn_odds?.home_ml }} /></>}
            {k === "spread" && <><Quote label={e.away.abbr} q={L.spread?.away} fallback={e.espn_odds?.spread_home != null ? { line: -e.espn_odds.spread_home } : undefined} /><Quote label={e.home.abbr} q={L.spread?.home} fallback={e.espn_odds?.spread_home != null ? { line: e.espn_odds.spread_home } : undefined} /></>}
            {k === "total" && <><Quote label="O" q={L.total?.over} fallback={{ line: e.espn_odds?.total }} /><Quote label="U" q={L.total?.under} fallback={{ line: e.espn_odds?.total }} /></>}
          </div>
        ))}
      </div>
    </div>
  );
}

function TeamBig({ t }: { t: GameDetail["event"]["home"] }) {
  return (
    <div className="flex flex-col items-center gap-2 text-center">
      {t.logo ? <img src={t.logo} alt="" className="w-16 h-16" /> : <div className="w-16 h-16 rounded-full bg-white/10" />}
      <div className="text-xl font-semibold">{t.abbr}</div>
      <div className="text-xs text-gray-500">{t.name}{t.record && ` · ${t.record}`}</div>
    </div>
  );
}

function Section({ title, sub, children, right }: { title: string; sub?: string; children: ReactNode; right?: ReactNode }) {
  return (
    <section className="space-y-2">
      <div className="flex items-end gap-3">
        <div><h2 className="text-base font-semibold">{title}</h2>{sub && <div className="text-xs text-gray-500">{sub}</div>}</div>
        {right && <div className="ml-auto">{right}</div>}
      </div>
      {children}
    </section>
  );
}

function Help({ text }: { text: string }) {
  return <span title={text} className="inline-flex text-gray-500 hover:text-gray-300 cursor-help align-middle ml-1"><Info size={12} /></span>;
}

/** Cell shaded by league rank (#1 strongest shade). */
function RankCell({ m, className }: { m?: MetricVal; className?: string }) {
  if (!m) return <td className={className} />;
  const t = m.rank && m.of > 1 ? 1 - (m.rank - 1) / (m.of - 1) : 0;
  return (
    <td className={clsx("px-3 py-2 text-center", className)} style={{ background: m.rank ? `rgba(139,92,246,${0.06 + 0.42 * t})` : undefined }}>
      <div className="font-mono text-sm">{fmtMetric(m)}</div>
      <div className="text-[10px] text-gray-400">{m.rank ? `#${m.rank} / ${m.of}` : "—"}</div>
    </td>
  );
}

// ---------------------------------------------------------------------------
// Overview

function Overview({ d, onExplore }: { d: GameDetail; onExplore: (k: string) => void }) {
  const e = d.event;
  const m = e.model;
  const a = d.analysis ?? {};
  const team = (abbr: string) => (abbr === e.home.abbr ? e.home : e.away);
  return (
    <div className="space-y-6">
      {m ? (
        <Section title="Model forecast" sub={`${m.engine} · frozen prediction ${m.ledger_id}`}>
          <div className="grid md:grid-cols-4 gap-3">
            <Box label="Model score">{e.away.abbr} {m.away_score} <span className="text-gray-600">/</span> {e.home.abbr} {m.home_score}
              {m.away_range?.q10 != null && <div className="text-[11px] text-gray-500">80% ranges {m.away_range.q10}-{m.away_range.q90} / {m.home_range?.q10}-{m.home_range?.q90}</div>}</Box>
            <Box label="Model line">{m.spread_home == null ? "—" : m.spread_home <= 0 ? `${e.home.abbr} ${fmtLine(m.spread_home)}` : `${e.away.abbr} ${fmtLine(-m.spread_home)}`}</Box>
            <Box label="Model total">{m.total ?? "—"}{m.total_range?.q10 != null && <div className="text-[11px] text-gray-500">80% range {m.total_range.q10}-{m.total_range.q90}</div>}</Box>
            <Box label="Win probability">{e.away.abbr} {pct(m.win_away)} · {e.home.abbr} {pct(m.win_home)}
              {m.overtime != null && <div className="text-[11px] text-gray-500">Overtime {pct(m.overtime)}</div>}</Box>
          </div>
          {m.first_half && (
            <div className="text-xs text-gray-400">First half / period: {e.away.abbr} {m.first_half.away} - {e.home.abbr} {m.first_half.home}, total {m.first_half.total}</div>
          )}
          {d.edges.length > 0 && (
            <div className="overflow-auto rounded-lg border border-glass">
              <table className="w-full text-sm">
                <thead className="text-[10px] uppercase tracking-wider text-gray-500 bg-surface"><tr>
                  <th className="text-left px-3 py-2">Market</th><th className="px-3 py-2 text-right">Best price</th><th className="px-3 py-2 text-right">Model</th>
                  <th className="px-3 py-2 text-right">Implied</th><th className="px-3 py-2 text-right">Edge</th><th className="px-3 py-2 text-right">EV / $1</th></tr></thead>
                <tbody>{d.edges.map((x) => (
                  <tr key={`${x.market}${x.side}`} className="border-t border-glass">
                    <td className="px-3 py-2">{x.market === "total" ? `${x.side === "over" ? "Over" : "Under"} ${x.line}` : `${x.side === "home" ? e.home.abbr : e.away.abbr} ${x.market === "spread" ? fmtLine(x.line) : "ML"}`}</td>
                    <td className="px-3 py-2 text-right font-mono">{fmtOdds(x.price)} <span className="text-[10px] text-gray-500">{bookName(x.book)}</span></td>
                    <td className="px-3 py-2 text-right font-mono">{pct(x.p_model)}</td>
                    <td className="px-3 py-2 text-right font-mono text-gray-400">{pct(x.p_implied)}</td>
                    <td className={clsx("px-3 py-2 text-right font-mono", x.edge > 0 ? "text-emerald-light" : "text-gray-500")}>{signed(x.edge)}</td>
                    <td className={clsx("px-3 py-2 text-right font-mono", x.ev > 0 ? "text-emerald-light" : "text-gray-500")}>{x.ev >= 0 ? "+" : ""}{x.ev.toFixed(3)}</td>
                  </tr>))}</tbody>
              </table>
              <div className="px-3 py-1.5 text-[11px] text-gray-500 border-t border-glass">Implied = the price's break-even (vig included). Our backtests show game-line models rarely beat closing lines, so treat edges as a starting point. AI Picks applies track-record caps.</div>
            </div>
          )}
        </Section>
      ) : <Msg>No frozen model forecast for this game yet. The autopilot freezes each game's prediction before it starts.</Msg>}

      {a.stands_out && (
        <Section title="What stands out" sub="Season efficiency comparisons describe performance so far, not projected yards or a betting edge.">
          <div className="grid md:grid-cols-2 gap-3">
            {a.stands_out.map((s) => (
              <div key={s.team} className="rounded-lg border border-glass bg-surface/60 p-4">
                <div className="flex items-center gap-2 mb-2">{team(s.team).logo && <img src={team(s.team).logo!} className="w-5 h-5" alt="" />}<span className="font-semibold text-sm">{s.team}</span></div>
                <div className="text-sm text-gray-200">{s.text}</div>
              </div>
            ))}
          </div>
        </Section>
      )}

      {a.has_ball && (
        <Section title={d.sport === "nfl" ? "When each team has the ball" : "Offense vs defense"}
          sub={d.sport === "nfl" ? `Season ${a.season} through week ${a.through_week} · what they do, what they face, and where to look next` : `${a.season_label ?? "This season"} · what each offense does, what it faces, and where to look next`}
          right={<div className="flex items-center gap-2 text-[11px] text-gray-500">Lower<div className="w-20 h-1.5 rounded bg-gradient-to-r from-violet/10 to-violet/60" />Higher rank</div>}>
          <div className="text-[11px] text-gray-500">{d.sport === "nfl" ? "EPA = how much a play changes the expected score. " : ""}Higher is better on offense; lower allowed is better on defense. Shading follows league rank (#1 = best offense, stingiest defense); usage rows rank frequency, not quality.</div>
          <div className="grid lg:grid-cols-2 gap-3">{a.has_ball.map((h) => <HasBallCard key={h.offense} h={h} d={d} onExplore={onExplore} />)}</div>
        </Section>
      )}

      <Section title="Form & records">
        <div className="grid md:grid-cols-2 gap-3">
          {[e.away, e.home].map((t) => {
            const log = a.logs?.find((l) => l.team === t.abbr);
            const rec = a.season_records?.find((r) => r.team === t.abbr);
            const l5 = d.espn.last_five.find((x) => x.team === t.espn_abbr || x.team === t.abbr);
            const ats = d.espn.ats.find((x) => x.team === t.espn_abbr || x.team === t.abbr);
            return (
              <div key={t.abbr} className="rounded-lg border border-glass bg-surface/60 p-4 space-y-3">
                <div className="flex items-center gap-2">{t.logo && <img src={t.logo} className="w-5 h-5" alt="" />}<span className="font-semibold">{t.abbr}</span>
                  {rec && <span className="ml-auto text-xs text-gray-400">This season SU {rec.su} · ATS {rec.ats} · O/U {rec.ou}</span>}</div>
                {log ? (
                  <>
                    <div className="flex gap-1">{[...log.games].reverse().map((g) => (
                      <div key={g.game_id} title={`${g.home ? "vs" : "@"} ${g.opponent} ${g.score}`}
                        className={clsx("flex-1 text-center rounded py-1 text-[11px] font-medium", g.result === "W" ? "bg-emerald/20 text-emerald-light" : g.result === "L" ? "bg-crimson/20 text-crimson-light" : "bg-white/10")}>{g.result}</div>))}</div>
                    <div className="text-xs text-gray-400">Last {log.games.length}: SU {log.su} · ATS {log.ats} · O/U {log.ou}</div>
                    <table className="w-full text-xs">
                      <thead className="text-[10px] uppercase text-gray-500"><tr><th className="text-left py-1">Game</th><th className="text-left">Opp</th><th className="text-right">Score</th><th className="text-right">ATS</th><th className="text-right">O/U</th></tr></thead>
                      <tbody>{log.games.map((g) => (
                        <tr key={g.game_id} className="border-t border-glass">
                          <td className="py-1.5 text-gray-400">{g.game_type === "REG" ? `W${g.week}` : g.game_type} {g.season}</td>
                          <td>{g.home ? "vs" : "@"} {g.opponent}</td>
                          <td className="text-right font-mono"><span className={g.result === "W" ? "text-emerald-light" : g.result === "L" ? "text-crimson-light" : ""}>{g.result}</span> {g.score}</td>
                          <td className="text-right font-mono"><span className={g.ats === "W" ? "text-emerald-light" : g.ats === "L" ? "text-crimson-light" : "text-gray-400"}>{g.ats ?? "—"}</span> <span className="text-gray-500">{fmtLine(g.line)}</span></td>
                          <td className="text-right font-mono"><span className={g.ou === "O" ? "text-emerald-light" : g.ou === "U" ? "text-crimson-light" : "text-gray-400"}>{g.ou ?? "—"}</span> <span className="text-gray-500">{g.total_line ?? ""}</span></td>
                        </tr>))}</tbody>
                    </table>
                  </>
                ) : l5 && l5.data.length > 0 ? (
                  <>
                    <div className="flex gap-1">{l5.data.map((g, i) => (
                      <div key={i} title={`${g.at ?? ""} ${g.opponent ?? ""} ${g.score ?? ""}`}
                        className={clsx("flex-1 text-center rounded py-1 text-[11px] font-medium", g.result === "W" ? "bg-emerald/20 text-emerald-light" : g.result === "L" ? "bg-crimson/20 text-crimson-light" : "bg-white/10")}>{g.result ?? "—"}</div>))}</div>
                    <table className="w-full text-xs"><tbody>{l5.data.map((g, i) => (
                      <tr key={i} className="border-t border-glass"><td className="py-1.5 text-gray-400">{g.date ? new Date(g.date).toLocaleDateString(undefined, { month: "short", day: "numeric" }) : ""}</td>
                        <td>{g.at} {g.opponent}</td><td className="text-right font-mono">{g.result} {g.score}</td></tr>))}</tbody></table>
                  </>
                ) : <div className="text-xs text-gray-500">No recent games.</div>}
                {ats && ats.data.length > 0 && <div className="text-xs text-gray-400">{ats.data.map((r) => `${r.type}: ${r.summary}`).join(" · ")}</div>}
              </div>
            );
          })}
        </div>
      </Section>

      {d.espn.leaders.length > 0 && (
        <Section title="Season leaders">
          <div className="grid md:grid-cols-2 gap-3">{d.espn.leaders.map((t) => (
            <div key={t.team} className="rounded-lg border border-glass bg-surface/60 p-3 space-y-2">
              <div className="flex items-center gap-2 text-sm font-semibold">{t.logo && <img src={t.logo} className="w-5 h-5" alt="" />}{t.team}</div>
              {t.data.map((l) => (
                <div key={l.category} className="flex items-center gap-2 text-sm">
                  {l.headshot ? <img src={l.headshot} className="w-7 h-7 rounded-full object-cover bg-white/5" alt="" /> : <div className="w-7 h-7 rounded-full bg-white/5" />}
                  <div className="min-w-0"><div className="truncate">{l.name}</div><div className="text-[11px] text-gray-500">{l.category}</div></div>
                  <div className="ml-auto text-xs text-gray-300 text-right">{l.value}</div>
                </div>))}
            </div>))}</div>
        </Section>
      )}
      {d.espn.predictor && d.espn.predictor.home != null && (
        <div className="text-xs text-gray-500">For comparison, ESPN's matchup predictor: {e.away.abbr} {d.espn.predictor.away}% · {e.home.abbr} {d.espn.predictor.home}%.</div>
      )}
    </div>
  );
}

function Box({ label, children }: { label: string; children: ReactNode }) {
  return <div className="rounded-lg border border-glass bg-surface/60 px-4 py-3"><div className="text-[10px] uppercase tracking-wider text-gray-500">{label}</div><div className="font-mono mt-1">{children}</div></div>;
}

function HasBallCard({ h, d, onExplore }: { h: HasBall; d: GameDetail; onExplore: (k: string) => void }) {
  const logo = (t: string) => (t === d.event.home.abbr ? d.event.home.logo : d.event.away.logo);
  return (
    <div className="rounded-lg border border-glass bg-surface/60 overflow-hidden">
      <div className="flex items-center gap-2 px-4 py-3 border-b border-glass font-semibold text-sm">
        {logo(h.offense) && <img src={logo(h.offense)!} className="w-5 h-5" alt="" />}{h.offense} offense <span className="text-gray-500 font-normal">vs</span>
        {logo(h.defense) && <img src={logo(h.defense)!} className="w-5 h-5" alt="" />}{h.defense} defense
      </div>
      {h.sections.map((s) => (
        <div key={s.title} className="border-b border-glass last:border-0">
          <div className="px-4 pt-3"><div className="font-medium text-sm">{s.title}</div>{s.note && <div className="text-xs text-gray-400 mt-1">{s.note}</div>}</div>
          <table className="w-full text-sm mt-2">
            <thead className="text-[10px] uppercase tracking-wider text-gray-500"><tr><th className="text-left px-4 py-1">Metric</th><th className="px-3 w-28">{h.offense} offense</th><th className="px-3 w-28">{h.defense} defense</th></tr></thead>
            <tbody>{s.rows.map((r) => (
              <tr key={r.key} className="border-t border-glass/50">
                <td className="px-4 py-1.5 text-gray-300">{r.label}<Help text={r.help} /></td>
                <RankCell m={r.offense} /><RankCell m={r.defense} />
              </tr>))}</tbody>
          </table>
          <button onClick={() => onExplore(s.explore)} className="px-4 py-2 text-xs text-violet-light hover:underline">Explore {s.explore === "qb" ? "QB" : s.explore} matchups →</button>
        </div>
      ))}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Rankings

function Rankings({ d }: { d: GameDetail }) {
  const r = d.analysis?.rankings;
  if (!r) return <Msg>No rankings for this sport yet.</Msg>;
  const logo = (t: string) => (t === d.event.home.abbr ? d.event.home.logo : d.event.away.logo);
  return (
    <Section title="Rankings" sub={`What each defense allows · #1 allows the least · #${r[0]?.rows[0]?.of ?? 32} allows the most · vs league average`}>
      <div className="grid lg:grid-cols-2 gap-3">{r.map((x: Ranking) => (
        <div key={x.team} className="rounded-lg border border-glass bg-surface/60 overflow-hidden">
          <div className="flex items-center gap-2 px-4 py-3 border-b border-glass">
            {logo(x.team) && <img src={logo(x.team)!} className="w-5 h-5" alt="" />}
            <div><div className="font-semibold text-sm">{x.team} defense allows</div><div className="text-[11px] text-gray-500">What {x.faces} faces</div></div>
            <span className="ml-auto text-xs text-gray-500">{x.games} games</span>
          </div>
          <table className="w-full text-sm">
            <thead className="text-[10px] uppercase tracking-wider text-gray-500"><tr><th className="text-left px-4 py-1.5 w-14">Rank</th><th className="text-left">Metric</th><th className="text-right px-3">Allowed</th><th className="text-right px-4">vs league</th></tr></thead>
            <tbody>{x.rows.map((m) => {
              const good = (m.vs_league ?? 0) < 0;
              return (
                <tr key={m.key} className="border-t border-glass/50">
                  <td className="px-4 py-1.5"><span className="inline-block min-w-[28px] text-center rounded bg-violet/25 text-violet-light text-xs py-0.5">{m.rank}</span></td>
                  <td className="text-gray-200">{m.label}<Help text={m.help} /></td>
                  <td className="text-right px-3 font-mono">{fmtMetric(m)}</td>
                  <td className={clsx("text-right px-4 font-mono text-xs", m.usage ? "text-gray-400" : good ? "text-emerald-light" : "text-crimson-light")}>{fmtVsLeague(m)}</td>
                </tr>);
            })}</tbody>
          </table>
          {x.proe && (
            <div className="px-4 py-2 border-t border-glass text-xs text-gray-400 space-y-0.5">
              <div>{x.faces} pass rate over expected {fmtMetric(x.proe)} · #{x.proe.rank}<Help text={x.proe.help} /></div>
              <div>{x.team} run defense #{x.run_defense_rank ?? "—"} · pass defense #{x.pass_defense_rank ?? "—"} (EPA allowed)</div>
            </div>
          )}
        </div>))}</div>
    </Section>
  );
}

// ---------------------------------------------------------------------------
// Projections

const POS_ORDER = ["QB", "RB", "WR", "TE", "K", "G", "PG", "SG", "F", "SF", "PF", "C", "LW", "RW", "D", "SP", "P"];

function Projections({ d }: { d: GameDetail }) {
  const [q, setQ] = useState("");
  const [team, setTeam] = useState("all");
  const [open, setOpen] = useState<string | null>(null);
  if (!d.projections) return <Msg>No player projections yet: the model freezes a game's simulation before it starts.</Msg>;
  const players = [...d.projections.away, ...d.projections.home]
    .filter((p) => (team === "all" || p.team === team) && (!q || `${p.name} ${p.team}`.toLowerCase().includes(q.toLowerCase())));
  const groups = new Map<string, ProjPlayer[]>();
  for (const p of players) { const k = (p.pos ?? "Other").toUpperCase(); groups.set(k, [...(groups.get(k) ?? []), p]); }
  const order = [...groups.keys()].sort((a, b) => (POS_ORDER.indexOf(a) + 1 || 99) - (POS_ORDER.indexOf(b) + 1 || 99));
  return (
    <Section title="Player projections" sub="Model stats from the frozen game simulation with current sportsbook lines. Click a stat to see every book and the model's probability at each line."
      right={<div className="flex gap-2">
        <select value={team} onChange={(e) => setTeam(e.target.value)} className="bg-surface border border-glass rounded-md px-2 py-1.5 text-sm">
          <option value="all">All teams</option><option value={d.event.away.abbr}>{d.event.away.abbr}</option><option value={d.event.home.abbr}>{d.event.home.abbr}</option></select>
        <div className="relative"><Search size={13} className="absolute left-2.5 top-2.5 text-gray-500" />
          <input value={q} onChange={(e) => setQ(e.target.value)} placeholder="Search players or team..." className="w-56 bg-surface border border-glass rounded-md pl-8 pr-2 py-1.5 text-sm" /></div>
      </div>}>
      {order.map((pos) => {
        const list = groups.get(pos)!;
        const stats = [...new Set(list.flatMap((p) => Object.keys(p.stats)))].filter((k) => list.some((p) => (p.stats[k]?.mean ?? 0) > 0.005));
        const shown = stats.sort((a, b) => Number(list.some((p) => p.stats[b]?.books)) - Number(list.some((p) => p.stats[a]?.books)));
        return (
          <div key={pos} className="space-y-1">
            <div className="text-sm"><b>{pos}</b> <span className="text-gray-500 text-xs">{list.length} players</span></div>
            <div className="overflow-x-auto rounded-lg border border-glass">
              <table className="text-sm min-w-full">
                <thead className="text-[10px] uppercase tracking-wider text-gray-500 bg-surface"><tr>
                  <th className="text-left px-3 py-2 sticky left-0 bg-surface min-w-[170px]">Player</th>
                  {shown.map((k) => <th key={k} className="px-3 py-2 text-right whitespace-nowrap">{statLabel(k)}</th>)}</tr></thead>
                <tbody>{list.map((p) => (
                  <Fragment key={p.id}>
                    <tr className="border-t border-glass align-top">
                      <td className="px-3 py-2 sticky left-0 bg-obsidian"><div className="font-medium">{p.name}</div><div className="text-[11px] text-gray-500">{p.pos} · {p.team}{p.role && p.role !== p.pos ? ` · ${p.role}` : ""}</div></td>
                      {shown.map((k) => {
                        const s = p.stats[k];
                        const best = (side: string) => s?.books?.filter((b) => b.side === side).sort((x, y) => y.price - x.price)[0];
                        const o = best("over"), u = best("under");
                        const key = `${p.id}:${k}`;
                        return (
                          <td key={k} className="px-3 py-2 text-right whitespace-nowrap cursor-pointer hover:bg-white/5" onClick={() => setOpen(open === key ? null : key)}>
                            <div className="font-mono text-violet-light">{s?.mean != null ? (s.mean < 1 && ["tds", "rec_td", "rush_td", "pass_td", "goals", "hr"].includes(k) ? `${(s.mean * 100).toFixed(0)}%*` : s.mean.toFixed(2)) : "—"}</div>
                            {o && <div className="text-[11px] text-gray-400">O {o.line} <span className="font-mono">{fmtOdds(o.price)}</span>{o.p_model != null && <span className={clsx("ml-1", o.p_model > implied(o.price) ? "text-emerald-light" : "text-gray-500")}>{pct(o.p_model, 0)}</span>}</div>}
                            {u && <div className="text-[11px] text-gray-400">U {u.line} <span className="font-mono">{fmtOdds(u.price)}</span>{u.p_model != null && <span className={clsx("ml-1", u.p_model > implied(u.price) ? "text-emerald-light" : "text-gray-500")}>{pct(u.p_model, 0)}</span>}</div>}
                            {open === key && s?.books && (
                              <div className="mt-1 text-[10px] text-gray-500 text-left space-y-0.5">{s.books.map((b, i) => (
                                <div key={i}>{bookName(b.book)} {b.side === "over" ? "O" : "U"} {b.line} {fmtOdds(b.price)} · model {pct(b.p_model, 0)}</div>))}
                                <div>80% range {s.q10}-{s.q90}</div></div>)}
                          </td>);
                      })}
                    </tr>
                  </Fragment>))}</tbody>
              </table>
            </div>
          </div>);
      })}
      <div className="text-[11px] text-gray-500">Values are simulation means. * For touchdown-type stats under 1 per game we show the expected count as a percentage. Green % = model probability above the price's break-even.</div>
    </Section>
  );
}
const implied = (p: number) => (p < 0 ? -p / (-p + 100) : 100 / (p + 100));

// ---------------------------------------------------------------------------
// Picks

function Picks({ d }: { d: GameDetail }) {
  if (!d.picks.length) return <Msg>No AI picks for this game. Picks publish when the model finds a priced edge on a game it has frozen (within ~40 hours of start).</Msg>;
  return (
    <div className="grid md:grid-cols-2 gap-3">{d.picks.map((p) => (
      <div key={p.id} className={clsx("rounded-lg border bg-surface/60 p-4 space-y-2", p.tier === "hidden" ? "border-glass opacity-60" : "border-violet/30")}>
        <div className="flex items-center gap-2">
          <span className="rounded bg-emerald/15 text-emerald-light text-xs px-1.5 py-0.5">{p.grade}</span>
          <span className="text-[10px] uppercase text-gray-500">{p.tier}</span>
          <span className="ml-auto font-mono text-sm">{fmtOdds(p.price)} <span className="text-[10px] text-gray-500">{bookName(p.book)}</span></span>
        </div>
        <div className="font-medium">{p.subject} {p.side} {p.line ?? ""} <span className="text-gray-500 text-xs">{statLabel(p.market)}</span></div>
        <div className="grid grid-cols-3 gap-2 text-xs">
          <div><div className="text-gray-500">Model / market</div><div className="font-mono">{pct(p.p_model)} / {pct(p.p_market)}</div></div>
          <div><div className="text-gray-500">Edge</div><div className="font-mono text-emerald-light">{signed(p.edge)}</div></div>
          <div><div className="text-gray-500">Projection</div><div className="font-mono">{p.projection ?? "—"}</div></div>
        </div>
        <div className="text-xs text-gray-400 line-clamp-4">{p.writeup}</div>
        <div className="flex gap-3 text-xs">
          <Link to={`/picks?id=${p.id}`} className="text-violet-light hover:underline">Open in AI Picks</Link>
          {usableLink(p.link) && <a href={usableLink(p.link)!} target="_blank" rel="noreferrer" className="inline-flex items-center gap-1 text-gray-400 hover:text-gray-200">Bet at {bookName(p.book)}<ExternalLink size={11} /></a>}
        </div>
      </div>))}</div>
  );
}

// ---------------------------------------------------------------------------
// Matchups (NFL players)

function Matchups({ d, initial }: { d: GameDetail; initial: string }) {
  const [k, setK] = useState(initial);
  const [team, setTeam] = useState("all");
  const [q, setQ] = useState("");
  const m = d.analysis?.matchups;
  if (!m) return <Msg>No player matchups.</Msg>;
  const cols: Record<string, [string, string, (s: Record<string, number>) => string][]> = {
    receiving: [["Tgt/g", "targets_pg", (s) => s.targets_pg.toFixed(1)], ["Tgt share", "target_share", (s) => pct(s.target_share)], ["Air share", "air_share", (s) => pct(s.air_share)],
      ["aDOT", "adot", (s) => s.adot.toFixed(1)], ["Rec/g", "rec_pg", (s) => s.rec_pg.toFixed(1)], ["Yds/g", "rec_yds_pg", (s) => s.rec_yds_pg.toFixed(1)], ["TD", "rec_td", (s) => String(s.rec_td)]],
    rushing: [["Car/g", "carries_pg", (s) => s.carries_pg.toFixed(1)], ["Rush share", "rush_share", (s) => pct(s.rush_share)], ["Yds/g", "rush_yds_pg", (s) => s.rush_yds_pg.toFixed(1)],
      ["YPC", "ypc", (s) => s.ypc.toFixed(2)], ["EPA/carry", "epa_carry", (s) => (s.epa_carry >= 0 ? "+" : "") + s.epa_carry.toFixed(2)], ["TD", "rush_td", (s) => String(s.rush_td)]],
    qb: [["Att/g", "att_pg", (s) => s.att_pg.toFixed(1)], ["Comp %", "comp_pct", (s) => pct(s.comp_pct)], ["Yds/g", "pass_yds_pg", (s) => s.pass_yds_pg.toFixed(1)], ["Y/A", "ypa", (s) => s.ypa.toFixed(2)],
      ["EPA/db", "epa_db", (s) => (s.epa_db >= 0 ? "+" : "") + s.epa_db.toFixed(2)], ["CPOE", "cpoe", (s) => s.cpoe.toFixed(1)], ["Sack %", "sack_rate", (s) => pct(s.sack_rate)], ["TD-INT", "pass_td", (s) => `${s.pass_td}-${s.int}`]],
    touchdowns: [["TDs", "tds", (s) => String(s.tds)], ["TD/g", "td_pg", (s) => s.td_pg.toFixed(2)], ["RZ touches", "rz_touches", (s) => String(s.rz_touches)],
      ["RZ share", "rz_share", (s) => pct(s.rz_share)], ["RZ tgt", "rz_targets", (s) => String(s.rz_targets)], ["RZ car", "rz_carries", (s) => String(s.rz_carries)]],
  };
  const projKey: Record<string, string[]> = { receiving: ["rec_yds", "receptions"], rushing: ["rush_yds", "carries"], qb: ["pass_yds", "pass_td"], touchdowns: ["tds"] };
  const rows = (m[k as keyof typeof m] ?? []).filter((p: PlayerMatchup) => (team === "all" || p.team === team) && (!q || p.name.toLowerCase().includes(q.toLowerCase())));
  return (
    <Section title={{ receiving: "Receiving", rushing: "Rushing", qb: "QB matchups", touchdowns: "Touchdowns" }[k] ?? k}
      sub="Season usage before this game, the model's projection, and how the opposing defense ranks against that position"
      right={<div className="flex gap-2">
        <select value={team} onChange={(e) => setTeam(e.target.value)} className="bg-surface border border-glass rounded-md px-2 py-1.5 text-sm">
          <option value="all">All teams</option><option value={d.event.away.abbr}>{d.event.away.abbr}</option><option value={d.event.home.abbr}>{d.event.home.abbr}</option></select>
        <div className="relative"><Search size={13} className="absolute left-2.5 top-2.5 text-gray-500" />
          <input value={q} onChange={(e) => setQ(e.target.value)} placeholder="Search players..." className="w-48 bg-surface border border-glass rounded-md pl-8 pr-2 py-1.5 text-sm" /></div></div>}>
      <div className="flex gap-1">{["rushing", "receiving", "qb", "touchdowns"].map((x) => (
        <button key={x} onClick={() => setK(x)} className={clsx("px-3 py-1.5 rounded-md text-sm", k === x ? "bg-white/10 text-gray-100" : "text-gray-400 hover:bg-white/5")}>
          {{ rushing: "Rushing", receiving: "Receiving", qb: "QB Matchups", touchdowns: "Touchdowns" }[x]}</button>))}</div>
      <div className="overflow-x-auto rounded-lg border border-glass">
        <table className="w-full text-sm">
          <thead className="text-[10px] uppercase tracking-wider text-gray-500 bg-surface"><tr>
            <th className="text-left px-3 py-2">Player</th>{cols[k].map(([l]) => <th key={l} className="px-3 py-2 text-right">{l}</th>)}
            <th className="px-3 py-2 text-right">Projection</th><th className="px-3 py-2 text-center">Opp. defense</th></tr></thead>
          <tbody>{rows.map((p: PlayerMatchup) => (
            <tr key={p.id} className="border-t border-glass">
              <td className="px-3 py-2"><div className="font-medium">{p.name}</div><div className="text-[11px] text-gray-500">{p.pos} · {p.team} · {p.games} g · vs {p.opponent}</div></td>
              {cols[k].map(([l, , f]) => <td key={l} className="px-3 py-2 text-right font-mono">{f(p.stats)}</td>)}
              <td className="px-3 py-2 text-right font-mono text-violet-light text-xs">{projKey[k].map((s) => p.projection?.stats?.[s] != null ? `${statLabel(s)} ${(p.projection.stats[s] as number).toFixed(1)}` : null).filter(Boolean).join(" · ") || "—"}</td>
              <td className="px-3 py-2 text-center text-xs">
                <div>{p.opp_allows.label}: <span className="font-mono">{fmtMetric(p.opp_allows)}</span></div>
                <div className={clsx("font-medium", (p.opp_allows.rank ?? 0) > (p.opp_allows.of * 2) / 3 ? "text-emerald-light" : (p.opp_allows.rank ?? 99) <= p.opp_allows.of / 3 ? "text-crimson-light" : "text-gray-400")}>
                  #{p.opp_allows.rank ?? "—"} of {p.opp_allows.of}{(p.opp_allows.rank ?? 0) > (p.opp_allows.of * 2) / 3 ? " · soft" : (p.opp_allows.rank ?? 99) <= p.opp_allows.of / 3 ? " · tough" : ""}</div>
              </td>
            </tr>))}</tbody>
        </table>
      </div>
      <div className="text-[11px] text-gray-500">WR/CB and offensive-line matchups need coverage and blocking charts that no free 2026 feed publishes, so they aren't shown.</div>
    </Section>
  );
}

// ---------------------------------------------------------------------------
// Matchups (NBA / NHL / MLB players)

const impliedP = (price: number) => (price < 0 ? -price / (-price + 100) : 100 / (price + 100));

function GroupMatchups({ d, initial }: { d: GameDetail; initial: string }) {
  const groups = d.analysis?.player_matchups ?? [];
  const [k, setK] = useState(groups.some((g) => g.key === initial) ? initial : groups[0]?.key ?? "");
  const [team, setTeam] = useState("all");
  const [q, setQ] = useState("");
  const g: MatchupGroup | undefined = groups.find((x) => x.key === k);
  if (!g) return <Msg>No player matchups (needs a frozen model forecast for this game).</Msg>;
  const rows = g.rows.filter((p) => (team === "all" || p.team === team) && (!q || p.name.toLowerCase().includes(q.toLowerCase())));
  return (
    <Section title={g.label} sub="The model's projection, the main book line with our probability of the over, and how the opponent ranks at what it allows this player type"
      right={<div className="flex gap-2">
        <select value={team} onChange={(e) => setTeam(e.target.value)} className="bg-surface border border-glass rounded-md px-2 py-1.5 text-sm">
          <option value="all">All teams</option><option value={d.event.away.abbr}>{d.event.away.abbr}</option><option value={d.event.home.abbr}>{d.event.home.abbr}</option></select>
        <div className="relative"><Search size={13} className="absolute left-2.5 top-2.5 text-gray-500" />
          <input value={q} onChange={(e) => setQ(e.target.value)} placeholder="Search players..." className="w-48 bg-surface border border-glass rounded-md pl-8 pr-2 py-1.5 text-sm" /></div></div>}>
      <div className="flex gap-1 flex-wrap">{groups.map((x) => (
        <button key={x.key} onClick={() => setK(x.key)} className={clsx("px-3 py-1.5 rounded-md text-sm", k === x.key ? "bg-white/10 text-gray-100" : "text-gray-400 hover:bg-white/5")}>{x.label}</button>))}</div>
      <div className="overflow-x-auto rounded-lg border border-glass">
        <table className="w-full text-sm">
          <thead className="text-[10px] uppercase tracking-wider text-gray-500 bg-surface"><tr>
            <th className="text-left px-3 py-2">Player</th>{g.cols.map((c) => <th key={c.key} className="px-3 py-2 text-right">{c.label}</th>)}
            <th className="px-3 py-2 text-center">Opponent</th></tr></thead>
          <tbody>{rows.map((p) => {
            const o = p.opp_allows;
            const soft = o?.rank != null && o.rank > (o.of * 2) / 3;
            const tough = o?.rank != null && o.rank <= o.of / 3;
            return (
              <tr key={`${p.team}-${p.id}`} className="border-t border-glass">
                <td className="px-3 py-2"><div className="font-medium">{p.name}</div><div className="text-[11px] text-gray-500">{p.pos} · {p.team} · vs {p.opponent}</div></td>
                {g.cols.map((c) => {
                  const s = p.stats[c.key];
                  if (!s || s.mean == null) return <td key={c.key} className="px-3 py-2 text-right text-gray-600">—</td>;
                  const beats = s.p_over != null && s.price != null && s.p_over > impliedP(s.price);
                  return (
                    <td key={c.key} className="px-3 py-2 text-right">
                      <div className="font-mono">{s.mean.toFixed(s.mean < 10 ? 2 : 1)}</div>
                      {s.line != null && (
                        <div className="text-[10px] text-gray-500" title={`${bookName(s.book ?? "")} over ${s.line} ${fmtOdds(s.price ?? 0)}`}>
                          o{s.line} <span className={clsx(beats && "text-emerald-light")}>{s.p_over != null ? pct(s.p_over, 0) : ""}</span></div>)}
                    </td>);
                })}
                <td className="px-3 py-2 text-center text-xs">
                  {o ? (
                    <>
                      <div>{p.opp_side === "offense" ? `${p.opponent} ${o.label}` : `${o.label} allowed`}: <span className="font-mono">{fmtMetric(o)}</span></div>
                      <div className={clsx("font-medium", soft ? "text-emerald-light" : tough ? "text-crimson-light" : "text-gray-400")}>#{o.rank ?? "—"} of {o.of}{soft ? " · soft" : tough ? " · tough" : ""}</div>
                    </>) : "—"}
                </td>
              </tr>);
          })}</tbody>
        </table>
      </div>
      <div className="text-[11px] text-gray-500">Under each projection: the main book's over line and our probability of the over (green when it beats the price). Opponent rank: #1 = toughest matchup for this player type, higher = softer.</div>
    </Section>
  );
}

// ---------------------------------------------------------------------------
// Game / weekly report

function Report({ d }: { d: GameDetail }) {
  const r = d.report;
  if (!r || !r.sections.length) return <Msg>No report for this game yet.</Msg>;
  const e = d.event;
  const ask = `Write a betting preview for ${e.away.name} at ${e.home.name}: where the model disagrees with the market, the matchup keys, injuries and the props worth a look.`;
  return (
    <Section title={d.sport === "nfl" ? "Weekly report" : "Game report"}
      sub={`Written from the model, market, matchup ranks, injuries and form · updated ${new Date(r.generated_at).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })}`}
      right={<Link to={`/monster-gpt?${new URLSearchParams({ q: ask, sport: d.sport, game: e.id })}`} className="inline-flex items-center gap-1.5 rounded-md bg-violet/20 px-3 py-1.5 text-sm text-violet-light hover:bg-violet/30"><Bot size={14} />Deeper take from MonsterGPT</Link>}>
      <div className="grid gap-3">{r.sections.map((s) => (
        <div key={s.title} className="rounded-lg border border-glass bg-surface/60 p-4">
          <div className="text-[10px] uppercase tracking-wider text-violet-light font-medium mb-2">{s.title}</div>
          <div className="space-y-2">{s.paragraphs.map((p, i) => <p key={i} className="text-sm text-gray-200 leading-relaxed">{p}</p>)}</div>
        </div>))}</div>
    </Section>
  );
}

// ---------------------------------------------------------------------------
// Lines

function Lines({ d }: { d: GameDetail }) {
  const e = d.event;
  if (!d.books.length) return <Msg>No book lines recorded for this game yet.</Msg>;
  const markets: [string, string, string[]][] = [["Moneyline", "moneyline", ["away", "home"]], ["Spread", "spread", ["away", "home"]], ["Total", "total", ["over", "under"]]];
  const books = [...new Set(d.books.map((b) => b.book))].sort();
  const side = (s: string) => (s === "home" ? e.home.abbr : s === "away" ? e.away.abbr : s === "over" ? "Over" : "Under");
  return (
    <div className="space-y-4">{markets.map(([label, mk, sides]) => {
      const rows = d.books.filter((b) => b.market === mk);
      if (!rows.length) return null;
      return (
        <Section key={mk} title={label}>
          <div className="overflow-x-auto rounded-lg border border-glass">
            <table className="w-full text-sm">
              <thead className="text-[10px] uppercase tracking-wider text-gray-500 bg-surface"><tr><th className="text-left px-3 py-2">Book</th>{sides.map((s) => <th key={s} className="px-3 py-2 text-right">{side(s)}</th>)}</tr></thead>
              <tbody>{books.filter((b) => rows.some((r) => r.book === b)).map((b) => (
                <tr key={b} className="border-t border-glass">
                  <td className="px-3 py-2">{bookName(b)}</td>
                  {sides.map((s) => {
                    const r = rows.find((x) => x.book === b && x.side === s);
                    const best = Math.max(...rows.filter((x) => x.side === s && x.line === r?.line).map((x) => x.price));
                    return (
                      <td key={s} className="px-3 py-2 text-right font-mono">
                        {r ? <>{r.line != null && <span className="text-gray-400 mr-2">{mk === "total" ? r.line : fmtLine(r.line)}</span>}
                          {usableLink(r.link) ? <a href={usableLink(r.link)!} target="_blank" rel="noreferrer" className={clsx("hover:underline", r.price === best && "text-emerald-light")}>{fmtOdds(r.price)}</a>
                            : <span className={clsx(r.price === best && "text-emerald-light")}>{fmtOdds(r.price)}</span>}
                          {r.open_price != null && (r.open_price !== r.price || r.open_line !== r.line) && <div className="text-[10px] text-gray-500">open {r.open_line != null ? `${mk === "total" ? r.open_line : fmtLine(r.open_line)} ` : ""}{fmtOdds(r.open_price)}</div>}</> : "—"}
                      </td>);
                  })}
                </tr>))}</tbody>
            </table>
          </div>
        </Section>);
    })}
      <div className="text-[11px] text-gray-500">Green = best price at that line. Lines are the latest recorded per book (ESPN/DraftKings, The Odds API, SportsGameOdds).</div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Injuries / lineups

function Injuries({ d }: { d: GameDetail }) {
  const e = d.event;
  const tone = (s: string | null) => /out|injured reserve|ir\b|suspend/i.test(s ?? "") ? "text-crimson-light" : /doubt/i.test(s ?? "") ? "text-amber" : "text-amber/80";
  return (
    <div className="space-y-4">
      <div className="grid lg:grid-cols-2 gap-3">{[e.away, e.home].map((t) => {
        const inj = d.espn.injuries.find((x) => x.team === t.espn_abbr || x.team === t.abbr);
        return (
          <div key={t.abbr} className="rounded-lg border border-glass bg-surface/60 overflow-hidden">
            <div className="flex items-center gap-2 px-4 py-3 border-b border-glass font-semibold text-sm">{t.logo && <img src={t.logo} className="w-5 h-5" alt="" />}{t.abbr} injuries</div>
            {!inj || inj.data.length === 0 ? <div className="px-4 py-4 text-sm text-gray-500">No reported injuries.</div> : (
              <table className="w-full text-sm"><tbody>{inj.data.map((i, k) => (
                <tr key={k} className="border-t border-glass/50 first:border-0">
                  <td className="px-4 py-2"><div className="font-medium">{i.name}</div><div className="text-[11px] text-gray-500">{[i.pos, i.detail, i.side].filter(Boolean).join(" · ")}</div></td>
                  <td className={clsx("px-4 py-2 text-right text-xs", tone(i.status))}>{i.status}{i.return && <div className="text-[10px] text-gray-500">return {i.return}</div>}</td>
                </tr>))}</tbody></table>)}
          </div>);
      })}</div>
      {d.projections && (
        <Section title="Projected lineups" sub="Who the frozen simulation plays and in what role">
          <div className="grid lg:grid-cols-2 gap-3">{[["away", e.away.abbr], ["home", e.home.abbr]].map(([s, abbr]) => (
            <div key={s} className="rounded-lg border border-glass bg-surface/60 p-3">
              <div className="text-sm font-semibold mb-2">{abbr}</div>
              <div className="grid grid-cols-2 gap-x-4 gap-y-1 text-sm">{(d.projections![s as "home" | "away"]).map((p) => (
                <div key={p.id} className="flex justify-between gap-2"><span className="truncate">{p.name}</span><span className="text-xs text-gray-500">{p.role ?? p.pos}</span></div>))}</div>
            </div>))}</div>
        </Section>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Coaching / situations / team stats / weather

function MetricList({ rows }: { rows: MetricVal[] }) {
  return (
    <table className="w-full text-sm"><tbody>{rows.map((m) => (
      <tr key={m.key} className="border-t border-glass/50 first:border-0">
        <td className="px-4 py-1.5 text-gray-300">{m.label}<Help text={m.help} /></td>
        <RankCell m={m} className="w-28" />
      </tr>))}</tbody></table>
  );
}

function Coaching({ d }: { d: GameDetail }) {
  const c = d.analysis?.coaching;
  if (!c) return <Msg>No coaching data.</Msg>;
  return (
    <div className="grid lg:grid-cols-2 gap-3">{c.map((x) => (
      <div key={x.team} className="rounded-lg border border-glass bg-surface/60 overflow-hidden">
        <div className="px-4 py-3 border-b border-glass"><div className="font-semibold">{x.team} · {x.coach ?? "Head coach"}</div>
          {x.record && <div className="text-xs text-gray-500">{d.analysis?.season} record {x.record}</div>}</div>
        <div className="px-4 pt-3 text-xs uppercase tracking-wider text-gray-500">Offensive tendencies</div>
        <MetricList rows={x.offense} />
        <div className="px-4 pt-3 text-xs uppercase tracking-wider text-gray-500">Defensive tendencies</div>
        <MetricList rows={x.defense} />
      </div>))}</div>
  );
}

function Situations({ d }: { d: GameDetail }) {
  const s = d.analysis?.situations;
  if (!s) return <Msg>No situational data.</Msg>;
  return (
    <div className="grid lg:grid-cols-2 gap-3">{s.map((x) => (
      <div key={x.team} className="rounded-lg border border-glass bg-surface/60 overflow-hidden">
        <div className="px-4 py-3 border-b border-glass font-semibold">{x.team}</div>
        <div className="px-4 pt-3 text-xs uppercase tracking-wider text-gray-500">On offense</div><MetricList rows={x.offense} />
        <div className="px-4 pt-3 text-xs uppercase tracking-wider text-gray-500">On defense (allowed)</div><MetricList rows={x.defense} />
      </div>))}</div>
  );
}

function TeamStats({ d }: { d: GameDetail }) {
  const t = d.analysis?.team_stats;
  const e = d.event;
  if (!t) return <Msg>No team stats.</Msg>;
  return (
    <div className="overflow-x-auto rounded-lg border border-glass">
      <table className="w-full text-sm">
        <thead className="text-[10px] uppercase tracking-wider text-gray-500 bg-surface"><tr>
          <th className="text-left px-4 py-2">Stat</th><th className="px-3 py-2">{e.away.abbr} offense</th><th className="px-3 py-2">{e.away.abbr} defense</th>
          <th className="px-3 py-2">{e.home.abbr} offense</th><th className="px-3 py-2">{e.home.abbr} defense</th></tr></thead>
        <tbody>{t.rows.map((r) => (
          <tr key={r.key} className="border-t border-glass/60">
            <td className="px-4 py-1.5 text-gray-300">{r.label}<Help text={r.help} /></td>
            <RankCell m={r.away_off} /><RankCell m={r.away_def} /><RankCell m={r.home_off} /><RankCell m={r.home_def} />
          </tr>))}</tbody>
      </table>
    </div>
  );
}

function Weather({ d }: { d: GameDetail }) {
  const e = d.event;
  const gi = d.analysis?.game_info;
  const indoor = e.venue.indoor || gi?.roof === "dome" || gi?.roof === "closed";
  return (
    <div className="grid md:grid-cols-2 gap-3">
      <div className="rounded-lg border border-glass bg-surface/60 p-5 space-y-3">
        <div className="flex items-center gap-3">
          {indoor ? <Home size={28} className="text-gray-400" /> : <CloudSun size={28} className="text-amber" />}
          <div>
            <div className="text-2xl font-mono">{indoor ? "Indoor" : e.weather?.temp != null ? `${Math.round(e.weather.temp)}°F` : gi?.temp != null ? `${gi.temp}°F` : "—"}</div>
            <div className="text-sm text-gray-400">{indoor ? "Climate controlled" : e.weather?.text ?? "Forecast posts a few days before kickoff"}</div>
          </div>
        </div>
        {gi?.wind != null && <div className="flex items-center gap-2 text-sm text-gray-300"><Wind size={14} />Wind {gi.wind} mph</div>}
        {!indoor && <div className="text-[11px] text-gray-500">Forecast from ESPN/AccuWeather at kickoff time. Wind above ~15 mph historically trims passing and kicking.</div>}
      </div>
      <div className="rounded-lg border border-glass bg-surface/60 overflow-hidden">
        {e.venue.image && <img src={e.venue.image} alt="" className="w-full h-32 object-cover opacity-80" />}
        <div className="p-4 text-sm space-y-1">
          <div className="font-semibold">{e.venue.name}</div>
          <div className="text-gray-400">{[e.venue.city, e.venue.state].filter(Boolean).join(", ")}</div>
          {gi && <div className="text-gray-400">{[gi.roof && `Roof: ${gi.roof}`, gi.surface && `Surface: ${gi.surface.replace(/_/g, " ")}`].filter(Boolean).join(" · ")}</div>}
          {e.venue.grass != null && !gi && <div className="text-gray-400">{e.venue.grass ? "Grass" : "Artificial turf"}</div>}
          {gi && (gi.home_rest != null || gi.referee) && <div className="text-gray-400">{[gi.away_rest != null && `Rest: ${e.away.abbr} ${gi.away_rest}d / ${e.home.abbr} ${gi.home_rest}d`, gi.div_game && "Division game", gi.referee && `Referee ${gi.referee}`].filter(Boolean).join(" · ")}</div>}
        </div>
      </div>
    </div>
  );
}
