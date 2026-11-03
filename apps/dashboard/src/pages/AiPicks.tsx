import { useEffect, useMemo, useState, type ReactNode } from "react";
import { Link } from "react-router-dom";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import clsx from "clsx";
import { ExternalLink, X } from "lucide-react";
import { Card, Msg, PageTitle, Pills, Table, Td, Th } from "../components/ui";
import {
  BANKROLL_KEY, GRADES, QUALIFICATIONS, TIER_TEXT, money, num, odds, pct, picksApi, readBankroll, signedPct, splitWriteup, startLabel, teamLogo,
  type Edge, type Experiment, type MarketStatus, type Pick, type PickDetail, type PickFilters, type Qualification, type ScoreMarket, type Scorecard, type Tier,
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
const SORTS = [{ key: "grade", label: "Rated" }, { key: "price", label: "Price now" }, { key: "ev", label: "EV" }, { key: "edge", label: "Edge" }, { key: "prob", label: "Prob" }, { key: "odds", label: "Odds" }];
const TABS = [{ key: "picks", label: "Picks" }, { key: "how", label: "How it works" }, { key: "edges", label: "Stale lines" }, { key: "record", label: "Track record" }, { key: "models", label: "Models" }];
const QUALS = [{ key: "all", label: "Any status" }, ...QUALIFICATIONS.filter((q) => q.key !== "suspended").map((q) => ({ key: q.key, label: q.label }))];
const qualTone: Record<Qualification, string> = {
  verified_edge: "text-emerald-light border-emerald/40", watchlist: "text-violet border-violet/40", high_uncertainty: "text-amber border-amber/40",
  no_value: "text-gray-400 border-glass", insufficient_data: "text-gray-400 border-glass", suspended: "text-crimson-light border-crimson/40",
};
const STATUS_TEXT: Record<MarketStatus, string> = {
  verified: "verified edge", model_only: "beats baseline only", no_signal: "no verified signal", losing: "losing to market", insufficient_data: "insufficient data",
};
const statusTone: Record<MarketStatus, string> = {
  verified: "text-emerald-light", model_only: "text-violet", no_signal: "text-amber", losing: "text-crimson-light", insufficient_data: "text-gray-500",
};

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
      {tab === "picks" ? <Feed search={search} bankroll={bankroll} /> : tab === "how" ? <HowItWorks /> : tab === "edges" ? <StaleLines bankroll={bankroll} /> : tab === "models" ? <Models /> : <Record />}
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
  const [qual, setQual] = useState(url.get("qualification") ?? "all");
  const [open, setOpen] = useState<number | null>(url.get("pick") ? Number(url.get("pick")) : null);
  const q = useDebounced(search);
  const edge = useDebounced(minEdge);
  const prob = useDebounced(minProb);

  const s = sport === "all" ? undefined : sport;
  const filters: PickFilters = {
    sport: s, kind: kind === "all" ? undefined : kind, side: side === "all" ? undefined : side,
    grades: grades.length ? grades.join(",") : undefined, market, game, book,
    min_edge: edge > 0 ? edge / 100 : undefined, min_prob: prob > 0 ? prob / 100 : undefined, sort, q, include_hidden: hidden,
    qualification: qual === "all" ? undefined : qual,
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
          <Select value={qual} onChange={setQual} options={QUALS.map((q) => ({ id: q.key, label: q.label }))} />
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
          {s && <WhyNone sport={s} kind={kind === "all" ? undefined : kind} />}
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

const priceTone = (g: string) => (g === "F" ? "bg-crimson/10 text-crimson-light border-crimson/40" : gradeTone(g));

/** Price grade (is the current price worth it?) beside the rated grade (capped by the market's track record). */
function GradePair({ p, large }: { p: Pick; large?: boolean }) {
  const e = p.evaluation;
  const size = large ? "px-2 py-0.5 text-sm" : "px-1.5 py-0.5 text-xs";
  return (
    <span className="flex items-center gap-1.5">
      {e && (
        <span className={clsx("rounded border font-semibold", size, priceTone(e.price.grade))}
          title={`Price grade: EV per unit at the current price (${signedPct(e.price.ev)}). It changes when the price moves.`}>
          Price {e.price.grade}
        </span>
      )}
      <span className={clsx("rounded border font-semibold", size, gradeTone(p.grade))}
        title={`Rated grade: the edge grade capped by this market's track record (${p.tier}: at most ${e?.trust.cap ?? "?"}). ${TIER_TEXT[p.tier]}`}>
        Rated {p.grade}
      </span>
    </span>
  );
}

/** Step 4 at a glance: how many of the recheck items pass right now. */
function ChecksChip({ p }: { p: Pick }) {
  const e = p.evaluation;
  if (!e) return null;
  const bad = e.checks.filter((c) => c.ok !== true);
  return (
    <Chip tone="plain">
      <span className={e.all_clear ? "text-emerald-light" : "text-amber"} title={bad.length ? bad.map((c) => c.text).join("\n") : "All recheck items pass right now"}>
        {e.all_clear ? "✓" : "▲"} Recheck {e.checks_ok}/{e.checks_total}
      </span>
    </Chip>
  );
}

function PickCard({ p, bankroll, onOpen }: { p: Pick; bankroll: number; onOpen: () => void }) {
  const title = p.kind === "prop" ? p.subject_name : p.game_label;
  const e = p.evaluation;
  const price = e?.price.price ?? p.price;
  return (
    <div onClick={onOpen} className="cursor-pointer rounded-xl border border-glass bg-surface/60 p-4 space-y-3 hover:border-violet/40 transition-colors flex flex-col">
      <div className="flex items-center gap-2">
        <Avatar p={p} />
        <GradePair p={p} />
        <span className="rounded bg-white/5 px-1.5 py-0.5 text-[11px] uppercase text-gray-400">{p.sport}</span>
        <span className="ml-auto text-xs text-gray-400">{p.book_title}</span>
        <span className="font-mono text-violet">{odds(price)}</span>
      </div>
      <div>
        <div className="font-semibold leading-tight">{title}</div>
        <div className="text-sm text-gray-300">{p.label}{e?.price.moved && <span className="text-amber"> · now {num(e.price.line)}</span>}</div>
        <div className="text-xs text-gray-500 mt-0.5">{p.away} @ {p.home} · {startLabel(p.starts_at)}</div>
      </div>
      {/* step 2: model probability against the price */}
      <div className="grid grid-cols-4 gap-1.5 text-center">
        <Stat k="Model" v={pct(e?.price.p_model ?? p.p_model, 1)} />
        <Stat k="Market" v={pct(e?.price.p_market ?? p.p_market, 1)} />
        <Stat k="Edge" v={signedPct(e?.price.edge ?? p.edge)} tone={(e?.price.edge ?? p.edge) > 0 ? "pos" : "neg"} />
        <Stat k="EV now" v={signedPct(e?.price.ev ?? p.ev)} tone={(e?.price.ev ?? p.ev) > 0 ? "pos" : "neg"} />
      </div>
      <div className="flex flex-wrap gap-1.5">
        <ProjChip p={p} />
        {p.qualification && <QualBadge q={p.qualification} />}
        <span className={clsx("self-center text-[10px]", tierTone[p.tier])} title={TIER_TEXT[p.tier]}>track record: {p.tier}</span>
        <StakeChip stake={p.stake} bankroll={bankroll} />
        <PlayableChip p={p} />
        <ChecksChip p={p} />
      </div>
      <p className="text-xs text-gray-400 line-clamp-2 flex-1">{splitWriteup(p.writeup).case_for}</p>
      <BetButton p={p} />
    </div>
  );
}

function Stat({ k, v, tone }: { k: string; v: ReactNode; tone?: "pos" | "neg" }) {
  return (
    <div className="rounded-md border border-glass bg-white/[0.02] py-1">
      <div className="text-[10px] uppercase text-gray-500">{k}</div>
      <div className={clsx("font-mono text-sm", tone === "pos" ? "text-emerald-light" : tone === "neg" ? "text-crimson-light" : "text-gray-200")}>{v}</div>
    </div>
  );
}

function QualBadge({ q }: { q: Qualification }) {
  const d = QUALIFICATIONS.find((x) => x.key === q);
  return <span className={clsx("rounded border px-1.5 py-0.5 text-[10px]", qualTone[q])} title={d?.text}>{d?.label ?? q}</span>;
}

/** Model quality (is the engine reliable here?) and opportunity (is this price worth it?), graded apart. */
function GradesChip({ p }: { p: Pick }) {
  if (!p.model_quality || !p.opportunity_grade) return null;
  return (
    <Chip tone="plain">
      <span title="Model quality: the engine's validated reliability on this market (track record, calibration)">Model {p.model_quality}</span>
      <span className="text-gray-600"> · </span>
      <span title={`Opportunity: EV at the current price${p.ev_now != null ? ` (${signedPct(p.ev_now)})` : ""}`}>Price {p.opportunity_grade}</span>
    </Chip>
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
  const e = p.evaluation;
  return (
    <>
      <div className="flex items-center gap-4">
        <Avatar p={p} size={64} />
        <div className="space-y-1">
          <div className="flex items-center gap-2">
            <GradePair p={p} large />
            <span className="rounded bg-white/5 px-1.5 py-0.5 text-xs uppercase text-gray-400">{p.sport}</span>
            {p.qualification && <QualBadge q={p.qualification} />}
          </div>
          <div className="text-xl font-semibold">{p.kind === "prop" ? p.subject_name : p.game_label}</div>
          <div className="text-sm text-gray-400">{p.label} · {p.away} @ {p.home} · {startLabel(p.starts_at)}</div>
        </div>
      </div>

      <Step n={1} title="Current projection" note={`${p.engine} · ${p.model_version} · frozen before the game (ledger #${p.ledger_id})`}>
        <div className="flex flex-wrap gap-2">
          {p.projection != null && !p.market.startsWith("moneyline") && (
            <Box tone="violet"><span className="text-xs text-gray-400">{p.market.startsWith("spread") ? "PROJ MARGIN (HOME)" : "PROJECTION"}</span> <span className="font-mono text-lg">{num(p.projection, 2)}</span>
              {p.line != null && !p.market.startsWith("spread") && <span className="text-xs text-gray-500"> vs line {num(p.line)}</span>}</Box>
          )}
          {f.q10 != null && <Box><span className="text-xs text-gray-400">MEDIAN · 80% RANGE</span> <span className="font-mono">{num(f.q50)} · {num(f.q10)}–{num(f.q90)}</span></Box>}
          <Box><span className="text-xs text-gray-400">LINEUPS</span> <span className="text-sm">{f.lineup_mode ?? "—"}</span></Box>
        </div>
      </Step>

      <Step n={2} title="Probability vs price" note={e?.price.moved ? "the line has moved: numbers below are at the current line and price" : "numbers below are at the current price"}>
        <div className="flex flex-wrap gap-2">
          <Box><span className="text-gray-400 text-xs">{p.book_title}</span> <span className="font-mono text-violet text-lg">{odds(e?.price.price ?? p.price)}</span>
            {e && e.price.price !== p.price && <span className="text-xs text-gray-500"> published {odds(p.price)}</span>}</Box>
          <Box tone="violet"><span className="text-xs text-gray-400">MODEL PROB</span> <span className="font-mono text-lg">{pct(e?.price.p_model ?? p.p_model, 1)}</span></Box>
          <Box><span className="text-xs text-gray-400">MARKET (NO-VIG)</span> <span className="font-mono text-lg">{pct(e?.price.p_market ?? p.p_market, 1)}</span></Box>
          <Box tone="pos"><span className="text-xs text-gray-400">EDGE</span> <span className="font-mono text-lg text-emerald-light">{signedPct(e?.price.edge ?? p.edge)}</span></Box>
          <Box><span className="text-xs text-gray-400">EV / UNIT</span> <span className="font-mono text-lg">{signedPct(e?.price.ev ?? p.ev)}</span></Box>
          {e && <Box><span className="text-xs text-gray-400">PRICE GRADE</span> <span className="font-mono text-lg">{e.price.grade}</span>
            {e.price.published.grade !== e.price.grade && <span className="text-xs text-gray-500"> was {e.price.published.grade}</span>}</Box>}
        </div>
      </Step>

      <Step n={3} title="The case and the risk" note="why the model likes it, what could make it wrong, and how far to trust this market">
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

      <section className="rounded-lg border border-glass bg-surface/60 p-4 space-y-1 text-sm">
        <div className="flex items-center gap-2"><span className="text-xs uppercase tracking-wide text-gray-400">Trust</span>
          <span className={tierTone[p.tier]}>track record: {p.tier}</span><span className="text-gray-500">· grades capped at {e?.trust.cap ?? "—"}</span><GradesChip p={p} /></div>
        <div className="text-gray-300">{TIER_TEXT[p.tier]}. The rated grade is {p.grade}; the current price on its own grades {e?.price.grade ?? "—"}.</div>
      </section>
      </Step>

      <Step n={4} title="Recheck before acting" note="odds, injuries and lineups can move after a pick is generated; confirm the live market and your own risk limit">
        {e && (
          <ul className="space-y-1 text-sm">
            {e.checks.map((c) => (
              <li key={c.key} className="flex gap-2">
                <span className={c.ok === true ? "text-emerald-light" : c.ok === false ? "text-crimson-light" : "text-amber"}>{c.ok === true ? "✓" : c.ok === false ? "✗" : "?"}</span>
                <span className="text-gray-300">{c.text}</span>
              </li>
            ))}
          </ul>
        )}
      </Step>

      <section className="grid gap-4 md:grid-cols-2">
        <Card className="space-y-2">
          <div className="text-xs uppercase tracking-wide text-gray-400">Model</div>
          <Row k="Engine" v={`${p.engine} · ${p.model_version}`} />
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

function Step({ n, title, note, children }: { n: number; title: string; note?: string; children: ReactNode }) {
  return (
    <section className="space-y-2">
      <h3 className="flex items-baseline gap-2">
        <span className="rounded-full border border-violet/40 bg-violet/10 px-2 text-xs font-mono text-violet">{n}</span>
        <span className="font-medium">{title}</span>
        {note && <span className="text-xs text-gray-500">{note}</span>}
      </h3>
      <div className="space-y-3">{children}</div>
    </section>
  );
}

// ---------------------------------------------------------------------------
// How it works
// ---------------------------------------------------------------------------

function HowItWorks() {
  const q = useQuery({ queryKey: ["picks-record", undefined, 3650], queryFn: () => picksApi.record(undefined, 3650) });
  const all = q.data?.rows.find((r) => r.kind == null && r.sport == null);
  const tiers = q.data?.tiers ?? [];
  const count = (t: Tier) => tiers.filter((x) => x.tier === t).length;
  const steps: [string, string][] = [
    ["Start with a current projection", "Each sport's champion model simulates the game 100,000 times before it starts, from the data available then (teams, players, lineups), and the prediction is frozen with a hash so it cannot be edited later. A pick is only made from a projection for a game that has not started, and no new pick is made in the last two hours, when lines have mostly settled."],
    ["Compare probability with price", "The model's probability for a side is read straight from the simulated outcomes. The market's probability comes from the book's two prices with the margin removed (power method). The model is then blended with the market, with a weight it has to earn on settled lines; a model that has never beaten the market cannot move the price. Edge is the blended probability minus the market's; EV is what the current price pays on that probability."],
    ["Read the case and the risk", "Every pick carries a written case for it and a case against it, both limited to the numbers above, plus risk flags (push chance, projected lineups, a stale price). The track record says how far to trust the market: proven markets may grade up to A+, markets that beat only a naive model up to B+, unproven ones C+, and markets losing to their comparison C."],
    ["Recheck before acting", "Odds, injuries and lineups move after a pick is published. Each pick lists what to recheck: has the game started, is there still value at the current price, is the price above its playable floor, was it seen recently, did a player's injury status change. A status change suspends the pick automatically. Confirm the live price and stay within your own risk limit."],
  ];
  return (
    <div className="space-y-4 max-w-4xl">
      <p className="text-sm text-gray-300">
        A prediction estimates what may happen. A pick asks whether the current price offers enough value to consider. The same prediction can be worth a bet at one price and not
        at the next, so every pick shows the model probability, the market probability, the price, the edge, two grades and the reasoning, in this order.
      </p>
      <div className="grid gap-3 md:grid-cols-2">
        {steps.map(([t, d], i) => (
          <Card key={t} className="space-y-1.5">
            <div className="flex items-baseline gap-2"><span className="text-xs font-mono text-violet">STEP {String(i + 1).padStart(2, "0")}</span><span className="font-medium">{t}</span></div>
            <p className="text-sm text-gray-400 leading-relaxed">{d}</p>
          </Card>
        ))}
      </div>
      <Card className="space-y-2">
        <div className="font-medium">Two grades on every pick</div>
        <div className="text-sm text-gray-400 space-y-1.5">
          <p><span className="text-gray-200">Price grade</span> (A+ to F) is the expected value per unit at the current price: A+ at 10% or more, A at 7%, A− at 5%, B+ 4%, B 3%, B− 2%, C+ 1%, C above 0, F at or below 0. It changes whenever the price changes.</p>
          <p><span className="text-gray-200">Rated grade</span> is the edge grade capped by the market's track record. A big edge in a market that has never beaten the closing line is far more often model error than value, so it is rated no higher than the record allows. Caps lift automatically once a market proves itself on settled picks.</p>
        </div>
      </Card>
      <Card className="space-y-2">
        <div className="font-medium">Track record today</div>
        {q.error && <Msg tone="error">{(q.error as Error).message}</Msg>}
        {all ? (
          <div className="text-sm text-gray-400 space-y-1.5">
            <p>{all.graded.toLocaleString()} settled picks · {all.wins}-{all.losses}-{all.pushes} · ROI {signedPct(all.roi)} at the published prices
              {all.clv_n ? ` · average CLV ${signedPct(all.clv_mean)} over ${all.clv_n.toLocaleString()} picks with a closing price` : ""}.</p>
            <p>Markets by track record: {count("proven")} proven, {count("baseline")} beat a naive model only, {count("unproven")} unproven, {count("losing")} losing to their comparison.
              {count("proven") === 0 && " No market is proven right now, so no pick can be rated above B+, whatever its price grade."}</p>
          </div>
        ) : q.isLoading ? <Msg>Loading…</Msg> : <p className="text-sm text-gray-500">No settled picks yet.</p>}
        <p className="text-xs text-gray-500">AI picks are estimates, not guarantees: injuries, lineups, weather, price moves, model assumptions and ordinary variance can change the result. See the Track record and Models tabs for the evidence behind every number.</p>
      </Card>
    </div>
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
// Models: the validation scorecard
// ---------------------------------------------------------------------------

/** Why a sport has no picks: each market's validation status, from the scorecard. */
function WhyNone({ sport, kind }: { sport: string; kind?: string }) {
  const q = useQuery({ queryKey: ["scorecard", sport, kind], queryFn: () => picksApi.scorecard(sport, kind), staleTime: 300_000 });
  const ms = q.data?.markets ?? [];
  if (!ms.length) return null;
  const by = (st: MarketStatus) => ms.filter((m) => m.status === st).map((m) => `${m.market}${m.engine.endsWith("_team") ? " (team)" : ""}`);
  const parts = (["verified", "model_only", "no_signal", "losing", "insufficient_data"] as MarketStatus[])
    .map((st) => [st, by(st)] as const).filter(([, v]) => v.length);
  return (
    <span className="block mt-2 text-xs text-gray-400">
      {sport.toUpperCase()} validation: {parts.map(([st, v]) => `${STATUS_TEXT[st]}: ${v.join(", ")}`).join(" · ")}. See the Models tab for the evidence.
    </span>
  );
}

function Models() {
  const [sport, setSport] = useState("all");
  const [kind, setKind] = useState("all");
  const [open, setOpen] = useState<string | null>(null);
  const s = sport === "all" ? undefined : sport;
  const q = useQuery({ queryKey: ["scorecard", s, kind], queryFn: () => picksApi.scorecard(s, kind === "all" ? undefined : kind), refetchInterval: 300_000 });
  const d = q.data;
  const order: MarketStatus[] = ["verified", "model_only", "no_signal", "losing", "insufficient_data"];
  const rows = useMemo(() => [...(d?.markets ?? [])].sort((a, b) => order.indexOf(a.status) - order.indexOf(b.status) || (b.lines?.n ?? 0) - (a.lines?.n ?? 0)), [d]);
  const ci = (x?: { mean: number; lo: number; hi: number } | null, scale = 1, dp = 4) =>
    x == null ? "—" : `${(x.mean * scale).toFixed(dp)} [${(x.lo * scale).toFixed(dp)}, ${(x.hi * scale).toFixed(dp)}]`;
  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center gap-2">
        <Pills options={SPORTS} value={sport} onChange={setSport} />
        <Pills options={KINDS} value={kind} onChange={setKind} />
      </div>
      <p className="text-xs text-gray-500 max-w-4xl">
        Every engine and market is scored the same way across sports. Model quality comes from the track-record tier (backtests, then settled lines and picks)
        and calibration at real lines; status says what AI Picks may publish. Brier vs market below zero favours the model; intervals are 95% bootstrap,
        resampled by game. ROI excludes quarantined odds feeds. Each pick also gets an opportunity grade from the EV of its current price.
      </p>
      {q.error && <Msg tone="error">{(q.error as Error).message}</Msg>}
      {q.isLoading && <Msg>Scoring every engine…</Msg>}
      {d && (
        <>
          <div className="flex flex-wrap gap-2 text-xs">
            {order.map((st) => (
              <span key={st} className="rounded-md border border-glass bg-surface/60 px-2 py-1">
                <span className={statusTone[st]}>{STATUS_TEXT[st]}</span> <span className="font-mono text-gray-300">{rows.filter((r) => r.status === st).length}</span>
              </span>
            ))}
          </div>
          {d.quarantined_feeds.length > 0 && (
            <Msg tone="error">
              Quarantined odds feeds (bets won far more often than their prices allow, so the quotes belong to another line or market):{" "}
              {d.quarantined_feeds.map((f) => `${f.sport.toUpperCase()} ${f.market} @ ${f.book}`).join(", ")}. Their picks are voided and left out of evidence and ROI.
            </Msg>
          )}
          <Table head={<tr><Th>Market</Th><Th>Engine</Th><Th>Status</Th><Th>Quality</Th><Th>Lines</Th><Th>Calib. err.</Th><Th>Brier vs mkt [95%]</Th><Th>Picks</Th><Th>ROI [95%]</Th><Th>CLV</Th><Th>Max DD</Th></tr>}>
            {rows.map((m) => {
              const key = m.engine + m.market;
              return (
                <ModelRow key={key} m={m} open={open === key} onToggle={() => setOpen(open === key ? null : key)} ci={ci} />
              );
            })}
          </Table>
          <Experiments sc={d} sport={s} />
          <h2 className="font-medium pt-2">Engine forecasts vs their naive baseline</h2>
          <Table head={<tr><Th>Engine version</Th><Th>Market</Th><Th>n</Th><Th>Brier</Th><Th>Log loss</Th><Th>Calib. err.</Th><Th>vs baseline [95%]</Th></tr>}>
            {d.forecasts.filter((f) => f.champion && f.n >= 100).map((f) => (
              <tr key={f.version + f.market} className="border-t border-glass text-xs">
                <Td>{f.version}</Td><Td>{f.market}</Td><Td className="font-mono">{f.n}</Td><Td className="font-mono">{f.brier?.toFixed(4)}</Td>
                <Td className="font-mono">{f.logloss?.toFixed(4)}</Td><Td className="font-mono">{pct(f.ece, 1)}</Td>
                <Td className={clsx("font-mono", (f.brier_vs_baseline?.hi ?? 0) < 0 ? "text-emerald-light" : (f.brier_vs_baseline?.lo ?? 0) > 0 ? "text-crimson-light" : "")}>{ci(f.brier_vs_baseline)}</Td>
              </tr>
            ))}
          </Table>
        </>
      )}
    </div>
  );
}

const verdictTone: Record<string, string> = {
  shadow_candidate: "text-emerald-light", inconclusive: "text-amber", rejected: "text-crimson-light", insufficient_data: "text-gray-500",
};

/** Walk-forward ML experiments (scripts/ml): every learned model starts from the market and must beat it out of sample. */
function Experiments({ sc, sport }: { sc: Scorecard; sport?: string }) {
  const ex = (sc.experiments ?? []).filter((e) => !sport || e.sport === sport);
  const ds = (sc.dataset ?? []).filter((r) => !sport || r.sport === sport);
  if (!ex.length) return null;
  const iv = (x?: { mean: number; lo95: number; hi95: number } | null) => (x ? `${x.mean >= 0 ? "+" : ""}${x.mean.toFixed(4)} [${x.lo95.toFixed(4)}, ${x.hi95.toFixed(4)}]` : "—");
  const pick = (e: Experiment, m: string) => e.models.find((x) => x.model === m);
  return (
    <>
      <h2 className="font-medium pt-2">ML research: residual models vs the market</h2>
      <p className="text-xs text-gray-500 max-w-4xl">
        Walk-forward (train on earlier days or seasons, test the next). Elastic net, LightGBM and XGBoost all start from the market's probability.
        A model becomes a shadow candidate only if its 99% interval beats the market, it stays calibrated and it wins most folds; nothing is promoted automatically.
        {ds.length > 0 && <> Dataset: {ds.map((r) => `${r.sport.toUpperCase()} ${r.source} ${r.examples.toLocaleString()} examples / ${r.games} games`).join(" · ")}.</>}
      </p>
      <Table head={<tr><Th>Experiment</Th><Th>Verdict</Th><Th>Test rows</Th><Th>Engine raw vs mkt</Th><Th>Blend vs mkt</Th><Th>Best learned</Th><Th>Best vs mkt [95%]</Th><Th>Best ROI [95%]</Th></tr>}>
        {ex.map((e) => {
          const best = e.best_model ? pick(e, e.best_model) : undefined;
          return (
            <tr key={e.id} className="border-t border-glass text-xs" title={e.gate.why}>
              <Td>{e.name}</Td>
              <Td className={verdictTone[e.verdict]}>{e.verdict.replace("_", " ")}</Td>
              <Td className="font-mono">{e.n_test}</Td>
              <Td className="font-mono">{iv(pick(e, "engine_raw")?.brier_vs_market)}</Td>
              <Td className="font-mono">{iv(pick(e, "blend")?.brier_vs_market)}</Td>
              <Td>{e.best_model?.replace("residual_", "") ?? "—"}</Td>
              <Td className={clsx("font-mono", best?.brier_vs_market && best.brier_vs_market.hi95 < 0 ? "text-emerald-light" : best?.brier_vs_market && best.brier_vs_market.lo95 > 0 ? "text-crimson-light" : "")}>{iv(best?.brier_vs_market)}</Td>
              <Td className="font-mono">{best?.roi ? `${signedPct(best.roi.mean)} [${signedPct(best.roi.lo95)}, ${signedPct(best.roi.hi95)}] (${best.bets})` : "—"}</Td>
            </tr>
          );
        })}
      </Table>
    </>
  );
}

function ModelRow({ m, open, onToggle, ci }: { m: ScoreMarket; open: boolean; onToggle: () => void; ci: (x?: { mean: number; lo: number; hi: number } | null, scale?: number, dp?: number) => string }) {
  const bvm = m.lines?.brier_vs_market;
  return (
    <>
      <tr onClick={onToggle} className="border-t border-glass text-xs cursor-pointer hover:bg-white/[0.03]">
        <Td>{m.sport.toUpperCase()} · {m.market} <span className="text-gray-500">({m.kind})</span></Td>
        <Td className="text-gray-400">{m.engine.endsWith("_team") ? "team model" : "simulator"}</Td>
        <Td className={statusTone[m.status]}>
          {STATUS_TEXT[m.status]}
          {m.publication_hold && <span className="ml-1 rounded border border-crimson/40 px-1 text-[10px] text-crimson-light" title={m.publication_hold.reason}>on hold</span>}
        </Td>
        <Td><span className={clsx("rounded border px-1.5 py-0.5 font-mono", gradeTone(m.model_quality))}>{m.model_quality}</span></Td>
        <Td className="font-mono">{m.lines?.n ?? 0}</Td>
        <Td className="font-mono">{pct(m.lines?.ece, 1)}</Td>
        <Td className={clsx("font-mono", bvm && bvm.hi < 0 ? "text-emerald-light" : bvm && bvm.lo > 0 ? "text-crimson-light" : "")}>{ci(bvm)}</Td>
        <Td className="font-mono">{m.picks?.n ?? 0}{m.excluded_picks ? <span className="text-crimson-light"> (−{m.excluded_picks})</span> : null}</Td>
        <Td className={clsx("font-mono", m.picks && m.picks.roi_lo > 0 ? "text-emerald-light" : m.picks && m.picks.roi_hi < 0 ? "text-crimson-light" : "")}>
          {m.picks ? `${signedPct(m.picks.roi)} [${signedPct(m.picks.roi_lo)}, ${signedPct(m.picks.roi_hi)}]` : "—"}
        </Td>
        <Td className="font-mono">{m.picks?.clv_mean == null ? "—" : signedPct(m.picks.clv_mean, 2)}</Td>
        <Td className="font-mono">{m.picks ? `${m.picks.max_drawdown.toFixed(1)}u` : "—"}</Td>
      </tr>
      {open && (
        <tr className="bg-white/[0.02]">
          <td colSpan={11} className="px-3 py-2 text-xs text-gray-300 space-y-1">
            {m.reasons.map((r, i) => <div key={i}>{r}</div>)}
            {m.lines?.reliability && m.lines.reliability.length > 0 && (
              <div className="pt-1 text-gray-400">
                Reliability (predicted → observed, n): {m.lines.reliability.map((b) => `${pct(b.predicted)} → ${pct(b.observed)} (${b.n})`).join(" · ")}
              </div>
            )}
            {m.picks && <div className="text-gray-400">Sharpe {m.picks.sharpe.toFixed(3)} · Sortino {m.picks.sortino.toFixed(3)} · profit factor {m.picks.profit_factor?.toFixed(2) ?? "—"} · hit rate {pct(m.picks.hit_rate, 1)}</div>}
          </td>
        </tr>
      )}
    </>
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
