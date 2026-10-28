import { useEffect, useMemo, useState, type ReactNode } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import clsx from "clsx";
import { Card, Input, Msg, PageTitle, Pills, Stat, Table, Td, Th } from "../components/ui";
import {
  CATEGORIES, OBJECTIVES, OBJECTIVE_TEXT, ODDS_KEY, SPORTS, americanToDecimal, fmtAmerican, fmtOdds, parlaysApi, readOddsFormat,
  type GenFilters, type Leg, type OddsFormat, type SlateResponse, type StoredTicket, type Summary, type Ticket,
} from "../lib/parlays";
import { useSettings } from "../store/settings";

// Parlay Builder: auto-generated and custom multi-leg tickets. Legs in one game
// are graded on the same simulated games (their dependence is the simulator's);
// leg levels come from the AI Picks blend; prices are one book's.

const pct = (x: number | null | undefined, d = 1) => (x == null ? "—" : `${(x * 100).toFixed(d)}%`);
const signed = (x: number | null | undefined, d = 1) => (x == null ? "—" : `${x >= 0 ? "+" : ""}${(x * 100).toFixed(d)}%`);
const when = (iso: string | null | undefined) => (iso ? new Date(iso).toLocaleString(undefined, { weekday: "short", hour: "numeric", minute: "2-digit" }) : "—");
const ago = (iso: string | null | undefined) => {
  if (!iso) return "—";
  const m = Math.round((Date.now() - new Date(iso).getTime()) / 60000);
  return m < 1 ? "just now" : m < 60 ? `${m} min ago` : `${Math.round(m / 60)} h ago`;
};
const etToday = () => new Date(Date.now() - (new Date().getTimezoneOffset() === 0 ? 4 : 0) * 3600e3).toLocaleDateString("en-CA", { timeZone: "America/New_York" });
const shiftDate = (d: string, days: number) => { const x = new Date(`${d}T12:00:00Z`); x.setUTCDate(x.getUTCDate() + days); return x.toISOString().slice(0, 10); };

type Tab = "auto" | "custom" | "saved" | "history" | "validation";
const TABS: { key: Tab; label: string }[] = [
  { key: "auto", label: "Auto-Generated" }, { key: "custom", label: "Custom Builder" }, { key: "saved", label: "Saved Parlays" },
  { key: "history", label: "Historical Results" }, { key: "validation", label: "Model Validation" },
];

export function ParlayBuilder() {
  const [tab, setTab] = useState<Tab>("auto");
  const [sport, setSport] = useState("nba");
  const [date, setDate] = useState(etToday());
  const [fmt, setFmtState] = useState<OddsFormat>(readOddsFormat);
  const setFmt = (f: string) => { setFmtState(f as OddsFormat); try { localStorage.setItem(ODDS_KEY, f); } catch { /* private window */ } };
  // the custom ticket: leg ids and which are locked for "find alternatives"
  const [ticket, setTicket] = useState<string[]>([]);
  const [locked, setLocked] = useState<string[]>([]);
  const [compare, setCompare] = useState<Ticket[]>([]);
  const edit = (t: { legs: { id: string }[]; sport: string; date?: string }, lock: string[] = []) => {
    setSport(t.sport); if (t.date) setDate(t.date);
    setTicket(t.legs.map((l) => l.id)); setLocked(lock); setTab("custom");
  };
  const addCompare = (t: Ticket) => setCompare((c) => (c.some((x) => x.key === t.key) ? c : [...c, t].slice(-3)));
  const ctx: Ctx = { sport, date, fmt, edit, addCompare };

  return (
    <div className="space-y-4">
      <PageTitle
        title="Parlay Builder"
        subtitle="joint probabilities from the simulators · one-book prices · EV"
        right={<>
          <Pills options={SPORTS} value={sport} onChange={(s) => { setSport(s); setTicket([]); setLocked([]); }} />
          <div className="flex items-center gap-1 text-xs text-gray-400">
            <button className="px-1.5 hover:text-gray-100" onClick={() => setDate(shiftDate(date, -1))}>‹</button>
            <input type="date" value={date} onChange={(e) => e.target.value && setDate(e.target.value)} className="bg-surface border border-glass rounded-md px-2 py-0.5 text-sm text-gray-100" />
            <button className="px-1.5 hover:text-gray-100" onClick={() => setDate(shiftDate(date, 1))}>›</button>
            <span title="Slates are US Eastern dates">ET</span>
          </div>
          <Pills options={[{ key: "american", label: "US" }, { key: "decimal", label: "Dec" }, { key: "fractional", label: "Frac" }]} value={fmt} onChange={setFmt} />
        </>}
      />
      <div className="flex gap-1 border-b border-glass">
        {TABS.map((t) => (
          <button key={t.key} onClick={() => setTab(t.key)}
            className={clsx("px-3 py-2 text-sm -mb-px border-b-2", tab === t.key ? "border-violet text-violet-light" : "border-transparent text-gray-400 hover:text-gray-200")}>
            {t.label}{t.key === "custom" && ticket.length > 0 && <span className="ml-1 text-[11px] text-gray-500">({ticket.length})</span>}
          </button>
        ))}
      </div>
      {(tab === "auto" || tab === "custom") && <SlateBar sport={sport} date={date} />}
      {tab === "auto" && <AutoTab ctx={ctx} />}
      {tab === "custom" && <CustomTab ctx={ctx} ticket={ticket} setTicket={setTicket} locked={locked} setLocked={setLocked} />}
      {tab === "saved" && <SavedTab ctx={ctx} />}
      {tab === "history" && <HistoryTab sport={sport} fmt={fmt} />}
      {tab === "validation" && <ValidationTab />}
      {compare.length > 0 && <CompareTray tickets={compare} fmt={fmt} onClear={() => setCompare([])} onRemove={(k) => setCompare((c) => c.filter((x) => x.key !== k))} />}
    </div>
  );
}

interface Ctx { sport: string; date: string; fmt: OddsFormat; edit: (t: { legs: { id: string }[]; sport: string; date?: string }, lock?: string[]) => void; addCompare: (t: Ticket) => void }

// --------------------------------------------------------------------------
// Slate status: coverage, simulations, background job
// --------------------------------------------------------------------------

function useSlate(sport: string, date: string) {
  return useQuery({
    queryKey: ["parlay-slate", sport, date],
    queryFn: () => parlaysApi.slate(sport, date),
    refetchInterval: (q) => ((q.state.data as SlateResponse | undefined)?.job.running || (q.state.data?.pending.length ?? 0) > 0 ? 8000 : 120_000),
  });
}

