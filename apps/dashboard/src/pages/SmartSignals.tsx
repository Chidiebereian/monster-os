import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import clsx from "clsx";
import { CandlestickChart as ChartIcon } from "lucide-react";
import { api, type Signal, type PersistedSignal } from "../lib/api";
import { formatPct } from "../lib/odds";
import { Card, Msg, PageTitle, Pills, Table, Td, Th } from "../components/ui";
import { MarketCandles } from "../components/MarketCandles";

// Module 5: Smart Signals — score tracked sharp wallets' trades (Sharp) and
// find clusters (Consensus), from sport-intelligence.
export function SmartSignals() {
  const [tab, setTab] = useState("sharp");
  const [mode, setMode] = useState("flow");
  return (
    <div className="space-y-4">
      <PageTitle
        title="Smart Signals"
        subtitle="tail-able sharp wallet activity, scored 0–100"
        right={
          <>
            <Pills options={[{ key: "sharp", label: "Sharp" }, { key: "consensus", label: "Consensus" }, { key: "history", label: "History" }]} value={tab} onChange={setTab} />
            {tab === "sharp" && <Pills options={[{ key: "precision", label: "Precision" }, { key: "flow", label: "Flow" }]} value={mode} onChange={setMode} />}
          </>
        }
      />
      {tab === "sharp" && <Sharp mode={mode} />}
      {tab === "consensus" && <ConsensusFeed />}
      {tab === "history" && <History />}
    </div>
  );
}

function scoreColor(s: number) {
  return s >= 80 ? "text-emerald-light" : s >= 65 ? "text-amber" : "text-gray-400";
}

function Sharp({ mode }: { mode: string }) {
  const { data, isLoading, error } = useQuery({
    queryKey: ["signals", mode],
    queryFn: () => api.signals({ mode, category: "sports", include_venue: true, limit: 30 }),
    refetchInterval: 15_000,
  });
  if (isLoading) return <Msg>Scoring signals…</Msg>;
  if (error) return <Msg tone="error">{(error as Error).message}</Msg>;
  if (!data || data.signals.length === 0) return <Msg>No {mode} signals right now (needs tracked wallets with recent sports trades).</Msg>;
  return (
    <div className="grid gap-3 md:grid-cols-2 xl:grid-cols-3">
      {data.signals.map((s, i) => <SignalCard key={i} s={s} />)}
    </div>
  );
}

function SignalCard({ s }: { s: Signal }) {
  const [showChart, setShowChart] = useState(false);
  const slipPct = s.slippage != null ? `${s.slippage > 0 ? "+" : ""}${(s.slippage * 100).toFixed(1)}c` : "stale";
  return (
    <Card className="space-y-2">
      <div className="flex items-start gap-2">
        <div className={clsx("text-2xl font-bold font-mono leading-none", scoreColor(s.score))}>{s.score}</div>
        <div className="min-w-0">
          <div className="text-[10px] uppercase tracking-wide text-gray-500">score · {s.category}</div>
          <div className="text-sm font-medium truncate">{s.market_id}</div>
        </div>
        <button
          onClick={() => setShowChart((v) => !v)}
          title="Price action from tracked-wallet fills"
          className={clsx("ml-auto p-1 rounded transition-colors", showChart ? "text-violet-light bg-violet/15" : "text-gray-500 hover:text-gray-200")}
        >
          <ChartIcon size={14} />
        </button>
        <span className={clsx("text-[10px] px-1.5 py-0.5 rounded uppercase", s.side === "buy" ? "bg-emerald/15 text-emerald-light" : "bg-crimson/15 text-crimson-light")}>{s.side}</span>
      </div>
      {showChart && (
        <div className="pt-1 border-t border-glass">
          <MarketCandles marketId={s.market_id} refPrice={s.entry_price} refLabel="entry" />
        </div>
      )}
      <div className="grid grid-cols-3 gap-2 text-xs">
        <Mini label="Entry" value={s.entry_price.toFixed(2)} />
        <Mini label="Current" value={s.current_price != null ? s.current_price.toFixed(2) : "—"} />
        <Mini label="Slip" value={slipPct} tone={s.tailable ? "pos" : undefined} />
        <Mini label="Wallet" value={String(s.wallet_score)} />
        <Mini label="Cat WR" value={s.category_win_rate != null ? formatPct(s.category_win_rate) : "—"} />
        <Mini label="Size" value={`${s.relative_bet_size.toFixed(1)}x`} />
      </div>
      {s.venue && s.venue.best_venue && (
        <div className="text-[11px] text-gray-500 pt-1 border-t border-glass">
          best venue: <span className="text-gray-300">{s.venue.best_venue}</span>
          {s.venue.entry_saving != null && s.venue.entry_saving > 0 && <span className="text-emerald-light"> · save {(s.venue.entry_saving * 100).toFixed(1)}c</span>}
        </div>
      )}
    </Card>
  );
}

