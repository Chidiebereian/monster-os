import { useQuery } from "@tanstack/react-query";
import { api } from "../lib/api";
import { CandlestickChart } from "./CandlestickChart";

const INTERVAL_LABEL: Record<number, string> = { 3600: "1h", 14400: "4h", 86400: "1d" };

// Fetches OHLC candles for a market (by slug) or a wallet's most-traded market,
// then renders the SVG candlestick. Every number comes from whale-tracker
// /candles — the UI never synthesizes price history.
export function MarketCandles({
  marketId,
  wallet,
  refPrice,
  refLabel,
  interval = 3600,
  limit = 120,
  height,
}: {
  marketId?: string;
  wallet?: string;
  refPrice?: number | null;
  refLabel?: string;
  interval?: number;
  limit?: number;
  height?: number;
}) {
  const enabled = Boolean(marketId || wallet);
  const { data, isLoading, error } = useQuery({
    queryKey: ["candles", marketId ?? "", wallet ?? "", interval, limit],
    queryFn: () => api.candles({ market_id: marketId, wallet, interval, limit }),
    enabled,
    refetchInterval: 30_000,
  });

  if (!enabled) return null;
  if (isLoading) return <div className="text-xs text-gray-500 py-6 text-center">Loading price action…</div>;
  if (error) return <div className="text-xs text-crimson-light py-6 text-center">Chart failed: {(error as Error).message}</div>;
  if (!data || data.candles.length === 0)
    return <div className="text-xs text-gray-600 py-6 text-center">No stored fills to chart for this market yet.</div>;

  return (
    <div>
      <div className="flex items-center justify-between text-[10px] text-gray-500 mb-1">
        <span className="font-mono truncate max-w-[70%]">{data.market_id}</span>
        <span>{INTERVAL_LABEL[data.interval_secs] ?? `${data.interval_secs}s`} · {data.candles.length} candles · fills</span>
      </div>
      <CandlestickChart candles={data.candles} refPrice={refPrice} refLabel={refLabel} intervalSecs={data.interval_secs} height={height} />
    </div>
  );
}
