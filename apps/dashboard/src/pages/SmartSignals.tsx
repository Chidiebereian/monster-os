import { useMemo, useState } from "react";
import { Link } from "react-router-dom";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import clsx from "clsx";
import { ChevronDown, ChevronUp, ExternalLink, LineChart, Loader2, RefreshCw } from "lucide-react";
import { Card, Msg, Pills } from "../components/ui";
import { Avatar, catLabel } from "../components/pm";
import { formatPct, formatUsd } from "../lib/odds";
import { PM_CATEGORIES, cents, displayName, pmSignals, timeAgo, type Consensus, type PmSignal } from "../lib/pm";

// Smart Signals: trades from Polymarket wallets that are proven in the trade's
// own category, scored 0-100 and kept only while still tail-able. Built by
// whale-tracker's signals cycle (every 10 min) from Polymarket public data.
export function SmartSignals() {
  const [tab, setTab] = useState("sharp");
  const [mode, setMode] = useState("precision");
  const [category, setCategory] = useState("overall");
  return (
    <div className="space-y-4">
      <div className="flex items-center gap-3 flex-wrap">
        <h1 className="text-lg font-semibold">Smart Signals</h1>
        <span className="text-sm text-gray-500">a good wallet, in its proven category, at a still-clean price</span>
        <div className="ml-auto flex items-center gap-2 flex-wrap">
          <Pills options={[{ key: "sharp", label: "Sharp" }, { key: "consensus", label: "Consensus" }, { key: "history", label: "History" }]} value={tab} onChange={setTab} />
          {tab === "sharp" && <Pills options={[{ key: "precision", label: "Precision" }, { key: "flow", label: "Flow" }]} value={mode} onChange={setMode} />}
          <select value={category} onChange={(e) => setCategory(e.target.value)}
            className="bg-surface border border-glass rounded-md px-2 py-1 text-xs text-gray-200 focus:outline-none">
            {PM_CATEGORIES.map((c) => <option key={c.key} value={c.key}>Category: {c.label}</option>)}
          </select>
        </div>
      </div>
      <TrackRecord />
      {tab === "sharp" && <Sharp mode={mode} category={category} />}
      {tab === "consensus" && <ConsensusFeed category={category} />}
      {tab === "history" && <History category={category} />}
    </div>
  );
}

function scoreTone(s: number | null) {
  return s == null ? "text-gray-500" : s >= 80 ? "text-emerald-light" : s >= 65 ? "text-violet-light" : "text-gray-300";
}

function TrackRecord() {
  const { data } = useQuery({ queryKey: ["sig-record"], queryFn: pmSignals.record, refetchInterval: 300_000 });
  if (!data) return null;
  const by = Object.fromEntries(data.modes.map((m) => [m.mode, m]));
  const cell = (k: string, label: string) => {
    const m = by[k];
    return (
      <div className="flex items-baseline gap-2">
        <span className="text-gray-500">{label}</span>
        {m && m.resolved > 0 ? (
          <>
            <span className="font-mono text-gray-200">{m.won}/{m.resolved} won</span>
            <span className={clsx("font-mono", (m.roi ?? 0) >= 0 ? "text-emerald-light" : "text-crimson-light")}>
              ROI {m.roi != null ? `${m.roi >= 0 ? "+" : ""}${(m.roi * 100).toFixed(1)}%` : "—"}
            </span>
          </>
        ) : <span className="text-gray-600">no resolved signals yet</span>}
      </div>
    );
  };
  return (
    <Card className="!py-2 flex flex-wrap items-center gap-x-6 gap-y-1 text-xs">
      <span className="uppercase tracking-wide text-[10px] text-gray-500">Track record</span>
      {cell("precision", "Precision")}
      {cell("flow", "Flow")}
      <span className="text-gray-600">{data.unresolved} awaiting resolution · tailing $1 at the best ask when first shown</span>
    </Card>
  );
}