function SlateBar({ sport, date }: { sport: string; date: string }) {
  const { data, error, isLoading } = useSlate(sport, date);
  const [open, setOpen] = useState(false);
  const qc = useQueryClient();
  if (isLoading) return <Msg>Loading the slate and simulating its games (first load of a slate can take a while)…</Msg>;
  if (error) return <Msg tone="error">{(error as Error).message}</Msg>;
  if (!data) return null;
  const c = data.coverage;
  return (
    <Card className="space-y-2">
      <div className="flex items-center gap-4 flex-wrap text-xs text-gray-400">
        <span className="text-gray-200 font-medium">{SPORTS.find((s) => s.key === sport)?.label} · {data.date} (ET)</span>
        <span>{c.games} games with lines · <b className="text-gray-200">{c.simulated}</b> simulated{c.pending > 0 && <> · <span className="text-amber">{c.pending} simulating</span></>}{c.excluded > 0 && <> · {c.excluded} excluded</>}{c.errors > 0 && <> · <span className="text-crimson-light">{c.errors} failed</span></>}</span>
        <span>{c.eligible} of {c.legs} legs eligible</span>
        <span>quotes {ago(c.newest_quote)} (oldest {ago(c.oldest_quote)}, max age {c.max_age_minutes} min)</span>
        <span title={`parlay model ${data.model_version}`}>dependence weight {data.dep_weight.toFixed(2)}</span>
        {data.job.running && (
          <span className="text-amber">
            background simulation {data.job.done}/{data.job.total}{data.job.current && ` (game ${data.job.current})`}
            <button className="ml-2 underline" onClick={async () => { await parlaysApi.cancel(sport); qc.invalidateQueries({ queryKey: ["parlay-slate"] }); }}>cancel</button>
          </span>
        )}
        <button className="ml-auto text-violet-light hover:underline" onClick={() => setOpen(!open)}>{open ? "hide games" : "games"}</button>
      </div>
      {c.games === 0 && <div className="text-xs text-gray-500">No pre-game lines for this date yet. Lines arrive with the AI Picks feeds (NFL ~40 hours before kickoff); legs drop out five minutes before their game.</div>}
      {open && (
        <div className="grid md:grid-cols-2 xl:grid-cols-3 gap-2">
          {data.games.map((g) => (
            <div key={g.id} className="text-xs rounded-md border border-glass px-2 py-1.5">
              <div className="flex gap-2"><span className="font-medium text-gray-200">{g.label}</span><span className="text-gray-500">{when(g.starts_at)}</span>
                <span className={clsx("ml-auto", g.status === "simulated" ? "text-emerald-light" : g.status === "pending" ? "text-amber" : "text-crimson-light")}>{g.status}</span></div>
              <div className="text-gray-500">
                {g.status === "simulated" ? `${g.sims.toLocaleString()} sims in ${(g.sim_ms / 1000).toFixed(1)} s · ${g.lineup} lineups · ${g.eligible}/${g.legs} legs` : g.detail ?? `${g.legs} legs`}
                {g.missing_columns > 0 && ` · ${g.missing_columns} props without a simulated player`}
              </div>
            </div>
          ))}
        </div>
      )}
      {c.model_bias.some((b) => b.recentred && Math.abs(b.mean_logit) > 0.1) && (
        <div className="text-[11px] text-amber">
          Model lean recentred on this slate: {c.model_bias.filter((b) => b.recentred && Math.abs(b.mean_logit) > 0.1).map((b) => `${b.label} ${b.mean_logit < 0 ? "under" : "over"} ${(Math.abs(b.mean_logit)).toFixed(2)} logit (${b.lines} lines)`).join(" · ")}.
          A lean on nearly every line of a market is a model bias, not value; each market's average is set to the book's and players keep their relative ranking.
        </div>
      )}
      <div className="text-[11px] text-gray-600">{data.note}. Sports: NBA, NFL, NHL, MLB (WNBA has no engine in MonsterOS). MLB player props have no line feed yet.</div>
    </Card>
  );
}

// --------------------------------------------------------------------------
// Auto-generated
// --------------------------------------------------------------------------

const AUTO_KEY = "parlays.filters";
type Draft = Omit<GenFilters, "sport" | "date"> & { odds_min_am?: string; odds_max_am?: string };
const DEFAULT_DRAFT: Draft = { category: "mixed", legs: 2, objective: "ev_cons", scope: "any", limit: 20, max_per_player: 2, max_age: 180 };
const readDraft = (): Draft => { try { return { ...DEFAULT_DRAFT, ...JSON.parse(localStorage.getItem(AUTO_KEY) ?? "{}") }; } catch { return DEFAULT_DRAFT; } };

function toFilters(d: Draft, sport: string, date: string): GenFilters {
  const { odds_min_am, odds_max_am, ...rest } = d;
  const am = (s?: string) => (s && Number(s) && Math.abs(Number(s)) >= 100 ? americanToDecimal(Number(s)) : undefined);
  return { ...rest, sport, date, min_odds: am(odds_min_am), max_odds: am(odds_max_am) };
}