function Mini({ label, value, tone }: { label: string; value: string; tone?: "pos" }) {
  return (
    <div>
      <div className="text-[10px] uppercase text-gray-600">{label}</div>
      <div className={clsx("font-mono", tone === "pos" ? "text-emerald-light" : "text-gray-200")}>{value}</div>
    </div>
  );
}

function ConsensusFeed() {
  const { data, isLoading, error } = useQuery({ queryKey: ["consensus"], queryFn: () => api.consensus({ category: "sports", min_wallets: 2 }), refetchInterval: 20_000 });
  if (isLoading) return <Msg>Finding clusters…</Msg>;
  if (error) return <Msg tone="error">{(error as Error).message}</Msg>;
  if (!data || data.consensus.length === 0) return <Msg>No consensus clusters (needs ≥2 sharps on the same side).</Msg>;
  return (
    <Table head={<tr><Th className="text-right">Score</Th><Th>Market</Th><Th>Side</Th><Th className="text-right">Sharps</Th><Th className="text-right">Avg entry</Th><Th className="text-right">Current</Th><Th className="text-right">Avg wallet</Th></tr>}>
      {data.consensus.map((c, i) => (
        <tr key={i} className="border-t border-glass hover:bg-white/[0.03]">
          <Td className={clsx("text-right font-mono font-bold", scoreColor(c.score))}>{c.score}</Td>
          <Td className="text-gray-300 max-w-md truncate">{c.market_id}</Td>
          <Td className="uppercase text-xs">{c.side}</Td>
          <Td className="text-right font-mono">{c.num_wallets}</Td>
          <Td className="text-right font-mono text-gray-400">{c.avg_entry.toFixed(2)}</Td>
          <Td className="text-right font-mono">{c.current_price != null ? c.current_price.toFixed(2) : "—"}</Td>
          <Td className="text-right font-mono text-gray-400">{c.avg_wallet_score}</Td>
        </tr>
      ))}
    </Table>
  );
}

function History() {
  const active = useQuery({ queryKey: ["sig-active"], queryFn: () => api.signalsActive() });
  const history = useQuery({ queryKey: ["sig-history"], queryFn: () => api.signalsHistory() });
  return (
    <div className="space-y-4">
      <div>
        <h2 className="font-medium mb-2 text-emerald-light">Active ({active.data?.length ?? 0})</h2>
        <PersistedTable rows={active.data ?? []} />
      </div>
      <div>
        <h2 className="font-medium mb-2 text-gray-400">History ({history.data?.length ?? 0})</h2>
        <PersistedTable rows={history.data ?? []} history />
      </div>
    </div>
  );
}

function PersistedTable({ rows, history }: { rows: PersistedSignal[]; history?: boolean }) {
  if (rows.length === 0) return <Msg>None. Run /signals/snapshot (or set SNAPSHOT_INTERVAL_SECS) to populate.</Msg>;
  return (
    <Table head={<tr><Th className="text-right">Score</Th><Th>Market</Th><Th>Side</Th><Th className="text-right">Entry</Th>{history && <Th>Reason</Th>}<Th>Last seen</Th></tr>}>
      {rows.map((r) => (
        <tr key={r.id} className="border-t border-glass hover:bg-white/[0.03]">
          <Td className={clsx("text-right font-mono", scoreColor(r.score ?? 0))}>{r.score ?? "—"}</Td>
          <Td className="text-gray-300 max-w-md truncate">{r.market_id}</Td>
          <Td className="uppercase text-xs">{r.side}</Td>
          <Td className="text-right font-mono text-gray-400">{r.entry_price != null ? r.entry_price.toFixed(2) : "—"}</Td>
          {history && <Td className="text-xs text-gray-500 max-w-xs truncate">{r.close_reason}</Td>}
          <Td className="text-xs text-gray-500">{(r.closed_at ?? r.last_seen).replace("T", " ").slice(0, 16)}</Td>
        </tr>
      ))}
    </Table>
  );
}