function Sharp({ mode, category }: { mode: string; category: string }) {
  const qc = useQueryClient();
  const { data, isLoading, error, isFetching } = useQuery({
    queryKey: ["pm-signals", mode, category],
    queryFn: () => pmSignals.list({ mode, category, limit: 60 }),
    refetchInterval: 30_000,
  });
  const st = data?.status;
  return (
    <div className="space-y-3">
      <div className="flex items-center gap-2 text-[11px] text-gray-500">
        {st?.state === "running" && <><Loader2 size={12} className="animate-spin" /> scanning sharp wallets…</>}
        {st?.state === "ok" && st.last && <>last scan {timeAgo(Date.parse(st.last.started) / 1000)} · {st.last.wallets} sharp wallets · {st.last.candidates} recent positions checked · {st.last.seconds}s</>}
        {st?.state === "error" && <span className="text-crimson-light">last scan failed: {st.error}</span>}
        <button onClick={() => pmSignals.run().then(() => setTimeout(() => qc.invalidateQueries({ queryKey: ["pm-signals"] }), 1500))}
          className="ml-auto inline-flex items-center gap-1 border border-glass rounded px-2 py-0.5 hover:bg-white/5">
          <RefreshCw size={11} className={clsx(isFetching && "animate-spin")} /> Scan now
        </button>
      </div>
      <div className="text-[11px] text-gray-500">
        {mode === "precision"
          ? "Precision: proven in this category, no bot/arber pattern, ≤3¢ slippage, ≥$500 within 2¢ of the ask, score ≥ 70."
          : "Flow: any tracked sharp, score ≥ 50 and ≤8¢ slippage. More signals, broader coverage, weaker filter."}
      </div>
      {isLoading && <Msg>Loading signals…</Msg>}
      {error && <Msg tone="error">{(error as Error).message}</Msg>}
      {data && data.rows.length === 0 && (
        <Msg>{st?.state === "running" || st?.state === "never run" ? "The first scan is still running (a few minutes)." : `No ${mode} signals right now.`}</Msg>
      )}
      <div className="grid gap-3 md:grid-cols-2 2xl:grid-cols-3">
        {data?.rows.map((s) => <SignalCard key={s.id} s={s} mode={mode} />)}
      </div>
    </div>
  );
}

function Chip({ label, children, tone, title }: { label: string; children: React.ReactNode; tone?: string; title?: string }) {
  return (
    <div title={title} className="rounded-md border border-glass bg-obsidian/40 px-2 py-1">
      <div className="text-[9px] uppercase tracking-wide text-gray-600">{label}</div>
      <div className={clsx("font-mono text-xs", tone ?? "text-gray-200")}>{children}</div>
    </div>
  );
}

const PART_LABELS: Record<string, [string, number]> = {
  category_proof: ["Category proof", 30], overall_record: ["Overall record", 10], slippage: ["Slippage", 20],
  relative_size: ["Relative size", 15], position_value: ["Position value", 5], liquidity: ["Liquidity", 10],
  recency: ["Recency", 5], entry_band: ["Entry price", 5],
};
const FLAG_TEXT: Record<string, string> = {
  unproven_in_category: "not proven in this category",
  arber_or_maker: "arber / market-maker pattern",
  bot_pace: "bot-like trade pace",
};

