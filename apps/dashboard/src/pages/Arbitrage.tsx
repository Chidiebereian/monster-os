import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import clsx from "clsx";
import { api } from "../lib/api";
import { calculateArbitrage } from "../lib/arb";
import { useSettings, SPORTS } from "../store/settings";
import { formatPct, formatUsd } from "../lib/odds";
import { Card, Input, Msg, PageTitle, Stat, Table, Td, Th } from "../components/ui";

// Module 3: Arbitrage & Dutching. Top = client-side Dutching calculator; bottom
// = cross-venue scanner (arbitrage-engine /scan: Kalshi vs sportsbook no-vig
// consensus, with liquidity gates).
export function Arbitrage() {
  return (
    <div className="space-y-6">
      <PageTitle title="Arbitrage & Dutching" subtitle="guaranteed-profit staking + cross-venue scanner" />
      <DutchingCalculator />
      <Scanner />
    </div>
  );
}

const PRESETS = [50, 100, 500, 1000];

function DutchingCalculator() {
  const bankroll = useSettings((s) => s.bankroll);
  const setBankroll = useSettings((s) => s.setBankroll);
  const [oddsA, setOddsA] = useState("120");
  const [oddsB, setOddsB] = useState("-105");
  const a = Number(oddsA) || 0;
  const b = Number(oddsB) || 0;
  const r = calculateArbitrage(a, b, bankroll);
  const isArb = r.impliedSum < 1;

  return (
    <Card>
      <div className="flex items-center gap-3 mb-4">
        <h2 className="font-medium">Dutching calculator</h2>
        <span className={clsx("text-xs px-2 py-0.5 rounded", isArb ? "bg-emerald/15 text-emerald-light" : "bg-white/5 text-gray-400")}>
          implied sum {formatPct(r.impliedSum, 2)} {isArb ? "· arb!" : "· no arb"}
        </span>
        <div className="ml-auto flex items-center gap-2">
          {PRESETS.map((p) => (
            <button
              key={p}
              onClick={() => setBankroll(p)}
              className={clsx("px-2 py-1 text-xs rounded border border-glass", bankroll === p ? "bg-violet/20 text-violet-light" : "text-gray-400 hover:bg-white/5")}
            >
              ${p}
            </button>
          ))}
        </div>
      </div>
      <div className="grid grid-cols-2 md:grid-cols-4 gap-4 items-end">
        <label className="text-xs text-gray-500">Leg A (American)<Input type="number" value={oddsA} onChange={setOddsA} className="w-full mt-1" /></label>
        <label className="text-xs text-gray-500">Leg B (American)<Input type="number" value={oddsB} onChange={setOddsB} className="w-full mt-1" /></label>
        <label className="text-xs text-gray-500">Bankroll<Input type="number" value={bankroll} onChange={(v) => setBankroll(Number(v) || 0)} className="w-full mt-1" /></label>
        <Stat label="ROI" value={`${r.roiPct >= 0 ? "+" : ""}${r.roiPct.toFixed(2)}%`} tone={isArb ? "pos" : "neg"} />
      </div>
      <div className="grid grid-cols-2 md:grid-cols-4 gap-4 mt-4 pt-4 border-t border-glass">
        <Stat label="Stake A" value={formatUsd(r.stakeA)} />
        <Stat label="Stake B" value={formatUsd(r.stakeB)} />
        <Stat label="Guaranteed profit" value={formatUsd(r.profitMin)} tone={r.profitMin >= 0 ? "pos" : "neg"} />
        <Stat label="Max profit" value={formatUsd(r.profitMax)} />
      </div>
    </Card>
  );
}

function Scanner() {
  const sportKey = useSettings((s) => s.sportKey);
  const sport = SPORTS.find((s) => s.key === sportKey) ?? SPORTS[0];
  const [minTop, setMinTop] = useState("0");
  const { data, isLoading, error, isFetching } = useQuery({
    queryKey: ["scan", sport.key, minTop],
    queryFn: () => api.scan(sport.key, sport.series, { min_edge: -1, min_top: Number(minTop) || undefined }),
    refetchInterval: 20_000,
  });

  return (
    <div className="space-y-3">
      <div className="flex items-center gap-3">
        <h2 className="font-medium">Cross-venue scanner · {sport.label}</h2>
        <span className="text-xs text-gray-500">Kalshi vs sportsbook no-vig consensus (net of est. fee)</span>
        <div className="ml-auto flex items-center gap-2 text-xs text-gray-400">
          min top-size
          <Input type="number" value={minTop} onChange={setMinTop} className="w-20" />
          {isFetching && <span className="text-violet-light">updating…</span>}
        </div>
      </div>
      {isLoading && <Msg>Scanning…</Msg>}
      {error && <Msg tone="error">Scan failed: {(error as Error).message}</Msg>}
      {data && (
        <>
          <div className="text-xs text-gray-500">
            {data.kalshi_markets} Kalshi markets · {data.sportsbook_games} sportsbook games · {data.opportunities.length} opportunities
          </div>
          <Table
            head={
              <tr>
                <Th>Market</Th><Th>Side</Th><Th className="text-right">Price</Th>
                <Th className="text-right">Edge</Th><Th className="text-right">Net edge</Th><Th className="text-right">Net ROI</Th>
                <Th className="text-right">Top size</Th><Th>Status</Th>
              </tr>
            }
          >
            {data.opportunities.map((o, i) => (
              <tr key={i} className="border-t border-glass hover:bg-white/[0.03]">
                <Td className="text-gray-300">{o.game || o.market_id}</Td>
                <Td className="uppercase text-xs">{o.side}</Td>
                <Td className="text-right font-mono">{o.price.toFixed(2)}</Td>
                <Td className="text-right font-mono text-gray-400">{formatPct(o.edge)}</Td>
                <Td className={clsx("text-right font-mono", o.net_edge > 0 ? "text-emerald-light" : "text-crimson-light")}>{formatPct(o.net_edge)}</Td>
                <Td className="text-right font-mono text-gray-300">{formatPct(o.net_roi)}</Td>
                <Td className="text-right font-mono text-gray-400">{o.top_size != null ? formatUsd(o.top_size) : "—"}</Td>
                <Td className="text-xs">{o.in_play ? <span className="text-amber">in-play</span> : <span className="text-gray-500">pregame</span>}</Td>
              </tr>
            ))}
          </Table>
        </>
      )}
    </div>
  );
}