function AutoTab({ ctx }: { ctx: Ctx }) {
  const [draft, setDraft] = useState<Draft>(readDraft);
  const [applied, setApplied] = useState<Draft>(draft);
  const [adv, setAdv] = useState(false);
  const slate = useSlate(ctx.sport, ctx.date);
  const set = (p: Partial<Draft>) => setDraft((d) => ({ ...d, ...p }));
  const apply = () => { setApplied(draft); try { localStorage.setItem(AUTO_KEY, JSON.stringify(draft)); } catch { /* private window */ } };
  const filters = useMemo(() => toFilters(applied, ctx.sport, ctx.date), [applied, ctx.sport, ctx.date]);
  const q = useQuery({
    queryKey: ["parlay-gen", filters],
    queryFn: () => parlaysApi.generate(filters),
    refetchInterval: (x) => ((x.state.data?.pending.length ?? 0) > 0 ? 15_000 : 120_000),
  });
  const dirty = JSON.stringify(draft) !== JSON.stringify(applied);
  const markets = slate.data?.markets ?? [];
  const propMarkets = markets.filter((m) => m.category === "props");
  const books = slate.data?.books ?? [];
  const games = slate.data?.games ?? [];
  const csv = (s?: string) => (s ? s.split(",").filter(Boolean) : []);
  const toggle = (key: keyof Draft, v: string) => { const cur = csv(draft[key] as string); set({ [key]: (cur.includes(v) ? cur.filter((x) => x !== v) : [...cur, v]).join(",") } as Partial<Draft>); };

  return (
    <div className="space-y-3">
      <Card className="space-y-3">
        <div className="flex items-center gap-2 flex-wrap text-xs text-gray-400">
          <Pills options={CATEGORIES} value={draft.category ?? "mixed"} onChange={(v) => set({ category: v })} />
          <span>legs</span>
          <Pills options={[2, 3, 4, 5].map((n) => ({ key: String(n), label: String(n) }))} value={String(draft.legs ?? 2)} onChange={(v) => set({ legs: Number(v) })} />
          <Input type="number" value={draft.legs ?? 2} onChange={(v) => set({ legs: Math.min(10, Math.max(1, Number(v) || 2)) })} className="w-14" />
          <span>rank by</span>
          <select value={draft.objective} onChange={(e) => set({ objective: e.target.value })} className="bg-surface border border-glass rounded-md px-2 py-1 text-sm text-gray-100">
            {OBJECTIVES.map((o) => <option key={o.key} value={o.key}>{o.label}</option>)}
          </select>
          <Pills options={[{ key: "any", label: "Any games" }, { key: "cross", label: "Cross-game" }, { key: "same", label: "Same-game" }]} value={draft.scope ?? "any"} onChange={(v) => set({ scope: v })} />
          <button onClick={() => setAdv(!adv)} className="text-violet-light hover:underline">{adv ? "fewer filters" : "more filters"}</button>
          <button onClick={apply} className={clsx("ml-auto rounded-md px-3 py-1 text-sm", dirty ? "bg-violet text-white" : "bg-violet/20 text-violet-light")}>{dirty ? "Generate" : "Regenerate"}</button>
        </div>
        <div className="text-[11px] text-gray-500">{OBJECTIVE_TEXT[draft.objective ?? "ev_cons"]}</div>
        {adv && (
          <div className="space-y-2 text-xs text-gray-400">
            <div className="flex flex-wrap gap-x-4 gap-y-2 items-center">
              <Field label="odds (US) from"><Input value={draft.odds_min_am ?? ""} onChange={(v) => set({ odds_min_am: v })} placeholder="+200" className="w-20" /></Field>
              <Field label="to"><Input value={draft.odds_max_am ?? ""} onChange={(v) => set({ odds_max_am: v })} placeholder="+1000" className="w-20" /></Field>
              <Field label="min P(win) %"><Num v={draft.min_prob} set={(v) => set({ min_prob: v })} scale={100} /></Field>
              <Field label="max P(win) %"><Num v={draft.max_prob} set={(v) => set({ max_prob: v })} scale={100} /></Field>
              <Field label="min leg P %"><Num v={draft.min_leg_prob} set={(v) => set({ min_leg_prob: v })} scale={100} /></Field>
              <Field label="min EV %"><Num v={draft.min_ev} set={(v) => set({ min_ev: v })} scale={100} /></Field>
              <Field label="min cons. EV %"><Num v={draft.min_ev_cons} set={(v) => set({ min_ev_cons: v })} scale={100} /></Field>
            </div>
            <div className="flex flex-wrap gap-x-4 gap-y-2 items-center">
              <Field label="max legs / game"><Num v={draft.max_per_game} set={(v) => set({ max_per_game: v })} /></Field>
              <Field label="/ player"><Num v={draft.max_per_player} set={(v) => set({ max_per_player: v })} /></Field>
              <Field label="/ team"><Num v={draft.max_per_team} set={(v) => set({ max_per_team: v })} /></Field>
              <Field label="dependence lift from"><Num v={draft.min_lift} set={(v) => set({ min_lift: v })} step /></Field>
              <Field label="to"><Num v={draft.max_lift} set={(v) => set({ max_lift: v })} step /></Field>
              <Field label="max model range (pts)"><Num v={draft.max_range} set={(v) => set({ max_range: v })} scale={100} /></Field>
            </div>
            <div className="flex flex-wrap gap-x-4 gap-y-2 items-center">
              <Field label="side"><Pills options={[{ key: "", label: "Any" }, { key: "over", label: "Overs" }, { key: "under", label: "Unders" }, { key: "home", label: "Home" }, { key: "away", label: "Away" }]} value={draft.side ?? ""} onChange={(v) => set({ side: v || undefined })} /></Field>
              <Field label="model confidence"><Pills options={[{ key: "", label: "Any" }, { key: "baseline", label: "Baseline+" }, { key: "proven", label: "Proven" }]} value={draft.min_tier ?? ""} onChange={(v) => set({ min_tier: v || undefined })} /></Field>
              <label className="flex items-center gap-1"><input type="checkbox" checked={!!draft.verified_only} onChange={(e) => set({ verified_only: e.target.checked })} /> verified prices only (cross-game)</label>
              <label className="flex items-center gap-1"><input type="checkbox" checked={!!draft.include_flagged} onChange={(e) => set({ include_flagged: e.target.checked })} /> include flagged legs</label>
              <Field label="quote age ≤ (min)"><Num v={draft.max_age} set={(v) => set({ max_age: v })} /></Field>
              <Field label="results"><Num v={draft.limit} set={(v) => set({ limit: v })} /></Field>
              <Field label="leg pool"><Num v={draft.pool} set={(v) => set({ pool: v })} /></Field>
              <Field label="beam"><Num v={draft.beam} set={(v) => set({ beam: v })} /></Field>
            </div>
            <div className="flex flex-wrap gap-x-4 gap-y-2 items-center">
              <Field label="teams"><Input value={draft.teams ?? ""} onChange={(v) => set({ teams: v })} placeholder="BOS,DEN" className="w-28" /></Field>
              <Field label="exclude teams"><Input value={draft.exclude_teams ?? ""} onChange={(v) => set({ exclude_teams: v })} className="w-28" /></Field>
              <Field label="players"><Input value={draft.players ?? ""} onChange={(v) => set({ players: v })} placeholder="jokic,tatum" className="w-36" /></Field>
              <Field label="exclude players"><Input value={draft.exclude_players ?? ""} onChange={(v) => set({ exclude_players: v })} className="w-36" /></Field>
            </div>
            {books.length > 0 && (
              <Chips label="books" all={books.map((b) => ({ key: b.key, label: b.title }))} selected={csv(draft.books)} onToggle={(v) => toggle("books", v)} onClear={() => set({ books: undefined })} />
            )}
            {games.length > 0 && (
              <>
                <Chips label="only games" all={games.map((g) => ({ key: g.id, label: g.label }))} selected={csv(draft.games)} onToggle={(v) => toggle("games", v)} onClear={() => set({ games: undefined })} />
                <Chips label="exclude games" all={games.map((g) => ({ key: g.id, label: g.label }))} selected={csv(draft.exclude_games)} onToggle={(v) => toggle("exclude_games", v)} onClear={() => set({ exclude_games: undefined })} />
              </>
            )}
            {propMarkets.length > 0 && (draft.category === "props" || draft.category === "mixed") && (
              <Chips label="prop types" all={propMarkets.map((m) => ({ key: m.key, label: m.label }))} selected={csv(draft.props)} onToggle={(v) => toggle("props", v)} onClear={() => set({ props: undefined })} />
            )}
            <button className="text-gray-500 hover:text-gray-300 underline" onClick={() => setDraft(DEFAULT_DRAFT)}>reset filters</button>
          </div>
        )}
      </Card>
      {q.isLoading && <Msg>Searching combinations across the slate…</Msg>}
      {q.error && <Msg tone="error">{(q.error as Error).message}</Msg>}
      {q.data && (
        <>
          <div className="text-xs text-gray-500 flex flex-wrap gap-x-3">
            <span>{q.data.tickets.length} tickets</span>
            <span>{q.data.stats.eligible_legs} eligible legs → pool {q.data.stats.pool} → {q.data.stats.evaluated.toLocaleString()} tickets evaluated in {q.data.stats.ms} ms</span>
            <span>generated {ago(q.data.generated_at)}</span>
            {q.data.pending.length > 0 && <span className="text-amber">{q.data.pending.length} games still simulating: results will update</span>}
            {q.data.notes.map((n) => <span key={n}>{n}</span>)}
          </div>
          {q.data.tickets.length === 0 && <Msg>No ticket passes these filters. Loosen the odds, probability or EV limits, or pick another market.</Msg>}
          <div className="space-y-3">{q.data.tickets.map((t, i) => <TicketCard key={t.key} t={t} rank={i + 1} ctx={ctx} />)}</div>
        </>
      )}
    </div>
  );
}

function Field({ label, children }: { label: string; children: ReactNode }) { return <label className="flex items-center gap-1.5">{label}{children}</label>; }
function Num({ v, set, scale = 1, step }: { v?: number; set: (v?: number) => void; scale?: number; step?: boolean }) {
  return <Input type="number" value={v == null ? "" : +(v * scale).toFixed(4)} onChange={(x) => set(x === "" ? undefined : Number(x) / scale)} className={step ? "w-16" : "w-16"} />;
}
function Chips({ label, all, selected, onToggle, onClear }: { label: string; all: { key: string; label: string }[]; selected: string[]; onToggle: (k: string) => void; onClear: () => void }) {
  return (
    <div className="flex items-center gap-1 flex-wrap">
      <span className="mr-1">{label}</span>
      {all.map((x) => (
        <button key={x.key} onClick={() => onToggle(x.key)} className={clsx("px-2 py-0.5 rounded border border-glass", selected.includes(x.key) ? "bg-violet/20 text-violet-light" : "hover:bg-white/5")}>{x.label}</button>
      ))}
      {selected.length > 0 && <button onClick={onClear} className="underline ml-1">clear</button>}
    </div>
  );
}

// --------------------------------------------------------------------------
// Ticket card
// --------------------------------------------------------------------------

function PriceBadge({ t }: { t: Ticket }) {
  return t.price_verified
    ? <span className="text-[10px] rounded px-1.5 py-0.5 bg-emerald/15 text-emerald-light" title="Cross-game parlay: the book pays the product of these leg prices">verified price</span>
    : <span className="text-[10px] rounded px-1.5 py-0.5 bg-amber/15 text-amber" title={t.price_note ?? ""}>estimated price</span>;
}

