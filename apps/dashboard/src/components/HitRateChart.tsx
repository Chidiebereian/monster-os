// Pure-SVG hit-rate bar chart: one bar per past game (oldest left, newest
// right), green when it cleared the line, red when it fell short, grey on a
// push, with the line drawn dashed across. Values come straight from the
// backend game log — nothing is computed here but layout.

export interface HitBar {
  key: string;
  value: number;
  result: "over" | "under" | "push";
  label: string; // short x-axis tag, e.g. opponent
  title: string; // hover text
}

const H = 170;
const PAD_T = 14;
const PAD_B = 34;
const BAR_W = 26;
const GAP = 6;
const PAD_L = 34;

export function HitRateChart({ bars, line, unit, signed = false }: { bars: HitBar[]; line: number; unit?: string; signed?: boolean }) {
  if (bars.length === 0) return <div className="text-sm text-gray-500 py-6">No games.</div>;
  const values = bars.map((b) => b.value);
  const hi = Math.max(line, ...values, 0);
  const lo = signed ? Math.min(line, ...values, 0) : 0;
  const span = hi - lo || 1;
  const plotH = H - PAD_T - PAD_B;
  const y = (v: number) => PAD_T + ((hi - v) / span) * plotH;
  const W = PAD_L + bars.length * (BAR_W + GAP) + GAP;
  const zero = y(0);
  const fill = { over: "#10B981", under: "#EF4444", push: "#6B7280" } as const;

  return (
    <svg viewBox={`0 0 ${W} ${H}`} className="w-full" style={{ maxHeight: 220 }} role="img" aria-label="hit rate chart">
      {/* axis labels */}
      <text x={PAD_L - 6} y={y(hi) + 4} textAnchor="end" className="fill-gray-500" fontSize={10}>{fmt(hi)}</text>
      {lo < 0 && <text x={PAD_L - 6} y={y(lo) + 4} textAnchor="end" className="fill-gray-500" fontSize={10}>{fmt(lo)}</text>}
      <line x1={PAD_L} x2={W} y1={zero} y2={zero} stroke="rgba(255,255,255,0.12)" />
      {bars.map((b, i) => {
        const x = PAD_L + GAP + i * (BAR_W + GAP);
        const top = Math.min(y(b.value), zero);
        const h = Math.max(Math.abs(zero - y(b.value)), 1.5);
        return (
          <g key={b.key}>
            <title>{b.title}</title>
            <rect x={x} y={top} width={BAR_W} height={h} rx={2} fill={fill[b.result]} opacity={0.85} />
            <text x={x + BAR_W / 2} y={b.value >= 0 ? top - 3 : top + h + 10} textAnchor="middle" className="fill-gray-300" fontSize={9}>
              {fmt(b.value)}
            </text>
            <text x={x + BAR_W / 2} y={H - PAD_B + 14} textAnchor="middle" className="fill-gray-500" fontSize={9}>{b.label}</text>
          </g>
        );
      })}
      {/* the line */}
      <line x1={PAD_L} x2={W} y1={y(line)} y2={y(line)} stroke="#F59E0B" strokeDasharray="4 3" strokeWidth={1.5} />
      <text x={W - 4} y={y(line) - 4} textAnchor="end" fill="#F59E0B" fontSize={10}>
        {fmt(line)}{unit ? ` ${unit}` : ""}
      </text>
    </svg>
  );
}

function fmt(v: number) {
  return Number.isInteger(v) ? String(v) : v.toFixed(1);
}
