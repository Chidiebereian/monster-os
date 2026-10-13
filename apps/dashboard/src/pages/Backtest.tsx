import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import clsx from "clsx";
import { api, type Backtest as BacktestResult, type PlayerBacktest } from "../lib/api";
import { formatPct } from "../lib/odds";
import { Card, Input, Msg, PageTitle, Pills, Stat, Table, Td, Th } from "../components/ui";

const SPORTS = [
  { key: "nfl", label: "NFL" },
  { key: "nhl", label: "NHL" },
  { key: "nba", label: "NBA" },
];
const PLAYER_STATS: Record<string, { key: string; label: string }[]> = {
  nfl: [
    { key: "pass_yds", label: "Passing Yds" }, { key: "rush_yds", label: "Rushing Yds" },
    { key: "rec_yds", label: "Receiving Yds" }, { key: "receptions", label: "Receptions" }, { key: "pass_td", label: "Passing TDs" },
  ],
  nhl: [{ key: "shots", label: "Shots" }, { key: "points", label: "Points" }, { key: "assists", label: "Assists" }, { key: "saves", label: "Saves (G)" }],
  nba: [{ key: "points", label: "Points" }, { key: "rebounds", label: "Rebounds" }, { key: "assists", label: "Assists" }, { key: "threes", label: "3PM" }],
};

// Model Performance: leakage-free walk-forward backtests — team models (vs
// results + closing lines) and player models (vs actual outcomes, with interval
// calibration). Honest out-of-sample numbers, including where models fall short.
export function Backtest() {
  return (
    <div className="space-y-6">
      <PageTitle title="Model Performance" subtitle="walk-forward backtests · trained on prior data only (no leakage)" />
      <TeamBacktest />
      <PlayerModelBacktest />
    </div>
  );
}

function TeamBacktest() {
  const [sport, setSport] = useState("nfl");
  const [from, setFrom] = useState("2019");
  const [to, setTo] = useState("2024");
  const { data, isLoading, error, isFetching } = useQuery({
    queryKey: ["backtest", sport, from, to],
    queryFn: () => api.backtest(sport, { from: Number(from) || undefined, to: Number(to) || undefined }),
  });
  return (
    <div className="space-y-3">
      <div className="flex items-center gap-3 flex-wrap">
        <h2 className="font-medium">Team model</h2>
        <Pills options={SPORTS} value={sport} onChange={setSport} />
        <div className="ml-auto flex items-center gap-2 text-xs text-gray-400">
          seasons <Input type="number" value={from} onChange={setFrom} className="w-20" /> – <Input type="number" value={to} onChange={setTo} className="w-20" />
          {isFetching && <span className="text-violet-light">running…</span>}
        </div>
      </div>
      {isLoading && <Msg>Running walk-forward backtest…</Msg>}
      {error && <Msg tone="error">{(error as Error).message}</Msg>}
      {data && <TeamResults d={data} />}
    </div>
  );
}

function TeamResults({ d }: { d: BacktestResult }) {
  const beats = (pct: number) => (pct > 0.524 ? "pos" : pct < 0.476 ? "neg" : undefined) as "pos" | "neg" | undefined;
  const lines = d.has_market_lines !== false;
  return (
    <>
      <Card>
        <div className="text-xs text-gray-500 mb-3">{d.method} · test {d.test_seasons} · {d.games.toLocaleString()} games</div>
        <div className="grid grid-cols-2 md:grid-cols-5 gap-4">
          <Stat label="Straight-up acc" value={formatPct(d.straight_up_accuracy)} tone="pos" />
          <Stat label="Brier score" value={d.brier_score.toFixed(3)} />
          <Stat label="ATS vs line" value={lines ? `${d.ats_vs_closing_line.record} · ${formatPct(d.ats_vs_closing_line.pct)}` : "n/a"} tone={lines ? beats(d.ats_vs_closing_line.pct) : undefined} />
          <Stat label="+EV subset" value={lines ? `${d.plus_ev_subset.record} · ${formatPct(d.plus_ev_subset.pct)}` : "n/a"} tone={lines ? beats(d.plus_ev_subset.pct) : undefined} />
          <Stat label="Totals vs line" value={lines ? `${d.totals_vs_closing_line.record} · ${formatPct(d.totals_vs_closing_line.pct)}` : "n/a"} tone={lines ? beats(d.totals_vs_closing_line.pct) : undefined} />
        </div>
        <div className="text-[11px] text-gray-500 mt-3">
          {lines
            ? `Beating the closing line needs > 52.4% to clear the −110 vig (green). ${d.plus_ev_subset.note}. Good straight-up accuracy with ~50% ATS is the signature of a sound model the market has already priced in.`
            : "No stored closing lines for this sport, so only straight-up accuracy, Brier and calibration are shown."}
        </div>
      </Card>
      <div className="grid lg:grid-cols-2 gap-5 items-start">
        <Card>
          <h3 className="font-medium text-sm mb-1">Calibration</h3>
          <div className="text-[11px] text-gray-500 mb-2">predicted home-win probability vs actual frequency — on the diagonal = well-calibrated</div>
          <Calibration points={d.calibration} />
        </Card>
        <Card>
          <h3 className="font-medium text-sm mb-2">By season</h3>
          <Table head={<tr><Th>Season</Th><Th className="text-right">Games</Th><Th className="text-right">SU acc</Th><Th className="text-right">ATS</Th><Th className="text-right">ATS %</Th></tr>}>
            {d.by_season.map((s) => (
              <tr key={s.season} className="border-t border-glass">
                <Td>{s.season}</Td>
                <Td className="text-right font-mono text-gray-400">{s.games}</Td>
                <Td className="text-right font-mono">{formatPct(s.su_acc)}</Td>
                <Td className="text-right font-mono text-gray-400">{s.ats_record}</Td>
                <Td className="text-right font-mono">{s.ats_pct > 0 ? formatPct(s.ats_pct) : "—"}</Td>
              </tr>
            ))}
          </Table>
        </Card>
      </div>
    </>
  );
}

