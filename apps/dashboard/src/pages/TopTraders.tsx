import { Fragment, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import clsx from "clsx";
import { ChevronDown, ChevronRight } from "lucide-react";
import { api } from "../lib/api";
import { formatPct, formatUsd } from "../lib/odds";
import { MarketCandles } from "../components/MarketCandles";

// Module 5 (partial): Top Traders leaderboard over copy-trading /leaderboard —
// real Polymarket profit/volume per time window, with reconstructed win-rate.
const WINDOWS = ["all", "1d", "7d", "30d"];
const METRICS = [
  { key: "pnl", label: "PnL" },
  { key: "volume", label: "Volume" },
  { key: "win_rate", label: "Win rate" },
];

export function TopTraders() {
  const [window, setWindow] = useState("all");
  const [metric, setMetric] = useState("pnl");
  const [open, setOpen] = useState<string | null>(null);
  const { data, isLoading, error } = useQuery({
    queryKey: ["leaderboard", window, metric],
    queryFn: () => api.leaderboard({ window, metric, limit: 25 }),
    refetchInterval: 30_000,
  });

  return (
    <div className="space-y-4">
      <div className="flex items-center gap-3 flex-wrap">
        <h1 className="text-lg font-semibold">Top Traders</h1>
        <span className="text-sm text-gray-500">Polymarket wallets by real results</span>
        <div className="ml-auto flex items-center gap-2">
          <Pills options={WINDOWS.map((w) => ({ key: w, label: w }))} value={window} onChange={setWindow} />
          <Pills options={METRICS} value={metric} onChange={setMetric} />
        </div>
      </div>

      {isLoading && <div className="text-sm text-gray-400">Loading leaderboard…</div>}
      {error && <div className="text-sm text-crimson-light">Couldn’t load: {(error as Error).message}</div>}
      {data && (
        <div className="overflow-auto rounded-lg border border-glass">
          <table className="w-full text-sm">
            <thead className="bg-surface text-gray-400">
              <tr>
                <th className="w-6 px-2 py-2"></th>
                <th className="text-left px-3 py-2 text-xs uppercase">#</th>
                <th className="text-left px-3 py-2 text-xs uppercase">Trader</th>
                <th className="text-right px-3 py-2 text-xs uppercase">PnL</th>
                <th className="text-right px-3 py-2 text-xs uppercase">Volume</th>
                <th className="text-right px-3 py-2 text-xs uppercase">Win rate</th>
                <th className="text-right px-3 py-2 text-xs uppercase">ROI</th>
                <th className="text-left px-3 py-2 text-xs uppercase">Category</th>
              </tr>
            </thead>
            <tbody>
              {data.map((t, i) => {
                const isOpen = open === t.wallet;
                return (
                  <Fragment key={t.wallet}>
                    <tr
                      onClick={() => setOpen(isOpen ? null : t.wallet)}
                      className="border-t border-glass hover:bg-white/[0.03] cursor-pointer"
                      title="Show top-position price action"
                    >
                      <td className="px-2 py-2 text-gray-500">
                        {isOpen ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
                      </td>
                      <td className="px-3 py-2 text-gray-500">{i + 1}</td>
                      <td className="px-3 py-2">
                        <span className="font-medium">{t.label ?? "—"}</span>{" "}
                        <span className="text-gray-600 font-mono text-xs">
                          {t.wallet.slice(0, 6)}…{t.wallet.slice(-4)}
                        </span>
                      </td>
                      <td className="px-3 py-2 text-right font-mono text-emerald-light">{formatUsd(t.pnl)}</td>
                      <td className="px-3 py-2 text-right font-mono text-gray-300">{formatUsd(t.volume)}</td>
                      <td className="px-3 py-2 text-right font-mono text-gray-300">{formatPct(t.win_rate)}</td>
                      <td className="px-3 py-2 text-right font-mono text-gray-400">{formatPct(t.edge)}</td>
                      <td className="px-3 py-2 text-gray-400">{t.category ?? "—"}</td>
                    </tr>
                    {isOpen && (
                      <tr className="border-t border-glass bg-obsidian/40">
                        <td colSpan={8} className="px-4 py-3">
                          <div className="text-[11px] text-gray-500 mb-1">
                            Most-traded market · daily fills (price at which this wallet's money moved)
                          </div>
                          <MarketCandles wallet={t.wallet} interval={86400} limit={90} />
                        </td>
                      </tr>
                    )}
                  </Fragment>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}

function Pills({
  options,
  value,
  onChange,
}: {
  options: { key: string; label: string }[];
  value: string;
  onChange: (v: string) => void;
}) {
  return (
    <div className="flex rounded-md border border-glass overflow-hidden">
      {options.map((o) => (
        <button
          key={o.key}
          onClick={() => onChange(o.key)}
          className={clsx(
            "px-2.5 py-1 text-xs transition-colors",
            value === o.key ? "bg-violet/20 text-violet-light" : "text-gray-400 hover:bg-white/5",
          )}
        >
          {o.label}
        </button>
      ))}
    </div>
  );
}