function SignalCard({ s, mode }: { s: PmSignal; mode: string }) {
  const [open, setOpen] = useState(false);
  const [chart, setChart] = useState(false);
  const isNo = /^(no|under)$/i.test(s.outcome ?? "");
  const slip = s.slippage;
  return (
    <Card className="space-y-3">
      <div className="flex items-start gap-3">
        {s.icon ? <img src={s.icon} alt="" className="w-12 h-12 rounded-lg object-cover shrink-0" /> : <div className="w-12 h-12 rounded-lg bg-white/5 shrink-0" />}
        <div className="min-w-0 flex-1">
          <div className="text-[10px] uppercase tracking-wide text-gray-500">
            {s.category ? catLabel(s.category) : "—"} · <span className="text-violet-light">{mode}</span>
          </div>
          <a href={s.event_slug ? `https://polymarket.com/event/${s.event_slug}` : undefined} target="_blank" rel="noreferrer"
            className="font-medium leading-snug hover:underline line-clamp-2" title={s.title ?? ""}>{s.title}</a>
        </div>
        <div className="text-right shrink-0">
          <div className={clsx("text-3xl font-bold font-mono leading-none", scoreTone(s.score))}>{s.score}</div>
          <div className="text-[9px] uppercase tracking-wide text-gray-600 mt-1">score</div>
        </div>
      </div>
      <div className="flex flex-wrap gap-1.5 items-stretch">
        <Chip label="Trader entry" tone={isNo ? "text-crimson-light" : "text-emerald-light"}>{(s.outcome ?? "").toUpperCase()} {cents(s.entry_price)}</Chip>
        <Chip label="Position" title={`${s.entry_shares?.toLocaleString(undefined, { maximumFractionDigits: 0 })} shares bought in the last 48 h`}>{formatUsd(s.entry_usdc)}</Chip>
        <Chip label="Best ask now">{cents(s.current_price)}</Chip>
        <Chip label="Rel. size" title={s.normal_bet ? `normal bet ${formatUsd(s.normal_bet)}` : undefined}>{s.rel_size != null ? `${s.rel_size.toFixed(1)}x` : "—"}</Chip>
        <Chip label="Cat. P&L/share" title="All-time P&L in this category per share traded (Polymarket reports category volume in shares)" tone={(s.cat_roi ?? 0) > 0 ? "text-emerald-light" : "text-crimson-light"}>
          {s.cat_roi != null ? `${s.cat_roi >= 0 ? "+" : ""}${(s.cat_roi * 100).toFixed(1)}¢` : "—"}
        </Chip>
        <Chip label="Slip" tone={slip == null ? undefined : slip <= 0.01 ? "text-emerald-light" : slip <= 0.03 ? "text-amber" : "text-crimson-light"}>
          {slip != null ? `${slip >= 0 ? "+" : ""}${(slip * 100).toFixed(1)}¢` : "—"}
        </Chip>
        <Chip label="Depth 2¢" title="Dollars offered within 2¢ of the best ask">{formatUsd(s.depth_2c)}</Chip>
      </div>
      <Link to={`/wallets/${s.wallet}`} className="flex items-center gap-2 text-xs hover:opacity-90">
        <Avatar wallet={s.wallet} src={s.profile_image} size={22} />
        <span className="font-medium text-gray-200">{displayName(s.name, s.wallet)}</span>
        <span className="text-gray-500">
          {s.category && s.cat_rank ? `#${s.cat_rank.toLocaleString()} in ${catLabel(s.category)}` : "unranked here"}
          {s.cat_pnl != null && <> · {formatUsd(s.cat_pnl)} all-time</>}
        </span>
        <span className="ml-auto text-gray-500">wallet <span className={clsx("font-mono", scoreTone(s.wallet_score))}>{s.wallet_score}</span></span>
      </Link>
      {(s.flags ?? []).length > 0 && (
        <div className="flex flex-wrap gap-1">
          {(s.flags ?? []).map((f) => <span key={f} className="text-[10px] px-1.5 py-0.5 rounded bg-amber/15 text-amber">{FLAG_TEXT[f] ?? f}</span>)}
        </div>
      )}
      <div className="flex items-center gap-3 text-[11px] text-gray-500 border-t border-glass pt-2">
        <span>{s.last_trade_at ? `last buy ${timeAgo(Date.parse(s.last_trade_at) / 1000)}` : ""}</span>
        <button onClick={() => setChart((c) => !c)} className={clsx("inline-flex items-center gap-1 hover:text-gray-200", chart && "text-violet-light")}><LineChart size={12} /> Chart</button>
        <button onClick={() => setOpen((o) => !o)} className="inline-flex items-center gap-1 hover:text-gray-200">{open ? <ChevronUp size={12} /> : <ChevronDown size={12} />} Why</button>
        <a href={s.event_slug ? `https://polymarket.com/event/${s.event_slug}` : undefined} target="_blank" rel="noreferrer" className="ml-auto inline-flex items-center gap-1 hover:text-gray-200">Polymarket <ExternalLink size={10} /></a>
      </div>
      {chart && <PriceChart asset={s.asset} entry={s.entry_price} current={s.current_price} />}
      {open && s.parts && (
        <div className="space-y-1">
          {Object.entries(PART_LABELS).map(([k, [label, max]]) => {
            const v = s.parts?.[k] ?? 0;
            return (
              <div key={k} className="flex items-center gap-2 text-[11px]">
                <span className="w-28 text-gray-400">{label}</span>
                <div className="flex-1 h-1.5 rounded bg-white/5"><div className="h-1.5 rounded bg-violet" style={{ width: `${(v / max) * 100}%` }} /></div>
                <span className="w-12 text-right font-mono text-gray-300">{v.toFixed(1)}/{max}</span>
              </div>
            );
          })}
        </div>
      )}
    </Card>
  );
}