function TicketCard({ t, rank, ctx, actions = true }: { t: Ticket; rank?: number; ctx: Ctx; actions?: boolean }) {
  const [open, setOpen] = useState(false);
  const [lock, setLock] = useState<string[]>([]);
  const [saved, setSaved] = useState<number | null>(null);
  const save = useMutation({ mutationFn: () => parlaysApi.save(t.sport, t.legs.map((l) => l.id), t.date, t.book ?? undefined), onSuccess: (r) => setSaved(r.id) });
  return (
    <Card className="space-y-3">
      <div className="flex items-center gap-3 flex-wrap">
        {rank != null && <span className="text-gray-500 font-mono text-sm">#{rank}</span>}
        <span className="font-medium">{t.n_legs}-leg {t.same_game ? "same-game" : "parlay"}</span>
        <span className="text-xs text-gray-500">{t.categories.join(" + ")} · starts {when(t.starts_at)}</span>
        <span className="ml-auto flex items-center gap-2 text-sm">
          <span className="font-mono text-lg">{fmtOdds(t.price_decimal, ctx.fmt)}</span>
          <span className="text-gray-400">{t.book_title ?? "no single book"}</span>
          <PriceBadge t={t} />
        </span>
      </div>
      <div className="grid grid-cols-3 md:grid-cols-7 gap-3">
        <Stat label="P(all win)" value={pct(t.p_joint)} />
        <Stat label="Fair odds" value={fmtOdds(t.fair_decimal, ctx.fmt)} />
        <Stat label="EV / unit" value={signed(t.ev)} tone={t.ev > 0 ? "pos" : "neg"} />
        <Stat label="Conservative EV" value={signed(t.ev_cons)} tone={t.ev_cons > 0 ? "pos" : "neg"} />
        <Stat label="± sim error" value={pct(t.mc_se, 2)} />
        <Stat label="vs independent" value={`${t.lift.toFixed(2)}x`} />
        <Stat label="Model range" value={`${pct(t.range[0])}–${pct(t.range[1])}`} />
      </div>
      <Table head={<tr><Th>Leg</Th><Th>Game</Th><Th className="text-right">Price</Th><Th className="text-right">Market</Th><Th className="text-right">Model</Th><Th className="text-right">Sim</Th><Th className="text-right">P(win)</Th>{actions && <Th>Lock</Th>}</tr>}>
        {t.legs.map((l) => (
          <tr key={l.id} className="border-t border-glass">
            <Td><div className="font-medium">{l.label}</div>{(l.push > 0.005 || l.void > 0.005) && <div className="text-[11px] text-gray-500">push {pct(l.push)} · void {pct(l.void)}</div>}</Td>
            <Td className="text-xs text-gray-400">{l.game_label}<div className="text-gray-600">{when(l.starts_at)}</div></Td>
            <Td className="text-right font-mono">{fmtAmerican(l.price, ctx.fmt)}</Td>
            <Td className="text-right font-mono">{pct(l.p_market)}</Td>
            <Td className="text-right font-mono">{pct(l.p_blend)}</Td>
            <Td className="text-right font-mono text-gray-500">{pct(l.p_sim)}</Td>
            <Td className="text-right font-mono">{pct(l.p_win)}</Td>
            {actions && <Td><input type="checkbox" checked={lock.includes(l.id)} onChange={(e) => setLock(e.target.checked ? [...lock, l.id] : lock.filter((x) => x !== l.id))} /></Td>}
          </tr>
        ))}
      </Table>
      {t.warnings.length > 0 && <div className="text-xs text-amber space-y-0.5">{t.warnings.map((w) => <div key={w}>⚠ {w}</div>)}</div>}
      <div className="flex items-center gap-3 flex-wrap text-xs">
        <button className="text-violet-light hover:underline" onClick={() => setOpen(!open)}>{open ? "hide diagnostics" : "how this was calculated"}</button>
        {actions && <>
          <button className="text-violet-light hover:underline" onClick={() => ctx.edit(t)}>edit in builder</button>
          <button className="text-violet-light hover:underline" onClick={() => ctx.edit(t, lock.length ? lock : t.legs.map((l) => l.id))}>{lock.length ? `lock ${lock.length} & find alternatives` : "duplicate"}</button>
          <button className="text-violet-light hover:underline" onClick={() => ctx.addCompare(t)}>compare</button>
          <button className="text-violet-light hover:underline disabled:opacity-50" disabled={save.isPending || saved != null} onClick={() => save.mutate()}>{saved != null ? `saved #${saved}` : save.isPending ? "saving…" : "save"}</button>
          {save.error && <span className="text-crimson-light">{(save.error as Error).message}</span>}
        </>}
        <span className="ml-auto text-gray-600">{t.model_version} · quotes {ago(t.quotes_at)}</span>
      </div>
      {open && <Diagnostics t={t} fmt={ctx.fmt} />}
    </Card>
  );
}

function Diagnostics({ t, fmt }: { t: Ticket; fmt: OddsFormat }) {
  return (
    <div className="space-y-3 text-xs text-gray-400 border-t border-glass pt-3">
      <ul className="list-disc pl-4 space-y-0.5">{t.explain.map((e) => <li key={e}>{e}</li>)}</ul>
      <div className="grid md:grid-cols-2 gap-3">
        <div className="space-y-1">
          <div className="text-gray-300 font-medium">Probability</div>
          <Row k="Joint, blend marginals + simulated dependence" v={pct(t.p_dep, 2)} />
          <Row k="Independence baseline (same marginals)" v={pct(t.p_indep, 2)} />
          <Row k={`In force (weight ${t.dep_weight.toFixed(2)} on dependence)`} v={pct(t.p_joint, 2)} />
          <Row k="Raw simulator marginals" v={pct(t.p_raw, 2)} />
          <Row k="Market marginals + simulated dependence" v={pct(t.p_market, 2)} />
          <Row k="Market, legs independent (product of no-vig)" v={pct(t.p_market_indep, 2)} />
          <Row k="Conservative (halfway to market)" v={pct(t.p_cons, 2)} />
          <Row k="P(ticket loses)" v={pct(t.p_loss, 2)} />
          <Row k="Simulation std. error / effective sims" v={`${pct(t.mc_se, 3)} / ${Math.round(t.ess).toLocaleString()}`} />
        </div>
        <div className="space-y-1">
          <div className="text-gray-300 font-medium">Price and value</div>
          <Row k={`Offered (${t.book_title ?? "best per leg"})`} v={`${fmtOdds(t.price_decimal, fmt)} (${t.price_decimal.toFixed(3)})`} />
          {t.sgp_adjust < 0.9999 && <Row k="Product of leg prices × same-game correlation discount" v={`${fmtOdds(t.price_product, fmt)} × ${t.sgp_adjust.toFixed(2)}`} />}
          <Row k="Fair odds = 1 / P(all win)" v={fmtOdds(t.fair_decimal, fmt)} />
          <Row k="Expected payout per unit (pushes and voids drop out)" v={t.e_payout.toFixed(3)} />
          <Row k="EV = E[payout] − 1" v={signed(t.ev, 2)} />
          <Row k="Edge: P(win) − implied by the offered price" v={signed(t.edge, 2)} />
          <Row k="Conservative screening EV" v={signed(t.ev_cons, 2)} />
          {t.price_note && <div className="text-amber">{t.price_note}</div>}
        </div>
      </div>
      <div>
        <div className="text-gray-300 font-medium mb-1">Games</div>
        <Table head={<tr><Th>Game</Th><Th className="text-right">Legs</Th><Th className="text-right">P(game legs win)</Th><Th className="text-right">independent</Th><Th className="text-right">raw sim</Th><Th className="text-right">market</Th><Th className="text-right">sims (effective)</Th></tr>}>
          {t.games.map((g) => (
            <tr key={g.game} className="border-t border-glass">
              <Td>{g.label}</Td><Td className="text-right">{g.legs}</Td><Td className="text-right font-mono">{pct(g.p_win, 2)}</Td><Td className="text-right font-mono">{pct(g.p_indep, 2)}</Td>
              <Td className="text-right font-mono">{pct(g.p_win_raw, 2)}</Td><Td className="text-right font-mono">{pct(g.p_win_market, 2)}</Td>
              <Td className="text-right font-mono">{g.sims?.toLocaleString() ?? "—"} ({Math.round(g.ess).toLocaleString()})</Td>
            </tr>
          ))}
        </Table>
      </div>
      {t.pairs.length > 0 && (
        <div>
          <div className="text-gray-300 font-medium mb-1">Same-game dependence (pairs)</div>
          <Table head={<tr><Th>Pair</Th><Th className="text-right">P(A)</Th><Th className="text-right">P(B)</Th><Th className="text-right">P(both)</Th><Th className="text-right">if independent</Th><Th className="text-right">lift</Th><Th className="text-right">P(B | A)</Th></tr>}>
            {t.pairs.map((p) => (
              <tr key={p.a_id + p.b_id} className="border-t border-glass">
                <Td>{p.a} <span className="text-gray-600">+</span> {p.b}</Td>
                <Td className="text-right font-mono">{pct(p.p_a)}</Td><Td className="text-right font-mono">{pct(p.p_b)}</Td><Td className="text-right font-mono">{pct(p.p_both, 2)}</Td>
                <Td className="text-right font-mono">{pct(p.p_indep, 2)}</Td>
                <Td className={clsx("text-right font-mono", p.lift > 1.05 && "text-emerald-light", p.lift < 0.95 && "text-crimson-light")}>{p.lift.toFixed(2)}x</Td>
                <Td className="text-right font-mono">{pct(p.p_b_given_a)}</Td>
              </tr>
            ))}
          </Table>
        </div>
      )}
      <div className="text-gray-600">
        Legs in one game are graded on the same simulated games of the champion engine; the simulations are re-weighted (maximum-entropy tilt) so each leg's probability equals its
        model blend while the joint pattern keeps the simulator's dependence. Legs in different games are independent. Lift = P(both) ÷ (P(A)·P(B)); 1.00 = independent.
      </div>
    </div>
  );
}
function Row({ k, v }: { k: string; v: ReactNode }) { return <div className="flex gap-2"><span>{k}</span><span className="ml-auto font-mono text-gray-200">{v}</span></div>; }

// --------------------------------------------------------------------------
// Custom builder
// --------------------------------------------------------------------------

function CustomTab({ ctx, ticket, setTicket, locked, setLocked }: { ctx: Ctx; ticket: string[]; setTicket: (t: string[]) => void; locked: string[]; setLocked: (t: string[]) => void }) {
  const [search, setSearch] = useState("");
  const [cat, setCat] = useState("mixed");
  const [openGame, setOpenGame] = useState<string | null>(null);
  const [book, setBook] = useState<string>("");
  const slate = useSlate(ctx.sport, ctx.date);
  const legs = useQuery({
    queryKey: ["parlay-legs", ctx.sport, ctx.date, slate.data?.coverage.simulated],
    queryFn: () => parlaysApi.legs(ctx.sport, { date: ctx.date }),
    enabled: !!slate.data,
  });
  const ev = useQuery({
    queryKey: ["parlay-eval", ctx.sport, ctx.date, ticket, book],
    queryFn: () => parlaysApi.evaluate(ctx.sport, ticket, ctx.date, book || undefined),
    enabled: ticket.length > 0,
    refetchInterval: 120_000,
  });
  useEffect(() => { if (book && ev.data && !ev.data.books.some((b) => b.book === book)) setBook(""); }, [ev.data, book]);
  const byId = useMemo(() => new Map((legs.data?.legs ?? []).map((l) => [l.id, l])), [legs.data]);
  const add = (l: Leg) => { if (!ticket.includes(l.id)) setTicket([...ticket, l.id]); };
  const remove = (id: string) => { setTicket(ticket.filter((x) => x !== id)); setLocked(locked.filter((x) => x !== id)); };
  const needle = search.trim().toLowerCase();
  const shown = (legs.data?.legs ?? []).filter((l) => (cat === "mixed" || l.category === cat) && (!needle || l.label.toLowerCase().includes(needle) || (l.team ?? "").toLowerCase().includes(needle) || (l.game_label ?? "").toLowerCase().includes(needle)));
  const games = slate.data?.games ?? [];

  return (
    <div className="grid xl:grid-cols-[minmax(0,1fr)_minmax(0,1.1fr)] gap-4 items-start">
      <Card className="space-y-2">
        <div className="flex items-center gap-2 flex-wrap text-xs">
          <Input value={search} onChange={setSearch} placeholder="search players, teams, markets" className="w-56" />
          <Pills options={CATEGORIES} value={cat} onChange={setCat} />
        </div>
        {legs.isLoading && <div className="text-xs text-gray-500">Loading markets…</div>}
        {legs.error && <div className="text-xs text-crimson-light">{(legs.error as Error).message}</div>}
        <div className="space-y-1 max-h-[70vh] overflow-auto pr-1">
          {games.map((g) => {
            const gl = shown.filter((l) => l.game === g.id);
            if (needle && gl.length === 0) return null;
            const isOpen = openGame === g.id || (!!needle && gl.length > 0);
            return (
              <div key={g.id} className="rounded-md border border-glass">
                <button onClick={() => setOpenGame(openGame === g.id ? null : g.id)} className="w-full flex items-center gap-2 px-3 py-2 text-sm hover:bg-white/[0.03]">
                  <span className="font-medium">{g.label}</span><span className="text-xs text-gray-500">{when(g.starts_at)}</span>
                  <span className={clsx("ml-auto text-xs", g.status === "simulated" ? "text-gray-500" : "text-amber")}>{g.status === "simulated" ? `${gl.length} legs` : g.status}</span>
                </button>
                {isOpen && <LegList legs={gl} fmt={ctx.fmt} inTicket={ticket} onAdd={add} />}
              </div>
            );
          })}
        </div>
      </Card>
      <div className="space-y-3">
        <Card className="space-y-2">
          <div className="flex items-center gap-2">
            <span className="font-medium">Your ticket</span>
            <span className="text-xs text-gray-500">{ticket.length} legs</span>
            {ev.data && ev.data.books.length > 0 && (
              <select value={book} onChange={(e) => setBook(e.target.value)} className="ml-auto bg-surface border border-glass rounded-md px-2 py-1 text-xs text-gray-100">
                <option value="">best book</option>
                {ev.data.books.map((b) => <option key={b.book} value={b.book}>{b.book_title} {fmtOdds(b.price_decimal, ctx.fmt)}</option>)}
              </select>
            )}
            {ticket.length > 0 && <button className="text-xs text-gray-500 hover:text-gray-300 underline" onClick={() => { setTicket([]); setLocked([]); }}>clear</button>}
          </div>
          {ticket.length === 0 && <div className="text-xs text-gray-500">Add legs from the slate. Legs from one game are priced with their dependence; legs from different games multiply.</div>}
          {ticket.map((id) => {
            const l = byId.get(id) ?? ev.data?.ticket?.legs.find((x) => x.id === id);
            return (
              <div key={id} className="flex items-center gap-2 text-sm border-t border-glass pt-1.5">
                <input type="checkbox" title="lock for 'find the best additions'" checked={locked.includes(id)} onChange={(e) => setLocked(e.target.checked ? [...locked, id] : locked.filter((x) => x !== id))} />
                <span className="flex-1">{l ? <>{l.label} <span className="text-xs text-gray-500">{l.game_label}</span></> : <span className="text-amber">no longer offered: {id.split("|").slice(1).join(" ")}</span>}</span>
                <button className="text-gray-500 hover:text-crimson-light" onClick={() => remove(id)}>✕</button>
              </div>
            );
          })}
          {ev.data?.problems.map((p) => <div key={p} className="text-xs text-amber">⚠ {p}</div>)}
          {ev.isFetching && <div className="text-xs text-violet">evaluating…</div>}
          {ev.error && <div className="text-xs text-crimson-light">{(ev.error as Error).message}</div>}
        </Card>
        {ev.data?.ticket && <TicketCard t={ev.data.ticket} ctx={{ ...ctx, edit: (t, lock) => { setTicket(t.legs.map((x) => x.id)); if (lock) setLocked(lock); } }} />}
        {ticket.length > 0 && <Alternatives ctx={ctx} ticket={ticket} locked={locked.length ? locked : ticket} onUse={(t) => { setTicket(t.legs.map((l) => l.id)); }} />}
      </div>
    </div>
  );
}

function LegList({ legs, fmt, inTicket, onAdd }: { legs: Leg[]; fmt: OddsFormat; inTicket: string[]; onAdd: (l: Leg) => void }) {
  const groups = useMemo(() => {
    const m = new Map<string, Leg[]>();
    for (const l of legs) { const k = l.kind === "game" ? l.market_label : `${l.subject_name ?? l.subject} · ${l.market_label}`; m.set(k, [...(m.get(k) ?? []), l]); }
    return [...m.entries()];
  }, [legs]);
  if (legs.length === 0) return <div className="px-3 pb-2 text-xs text-gray-500">No legs match.</div>;
  return (
    <div className="px-3 pb-2 space-y-1.5">
      {groups.map(([k, ls]) => (
        <div key={k}>
          <div className="text-[11px] uppercase tracking-wide text-gray-500">{k}</div>
          <div className="flex flex-wrap gap-1">
            {ls.map((l) => (
              <button key={l.id} disabled={inTicket.includes(l.id)} onClick={() => onAdd(l)}
                title={[`market ${pct(l.p_market)} · model ${pct(l.p_blend)} · sim ${pct(l.p_sim)}`, ...l.flags].join("\n")}
                className={clsx("text-xs rounded border px-2 py-1 text-left", inTicket.includes(l.id) ? "border-violet bg-violet/15" : "border-glass hover:bg-white/5", !l.eligible && "opacity-60")}>
                <span>{l.kind === "game" ? l.label : l.label.replace(`${l.subject_name ?? ""} `, "")}</span>
                <span className="ml-1.5 font-mono text-gray-400">{fmtAmerican(l.best?.price, fmt)}</span>
                {l.ev != null && <span className={clsx("ml-1.5 font-mono", l.ev > 0 ? "text-emerald-light" : "text-gray-500")}>{signed(l.ev, 0)}</span>}
                {l.flags.length > 0 && <span className="ml-1 text-amber">•</span>}
              </button>
            ))}
          </div>
        </div>
      ))}
    </div>
  );
}

function Alternatives({ ctx, ticket, locked, onUse }: { ctx: Ctx; ticket: string[]; locked: string[]; onUse: (t: Ticket) => void }) {
  const [n, setN] = useState(Math.max(ticket.length + 1, 2));
  const [obj, setObj] = useState("ev_cons");
  const [cat, setCat] = useState("mixed");
  const [go, setGo] = useState(false);
  const f: GenFilters = { sport: ctx.sport, date: ctx.date, legs: n, objective: obj, category: cat, locked: locked.join(","), limit: 5 };
  const q = useQuery({ queryKey: ["parlay-alt", f], queryFn: () => parlaysApi.generate(f), enabled: go });
  return (
    <Card className="space-y-2">
      <div className="flex items-center gap-2 flex-wrap text-xs text-gray-400">
        <span className="text-gray-200 font-medium text-sm">Find the best additions</span>
        <span>keep {locked.length} locked legs, build</span>
        <Input type="number" value={n} onChange={(v) => setN(Math.max(locked.length, Math.min(10, Number(v) || 2)))} className="w-14" /> legs
        <select value={obj} onChange={(e) => setObj(e.target.value)} className="bg-surface border border-glass rounded-md px-2 py-1 text-xs text-gray-100">
          {OBJECTIVES.map((o) => <option key={o.key} value={o.key}>{o.label}</option>)}
        </select>
        <Pills options={CATEGORIES} value={cat} onChange={setCat} />
        <button onClick={() => { setGo(true); q.refetch(); }} className="ml-auto rounded-md bg-violet/20 text-violet-light px-3 py-1">Search</button>
      </div>
      {q.isFetching && <div className="text-xs text-violet">searching…</div>}
      {q.error && <div className="text-xs text-crimson-light">{(q.error as Error).message}</div>}
      {q.data && q.data.tickets.length === 0 && <div className="text-xs text-gray-500">Nothing compatible with the locked legs passes the filters.</div>}
      {q.data?.tickets.map((t, i) => (
        <div key={t.key} className="space-y-1">
          <TicketCard t={t} rank={i + 1} ctx={ctx} actions={false} />
          <button className="text-xs text-violet-light hover:underline" onClick={() => onUse(t)}>use this ticket</button>
        </div>
      ))}
    </Card>
  );
}

// --------------------------------------------------------------------------
// Compare
// --------------------------------------------------------------------------

function CompareTray({ tickets, fmt, onClear, onRemove }: { tickets: Ticket[]; fmt: OddsFormat; onClear: () => void; onRemove: (k: string) => void }) {
  const rows: [string, (t: Ticket) => ReactNode][] = [
    ["Legs", (t) => t.legs.map((l) => <div key={l.id}>{l.label}</div>)],
    ["Book / price", (t) => <>{t.book_title ?? "—"} {fmtOdds(t.price_decimal, fmt)} {t.price_verified ? "" : "(est.)"}</>],
    ["P(all win)", (t) => pct(t.p_joint)], ["Fair odds", (t) => fmtOdds(t.fair_decimal, fmt)], ["EV", (t) => signed(t.ev)], ["Conservative EV", (t) => signed(t.ev_cons)],
    ["± sim error", (t) => pct(t.mc_se, 2)], ["Lift vs independent", (t) => `${t.lift.toFixed(2)}x`], ["Model range", (t) => `${pct(t.range[0])}–${pct(t.range[1])}`],
  ];
  return (
    <Card className="space-y-2">
      <div className="flex items-center gap-2"><span className="font-medium">Compare</span><button className="ml-auto text-xs underline text-gray-500" onClick={onClear}>clear</button></div>
      <Table head={<tr><Th>{" "}</Th>{tickets.map((t) => <Th key={t.key}>{t.n_legs} legs <button className="ml-1 text-gray-500" onClick={() => onRemove(t.key)}>✕</button></Th>)}</tr>}>
        {rows.map(([k, f]) => <tr key={k} className="border-t border-glass"><Td className="text-xs text-gray-500">{k}</Td>{tickets.map((t) => <Td key={t.key} className="text-xs font-mono">{f(t)}</Td>)}</tr>)}
      </Table>
    </Card>
  );
}

// --------------------------------------------------------------------------
// Saved parlays (and the auto-generated history)
// --------------------------------------------------------------------------

function SavedTab({ ctx }: { ctx: Ctx }) {
  const [source, setSource] = useState("saved");
  const [status, setStatus] = useState("");
  const [open, setOpen] = useState<number | null>(null);
  const qc = useQueryClient();
  const q = useQuery({
    queryKey: ["parlay-tickets", ctx.sport, source, status],
    queryFn: () => parlaysApi.tickets({ sport: ctx.sport, saved: source === "saved" ? true : undefined, source: source === "auto" ? "auto" : undefined, status: status || undefined, limit: 300 }),
  });
  const unsave = useMutation({ mutationFn: (id: number) => parlaysApi.unsave(id), onSuccess: () => qc.invalidateQueries({ queryKey: ["parlay-tickets"] }) });
  return (
    <div className="space-y-3">
      <div className="flex items-center gap-2 text-xs text-gray-400">
        <Pills options={[{ key: "saved", label: "My saved tickets" }, { key: "auto", label: "Auto-generated history" }]} value={source} onChange={setSource} />
        <Pills options={[{ key: "", label: "All" }, { key: "open", label: "Open" }, { key: "settled", label: "Settled" }, { key: "void", label: "Void" }]} value={status} onChange={setStatus} />
      </div>
      {q.isLoading && <Msg>Loading…</Msg>}
      {q.error && <Msg tone="error">{(q.error as Error).message}</Msg>}
      {q.data && q.data.tickets.length === 0 && <Msg>{source === "saved" ? "No saved tickets yet. Save one from the auto-generated list or the custom builder." : "No auto-generated tickets recorded yet: the autopilot stores each slate's best tickets every 30 minutes."}</Msg>}
      {q.data && q.data.tickets.length > 0 && (
        <Table head={<tr><Th>Created</Th><Th>Legs</Th><Th>Book</Th><Th className="text-right">Price</Th><Th className="text-right">P(win)</Th><Th className="text-right">EV</Th><Th className="text-right">Close</Th><Th>Status</Th><Th className="text-right">P/L</Th><Th>{" "}</Th></tr>}>
          {q.data.tickets.map((t) => (
            <SavedRow key={t.id} t={t} fmt={ctx.fmt} open={open === t.id} onOpen={() => setOpen(open === t.id ? null : t.id)}
              onEdit={() => ctx.edit({ legs: t.legs, sport: t.sport, date: t.date })} onUnsave={t.saved ? () => unsave.mutate(t.id) : undefined} />
          ))}
        </Table>
      )}
    </div>
  );
}

function resultTone(r: string | null) { return r === "win" ? "text-emerald-light" : r === "loss" ? "text-crimson-light" : "text-gray-400"; }

function SavedRow({ t, fmt, open, onOpen, onEdit, onUnsave }: { t: StoredTicket; fmt: OddsFormat; open: boolean; onOpen: () => void; onEdit: () => void; onUnsave?: () => void }) {
  return (
    <>
      <tr className="border-t border-glass cursor-pointer hover:bg-white/[0.03]" onClick={onOpen}>
        <Td className="text-xs text-gray-400">{new Date(t.created_at).toLocaleString()}<div className="text-gray-600">{t.date} · {t.source}{t.category ? ` · ${t.category}` : ""}</div></Td>
        <Td className="text-xs">{t.legs.map((l) => <div key={l.id}>{l.label}</div>)}</Td>
        <Td className="text-xs">{t.book_title ?? "—"}{!t.price_verified && <span className="text-amber"> (est.)</span>}</Td>
        <Td className="text-right font-mono">{fmtOdds(t.price_decimal, fmt)}</Td>
        <Td className="text-right font-mono">{pct(t.p_joint)}</Td>
        <Td className={clsx("text-right font-mono", t.ev > 0 ? "text-emerald-light" : "text-crimson-light")}>{signed(t.ev)}</Td>
        <Td className="text-right font-mono text-gray-400">{fmtOdds(t.latest_price_decimal, fmt)}</Td>
        <Td className={clsx("text-xs", resultTone(t.result))}>{t.result ?? t.status}</Td>
        <Td className="text-right font-mono">{t.profit == null ? "—" : `${t.profit >= 0 ? "+" : ""}${t.profit.toFixed(2)}u`}</Td>
        <Td className="text-xs whitespace-nowrap" >
          <button className="text-violet-light hover:underline mr-2" onClick={(e) => { e.stopPropagation(); onEdit(); }}>edit / duplicate</button>
          {onUnsave && <button className="text-gray-500 hover:underline" onClick={(e) => { e.stopPropagation(); onUnsave(); }}>remove</button>}
        </Td>
      </tr>
      {open && (
        <tr><td colSpan={10} className="p-3 bg-obsidian/40 text-xs text-gray-400 space-y-1">
          {(t.legs_result ?? t.legs.map((l) => ({ id: l.id, label: l.label, result: "open", actual: null, note: "" }))).map((l) => (
            <div key={l.id} className="flex gap-2"><span className={clsx("w-12", resultTone(l.result))}>{l.result}</span><span>{l.label}</span>{l.actual != null && <span className="text-gray-500">actual {l.actual}</span>}{l.note && <span className="text-gray-600">{l.note}</span>}</div>
          ))}
          <div className="text-gray-600">model {t.model_version} · dependence weight {t.dep_weight} · lift {t.lift.toFixed(2)}x · ± {pct(t.mc_se, 2)} · prediction-time values are immutable</div>
        </td></tr>
      )}
    </>
  );
}

// --------------------------------------------------------------------------
// Historical results
// --------------------------------------------------------------------------

function HistoryTab({ sport, fmt }: { sport: string; fmt: OddsFormat }) {
  const [scope, setScope] = useState<"sport" | "all">("sport");
  const [source, setSource] = useState("");
  const [days, setDays] = useState("365");
  const q = useQuery({ queryKey: ["parlay-record", scope, sport, source, days], queryFn: () => parlaysApi.record({ sport: scope === "sport" ? sport : undefined, source: source || undefined, days: Number(days) || 365 }) });
  const settled = useQuery({ queryKey: ["parlay-settled", scope, sport, source], queryFn: () => parlaysApi.tickets({ sport: scope === "sport" ? sport : undefined, status: "settled", source: source || undefined, limit: 50 }) });
  const d = q.data;
  return (
    <div className="space-y-3">
      <div className="flex items-center gap-2 text-xs text-gray-400 flex-wrap">
        <Pills options={[{ key: "sport", label: sport.toUpperCase() }, { key: "all", label: "All sports" }]} value={scope} onChange={(v) => setScope(v as "sport" | "all")} />
        <Pills options={[{ key: "", label: "All tickets" }, { key: "auto", label: "Auto" }, { key: "custom", label: "Saved custom" }]} value={source} onChange={setSource} />
        <Pills options={[{ key: "30", label: "30 d" }, { key: "90", label: "90 d" }, { key: "365", label: "1 y" }]} value={days} onChange={setDays} />
      </div>
      {q.isLoading && <Msg>Loading…</Msg>}
      {q.error && <Msg tone="error">{(q.error as Error).message}</Msg>}
      {d && (
        <>
          <Card className="space-y-3">
            <div className="grid grid-cols-3 md:grid-cols-6 gap-3">
              <Stat label="Settled" value={d.overall.n} />
              <Stat label="W-L-P" value={`${d.overall.wins}-${d.overall.losses}-${d.overall.pushes}`} />
              <Stat label="ROI (1u each)" value={`${signed(d.overall.roi)}${d.overall.roi_se != null ? ` ± ${pct(d.overall.roi_se)}` : ""}`} tone={d.overall.roi > 0 ? "pos" : "neg"} />
              <Stat label="Hit rate vs predicted" value={`${pct(d.overall.hit_rate)} / ${pct(d.overall.mean_p)}`} />
              <Stat label="Brier vs independent" value={fmtDiff(d.overall.brier_vs_indep, d.overall.brier_vs_indep_se)} />
              <Stat label="Brier vs market" value={fmtDiff(d.overall.brier_vs_market, d.overall.brier_vs_market_se)} />
            </div>
            <div className="text-[11px] text-gray-500">{d.note} {d.stake}. Open: {d.open}. Brier differences: negative = the model's joint probabilities beat the baseline; ± one standard error clustered by slate.</div>
          </Card>
          {d.overall.n === 0 && <Msg>No settled tickets yet. The autopilot records each slate's best tickets and grades them as games finish; results appear here after the first slate settles.</Msg>}
          {d.calibration.length > 0 && (
            <Card className="space-y-2">
              <div className="font-medium text-sm">Calibration (tickets whose legs all won or lost)</div>
              <div className="space-y-1">
                {d.calibration.map((b) => (
                  <div key={b.lo} className="flex items-center gap-3 text-xs">
                    <span className="w-24 text-gray-400">{pct(b.lo, 0)}–{pct(b.hi, 0)}</span>
                    <div className="relative flex-1 h-4 bg-white/5 rounded">
                      <div className="absolute h-4 bg-violet/30 rounded" style={{ left: `${b.ci[0] * 100}%`, width: `${(b.ci[1] - b.ci[0]) * 100}%` }} title="95% Wilson interval of the observed rate" />
                      <div className="absolute h-4 w-0.5 bg-amber" style={{ left: `${b.predicted * 100}%` }} title="predicted" />
                      <div className="absolute h-4 w-1 bg-emerald-light rounded" style={{ left: `${b.observed * 100}%` }} title="observed" />
                    </div>
                    <span className="w-48 font-mono text-gray-400">pred {pct(b.predicted)} · obs {pct(b.observed)} · n {b.n}</span>
                  </div>
                ))}
              </div>
              <div className="text-[11px] text-gray-500">amber = predicted, green = observed, band = 95% interval of the observed rate.</div>
            </Card>
          )}
          {d.overall.n > 0 && (
            <div className="grid xl:grid-cols-2 gap-3">
              {([["By leg count", d.by_legs], ["By market", d.by_category], ["By odds", d.by_odds], ["By dependence", d.by_dependence], ["Same vs cross game", d.by_scope], ["By sport", d.by_sport]] as [string, Summary[]][]).map(([title, rows]) => (
                <Card key={title} className="space-y-1">
                  <div className="font-medium text-sm">{title}</div>
                  <Table head={<tr><Th>{" "}</Th><Th className="text-right">n</Th><Th className="text-right">W-L</Th><Th className="text-right">ROI</Th><Th className="text-right">hit / pred</Th><Th className="text-right">Brier vs indep</Th></tr>}>
                    {rows.map((r) => (
                      <tr key={r.key} className="border-t border-glass">
                        <Td className="text-xs">{r.key}</Td><Td className="text-right font-mono">{r.n}</Td><Td className="text-right font-mono">{r.wins}-{r.losses}</Td>
                        <Td className="text-right font-mono">{signed(r.roi)}</Td><Td className="text-right font-mono">{pct(r.hit_rate)} / {pct(r.mean_p)}</Td>
                        <Td className="text-right font-mono">{fmtDiff(r.brier_vs_indep, r.brier_vs_indep_se)}</Td>
                      </tr>
                    ))}
                  </Table>
                </Card>
              ))}
            </div>
          )}
          {settled.data && settled.data.tickets.length > 0 && (
            <Card className="space-y-1">
              <div className="font-medium text-sm">Recently settled</div>
              <Table head={<tr><Th>Date</Th><Th>Legs</Th><Th className="text-right">Price</Th><Th className="text-right">P(win)</Th><Th>Result</Th><Th className="text-right">P/L</Th></tr>}>
                {settled.data.tickets.map((t) => (
                  <tr key={t.id} className="border-t border-glass">
                    <Td className="text-xs text-gray-400">{t.date}</Td><Td className="text-xs">{t.legs.map((l) => <div key={l.id}>{l.label}</div>)}</Td>
                    <Td className="text-right font-mono">{fmtOdds(t.price_decimal, fmt)}</Td><Td className="text-right font-mono">{pct(t.p_joint)}</Td>
                    <Td className={clsx("text-xs", resultTone(t.result))}>{t.result}</Td><Td className="text-right font-mono">{t.profit == null ? "—" : `${t.profit >= 0 ? "+" : ""}${t.profit.toFixed(2)}u`}</Td>
                  </tr>
                ))}
              </Table>
            </Card>
          )}
        </>
      )}
    </div>
  );
}
const fmtDiff = (x: number | null, se: number | null) => (x == null ? "—" : `${x >= 0 ? "+" : ""}${x.toFixed(4)}${se != null ? ` ± ${se.toFixed(4)}` : ""}`);

// --------------------------------------------------------------------------
// Model validation
// --------------------------------------------------------------------------

function ValidationTab() {
  const q = useQuery({ queryKey: ["parlay-validation"], queryFn: parlaysApi.validation, refetchInterval: 60_000 });
  const bankroll = useSettings((s) => s.bankroll);
  if (q.isLoading) return <Msg>Loading…</Msg>;
  if (q.error) return <Msg tone="error">{(q.error as Error).message}</Msg>;
  const d = q.data!;
  const p = d.preview;
  return (
    <div className="space-y-3">
      <Card className="space-y-2 text-sm">
        <div className="flex gap-6 flex-wrap">
          <Stat label="Parlay model in force" value={<span className="text-base">{d.champion}</span>} />
          <Stat label="Dependence weight" value={d.dep_weight.toFixed(2)} />
          <Stat label="Next test: candidate" value={p.candidate.toFixed(2)} />
          <Stat label="Train / test tickets" value={`${p.train_n} / ${p.test_n}`} />
          <Stat label="Test Brier diff" value={fmtDiff(p.test_diff, p.test_se)} />
        </div>
        <div className="text-xs text-gray-400">{p.reason.charAt(0).toUpperCase() + p.reason.slice(1)}. {d.rule.charAt(0).toUpperCase() + d.rule.slice(1)}.</div>
        <div className="text-xs text-gray-500">
          P(ticket) = w · P(dependent) + (1 − w) · P(independent). w = 1 trusts the simulators' joint outcomes; w = 0 ignores dependence. The leg probabilities themselves are learned by the AI Picks
          loop (blend weights fitted on settled lines; Model Intelligence). Stakes elsewhere use your ${bankroll} bankroll; this tool reports EV per unit and never sizes parlays.
        </div>
        {p.train_brier.length > 0 && p.train_n > 0 && (
          <div className="flex gap-3 text-xs font-mono text-gray-400">{p.train_brier.map((b) => <span key={b.a}>w={b.a.toFixed(2)}: {b.brier.toFixed(4)}</span>)}</div>
        )}
      </Card>
      <Card className="space-y-1">
        <div className="font-medium text-sm">Version history</div>
        <Table head={<tr><Th>Version</Th><Th>w</Th><Th>Status</Th><Th>Created</Th><Th>Promoted</Th><Th>Evidence</Th></tr>}>
          {d.models.map((m) => (
            <tr key={m.version} className="border-t border-glass text-xs">
              <Td>{m.version}</Td><Td className="font-mono">{m.dep_weight.toFixed(2)}</Td><Td>{m.status}</Td><Td>{new Date(m.created_at).toLocaleDateString()}</Td>
              <Td>{m.promoted_at ? new Date(m.promoted_at).toLocaleDateString() : "—"}</Td><Td className="text-gray-500">{String((m.evidence as { reason?: string; note?: string }).reason ?? (m.evidence as { note?: string }).note ?? "")}</Td>
            </tr>
          ))}
        </Table>
      </Card>
      <Card className="space-y-1">
        <div className="font-medium text-sm">Recent automated runs</div>
        <Table head={<tr><Th>Sport</Th><Th>Slate</Th><Th>Started</Th><Th className="text-right">Games</Th><Th className="text-right">Legs</Th><Th className="text-right">Stored</Th><Th className="text-right">Evaluated</Th><Th className="text-right">Sim s</Th><Th className="text-right">Search s</Th><Th>OK</Th></tr>}>
          {d.runs.map((r, i) => {
            const x = r.detail as Record<string, number | string | undefined>;
            return (
              <tr key={i} className="border-t border-glass text-xs">
                <Td>{r.sport}</Td><Td>{r.date}</Td><Td>{new Date(r.started_at).toLocaleString()}</Td><Td className="text-right font-mono">{x.simulated ?? 0}/{x.games ?? 0}</Td>
                <Td className="text-right font-mono">{x.eligible ?? "—"}/{x.legs ?? "—"}</Td><Td className="text-right font-mono">{x.stored ?? "—"}</Td><Td className="text-right font-mono">{x.evaluated ?? "—"}</Td>
                <Td className="text-right font-mono">{x.sim_secs ?? "—"}</Td><Td className="text-right font-mono">{x.generate_secs ?? "—"}</Td>
                <Td className={r.ok ? "text-emerald-light" : "text-crimson-light"}>{r.ok == null ? "running" : r.ok ? "ok" : String(x.error ?? "failed").slice(0, 60)}</Td>
              </tr>
            );
          })}
        </Table>
      </Card>
    </div>
  );
}
