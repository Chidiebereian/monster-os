import { useState } from "react";
import type { Candle } from "../lib/api";

// Pure-SVG OHLC candlestick chart (no chart library — keeps the bundle lean and
// matches the Projections SVG approach). Prices are Polymarket probabilities in
// [0,1]. Green body = close >= open, red = close < open; the thin wick spans the
// bucket's high/low. Candles are rendered exactly as the backend aggregated the
// stored fills — nothing is interpolated or invented here.
export function CandlestickChart({
  candles,
  refPrice,
  refLabel,
  height = 180,
  intervalSecs,
}: {
  candles: Candle[];
  refPrice?: number | null;
  refLabel?: string;
  height?: number;
  intervalSecs?: number;
}) {
  const [hover, setHover] = useState<number | null>(null);
  if (candles.length === 0) return null;

  const W = 640;
  const H = height;
  const padL = 44;
  const padR = 10;
  const padT = 10;
  const padB = 22;
  const plotW = W - padL - padR;
  const plotH = H - padT - padB;

  const lows = candles.map((c) => c.l);
  const highs = candles.map((c) => c.h);
  let lo = Math.min(...lows, refPrice ?? Infinity);
  let hi = Math.max(...highs, refPrice ?? -Infinity);
  if (!isFinite(lo) || !isFinite(hi)) return null;
  const span = hi - lo || 0.02;
  lo = Math.max(0, lo - span * 0.08);
  hi = Math.min(1, hi + span * 0.08);

  const y = (p: number) => padT + plotH - ((p - lo) / (hi - lo)) * plotH;
  const n = candles.length;
  const slot = plotW / n;
  const bodyW = Math.max(1.5, Math.min(slot * 0.62, 12));
  const x = (i: number) => padL + slot * (i + 0.5);

  const fmtP = (p: number) => p.toFixed(3);
  const ticks = [lo, lo + (hi - lo) / 2, hi];
  const fmtTime = (t: number) => {
    const d = new Date(t * 1000);
    const day = intervalSecs && intervalSecs >= 86400;
    return day
      ? d.toLocaleDateString(undefined, { month: "short", day: "numeric" })
      : d.toLocaleString(undefined, { month: "short", day: "numeric", hour: "2-digit" });
  };

  const hc = hover != null ? candles[hover] : null;

  return (
    <div className="relative">
      <svg viewBox={`0 0 ${W} ${H}`} width="100%" className="select-none" role="img">
        {/* price gridlines + labels */}
        {ticks.map((p, i) => (
          <g key={i}>
            <line x1={padL} x2={W - padR} y1={y(p)} y2={y(p)} className="stroke-white/5" strokeWidth={1} />
            <text x={padL - 6} y={y(p) + 3} textAnchor="end" className="fill-gray-500 text-[9px] font-mono">
              {fmtP(p)}
            </text>
          </g>
        ))}

        {/* reference price (e.g. the signal's entry) */}
        {refPrice != null && refPrice >= lo && refPrice <= hi && (
          <g>
            <line
              x1={padL}
              x2={W - padR}
              y1={y(refPrice)}
              y2={y(refPrice)}
              className="stroke-violet"
              strokeWidth={1}
              strokeDasharray="4 3"
            />
            <text x={W - padR} y={y(refPrice) - 3} textAnchor="end" className="fill-violet-light text-[9px] font-mono">
              {refLabel ?? "ref"} {fmtP(refPrice)}
            </text>
          </g>
        )}

        {/* candles */}
        {candles.map((c, i) => {
          const up = c.c >= c.o;
          const cls = up ? "text-emerald-light" : "text-crimson-light";
          const cx = x(i);
          const yO = y(c.o);
          const yC = y(c.c);
          const top = Math.min(yO, yC);
          const bh = Math.max(1, Math.abs(yC - yO));
          return (
            <g
              key={i}
              className={cls}
              onMouseEnter={() => setHover(i)}
              onMouseLeave={() => setHover((h) => (h === i ? null : h))}
            >
              {/* invisible hit area */}
              <rect x={cx - slot / 2} y={padT} width={slot} height={plotH} fill="transparent" />
              <line x1={cx} x2={cx} y1={y(c.h)} y2={y(c.l)} stroke="currentColor" strokeWidth={1} />
              <rect
                x={cx - bodyW / 2}
                y={top}
                width={bodyW}
                height={bh}
                fill="currentColor"
                opacity={hover == null || hover === i ? 0.9 : 0.5}
              />
            </g>
          );
        })}

        {/* time axis ends */}
        <text x={padL} y={H - 7} className="fill-gray-500 text-[9px] font-mono">
          {fmtTime(candles[0].t)}
        </text>
        <text x={W - padR} y={H - 7} textAnchor="end" className="fill-gray-500 text-[9px] font-mono">
          {fmtTime(candles[n - 1].t)}
        </text>
      </svg>

      {hc && (
        <div className="absolute top-1 right-2 text-[10px] font-mono bg-surface/90 border border-glass rounded px-2 py-1 pointer-events-none">
          <span className="text-gray-500">{fmtTime(hc.t)}</span>{" "}
          <span className="text-gray-300">O {fmtP(hc.o)}</span>{" "}
          <span className="text-emerald-light">H {fmtP(hc.h)}</span>{" "}
          <span className="text-crimson-light">L {fmtP(hc.l)}</span>{" "}
          <span className={hc.c >= hc.o ? "text-emerald-light" : "text-crimson-light"}>C {fmtP(hc.c)}</span>{" "}
          <span className="text-gray-500">· {hc.n} fills</span>
        </div>
      )}
    </div>
  );
}