function PriceChart({ asset, entry, current }: { asset: string; entry: number | null; current: number | null }) {
  const [range, setRange] = useState("1w");
  const { data, isLoading } = useQuery({ queryKey: ["sig-chart", asset, range], queryFn: () => pmSignals.chart(asset, range) });
  const geo = useMemo(() => {
    const pts = data?.points ?? [];
    if (pts.length < 2) return null;
    const W = 600, H = 140, pad = 6;
    const t0 = pts[0].t, t1 = pts[pts.length - 1].t;
    const vals = [...pts.map((p) => p.p), ...(entry != null ? [entry] : []), ...(current != null ? [current] : [])];
    let lo = Math.min(...vals), hi = Math.max(...vals);
    const m = Math.max(0.01, (hi - lo) * 0.1); lo -= m; hi += m;
    const x = (t: number) => pad + ((t - t0) / (t1 - t0 || 1)) * (W - 2 * pad);
    const y = (v: number) => pad + (1 - (v - lo) / (hi - lo)) * (H - 2 * pad);
    return { W, H, y, path: pts.map((p, i) => `${i ? "L" : "M"}${x(p.t).toFixed(1)},${y(p.p).toFixed(1)}`).join("") };
  }, [data, entry, current]);
  return (
    <div className="space-y-1">
      <Pills options={[{ key: "1d", label: "1D" }, { key: "1w", label: "1W" }, { key: "1m", label: "1M" }]} value={range} onChange={setRange} />
      {isLoading && <div className="h-[140px] flex items-center justify-center"><Loader2 className="animate-spin text-gray-600" size={16} /></div>}
      {!isLoading && !geo && <div className="text-[11px] text-gray-500">No price history.</div>}
      {geo && (
        <svg viewBox={`0 0 ${geo.W} ${geo.H}`} className="w-full h-[140px]" preserveAspectRatio="none">
          <path d={geo.path} fill="none" stroke="#a78bfa" strokeWidth={2} vectorEffect="non-scaling-stroke" />
          {entry != null && <line x1={0} x2={geo.W} y1={geo.y(entry)} y2={geo.y(entry)} stroke="#34d399" strokeDasharray="5 4" vectorEffect="non-scaling-stroke" />}
          {current != null && <line x1={0} x2={geo.W} y1={geo.y(current)} y2={geo.y(current)} stroke="#f59e0b" strokeDasharray="2 3" vectorEffect="non-scaling-stroke" />}
        </svg>
      )}
      <div className="flex gap-3 text-[10px] text-gray-500">
        <span><span className="inline-block w-3 border-t border-dashed border-emerald-light align-middle mr-1" />trader entry {cents(entry)}</span>
        <span><span className="inline-block w-3 border-t border-dotted border-amber align-middle mr-1" />best ask now {cents(current)}</span>
      </div>
    </div>
  );
}

function ConsensusFeed({ category }: { category: string }) {
  const [min, setMin] = useState(2);
  const { data, isLoading, error } = useQuery({
    queryKey: ["pm-consensus", category, min],
    queryFn: () => pmSignals.consensus({ category, min_wallets: min }),
    refetchInterval: 60_000,
  });
  return (
    <div className="space-y-3">
      <div className="flex items-center gap-2 text-xs text-gray-500">
        Markets where several tracked sharps hold the same outcome now (flow-qualified)
        <div className="ml-auto"><Pills options={[{ key: "2", label: "2+ sharps" }, { key: "3", label: "3+" }, { key: "5", label: "5+" }]} value={String(min)} onChange={(v) => setMin(Number(v))} /></div>
      </div>
      {isLoading && <Msg>Finding clusters…</Msg>}
      {error && <Msg tone="error">{(error as Error).message}</Msg>}
      {data && data.rows.length === 0 && <Msg>No market has {min}+ sharps on the same side right now.</Msg>}
      <div className="grid gap-3 md:grid-cols-2 2xl:grid-cols-3">
        {data?.rows.map((c) => <ConsensusCard key={c.asset} c={c} />)}
      </div>
    </div>
  );
}

