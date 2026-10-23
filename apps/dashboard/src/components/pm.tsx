// Shared pieces for the Polymarket research pages: wallet avatar, P&L area
// chart and category donut. Plain SVG — the dashboard ships no chart library.
import { useMemo, useState } from "react";
import clsx from "clsx";
import { formatUsd } from "../lib/odds";

/** Avatar image, else a gradient seeded from the wallet so each wallet is recognisable. */
export function Avatar({ wallet, src, size = 32 }: { wallet: string; src?: string | null; size?: number }) {
  const [broken, setBroken] = useState(false);
  if (src && !broken) {
    return (
      <img src={src} onError={() => setBroken(true)} alt="" width={size} height={size}
        className="rounded-full object-cover shrink-0" style={{ width: size, height: size }} />
    );
  }
  const h = parseInt(wallet.slice(2, 8), 16) % 360;
  const h2 = (h + 90) % 360;
  return (
    <div className="rounded-full shrink-0"
      style={{ width: size, height: size, background: `linear-gradient(135deg, hsl(${h} 80% 60%), hsl(${h2} 80% 55%))` }} />
  );
}

export const CAT_COLORS: Record<string, string> = {
  POLITICS: "#7c5cff", SPORTS: "#22c55e", CRYPTO: "#f59e0b", CULTURE: "#ec4899", MENTIONS: "#06b6d4",
  WEATHER: "#60a5fa", ECONOMICS: "#a3e635", TECH: "#f97316", FINANCE: "#14b8a6", ESPORTS: "#e11d48",
};
export const catLabel = (c: string) => c.charAt(0) + c.slice(1).toLowerCase();

/** Donut of where a wallet's profit came from (categories with P&L > 0). */
export function CategoryDonut({ cats }: { cats: { category: string; pnl: number }[] }) {
  const pos = cats.filter((c) => c.pnl > 0);
  const total = pos.reduce((s, c) => s + c.pnl, 0);
  if (total <= 0) return <div className="text-xs text-gray-500">No profitable category yet.</div>;
  const r = 52, c = 2 * Math.PI * r;
  let acc = 0;
  return (
    <div className="flex items-center gap-4">
      <svg width={140} height={140} viewBox="0 0 140 140" className="shrink-0">
        <g transform="rotate(-90 70 70)">
          {pos.map((p) => {
            const len = (p.pnl / total) * c;
            const el = (
              <circle key={p.category} cx={70} cy={70} r={r} fill="none" stroke={CAT_COLORS[p.category] ?? "#888"}
                strokeWidth={22} strokeDasharray={`${len} ${c - len}`} strokeDashoffset={-acc} />
            );
            acc += len;
            return el;
          })}
        </g>
      </svg>
      <div className="space-y-1 text-xs min-w-0">
        {pos.map((p) => (
          <div key={p.category} className="flex items-center gap-2">
            <span className="w-2.5 h-2.5 rounded-sm shrink-0" style={{ background: CAT_COLORS[p.category] ?? "#888" }} />
            <span className="text-gray-300">{catLabel(p.category)}</span>
            <span className="text-gray-500 font-mono">{((p.pnl / total) * 100).toFixed(0)}%</span>
          </div>
        ))}
      </div>
    </div>
  );
}

/** Cumulative realized P&L over time, with a hover readout. */
export function PnlChart({ points, height = 200 }: { points: { t: number; pnl: number }[]; height?: number }) {
  const [hover, setHover] = useState<number | null>(null);
  const W = 640, H = height, pad = 6;
  const geo = useMemo(() => {
    if (points.length < 2) return null;
    const ts = points.map((p) => p.t), ys = points.map((p) => p.pnl);
    const t0 = Math.min(...ts), t1 = Math.max(...ts);
    let y0 = Math.min(...ys, 0), y1 = Math.max(...ys, 0);
    if (y1 === y0) y1 = y0 + 1;
    const x = (t: number) => pad + ((t - t0) / (t1 - t0 || 1)) * (W - 2 * pad);
    const y = (v: number) => pad + (1 - (v - y0) / (y1 - y0)) * (H - 2 * pad);
    const line = points.map((p, i) => `${i ? "L" : "M"}${x(p.t).toFixed(1)},${y(p.pnl).toFixed(1)}`).join("");
    const area = `${line}L${x(t1).toFixed(1)},${y(0).toFixed(1)}L${x(t0).toFixed(1)},${y(0).toFixed(1)}Z`;
    return { x, y, line, area, zero: y(0), t0, t1 };
  }, [points, H]);
  if (!geo) return <div className="text-xs text-gray-500 py-10 text-center">Not enough history for this range.</div>;
  const up = points[points.length - 1].pnl - points[0].pnl >= 0;
  const stroke = up ? "#a78bfa" : "#f87171";
  const hp = hover != null ? points[hover] : null;
  return (
    <div className="relative">
      <svg viewBox={`0 0 ${W} ${H}`} className="w-full" style={{ height }} preserveAspectRatio="none"
        onMouseLeave={() => setHover(null)}
        onMouseMove={(e) => {
          const r = (e.currentTarget as SVGSVGElement).getBoundingClientRect();
          const frac = (e.clientX - r.left) / r.width;
          const t = geo.t0 + frac * (geo.t1 - geo.t0);
          let best = 0;
          points.forEach((p, i) => { if (Math.abs(p.t - t) < Math.abs(points[best].t - t)) best = i; });
          setHover(best);
        }}>
        <defs>
          <linearGradient id="pnlfill" x1="0" x2="0" y1="0" y2="1">
            <stop offset="0%" stopColor={stroke} stopOpacity={0.35} />
            <stop offset="100%" stopColor={stroke} stopOpacity={0} />
          </linearGradient>
        </defs>
        <line x1={0} x2={W} y1={geo.zero} y2={geo.zero} stroke="#ffffff22" strokeDasharray="4 4" />
        <path d={geo.area} fill="url(#pnlfill)" />
        <path d={geo.line} fill="none" stroke={stroke} strokeWidth={2} vectorEffect="non-scaling-stroke" />
        {hp && <circle cx={geo.x(hp.t)} cy={geo.y(hp.pnl)} r={4} fill={stroke} />}
      </svg>
      {hp && (
        <div className="absolute top-1 left-2 text-[11px] bg-obsidian/90 border border-glass rounded px-2 py-1 pointer-events-none">
          <span className="text-gray-400">{new Date(hp.t * 1000).toLocaleString(undefined, { month: "short", day: "numeric", year: "numeric", hour: points.length > 200 ? undefined : "numeric" })}</span>{" "}
          <span className={clsx("font-mono", hp.pnl >= 0 ? "text-emerald-light" : "text-crimson-light")}>{formatUsd(hp.pnl)}</span>
        </div>
      )}
    </div>
  );
}

export function Money({ v, signed }: { v: number | null | undefined; signed?: boolean }) {
  if (v == null) return <span className="text-gray-600">—</span>;
  return (
    <span className={clsx("font-mono", v > 0 ? "text-emerald-light" : v < 0 ? "text-crimson-light" : "text-gray-300")}>
      {signed && v > 0 ? "+" : ""}{formatUsd(v)}
    </span>
  );
}
