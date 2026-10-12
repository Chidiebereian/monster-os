import { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import clsx from "clsx";
import { api } from "../lib/api";
import { formatUsd } from "../lib/odds";
import { Card, Msg, PageTitle, Table, Td, Th } from "../components/ui";

// Module 4: Whale Watch. Track large position holders / smart money: pick a
// tracked wallet and watch its recent fills (by size) and net market exposure.
export function WhaleWatch() {
  const { data: traders } = useQuery({ queryKey: ["lb-whales"], queryFn: () => api.leaderboard({ metric: "pnl", limit: 20 }) });
  const [wallet, setWallet] = useState<string | null>(null);
  useEffect(() => {
    if (!wallet && traders && traders.length) setWallet(traders[0].wallet);
  }, [traders, wallet]);

  return (
    <div className="space-y-4">
      <PageTitle title="Whale Watch" subtitle="large position holders & smart-money movements" />
      <div className="grid grid-cols-1 lg:grid-cols-[18rem_1fr] gap-4">
        <Card className="p-0 overflow-hidden">
          <div className="px-3 py-2 text-xs uppercase tracking-wide text-gray-500 border-b border-glass">Tracked wallets</div>
          <div className="max-h-[70vh] overflow-auto">
            {traders?.map((t) => (
              <button
                key={t.wallet}
                onClick={() => setWallet(t.wallet)}
                className={clsx(
                  "w-full text-left px-3 py-2 text-sm border-b border-glass/50 hover:bg-white/5",
                  wallet === t.wallet && "bg-violet/10",
                )}
              >
                <div className="flex justify-between">
                  <span className="font-medium">{t.label ?? `${t.wallet.slice(0, 6)}…`}</span>
                  <span className="font-mono text-emerald-light text-xs">{formatUsd(t.pnl)}</span>
                </div>
                <div className="text-[11px] text-gray-600 font-mono">{t.wallet.slice(0, 10)}…{t.wallet.slice(-4)}</div>
              </button>
            ))}
          </div>
        </Card>
        <div className="space-y-4">{wallet && <WalletDetail wallet={wallet} />}</div>
      </div>
    </div>
  );
}

function WalletDetail({ wallet }: { wallet: string }) {
  const trades = useQuery({ queryKey: ["trades", wallet], queryFn: () => api.walletTrades(wallet, 50), refetchInterval: 15_000 });
  const exposure = useQuery({ queryKey: ["exposure", wallet], queryFn: () => api.exposure(wallet, 15) });

  return (
    <>
      <div>
        <h2 className="font-medium mb-2">Net exposure</h2>
        {exposure.isLoading && <Msg>Loading…</Msg>}
        {exposure.error && <Msg tone="error">{(exposure.error as Error).message}</Msg>}
        {exposure.data && exposure.data.length === 0 && <Msg>No open exposure from stored trades.</Msg>}
        {exposure.data && exposure.data.length > 0 && (
          <Table head={<tr><Th>Market</Th><Th>Outcome</Th><Th className="text-right">Net shares</Th><Th className="text-right">Avg entry</Th><Th className="text-right">Cost basis</Th></tr>}>
            {exposure.data.map((e, i) => (
              <tr key={i} className="border-t border-glass hover:bg-white/[0.03]">
                <Td className="text-gray-300 max-w-md truncate">{e.market_id}</Td>
                <Td className="font-medium">{e.outcome || "—"}</Td>
                <Td className="text-right font-mono">{e.net_shares.toLocaleString(undefined, { maximumFractionDigits: 0 })}</Td>
                <Td className="text-right font-mono text-gray-400">{e.avg_entry != null ? e.avg_entry.toFixed(3) : "—"}</Td>
                <Td className="text-right font-mono text-gray-300">{formatUsd(e.cost_basis)}</Td>
              </tr>
            ))}
          </Table>
        )}
      </div>

      <div>
        <h2 className="font-medium mb-2">Recent fills</h2>
        {trades.data && (
          <Table head={<tr><Th>Time</Th><Th>Market</Th><Th>Side</Th><Th className="text-right">Price</Th><Th className="text-right">Size</Th></tr>}>
            {[...(trades.data ?? [])]
              .sort((a, b) => b.size - a.size)
              .slice(0, 25)
              .map((t, i) => (
                <tr key={i} className="border-t border-glass hover:bg-white/[0.03]">
                  <Td className="text-gray-500 text-xs whitespace-nowrap">{t.ts.replace("T", " ").slice(0, 16)}</Td>
                  <Td className="text-gray-300 max-w-sm truncate">{t.market_id}</Td>
                  <Td className={clsx("uppercase text-xs", t.side === "buy" ? "text-emerald-light" : "text-crimson-light")}>{t.side}</Td>
                  <Td className="text-right font-mono">{t.price.toFixed(3)}</Td>
                  <Td className="text-right font-mono">{t.size.toLocaleString(undefined, { maximumFractionDigits: 0 })}</Td>
                </tr>
              ))}
          </Table>
        )}
      </div>
    </>
  );
}