function PlayerModelBacktest() {
  const [sport, setSport] = useState("nba");
  const [stat, setStat] = useState("points");
  const stats = PLAYER_STATS[sport] ?? [];
  function pickSport(s: string) {
    setSport(s);
    setStat(PLAYER_STATS[s]?.[0]?.key ?? "");
  }
  const { data, isLoading, error, isFetching } = useQuery({
    queryKey: ["player-backtest", sport, stat],
    queryFn: () => api.playerBacktest(sport, stat, { test_games: 12, min_games: 40 }),
    enabled: Boolean(stat),
  });
  return (
    <div className="space-y-3">
      <div className="flex items-center gap-3 flex-wrap">
        <h2 className="font-medium">Player model</h2>
        <Pills options={SPORTS} value={sport} onChange={pickSport} />
        <Pills options={stats.map((s) => ({ key: s.key, label: s.label }))} value={stat} onChange={setStat} />
        {isFetching && <span className="text-xs text-violet-light">running…</span>}
      </div>
      {isLoading && <Msg>Backtesting the player model (walk-forward)…</Msg>}
      {error && <Msg tone="error">{(error as Error).message}</Msg>}
      {data && <PlayerResults d={data} />}
    </div>
  );
}

function PlayerResults({ d }: { d: PlayerBacktest }) {
  const beatsBaseline = d.model.mae < d.baseline_career_avg.mae;
  return (
    <>
      <Card>
        <div className="text-xs text-gray-500 mb-3">{d.method} · {d.players.toLocaleString()} players · {d.samples.toLocaleString()} player-games · {d.label} ({d.unit})</div>
        <div className="grid grid-cols-2 md:grid-cols-5 gap-4">
          <Stat label="Model MAE" value={d.model.mae.toFixed(2)} tone={beatsBaseline ? "pos" : "neg"} />
          <Stat label="Baseline MAE" value={d.baseline_career_avg.mae.toFixed(2)} />
          <Stat label="Model RMSE" value={d.model.rmse.toFixed(2)} />
          <Stat label="Bias" value={`${d.model.bias >= 0 ? "+" : ""}${d.model.bias.toFixed(2)}`} tone={Math.abs(d.model.bias) < 0.3 ? "pos" : "neg"} />
          <Stat label="80 / 90% coverage" value={`${formatPct(d.model.coverage_80)} / ${formatPct(d.model.coverage_90)}`} />
        </div>
        <div className="text-[11px] text-gray-500 mt-3">
          <span className={clsx(beatsBaseline ? "text-emerald-light" : "text-crimson-light")}>
            {beatsBaseline ? "Model MAE beats the career-average baseline" : "Model does not beat the baseline"}
          </span>{" "}
          — lower is better. Coverage should sit near 80% / 90%: well below means the predictive intervals are too narrow
          (overconfident); bias far from 0 means the projection runs high/low. {d.note}
        </div>
      </Card>
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
      {[0, 0.25, 0.5, 0.75, 1].map((t) => (
        <g key={t}>
          <line x1={x(t)} y1={y(0)} x2={x(t)} y2={y(1)} className="stroke-white/5" />
          <line x1={x(0)} y1={y(t)} x2={x(1)} y2={y(t)} className="stroke-white/5" />
          <text x={x(t)} y={H - pad + 14} textAnchor="middle" className="fill-gray-600 text-[8px]">{Math.round(t * 100)}</text>
          <text x={pad - 6} y={y(t) + 3} textAnchor="end" className="fill-gray-600 text-[8px]">{Math.round(t * 100)}</text>
        </g>
      ))}
      <line x1={x(0)} y1={y(0)} x2={x(1)} y2={y(1)} className="stroke-violet/50" strokeDasharray="4 3" />
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
