import { useMemo, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import clsx from "clsx";
import { ChevronDown, ChevronUp, ExternalLink, RefreshCw, TriangleAlert } from "lucide-react";
import { useSettings } from "../store/settings";
import { formatUsd } from "../lib/odds";
import { Card, Input, Msg, Pills } from "./ui";
import { VENUE_LABEL, ageLabel, cents, pmArb, sizeFor, type PmLeg, type PmOpportunity, type PmSnapshot } from "../lib/pmArb";

/** Dollars with cents below $1,000 (arb profits are often cents). */
const usd = (n: number) => (Math.abs(n) < 1000 ? `${n < 0 ? "-" : ""}$${Math.abs(n).toFixed(2)}` : formatUsd(n));

// Prediction-market arbitrage: Kalshi vs Polymarket futures (champion, conference,
// division) and game winners. Research only: nothing here places orders.

const LEAGUES = [{ key: "all", label: "All" }, { key: "nfl", label: "NFL" }, { key: "nba", label: "NBA" }, { key: "nhl", label: "NHL" }, { key: "mlb", label: "MLB" }];
const KINDS = [{ key: "all", label: "All" }, { key: "futures", label: "Futures" }, { key: "game", label: "Games" }];
const SORTS = [{ key: "roi", label: "% Return" }, { key: "profit", label: "$ Depth profit" }, { key: "end", label: "End date" }, { key: "age", label: "Newest" }];

export function PmArb() {
  const qc = useQueryClient();
  const bankroll = useSettings((s) => s.bankroll);
  const setBankroll = useSettings((s) => s.setBankroll);
  const [league, setLeague] = useState("all");
  const [kind, setKind] = useState("all");
  const [sort, setSort] = useState("roi");
  const [q, setQ] = useState("");
  const [refreshing, setRefreshing] = useState(false);
  const [refreshErr, setRefreshErr] = useState<string | null>(null);
  const { data, isLoading, error } = useQuery({ queryKey: ["pm-arb"], queryFn: pmArb.get, refetchInterval: 30_000 });

  const rows = useMemo(() => {
    let r = (data?.opportunities ?? []).filter((o) => (league === "all" || o.league === league) && (kind === "all" || o.kind === kind));
    const needle = q.trim().toLowerCase();
    if (needle) r = r.filter((o) => `${o.team} ${o.family}`.toLowerCase().includes(needle));
    const by: Record<string, (a: PmOpportunity, b: PmOpportunity) => number> = {
      roi: (a, b) => b.roi - a.roi,
      profit: (a, b) => b.depth.profit - a.depth.profit,
      end: (a, b) => (a.days_to_resolve ?? 1e9) - (b.days_to_resolve ?? 1e9),
      age: (a, b) => new Date(b.first_seen).getTime() - new Date(a.first_seen).getTime(),
    };
    return [...r].sort(by[sort]);
  }, [data, league, kind, sort, q]);

  const refresh = async () => {
    setRefreshing(true);
    setRefreshErr(null);
    try {
      const s = await pmArb.refresh();
      qc.setQueryData<PmSnapshot>(["pm-arb"], s);
    } catch (e) {
      setRefreshErr((e as Error).message);
    } finally {
      setRefreshing(false);
    }
  };

  const total = rows.reduce((t, o) => t + o.depth.profit, 0);
  return (
    <div className="space-y-3">
      <div className="flex flex-wrap items-center gap-2">
        <Pills options={KINDS} value={kind} onChange={setKind} />
        <Pills options={LEAGUES} value={league} onChange={setLeague} />
        <Pills options={SORTS} value={sort} onChange={setSort} />
        <div className="ml-auto flex items-center gap-2 text-xs text-gray-400">
          <label className="flex items-center gap-1">Bankroll $<Input type="number" value={bankroll} onChange={(v) => setBankroll(Number(v) || 0)} className="w-24" /></label>
          <Input value={q} onChange={setQ} placeholder="Search team or market" className="w-48" />
          <button onClick={refresh} disabled={refreshing} title="Scan now (at most every 30 s)"
            className="flex items-center gap-1 px-2 py-1 rounded border border-glass hover:bg-white/5 disabled:opacity-50">
            <RefreshCw size={13} className={clsx(refreshing && "animate-spin")} /> Scan
          </button>
        </div>
      </div>

      {isLoading && <Msg>Loading the latest scan…</Msg>}
      {error && <Msg tone="error">Couldn't load prediction-market arbitrage: {(error as Error).message}. Is arbitrage-engine running?</Msg>}
      {refreshErr && <Msg tone="error">{refreshErr}</Msg>}
      {data?.status && <Msg>{data.status}; results appear in about a minute.</Msg>}

      {data?.scanned_at && (
        <div className="text-xs text-gray-500 flex flex-wrap gap-x-3">
          <span><span className="text-emerald-light font-medium">{rows.length}</span> arbs</span>
          <span>depth profit available <span className="text-emerald-light">{usd(total)}</span></span>
          <span>{data.pairs} matched pairs from {data.markets?.kalshi ?? 0} Kalshi and {data.markets?.polymarket ?? 0} Polymarket outcomes</span>
          <span>scanned {ageLabel(data.scanned_at)} ago in {data.seconds}s</span>
        </div>
      )}

      <div className="text-[11px] text-gray-500 flex items-start gap-1.5">
        <TriangleAlert size={13} className="text-amber shrink-0 mt-px" />
        Prices are the venues' order books at scan time, after taker fees. A pair is only an arb if both contracts resolve as
        exact opposites: read both rules, check the live books, and place both legs; a partial fill leaves you unhedged.
      </div>

      {rows.map((o) => <Row key={o.id} o={o} bankroll={bankroll} />)}

      {data?.scanned_at && rows.length === 0 && (
        <Card>
          <div className="text-sm text-gray-300 mb-2">No arbitrage right now. Closest pairs (cost of YES + NO, incl. fees):</div>
          <div className="text-xs font-mono text-gray-400 space-y-0.5">
            {(data.closest ?? []).map((c, i) => <div key={i}>{(c.unit_cost * 100).toFixed(2)}¢ · {c.family} · {c.team}</div>)}
          </div>
        </Card>
      )}

      {data?.scanned_at && (
        <div className="text-[11px] text-gray-600">
          Not matched: {Object.entries(data.unmatched ?? {}).map(([k, v]) => `${v} ${k}`).join(" · ") || "none"}
          {(data.unresolved?.length ?? 0) > 0 && <> · names not tied to one team: {data.unresolved!.join(", ")}</>}
          {(data.errors?.length ?? 0) > 0 && <span className="text-crimson-light"> · errors: {data.errors!.slice(0, 3).join("; ")}</span>}
        </div>
      )}
    </div>
  );
}

function Row({ o, bankroll }: { o: PmOpportunity; bankroll: number }) {
  const [open, setOpen] = useState(false);
  const size = sizeFor(o, bankroll);
  const warn = o.checks.filter((c) => !c.ok);
  return (
    <Card className="!p-0">
      <button onClick={() => setOpen(!open)} className="w-full text-left px-4 py-3 grid grid-cols-12 gap-3 items-center hover:bg-white/[0.02]">
        <div className="col-span-12 md:col-span-5 min-w-0">
          <div className="text-sm text-gray-100 truncate">{o.team}</div>
          <div className="text-xs text-gray-500 truncate">{o.family}{o.kind === "futures" && o.days_to_resolve != null && ` · ends in ${o.days_to_resolve} days`}</div>
          {warn.length > 0 && <div className="text-[11px] text-amber mt-0.5">check: {warn.map((w) => w.name).join(", ")}</div>}
        </div>
        <div className="col-span-1 text-center"><div className="text-[10px] text-gray-500 uppercase">Age</div><div className="text-sm text-emerald-light font-mono">{ageLabel(o.first_seen)}</div></div>
        <div className="col-span-4 md:col-span-3 text-xs font-mono space-y-0.5">
          {o.legs.map((l) => (
            <div key={l.venue} className="flex justify-between gap-2">
              <span className="text-gray-400">{VENUE_LABEL[l.venue] ?? l.venue}</span>
              <span className={l.side === "yes" ? "text-emerald-light" : "text-crimson-light"}>{l.side === "yes" ? "Y" : "N"} {cents(l.price)}</span>
            </div>
          ))}
        </div>
        <div className="col-span-3 md:col-span-1 text-right"><div className="text-[10px] text-gray-500 uppercase">Return</div><div className="text-sm font-mono text-violet-light">+{(o.roi * 100).toFixed(2)}%</div></div>
        <div className="col-span-3 md:col-span-2 text-right">
          <div className="text-[10px] text-gray-500 uppercase">Depth profit</div>
          <div className="text-sm font-mono text-emerald-light">+{usd(o.depth.profit)}</div>
          <div className="text-[10px] text-gray-500 flex items-center justify-end gap-1">{Math.round(o.depth.contracts).toLocaleString()} pairs {open ? <ChevronUp size={12} /> : <ChevronDown size={12} />}</div>
        </div>
      </button>

      {open && (
        <div className="border-t border-glass px-4 py-3 space-y-3">
          <div className="grid grid-cols-2 md:grid-cols-5 gap-3 text-sm">
            <Box label="Profit" value={`${size.profit >= 0 ? "+" : ""}${usd(size.profit)}`} tone="pos" />
            <Box label="Return" value={`${size.cost > 0 ? ((size.profit / size.cost) * 100).toFixed(2) : "0.00"}%`} />
            <Box label="Pairs (contracts per side)" value={size.contracts.toLocaleString(undefined, { maximumFractionDigits: 2 })} />
            <Box label="Cost incl. fees" value={usd(size.cost)} />
            <Box label="Payout" value={usd(size.payout)} />
          </div>
          {size.contracts < 1 && <Msg>The bankroll buys less than one pair at these prices.</Msg>}
          <div className="grid md:grid-cols-2 gap-3">
            <LegCard l={o.legs[0]} stake={size.stakeA} contracts={size.contracts} />
            <LegCard l={o.legs[1]} stake={size.stakeB} contracts={size.contracts} />
          </div>
          <div className="text-xs space-y-0.5">
            {o.checks.map((c) => (
              <div key={c.name} className={c.ok ? "text-gray-400" : "text-amber"}>{c.ok ? "✓" : "!"} {c.name}: <span className="text-gray-500">{c.detail}</span></div>
            ))}
          </div>
          <div className="text-[11px] text-gray-500">
            Depth: {o.depth.steps.map((s) => `${Math.round(s.contracts).toLocaleString()} @ ${(s.unit_cost * 100).toFixed(2)}¢`).join(" · ")}
            {" · "}fees counted {usd(o.depth.fees)}. Kalshi rounds its fee up to the cent per order, so small orders pay slightly more.
          </div>
        </div>
      )}
    </Card>
  );
}

function Box({ label, value, tone }: { label: string; value: string; tone?: "pos" }) {
  return (
    <div className="rounded border border-glass px-3 py-2">
      <div className="text-[10px] text-gray-500 uppercase">{label}</div>
      <div className={clsx("font-mono", tone === "pos" ? "text-emerald-light" : "text-gray-200")}>{value}</div>
    </div>
  );
}

function LegCard({ l, stake, contracts }: { l: PmLeg; stake: number; contracts: number }) {
  const [rules, setRules] = useState(false);
  return (
    <div className="rounded border border-glass p-3 space-y-2">
      <div className="flex items-center justify-between">
        <span className="text-sm text-gray-200">{VENUE_LABEL[l.venue] ?? l.venue}</span>
        <span className={clsx("text-sm font-mono", l.side === "yes" ? "text-emerald-light" : "text-crimson-light")}>Buy {l.side.toUpperCase()} {cents(l.price)}</span>
      </div>
      <div className="text-xs text-gray-400">{l.event_title} · <span className="text-gray-300">{l.outcome}</span></div>
      <div className="flex justify-between text-xs"><span className="text-gray-500">Stake</span><span className="font-mono text-gray-200">{usd(stake)} for {contracts.toLocaleString(undefined, { maximumFractionDigits: 2 })} contracts</span></div>
      <div className="flex justify-between text-xs">
        <span className="text-gray-500">Best-price liquidity</span>
        <span className="font-mono text-gray-300">{l.top_size != null ? `${Math.round(l.top_size).toLocaleString()} contracts` : "—"}</span>
      </div>
      <div className="flex justify-between text-xs">
        <span className="text-gray-500">Taker fee</span>
        <span className={clsx("font-mono", l.fee_known ? "text-gray-300" : "text-amber")}>{l.fee_known ? `${cents(l.fee_per_contract)} / contract` : "unknown, not counted"}</span>
      </div>
      <button onClick={() => setRules(!rules)} className="text-xs text-violet-light hover:underline">{rules ? "Hide" : "Read"} the rules</button>
      {rules && <div className="text-[11px] text-gray-400 whitespace-pre-wrap max-h-48 overflow-auto border border-glass rounded p-2">{l.rules || "The venue gave no rules text."}</div>}
      <a href={l.url} target="_blank" rel="noreferrer" className="flex items-center justify-center gap-1 text-xs border border-glass rounded py-1.5 hover:bg-white/5">
        Go to {VENUE_LABEL[l.venue] ?? l.venue} <ExternalLink size={12} />
      </a>
    </div>
  );
}
