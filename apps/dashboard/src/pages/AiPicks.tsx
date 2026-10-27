import { useEffect, useMemo, useState, type ReactNode } from "react";
import { Link } from "react-router-dom";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import clsx from "clsx";
import { ExternalLink, X } from "lucide-react";
import { Card, Msg, PageTitle, Pills, Table, Td, Th } from "../components/ui";
import {
  BANKROLL_KEY, GRADES, TIER_TEXT, money, num, odds, pct, picksApi, readBankroll, signedPct, splitWriteup, startLabel, teamLogo,
  type Edge, type Pick, type PickDetail, type PickFilters, type Tier,
} from "../lib/picks";

// AI Picks: the curated feed. Every pick is a live two-way line priced by the
// champion engine's frozen forecast; the grade comes from the edge and is capped
// by how that engine has actually done on that market (backtests, then settled picks).

const SPORTS = [
  { key: "all", label: "All" },
  { key: "nba", label: "NBA" },
  { key: "nhl", label: "NHL" },
  { key: "nfl", label: "NFL" },
  { key: "mlb", label: "MLB" },
];
const KINDS = [{ key: "all", label: "All" }, { key: "prop", label: "Player Props" }, { key: "game", label: "Game Lines" }];
const SIDES = [{ key: "all", label: "All" }, { key: "over", label: "Over" }, { key: "under", label: "Under" }];
const SORTS = [{ key: "grade", label: "Grade" }, { key: "edge", label: "Edge" }, { key: "prob", label: "Prob" }, { key: "odds", label: "Odds" }];
const TABS = [{ key: "picks", label: "Picks" }, { key: "edges", label: "Stale lines" }, { key: "record", label: "Track record" }];

const gradeTone = (g: string) =>
  g.startsWith("A") ? "bg-emerald/15 text-emerald-light border-emerald/40" : g.startsWith("B") ? "bg-violet/15 text-violet border-violet/40" : "bg-amber/15 text-amber border-amber/40";
const tierTone: Record<Tier, string> = {
  proven: "text-emerald-light", baseline: "text-violet", unproven: "text-amber", losing: "text-crimson-light",
};

