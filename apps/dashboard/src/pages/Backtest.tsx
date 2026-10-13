import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { api, type Backtest as BacktestResult } from "../lib/api";
import { formatPct } from "../lib/odds";
import { Card, Input, Msg, PageTitle, Stat, Table, Td, Th } from "../components/ui";

// Model Performance: leakage-free walk-forward backtest of the team model. For
// each test season it trains on PRIOR seasons only, then scores predictions
// against actual results and the stored closing lines. Honest out-of-sample
// numbers — including where the model does NOT beat the market.
export function Backtest() {
  const [from, setFrom] = useState("2019");
  const [to, setTo] = useState("2024");
  const { data, isLoading, error, isFetching } = useQuery({
    queryKey: ["backtest", from, to],
    queryFn: () => api.backtest("nfl", { from: Number(from) || undefined, to: Number(to) || undefined }),
  });

  return (
    <div className="space-y-5">
      <PageTitle
        title="Model Performance"
        subtitle="walk-forward backtest · trained on prior seasons only (no leakage)"
        right={
          <div className="flex items-center gap-2 text-xs text-gray-400">
            NFL team model · seasons
            <Input type="number" value={from} onChange={setFrom} className="w-20" />
            <span>–</span>
            <Input type="number" value={to} onChange={setTo} className="w-20" />
            {isFetching && <span className="text-violet-light">running…</span>}
          </div>
        }
      />

      {isLoading && <Msg>Running walk-forward backtest…</Msg>}
      {error && <Msg tone="error">{(error as Error).message}</Msg>}
      {data && <Results d={data} />}
    </div>
  );
}

function Results({ d }: { d: BacktestResult }) {
  const beats = (pct: number) => (pct > 0.524 ? "pos" : pct < 0.476 ? "neg" : undefined) as "pos" | "neg" | undefined;
  return (
    <>
      <Card>
        <div className="text-xs text-gray-500 mb-3">
          {d.method} · test {d.test_seasons} · {d.games.toLocaleString()} games
        </div>
        <div className="grid grid-cols-2 md:grid-cols-5 gap-4">
          <Stat label="Straight-up acc" value={formatPct(d.straight_up_accuracy)} tone="pos" />
          <Stat label="Brier score" value={d.brier_score.toFixed(3)} />
          <Stat label="ATS vs line" value={`${d.ats_vs_closing_line.record} · ${formatPct(d.ats_vs_closing_line.pct)}`} tone={beats(d.ats_vs_closing_line.pct)} />
          <Stat label="+EV subset" value={`${d.plus_ev_subset.record} · ${formatPct(d.plus_ev_subset.pct)}`} tone={beats(d.plus_ev_subset.pct)} />
          <Stat label="Totals vs line" value={`${d.totals_vs_closing_line.record} · ${formatPct(d.totals_vs_closing_line.pct)}`} tone={beats(d.totals_vs_closing_line.pct)} />
        </div>
        <div className="text-[11px] text-gray-500 mt-3">
          Beating the closing line needs &gt; 52.4% to clear the −110 vig (green). {d.plus_ev_subset.note}.
          A good straight-up accuracy with ~50% ATS is the honest signature of a sound model that the market has already priced in.
        </div>
      </Card>

      <div className="grid lg:grid-cols-2 gap-5 items-start">
        <Card>
          <h2 className="font-medium text-sm mb-1">Calibration</h2>
          <div className="text-[11px] text-gray-500 mb-2">predicted home-win probability vs actual frequency — on the diagonal = well-calibrated</div>
          <Calibration points={d.calibration} />
        </Card>

        <Card>
          <h2 className="font-medium text-sm mb-2">By season</h2>
          <Table head={<tr><Th>Season</Th><Th className="text-right">Games</Th><Th className="text-right">Train</Th><Th className="text-right">SU acc</Th><Th className="text-right">ATS</Th><Th className="text-right">ATS %</Th></tr>}>
            {d.by_season.map((s) => (
              <tr key={s.season} className="border-t border-glass">
                <Td>{s.season}</Td>
                <Td className="text-right font-mono text-gray-400">{s.games}</Td>
                <Td className="text-right font-mono text-gray-500">{s.train_games}</Td>
                <Td className="text-right font-mono">{formatPct(s.su_acc)}</Td>
                <Td className="text-right font-mono text-gray-400">{s.ats_record}</Td>
                <Td className="text-right font-mono">{formatPct(s.ats_pct)}</Td>
              </tr>
            ))}
          </Table>
        </Card>
      </div>
    </>
  );
}

function Calibration({ points }: { points: BacktestResult["calibration"] }) {
  const W = 320;
  const H = 320;
  const pad = 36;
  const x = (p: number) => pad + p * (W - 2 * pad);
  const y = (p: number) => H - pad - p * (H - 2 * pad);
  const maxN = Math.max(...points.map((p) => p.n), 1);
  return (
    <svg viewBox={`0 0 ${W} ${H}`} width="100%" className="max-w-sm">
      {/* grid */}
      {[0, 0.25, 0.5, 0.75, 1].map((t) => (
        <g key={t}>
          <line x1={x(t)} y1={y(0)} x2={x(t)} y2={y(1)} className="stroke-white/5" />
          <line x1={x(0)} y1={y(t)} x2={x(1)} y2={y(t)} className="stroke-white/5" />
          <text x={x(t)} y={H - pad + 14} textAnchor="middle" className="fill-gray-600 text-[8px]">{Math.round(t * 100)}</text>
          <text x={pad - 6} y={y(t) + 3} textAnchor="end" className="fill-gray-600 text-[8px]">{Math.round(t * 100)}</text>
        </g>
      ))}
      {/* perfect-calibration diagonal */}
      <line x1={x(0)} y1={y(0)} x2={x(1)} y2={y(1)} className="stroke-violet/50" strokeDasharray="4 3" />
      {/* points */}
      {points.map((p, i) => (
        <circle key={i} cx={x(p.predicted)} cy={y(p.actual)} r={3 + 6 * (p.n / maxN)} className="fill-emerald/60 stroke-emerald-light" strokeWidth={0.5}>
          <title>{p.bucket}: predicted {formatPct(p.predicted)}, actual {formatPct(p.actual)} (n={p.n})</title>
        </circle>
      ))}
      <text x={W / 2} y={H - 6} textAnchor="middle" className="fill-gray-500 text-[9px]">predicted win %</text>
      <text x={10} y={H / 2} textAnchor="middle" transform={`rotate(-90 10 ${H / 2})`} className="fill-gray-500 text-[9px]">actual win %</text>
    </svg>
  );
}