function ConsensusCard({ c }: { c: Consensus }) {
  const isNo = /^(no|under)$/i.test(c.outcome ?? "");
  const slip = c.current_price != null && c.avg_entry != null ? c.current_price - c.avg_entry : null;
  return (
    <Card className="space-y-3">
      <div className="flex items-start gap-3">
        {c.icon ? <img src={c.icon} alt="" className="w-12 h-12 rounded-lg object-cover shrink-0" /> : <div className="w-12 h-12 rounded-lg bg-white/5" />}
        <div className="min-w-0 flex-1">
          <div className="text-[10px] uppercase tracking-wide text-gray-500">{c.category ? catLabel(c.category) : "—"} · consensus</div>
          <a href={c.event_slug ? `https://polymarket.com/event/${c.event_slug}` : undefined} target="_blank" rel="noreferrer" className="font-medium leading-snug hover:underline line-clamp-2">{c.title}</a>
        </div>
        <div className="text-right"><div className={clsx("text-3xl font-bold font-mono leading-none", scoreTone(c.score))}>{c.score}</div><div className="text-[9px] uppercase text-gray-600 mt-1">score</div></div>
      </div>
      <div className="flex flex-wrap gap-1.5">
        <Chip label="Side" tone={isNo ? "text-crimson-light" : "text-emerald-light"}>{(c.outcome ?? "").toUpperCase()}</Chip>
        <Chip label="Sharps">{c.wallets}</Chip>
        <Chip label="Opposed" title="Tracked sharps holding the other side of this market" tone={c.opposed > 0 ? "text-amber" : "text-gray-400"}>{c.opposed}</Chip>
        <Chip label="Total in">{formatUsd(c.usdc)}</Chip>
        <Chip label="Avg entry">{cents(c.avg_entry)}</Chip>
        <Chip label="Best ask">{cents(c.current_price)}</Chip>
        <Chip label="Slip" tone={slip == null ? undefined : slip <= 0.01 ? "text-emerald-light" : slip <= 0.03 ? "text-amber" : "text-crimson-light"}>{slip != null ? `${slip >= 0 ? "+" : ""}${(slip * 100).toFixed(1)}¢` : "—"}</Chip>
        <Chip label="Avg wallet">{c.avg_wallet_score.toFixed(0)}</Chip>
      </div>
      <div className="space-y-1">
        {(c.members ?? []).slice(0, 5).map((m) => (
          <Link key={m.wallet} to={`/wallets/${m.wallet}`} className="flex items-center gap-2 text-[11px] hover:opacity-90">
            <Avatar wallet={m.wallet} size={16} />
            <span className="text-gray-300">{displayName(m.name, m.wallet)}</span>
            <span className="ml-auto font-mono text-gray-400">{formatUsd(m.usdc)} @ {cents(m.entry)}</span>
            <span className={clsx("font-mono w-6 text-right", scoreTone(m.wallet_score))}>{m.wallet_score}</span>
          </Link>
        ))}
      </div>
    </Card>
  );
}

function History({ category }: { category: string }) {
  const { data, isLoading, error } = useQuery({ queryKey: ["pm-sig-history", category], queryFn: () => pmSignals.history({ category, limit: 150 }), refetchInterval: 120_000 });
  if (isLoading) return <Msg>Loading history…</Msg>;
  if (error) return <Msg tone="error">{(error as Error).message}</Msg>;
  if (!data || data.rows.length === 0) return <Msg>No closed signals yet. Signals move here when they go stale, the trader exits, the price runs away, or the market resolves.</Msg>;
  return (
    <div className="overflow-auto rounded-lg border border-glass">
      <table className="w-full text-sm">
        <thead className="bg-surface text-[11px] uppercase text-gray-400">
          <tr><th className="text-left px-3 py-2">Signal</th><th className="text-right px-3 py-2">Best score</th><th className="text-left px-3 py-2">Mode</th>
            <th className="text-right px-3 py-2">Entry</th><th className="text-right px-3 py-2">Shown at</th><th className="text-left px-3 py-2">Closed</th><th className="text-left px-3 py-2">Result</th></tr>
        </thead>
        <tbody>
          {data.rows.map((s) => (
            <tr key={s.id} className="border-t border-glass">
              <td className="px-3 py-2">
                <div className="truncate max-w-[380px]" title={s.title ?? ""}>{s.title}</div>
                <div className="text-[11px] text-gray-500">{(s.outcome ?? "").toUpperCase()} · {displayName(s.name, s.wallet)} · {s.category ? catLabel(s.category) : ""}</div>
              </td>
              <td className={clsx("px-3 py-2 text-right font-mono", scoreTone(s.best_score))}>{s.best_score}</td>
              <td className="px-3 py-2 text-xs text-gray-400">{s.precision ? "precision" : "flow"}</td>
              <td className="px-3 py-2 text-right font-mono text-gray-300">{cents(s.entry_price)}</td>
              <td className="px-3 py-2 text-right font-mono text-gray-300">{cents(s.price_at_signal)}</td>
              <td className="px-3 py-2 text-xs text-gray-500"><div>{s.close_reason}</div><div>{s.closed_at ? timeAgo(Date.parse(s.closed_at) / 1000) : ""}</div></td>
              <td className="px-3 py-2 text-xs">
                {s.result === "won" && <span className="px-1.5 py-0.5 rounded bg-emerald/15 text-emerald-light">won</span>}
                {s.result === "lost" && <span className="px-1.5 py-0.5 rounded bg-crimson/15 text-crimson-light">lost</span>}
                {s.result === "void" && <span className="text-gray-400">void</span>}
                {!s.result && <span className="text-gray-600">pending</span>}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
      <div className="px-3 py-2 text-[11px] text-gray-600">Win rate of past signals: {formatPct(data.rows.filter((r) => r.result === "won").length / Math.max(1, data.rows.filter((r) => r.result === "won" || r.result === "lost").length))} of the resolved rows shown.</div>
    </div>
  );
}