export function AiPicks() {
  const [tab, setTab] = useState(new URLSearchParams(window.location.search).get("tab") ?? "picks");
  const [search, setSearch] = useState("");
  const [bankroll, setBankroll] = useState(readBankroll);
  const saveBankroll = (v: number) => { setBankroll(v); try { localStorage.setItem(BANKROLL_KEY, String(v)); } catch { /* private window */ } };
  return (
    <div className="space-y-4">
      <PageTitle
        title="AI Picks"
        subtitle="model-backed plays · probability, edge and the reasoning behind each pick"
        right={
          <>
            <Pills options={TABS} value={tab} onChange={setTab} />
            <Link to="/projections" className="text-xs text-gray-400 hover:text-gray-200 px-2">Projections →</Link>
            <label className="flex items-center gap-1 text-xs text-gray-500" title="Stakes are shown as dollars of this bankroll (stored in this browser only)">
              Bankroll $
              <input type="number" min={1} value={bankroll} onChange={(e) => Number(e.target.value) > 0 && saveBankroll(Number(e.target.value))}
                className="w-20 rounded-md border border-glass bg-surface px-2 py-1 text-xs font-mono outline-none focus:border-violet/50" />
            </label>
            {tab === "picks" && (
              <input value={search} onChange={(e) => setSearch(e.target.value)} placeholder="Search picks…"
                className="w-48 rounded-md border border-glass bg-surface px-2.5 py-1 text-xs outline-none focus:border-violet/50" />
            )}
          </>
        }
      />
      {tab === "picks" ? <Feed search={search} bankroll={bankroll} /> : tab === "edges" ? <StaleLines bankroll={bankroll} /> : <Record />}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Feed
// ---------------------------------------------------------------------------

function useDebounced<T>(v: T, ms = 300) {
  const [d, setD] = useState(v);
  useEffect(() => { const t = setTimeout(() => setD(v), ms); return () => clearTimeout(t); }, [v, ms]);
  return d;
}

function Feed({ search, bankroll }: { search: string; bankroll: number }) {
  const qc = useQueryClient();
  // ?sport=nhl&hidden=1&pick=123 deep-link into the feed
  const url = new URLSearchParams(window.location.search);
  const [sport, setSport] = useState(url.get("sport") ?? "all");
  const [kind, setKind] = useState("all");
  const [side, setSide] = useState("all");
  const [grades, setGrades] = useState<string[]>([]);
  const [market, setMarket] = useState("all");
  const [game, setGame] = useState("all");
  const [book, setBook] = useState("all");
  const [minEdge, setMinEdge] = useState(0);
  const [minProb, setMinProb] = useState(0);
  const [sort, setSort] = useState("grade");
  const [hidden, setHidden] = useState(url.get("hidden") === "1");
  const [open, setOpen] = useState<number | null>(url.get("pick") ? Number(url.get("pick")) : null);
  const q = useDebounced(search);
  const edge = useDebounced(minEdge);
  const prob = useDebounced(minProb);

  const s = sport === "all" ? undefined : sport;
  const filters: PickFilters = {
    sport: s, kind: kind === "all" ? undefined : kind, side: side === "all" ? undefined : side,
    grades: grades.length ? grades.join(",") : undefined, market, game, book,
    min_edge: edge > 0 ? edge / 100 : undefined, min_prob: prob > 0 ? prob / 100 : undefined, sort, q, include_hidden: hidden,
  };
  const picks = useQuery({ queryKey: ["picks", filters], queryFn: () => picksApi.list(filters), refetchInterval: 60_000 });
  const meta = useQuery({ queryKey: ["picks-meta", s], queryFn: () => picksApi.meta(s), refetchInterval: 120_000 });
  const run = useMutation({
    mutationFn: () => picksApi.run(s, true),
    onSuccess: () => ["picks", "picks-meta"].forEach((k) => qc.invalidateQueries({ queryKey: [k] })),
  });
  useEffect(() => { setMarket("all"); setGame("all"); }, [sport]);
  const m = meta.data;
  const toggleGrade = (g: string) => setGrades((xs) => (xs.includes(g) ? xs.filter((x) => x !== g) : [...xs, g]));

  return (
    <div className="space-y-4">
      <Card className="space-y-3">
        <div className="flex flex-wrap items-center gap-2">
          <Pills options={SPORTS} value={sport} onChange={setSport} />
          <Pills options={KINDS} value={kind} onChange={setKind} />
          <Pills options={SIDES} value={side} onChange={setSide} />
          <div className="flex rounded-md border border-glass overflow-hidden">
            {GRADES.map((g) => (
              <button key={g} onClick={() => toggleGrade(g)}
                className={clsx("px-2 py-1 text-xs font-mono transition-colors", grades.includes(g) ? gradeTone(g) : "text-gray-500 hover:bg-white/5")}>{g}</button>
            ))}
          </div>
        </div>
        <div className="flex flex-wrap items-center gap-3">
          <Select value={market} onChange={setMarket} options={[{ id: "all", label: "All Markets" }, ...(m?.markets ?? [])]} />
          <Select value={game} onChange={setGame} options={[{ id: "all", label: "All Games" }, ...(m?.games ?? [])]} />
          <Select value={book} onChange={setBook} options={[{ id: "all", label: "All Books" }, ...(m?.books ?? [])]} />
          <Slider label="Edge" value={minEdge} max={20} onChange={setMinEdge} />
          <Slider label="Prob" value={minProb} max={90} onChange={setMinProb} />
          <div className="ml-auto flex items-center gap-2 text-xs text-gray-500">Sort <Pills options={SORTS} value={sort} onChange={setSort} /></div>
        </div>
        <div className="flex flex-wrap items-center gap-3 text-[11px] text-gray-500">
          <label className="flex items-center gap-1.5 cursor-pointer">
            <input type="checkbox" checked={hidden} onChange={(e) => setHidden(e.target.checked)} className="accent-violet-500" />
            Include unproven models{m?.counts ? ` (${m.counts.hidden} of ${m.counts.total} open picks are hidden)` : ""}
          </label>
          <span>·</span>
          <Freshness meta={m} />
          <span>·</span>
          <span>writeups: {!m ? "…" : m.llm_writeups.reachable ? `MonsterGPT · ${m.llm_writeups.model} (every number checked)` : m.llm_writeups.configured ? `fact templates · ${m.llm_writeups.model} is configured but not reachable` : "fact templates (set PICKS_LLM_BASE_URL for MonsterGPT)"}</span>
          <button onClick={() => run.mutate()} disabled={run.isPending}
            className="ml-auto rounded-md border border-glass px-2 py-1 text-gray-300 hover:bg-white/5 disabled:opacity-40">
            {run.isPending ? "Refreshing lines…" : "Refresh lines now"}
          </button>
        </div>
        {run.error && <div className="text-xs text-crimson-light">{(run.error as Error).message}</div>}
      </Card>

      {picks.error && <Msg tone="error">{(picks.error as Error).message}</Msg>}
      {picks.isLoading && <Msg>Loading picks…</Msg>}
      {picks.data && picks.data.picks.length === 0 && (
        <Msg>
          No picks match.{" "}
          {!hidden && m?.counts && m.counts.hidden > 0
            ? `${m.counts.hidden} picks come from engines without a track record on their market and are hidden; tick "Include unproven models" to see them as leans.`
            : "Picks appear when a champion engine has frozen a forecast for a game and a book's line disagrees with it by at least the minimum edge."}
        </Msg>
      )}
      <div className="grid gap-4 md:grid-cols-2 xl:grid-cols-3">
        {picks.data?.picks.map((p) => <PickCard key={p.id} p={p} bankroll={bankroll} onOpen={() => setOpen(p.id)} />)}
      </div>
      {open != null && <PickModal id={open} bankroll={bankroll} onClose={() => setOpen(null)} />}
    </div>
  );
}

function Select({ value, onChange, options }: { value: string; onChange: (v: string) => void; options: { id: string; label: string }[] }) {
  return (
    <select value={value} onChange={(e) => onChange(e.target.value)}
      className="rounded-md border border-glass bg-surface px-2.5 py-1.5 text-xs text-gray-300 outline-none focus:border-violet/50 max-w-[16rem]">
      {options.map((o) => <option key={o.id} value={o.id}>{o.label}</option>)}
    </select>
  );
}

function Slider({ label, value, max, onChange }: { label: string; value: number; max: number; onChange: (v: number) => void }) {
  return (
    <label className="flex items-center gap-2 text-xs">
      <span className="text-violet">{label}</span>
      <input type="range" min={0} max={max} step={1} value={value} onChange={(e) => onChange(Number(e.target.value))} className="w-28 accent-violet-500" />
      <span className="font-mono text-violet w-8">{value}%</span>
    </label>
  );
}

function Freshness({ meta }: { meta?: { lines: { source: string; sport: string; last: string }[] } }) {
  if (!meta || !meta.lines.length) return <span>no lines fetched in the last day</span>;
  const by: Record<string, number> = {};
  for (const l of meta.lines) by[l.source] = Math.max(by[l.source] ?? 0, new Date(l.last).getTime());
  const name: Record<string, string> = { espn: "ESPN", oddsapi: "Odds API", sgo: "SportsGameOdds" };
  return <span>lines: {Object.entries(by).map(([k, t]) => `${name[k] ?? k} ${ago(t)}`).join(" · ")}</span>;
}

const ago = (t: number) => {
  const m = Math.round((Date.now() - t) / 60000);
  return m < 1 ? "just now" : m < 60 ? `${m} min ago` : `${Math.round(m / 60)} h ago`;
};

function Avatar({ p, size = 44 }: { p: Pick; size?: number }) {
  const [bad, setBad] = useState(false);
  const team = p.kind === "prop" ? p.subject_team : p.side === "home" ? p.home : p.side === "away" ? p.away : p.home;
  const src = !bad ? (p.headshot ?? teamLogo(p.sport, team)) : null;
  return (
    <div className="relative shrink-0 rounded-full bg-white/5 border border-glass overflow-hidden flex items-center justify-center" style={{ width: size, height: size }}>
      {src ? <img src={src} alt="" className="w-full h-full object-cover" onError={() => setBad(true)} />
        : <span className="text-[11px] text-gray-400">{(team ?? p.subject_name).slice(0, 3)}</span>}
    </div>
  );
}

function Chip({ children, tone }: { children: ReactNode; tone?: "violet" | "pos" | "neg" | "plain" }) {
  return (
    <span className={clsx("rounded-md border px-2 py-0.5 text-xs font-mono",
      tone === "pos" ? "border-emerald/30 bg-emerald/10 text-emerald-light" : tone === "neg" ? "border-crimson/30 bg-crimson/10 text-crimson-light"
        : tone === "plain" ? "border-glass text-gray-300" : "border-violet/30 bg-violet/10 text-violet")}>{children}</span>
  );
}

function ProjChip({ p }: { p: Pick }) {
  if (p.projection == null || p.line == null || p.market.startsWith("moneyline")) return null;
  if (p.market.startsWith("spread")) {
    // projection is the home margin; show it from the picked team's side
    const m = p.side === "home" ? p.projection : -p.projection;
    return <Chip>Proj margin {m >= 0 ? "+" : ""}{num(m)}</Chip>;
  }
  const d = p.projection - p.line;
  return <Chip>Proj {num(p.projection, 2)} <span className="text-gray-500">/ {num(p.line)}</span> <span className={d >= 0 ? "text-emerald-light" : "text-crimson-light"}>({d >= 0 ? "+" : ""}{d.toFixed(2)})</span></Chip>;
}

/** Suggested stake: fraction of bankroll and dollars. */
function StakeChip({ stake, bankroll }: { stake: number | null; bankroll: number }) {
  if (stake == null) return null;
  if (stake <= 0) return <Chip tone="plain"><span className="text-gray-500" title="No stake: the engine has no track record on this market, or Pinnacle's price makes the bet negative">No stake</span></Chip>;
  return <Chip tone="plain"><span title="Fractional Kelly on the blended probability, scaled by track record">Stake {pct(stake, 1)} · {money(stake * bankroll)}</span></Chip>;
}

function PickCard({ p, bankroll, onOpen }: { p: Pick; bankroll: number; onOpen: () => void }) {
  const title = p.kind === "prop" ? p.subject_name : p.game_label;
  return (
    <div onClick={onOpen} className="cursor-pointer rounded-xl border border-glass bg-surface/60 p-4 space-y-3 hover:border-violet/40 transition-colors flex flex-col">
      <div className="flex items-center gap-2">
        <Avatar p={p} />
        <span className={clsx("rounded border px-1.5 py-0.5 text-xs font-semibold", gradeTone(p.grade))}>{p.grade}</span>
        <span className="rounded bg-white/5 px-1.5 py-0.5 text-[11px] uppercase text-gray-400">{p.sport}</span>
        {p.tier !== "proven" && <span className={clsx("text-[10px]", tierTone[p.tier])} title={TIER_TEXT[p.tier]}>{p.tier}</span>}
        <span className="ml-auto text-xs text-gray-400">{p.book_title}</span>
        <span className="font-mono text-violet">{odds(p.price)}</span>
      </div>
      <div>
        <div className="font-semibold leading-tight">{title}</div>
        <div className="text-sm text-gray-300">{p.label}</div>
        <div className="text-xs text-gray-500 mt-0.5">{p.away} @ {p.home} · {startLabel(p.starts_at)}</div>
      </div>
      <div className="flex flex-wrap gap-1.5">
        <ProjChip p={p} />
        <Chip>Prob {pct(p.p_model)}</Chip>
        <Chip tone="pos">Edge {signedPct(p.edge)}</Chip>
        <StakeChip stake={p.stake} bankroll={bankroll} />
        <PlayableChip p={p} />
      </div>
      <p className="text-xs text-gray-400 line-clamp-2 flex-1">{splitWriteup(p.writeup).case_for}</p>
      <BetButton p={p} />
    </div>
  );
}

/** Price sensitivity: how far the price can drift before the pick is no longer worth it. */
function PlayableChip({ p }: { p: Pick }) {
  if (p.playable_price == null) return null;
  if (p.playable_now === false) return <Chip tone="plain"><span className="text-amber" title={`The best price now is worse than ${odds(p.playable_price)}`}>Price gone · was playable to {odds(p.playable_price)}</span></Chip>;
  if (p.playable_now == null && p.status === "open") return <Chip tone="plain"><span className="text-amber" title="The line has moved since publishing; the price floor was for the old line">Line moved · re-check</span></Chip>;
  return <Chip tone="plain"><span title="The worst odds at which this pick still has the minimum expected value">Playable to {odds(p.playable_price)}</span></Chip>;
}

function BetButton({ p }: { p: Pick }) {
  if (!p.link) return <div className="rounded-md border border-glass bg-white/[0.02] py-2 text-center text-xs text-gray-500">No link available</div>;
  return (
    <a href={p.link} target="_blank" rel="noreferrer noopener" onClick={(e) => e.stopPropagation()}
      className="flex items-center justify-center gap-1.5 rounded-md border border-violet/40 bg-violet/15 py-2 text-sm text-violet hover:bg-violet/25">
      Bet {odds(p.price)} at {p.book_title} <ExternalLink size={13} />
    </a>
  );
}

// ---------------------------------------------------------------------------
// Detail
// ---------------------------------------------------------------------------

function PickModal({ id, bankroll, onClose }: { id: number; bankroll: number; onClose: () => void }) {
  const q = useQuery({ queryKey: ["pick", id], queryFn: () => picksApi.get(id) });
  useEffect(() => {
    const k = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    window.addEventListener("keydown", k);
    return () => window.removeEventListener("keydown", k);
  }, [onClose]);
  return (
    <div className="fixed inset-0 z-50 bg-black/70 flex items-start justify-center overflow-y-auto p-4 md:p-10" onClick={onClose}>
      <div className="w-full max-w-4xl rounded-xl border border-glass bg-obsidian p-5 space-y-5" onClick={(e) => e.stopPropagation()}>
        <button onClick={onClose} className="float-right text-gray-400 hover:text-gray-200" aria-label="Close"><X size={18} /></button>
        {q.error && <Msg tone="error">{(q.error as Error).message}</Msg>}
        {q.isLoading && <Msg>Loading…</Msg>}
        {q.data && <Detail p={q.data} bankroll={bankroll} />}
      </div>
    </div>
  );
}

function Detail({ p, bankroll }: { p: PickDetail; bankroll: number }) {
  const f = p.facts;
  const moved = p.latest && p.latest.line != null && p.line != null && p.latest.line !== p.line;
  const w = splitWriteup(p.writeup);
  return (
    <>
      <div className="flex items-center gap-4">
        <Avatar p={p} size={64} />
        <div className="space-y-1">
          <div className="flex items-center gap-2">
            <span className={clsx("rounded border px-2 py-0.5 text-sm font-semibold", gradeTone(p.grade))}>{p.grade}</span>
            <span className="rounded bg-white/5 px-1.5 py-0.5 text-xs uppercase text-gray-400">{p.sport}</span>
            <span className={clsx("text-xs", tierTone[p.tier])}>{TIER_TEXT[p.tier]}</span>
          </div>
          <div className="text-xl font-semibold">{p.kind === "prop" ? p.subject_name : p.game_label}</div>
          <div className="text-sm text-gray-400">{p.label} · {p.away} @ {p.home} · {startLabel(p.starts_at)}</div>
        </div>
      </div>

      <div className="flex flex-wrap gap-2">
        <Box><span className="text-gray-400 text-xs">{p.book_title}</span> <span className="font-mono text-violet text-lg">{odds(p.price)}</span></Box>
        <Box tone="violet"><span className="text-xs text-gray-400">MODEL PROB</span> <span className="font-mono text-lg">{pct(p.p_model, 1)}</span></Box>
        <Box><span className="text-xs text-gray-400">MARKET (NO-VIG)</span> <span className="font-mono text-lg">{pct(p.p_market, 1)}</span></Box>
        <Box tone="pos"><span className="text-xs text-gray-400">EDGE</span> <span className="font-mono text-lg text-emerald-light">{signedPct(p.edge)}</span></Box>
        <Box><span className="text-xs text-gray-400">EV / UNIT</span> <span className="font-mono text-lg">{signedPct(p.ev)}</span></Box>
        {p.projection != null && !p.market.startsWith("moneyline") && (
          <Box tone="violet"><span className="text-xs text-gray-400">{p.market.startsWith("spread") ? "PROJ MARGIN (HOME)" : "PROJ"}</span> <span className="font-mono text-lg">{num(p.projection, 2)}</span>
            {p.line != null && !p.market.startsWith("spread") && <span className="text-xs text-gray-500"> / {num(p.line)}</span>}</Box>
        )}
      </div>

      <section className="space-y-2">
        <h3 className="text-xs uppercase tracking-wide text-gray-400">MonsterGPT analysis <span className="normal-case text-gray-600">· {p.writeup_source === "template" ? "written from the facts below" : `${p.writeup_source.replace("llm:", "")}, every number checked against the facts`}</span></h3>
        <div className="rounded-lg border border-glass bg-surface/60 p-4 text-sm leading-relaxed text-gray-200">{w.case_for}</div>
      </section>

      {w.against && (
        <section className="space-y-2">
          <h3 className="text-xs uppercase tracking-wide text-gray-400">What could make it wrong</h3>
          <div className="rounded-lg border border-amber/30 bg-amber/5 p-4 text-sm leading-relaxed text-gray-200">{w.against}</div>
        </section>
      )}

      {p.risk_flags.length > 0 && (
        <section className="space-y-2">
          <h3 className="text-xs uppercase tracking-wide text-gray-400">Risk flags</h3>
          <ul className="space-y-1 text-sm">
            {p.risk_flags.map((r) => <li key={r.code} className="flex gap-2"><span className="text-amber">▲</span><span className="text-gray-300">{r.text}</span></li>)}
          </ul>
        </section>
      )}

      <section className="grid gap-4 md:grid-cols-2">
        <Card className="space-y-2">
          <div className="text-xs uppercase tracking-wide text-gray-400">Model</div>
          <Row k="Engine" v={`${p.engine} · ${p.model_version}`} />
          <Row k="Lineups" v={f.lineup_mode ?? "—"} />
          {f.q10 != null && <Row k="Median · 80% range" v={`${num(f.q50)} · ${num(f.q10)}–${num(f.q90)}`} />}
          {f.p_raw != null && <Row k="Engine alone · blended" v={`${pct(f.p_raw, 1)} · ${pct(p.p_model, 1)}`} />}
          {f.p_naive != null && <Row k="Own prior frequency" v={`${pct(f.p_naive, 1)} (naive baseline${f.w_naive != null ? `, model weight ${f.w_naive.toFixed(2)}` : ""})`} />}
          {f.w_market != null && <Row k="Trust vs market" v={`${f.w_market.toFixed(2)} · ${f.blend_text ?? ""}`} />}
          {f.p_active != null && <Row k="Plays in" v={pct(f.p_active, 1)} />}
          {f.p_push != null && f.p_push > 0 && <Row k="Push chance" v={pct(f.p_push, 1)} />}
          {f.proj_minutes != null && <Row k="Minutes (sim · last 5)" v={`${num(f.proj_minutes)} · ${num(f.l5_minutes)}`} />}
          <Row k="Prediction" v={<span>ledger #{p.ledger_id}</span>} />
        </Card>
        <Card className="space-y-2">
          <div className="text-xs uppercase tracking-wide text-gray-400">Market</div>
          <Row k="Published" v={`${num(p.line)} at ${odds(p.price)} (${p.book_title}) · ${new Date(p.published_at).toLocaleString()}`} />
          {p.latest && <Row k="Now" v={<span className={moved ? "text-amber" : ""}>{num(p.latest.line)} at {odds(p.latest.price)} · edge {signedPct(p.latest.edge)}</span>} />}
          {p.fair_price != null && <Row k="Fair price (break-even)" v={odds(p.fair_price)} />}
          {p.playable_price != null && <Row k="Playable to" v={<span className={p.playable_now === false ? "text-amber" : ""}>{odds(p.playable_price)}{f.playable_min_ev != null ? ` (+${pct(f.playable_min_ev, 0)} EV or better)` : ""}{p.playable_now === false ? " · price now worse" : p.playable_now == null && p.status === "open" ? " · line moved, re-check" : ""}</span>} />}
          {f.open_line != null && <Row k="Opened at" v={num(f.open_line)} />}
          <Row k="Books quoting" v={f.books_quoting ?? 1} />
          {f.p_sharp != null && <Row k="Sharp check" v={<span className={(f.sharp_ev ?? 0) < 0 ? "text-amber" : "text-emerald-light"}>{f.sharp_text}: {pct(f.p_sharp, 1)} · {signedPct(f.sharp_ev)} per unit at this price</span>} />}
          {p.stake != null && <Row k="Suggested stake" v={p.stake > 0 ? `${pct(p.stake, 2)} of bankroll · ${money(p.stake * bankroll)}` : "none"} />}
          {p.status !== "open" && <Row k="Result" v={p.result ? `${p.result}${p.actual != null ? ` (actual ${num(p.actual)})` : ""} · ${p.profit != null ? `${p.profit >= 0 ? "+" : ""}${p.profit.toFixed(2)}u` : ""}${p.clv != null ? ` · CLV ${signedPct(p.clv)}` : ""}` : p.status} />}
        </Card>
      </section>

      {p.kind === "prop" && f.game_log && f.game_log.length > 0 && p.line != null && (
        <section className="space-y-2">
          <h3 className="text-xs uppercase tracking-wide text-gray-400">Last {f.game_log.length} games vs {num(p.line)}
            {f.l10_hits != null && <span className="normal-case text-gray-500"> · {p.side} in {f.l10_hits}/{f.l10_games} · avg {num(f.l10_avg)}{f.l20_avg != null ? ` (L20 ${num(f.l20_avg)})` : ""}</span>}</h3>
          <GameLog log={f.game_log} line={p.line} side={p.side} />
        </section>
      )}
      {p.kind === "game" && (f.home_form || f.away_form) && (
        <section className="grid gap-4 md:grid-cols-2">
          {([["home", p.home, f.home_form], ["away", p.away, f.away_form]] as const).map(([k, t, v]) => v && (
            <Card key={k} className="space-y-1"><div className="text-xs uppercase text-gray-400">{t} · last {v.games}</div>
              <div className="text-sm">{v.w}-{v.l} · {num(v.pf)} scored · {num(v.pa)} allowed per game</div></Card>
          ))}
        </section>
      )}

      {p.books && p.books.length > 0 && (
        <section className="space-y-2">
          <h3 className="text-xs uppercase tracking-wide text-gray-400">Every book's current price</h3>
          <Table head={<tr><Th>Book</Th><Th>Side</Th><Th>Line</Th><Th>Price</Th><Th>Seen</Th><Th> </Th></tr>}>
            {p.books.map((b) => (
              <tr key={b.book + b.side} className={clsx("border-t border-glass", b.side === p.side && "bg-violet/5")}>
                <Td>{b.book_title}</Td><Td className="capitalize">{b.side}</Td><Td className="font-mono">{num(b.line)}</Td>
                <Td className="font-mono">{odds(b.price)}</Td><Td className="text-xs text-gray-500">{ago(new Date(b.observed_at).getTime())}</Td>
                <Td>{b.link ? <a href={b.link} target="_blank" rel="noreferrer noopener" className="text-violet text-xs inline-flex items-center gap-1">bet <ExternalLink size={11} /></a> : <span className="text-xs text-gray-600">no link</span>}</Td>
              </tr>
            ))}
          </Table>
        </section>
      )}
      <BetButton p={p} />
    </>
  );
}

function Box({ children, tone }: { children: ReactNode; tone?: "violet" | "pos" }) {
  return <div className={clsx("rounded-lg border px-3 py-2 flex items-baseline gap-2", tone === "violet" ? "border-violet/30 bg-violet/10" : tone === "pos" ? "border-emerald/30 bg-emerald/5" : "border-glass bg-surface/60")}>{children}</div>;
}
function Row({ k, v }: { k: string; v: ReactNode }) {
  return <div className="flex justify-between gap-3 text-sm"><span className="text-gray-500">{k}</span><span className="text-right text-gray-200">{v}</span></div>;
}

function GameLog({ log, line, side }: { log: { opp: string; value: number | null }[]; line: number; side: string }) {
  const max = Math.max(line * 1.6, ...log.map((g) => g.value ?? 0), 1);
  return (
    <div className="flex items-end gap-1.5 h-32 rounded-lg border border-glass bg-surface/60 px-3 pt-3 pb-6 relative">
      <div className="absolute left-3 right-3 border-t border-dashed border-amber/60" style={{ bottom: `${24 + (line / max) * 104}px` }} />
      {[...log].reverse().map((g, i) => {
        const v = g.value ?? 0;
        const hit = side === "over" ? v > line : v < line;
        return (
          <div key={i} className="flex-1 flex flex-col items-center justify-end h-full relative">
            <div className="text-[10px] font-mono text-gray-300">{num(v)}</div>
            <div className={clsx("w-full rounded-t", hit ? "bg-emerald/60" : "bg-crimson/50")} style={{ height: `${(v / max) * 104}px` }} />
            <div className="absolute -bottom-5 text-[9px] text-gray-500">{g.opp}</div>
          </div>
        );
      })}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Track record
// ---------------------------------------------------------------------------

function Record() {
  const [sport, setSport] = useState("all");
  const [days, setDays] = useState("90");
  const s = sport === "all" ? undefined : sport;
  const q = useQuery({ queryKey: ["picks-record", s, days], queryFn: () => picksApi.record(s, Number(days)) });
  const d = q.data;
  const top = useMemo(() => d?.rows.filter((r) => r.kind == null) ?? [], [d]);
  const byMarket = useMemo(() => d?.rows.filter((r) => r.market != null && r.tier == null) ?? [], [d]);
  return (
    <div className="space-y-4">
      <div className="flex items-center gap-2">
        <Pills options={SPORTS} value={sport} onChange={setSport} />
        <Pills options={[{ key: "30", label: "30d" }, { key: "90", label: "90d" }, { key: "365", label: "1y" }, { key: "3650", label: "All" }]} value={days} onChange={setDays} />
      </div>
      <p className="text-xs text-gray-500 max-w-3xl">
        Picks are scored at the price they were published at (1 unit each); Kelly ROI weights each pick by its suggested stake. CLV compares the de-vigged closing price at the same book with the published one; positive means the market moved toward the pick.
        Brier vs market below zero means the model's probabilities beat the market's on these picks. Grades unlock automatically: an engine-market pair is "proven" after 150+ settled picks that beat the market by more than two standard errors.
      </p>
      {q.error && <Msg tone="error">{(q.error as Error).message}</Msg>}
      {d && (
        <>
          <Table head={<tr><Th>Scope</Th><Th>Graded</Th><Th>W-L-P</Th><Th>Win %</Th><Th>Units</Th><Th>ROI</Th><Th>Kelly ROI</Th><Th>CLV</Th><Th>Brier vs mkt</Th></tr>}>
            {[...top, ...byMarket].map((r, i) => (
              <tr key={i} className="border-t border-glass">
                <Td className="text-xs">{r.sport ? `${r.sport.toUpperCase()}${r.kind ? ` · ${r.kind}` : ""}${r.market ? ` · ${r.market}` : ""}` : "All picks"}</Td>
                <Td className="font-mono">{r.graded}</Td>
                <Td className="font-mono">{r.wins}-{r.losses}-{r.pushes}</Td>
                <Td className="font-mono">{pct(r.win_rate, 1)}</Td>
                <Td className={clsx("font-mono", r.units >= 0 ? "text-emerald-light" : "text-crimson-light")}>{r.units >= 0 ? "+" : ""}{r.units.toFixed(2)}</Td>
                <Td className="font-mono">{signedPct(r.roi)}</Td>
                <Td className="font-mono">{signedPct(r.kelly_roi)}</Td>
                <Td className="font-mono">{r.clv_n ? `${signedPct(r.clv_mean)} (${r.clv_n})` : "—"}</Td>
                <Td className="font-mono">{r.brier_vs_market == null ? "—" : r.brier_vs_market.toFixed(4)}</Td>
              </tr>
            ))}
            {top.length === 0 && <tr><Td className="text-gray-500 text-sm">No settled picks yet.</Td></tr>}
          </Table>
          <h2 className="font-medium pt-2">What each grade may claim</h2>
          <div className="grid gap-3 md:grid-cols-3">
            {d.tiers.map((t) => (
              <div key={t.engine + t.market} className="rounded-lg border border-glass bg-surface/60 px-3 py-2 text-xs flex items-center gap-2">
                <span className="text-gray-300">{t.engine.replace("_team", " team")} · {t.market}</span>
                <span className={clsx("ml-auto", tierTone[t.tier])}>{t.tier}</span>
                <span className="font-mono text-gray-500">max {t.cap}</span>
              </div>
            ))}
          </div>
          <Table head={<tr><Th>Engine</Th><Th>Market</Th><Th>Evidence</Th><Th>vs</Th><Th>n</Th><Th>Seasons better / worse</Th><Th>Brier diff ± SE</Th><Th>Engine version</Th></tr>}>
            {d.evidence.map((e) => (
              <tr key={e.engine + e.market + e.source + e.comparison} className="border-t border-glass text-xs">
                <Td>{e.engine}</Td><Td>{e.market}</Td><Td>{e.source}</Td><Td>{e.comparison}</Td><Td className="font-mono">{e.n}</Td>
                <Td className="font-mono">{e.source === "backtest" ? `${e.better} / ${e.worse} of ${e.seasons}` : "—"}</Td>
                <Td className={clsx("font-mono", (e.metric ?? 0) < 0 ? "text-emerald-light" : "text-crimson-light")}>{e.metric == null ? "—" : `${e.metric >= 0 ? "+" : ""}${e.metric.toFixed(4)} ± ${(e.se ?? 0).toFixed(4)}`}</Td>
                <Td className="font-mono text-gray-500">{String(e.detail.engine_version ?? "")}</Td>
              </tr>
            ))}
          </Table>
        </>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Stale lines
// ---------------------------------------------------------------------------

function StaleLines({ bankroll }: { bankroll: number }) {
  const [sport, setSport] = useState("all");
  const [status, setStatus] = useState("open");
  const s = sport === "all" ? undefined : sport;
  const q = useQuery({ queryKey: ["picks-edges", s, status], queryFn: () => picksApi.edges(s, status), refetchInterval: 60_000 });
  const d = q.data;
  const all = d?.record.find((r) => r.fair_source == null);
  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center gap-2">
        <Pills options={SPORTS} value={sport} onChange={setSport} />
        <Pills options={[{ key: "open", label: "Open" }, { key: "settled", label: "Settled" }]} value={status} onChange={setStatus} />
      </div>
      <p className="text-xs text-gray-500 max-w-3xl">
        No model here. Pinnacle moves first and charges the least vig, so its de-vigged price is the best free estimate of the true odds. A book still offering a price that beats it
        {d ? ` by ${pct(d.settings.min_ev, 0)} EV or more` : ""} is listed (when Pinnacle does not quote the line, the median of {d ? d.settings.consensus_books : 4}+ other books from the same snapshot stands in, at half stake).
        Gaps above {d ? pct(d.settings.max_ev, 0) : "12%"} are dropped as mismatched lines. Stale prices get corrected fast: check the price is still there before betting.
      </p>
      {all && all.graded > 0 && (
        <div className="flex flex-wrap gap-2 text-xs">
          <Chip tone="plain">{all.graded} settled · {all.wins}-{all.losses}-{all.pushes}</Chip>
          <Chip tone={all.units >= 0 ? "pos" : "neg"}>{all.units >= 0 ? "+" : ""}{all.units.toFixed(2)}u · ROI {signedPct(all.roi)}</Chip>
          <Chip tone="plain">Kelly ROI {signedPct(all.kelly_roi)}</Chip>
          <Chip tone="plain">EV at the close {signedPct(all.close_ev_mean)} ({all.close_ev_n})</Chip>
        </div>
      )}
      {q.error && <Msg tone="error">{(q.error as Error).message}</Msg>}
      {q.isLoading && <Msg>Loading…</Msg>}
      {d && d.edges.length === 0 && <Msg>{status === "open" ? "No stale lines right now. They appear when a book lags the sharp price; lines refresh every few hours." : "Nothing settled yet."}</Msg>}
      {d && d.edges.length > 0 && (
        <Table head={<tr><Th>Selection</Th><Th>Game</Th><Th>Book</Th><Th>Price</Th><Th>Fair</Th><Th>EV</Th><Th>Stake</Th>{status === "open" ? <><Th>Now</Th><Th>Seen</Th><Th> </Th></> : <><Th>Result</Th><Th>EV at close</Th></>}</tr>}>
          {d.edges.map((e) => <EdgeRow key={e.id} e={e} bankroll={bankroll} open={status === "open"} />)}
        </Table>
      )}
    </div>
  );
}

function EdgeRow({ e, bankroll, open }: { e: Edge; bankroll: number; open: boolean }) {
  const gone = e.latest.price != null && e.latest.price !== e.price;
  return (
    <tr className="border-t border-glass text-sm">
      <Td><div className="font-medium">{e.kind === "prop" ? e.subject_name : e.label}</div><div className="text-xs text-gray-500">{e.kind === "prop" ? e.label : e.market_label}</div></Td>
      <Td className="text-xs text-gray-400">{e.away} @ {e.home}<div>{startLabel(e.starts_at)}</div></Td>
      <Td className="text-xs">{e.book_title}</Td>
      <Td className="font-mono text-violet">{odds(e.price)}</Td>
      <Td className="text-xs"><span className="font-mono">{pct(e.p_fair, 1)}</span> <span className="text-gray-500">{e.fair_source === "pinnacle" ? "Pinnacle" : `consensus of ${e.fair_books}`}</span></Td>
      <Td className="font-mono text-emerald-light">{signedPct(e.ev)}</Td>
      <Td className="font-mono text-xs">{pct(e.stake, 2)} · {money(e.stake * bankroll)}</Td>
      {open ? (
        <>
          <Td className={clsx("font-mono text-xs", gone && "text-amber")}>{gone ? `${odds(e.latest.price)} (${signedPct(e.latest.ev)})` : "same"}</Td>
          <Td className="text-xs text-gray-500">{ago(new Date(e.observed_at).getTime())}</Td>
          <Td>{e.link ? <a href={e.link} target="_blank" rel="noreferrer noopener" className="text-violet text-xs inline-flex items-center gap-1">bet <ExternalLink size={11} /></a> : <span className="text-xs text-gray-600">no link</span>}</Td>
        </>
      ) : (
        <>
          <Td className={clsx("text-xs", e.result === "win" ? "text-emerald-light" : e.result === "loss" ? "text-crimson-light" : "text-gray-400")}>{e.result ?? e.status}{e.actual != null ? ` (${num(e.actual)})` : ""}</Td>
          <Td className="font-mono text-xs">{signedPct(e.close_ev)}</Td>
        </>
      )}
    </tr>
  );
}
