import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { api } from "../lib/api";
import { normalPdf, probOver } from "../lib/stats";
import { americanToImpliedProb } from "../lib/odds";
import { formatPct } from "../lib/odds";
import { Card, Input, Msg, PageTitle, Stat } from "../components/ui";

// Module 2 (AI Picks & Projections): a model projection with an interactive
// distribution curve. The simulation-engine confirms the posterior summary
// (mean/median/p10/p90); the Over/Under probability at a draggable line uses
// the same normal approximation the engine does.
export function Projections() {
  const [mean, setMean] = useState("117");
  const [sd, setSd] = useState("10");
  const [line, setLine] = useState(117);
  const [marketOdds, setMarketOdds] = useState("-110");

  const m = Number(mean) || 0;
  const s = Math.max(Number(sd) || 1, 0.1);

  const sim = useQuery({
    queryKey: ["simulate", m, s],
    queryFn: () => api.simulate({ mean: m, sd: s, n: 10000, seed: 42 }),
  });

  const pOver = probOver(line, m, s);
  const pUnder = 1 - pOver;
  const marketImplied = americanToImpliedProb(Number(marketOdds) || 0);
  const edge = pOver - marketImplied;

  return (
    <div className="space-y-5">
      <PageTitle title="Projections" subtitle="model distribution vs the line · NFL · NBA · MLB · NHL" />
      <Card className="space-y-4">
        <div className="grid md:grid-cols-4 gap-4 items-end">
          <label className="text-xs text-gray-500">Model mean<Input type="number" value={mean} onChange={setMean} className="w-full mt-1" /></label>
          <label className="text-xs text-gray-500">Model SD<Input type="number" value={sd} onChange={setSd} className="w-full mt-1" /></label>
          <label className="text-xs text-gray-500">Market odds (Over)<Input type="number" value={marketOdds} onChange={setMarketOdds} className="w-full mt-1" /></label>
          <Stat label="Edge (Over)" value={`${edge >= 0 ? "+" : ""}${formatPct(edge)}`} tone={edge >= 0 ? "pos" : "neg"} />
        </div>

        <Curve mean={m} sd={s} line={line} />

        <div>
          <div className="flex justify-between text-xs text-gray-500 mb-1">
            <span>Line: <span className="text-gray-200 font-mono">{line.toFixed(1)}</span></span>
            <span>drag to recompute</span>
          </div>
          <input
            type="range"
            min={m - 3 * s}
            max={m + 3 * s}
            step={(6 * s) / 200}
            value={line}
            onChange={(e) => setLine(Number(e.target.value))}
            className="w-full accent-violet"
          />
        </div>

        <div className="grid grid-cols-2 md:grid-cols-4 gap-4 pt-3 border-t border-glass">
          <Stat label="P(Over)" value={formatPct(pOver)} tone="pos" />
          <Stat label="P(Under)" value={formatPct(pUnder)} tone="neg" />
          <Stat label="Market implied" value={formatPct(marketImplied)} />
          <Stat label="Model mean" value={m.toFixed(1)} />
        </div>
      </Card>

      <div>
        <h2 className="font-medium mb-2">Simulation (seeded Monte Carlo, n=10k)</h2>
        {sim.isLoading && <Msg>Simulating…</Msg>}
        {sim.error && <Msg tone="error">{(sim.error as Error).message}</Msg>}
        {sim.data && (
          <div className="grid grid-cols-2 md:grid-cols-5 gap-4">
            <Stat label="Mean" value={sim.data.mean.toFixed(2)} />
            <Stat label="Median" value={sim.data.median.toFixed(2)} />
            <Stat label="p10" value={sim.data.p10.toFixed(2)} />
            <Stat label="p90" value={sim.data.p90.toFixed(2)} />
            <Stat label="Draws" value={sim.data.n.toLocaleString()} />
          </div>
        )}
      </div>
    </div>
  );
}

function Curve({ mean, sd, line }: { mean: number; sd: number; line: number }) {
  const W = 640;
  const H = 180;
  const lo = mean - 3.5 * sd;
  const hi = mean + 3.5 * sd;
  const peak = normalPdf(mean, mean, sd);
  const x = (v: number) => ((v - lo) / (hi - lo)) * W;
  const y = (p: number) => H - (p / peak) * (H - 16) - 4;

  const pts: string[] = [];
  const over: string[] = [];
  for (let i = 0; i <= 200; i++) {
    const v = lo + ((hi - lo) * i) / 200;
    const px = x(v);
    const py = y(normalPdf(v, mean, sd));
    pts.push(`${px},${py}`);
    if (v >= line) over.push(`${px},${py}`);
  }
  const lineX = x(line);

  return (
    <svg viewBox={`0 0 ${W} ${H}`} className="w-full h-44">
      {/* shaded Over region */}
      {over.length > 1 && (
        <polygon
          points={`${lineX},${H} ${over.join(" ")} ${x(hi)},${H}`}
          fill="rgba(16,185,129,0.18)"
        />
      )}
      {/* curve */}
      <polyline points={pts.join(" ")} fill="none" stroke="#8B5CF6" strokeWidth={2} />
      {/* line marker */}
      <line x1={lineX} y1={6} x2={lineX} y2={H} stroke="#F59E0B" strokeWidth={1.5} strokeDasharray="4 3" />
      {/* mean marker */}
      <line x1={x(mean)} y1={14} x2={x(mean)} y2={H} stroke="rgba(255,255,255,0.25)" strokeWidth={1} />
      <text x={x(mean)} y={11} fill="rgba(255,255,255,0.4)" fontSize="9" textAnchor="middle">median</text>
      <text x={lineX} y={11} fill="#F59E0B" fontSize="9" textAnchor="middle">line</text>
    </svg>
  );
}
