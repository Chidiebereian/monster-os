import { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import clsx from "clsx";
import { api, type MlbBacktest, type MlbBinaryReport, type MlbDist, type MlbSide, type MlbSim, type MlbSlateGame } from "../lib/api";
import { formatPct } from "../lib/odds";
import { Card, Input, Msg, PageTitle, Pills, Stat, Table, Td, Th } from "../components/ui";

// MLB Game Sim: pick a game, run the plate-appearance simulator (100k games by
// default) and read every player's prop distribution off the SAME simulated
// games. All probabilities come from projection-service; the only arithmetic
// here is summing the returned pmf for a custom line.

const BAT_PROPS: { key: string; label: string; line: number }[] = [
  { key: "h", label: "1+ H", line: 0.5 },
  { key: "tb", label: "2+ TB", line: 1.5 },
  { key: "hr", label: "HR", line: 0.5 },
  { key: "hrr", label: "2+ H+R+RBI", line: 1.5 },
  { key: "k", label: "1+ K", line: 0.5 },
  { key: "bb", label: "1+ BB", line: 0.5 },
];
const BAT_STAT_LABEL: Record<string, string> = {
  pa: "Plate appearances", h: "Hits", "1b": "Singles", "2b": "Doubles", "3b": "Triples", hr: "Home runs", tb: "Total bases",
  bb: "Walks", k: "Strikeouts", hbp: "Hit by pitch", r: "Runs", rbi: "RBI", hrr: "Hits+Runs+RBI",
};
const PIT_STAT_LABEL: Record<string, string> = {
  k: "Strikeouts", outs: "Outs recorded", h: "Hits allowed", er: "Earned runs", bb: "Walks allowed", bf: "Batters faced", hr: "HR allowed", r: "Runs allowed",
};

function lineFromPmf(pmf: number[], line: number) {
  let over = 0, under = 0, push = 0;
  pmf.forEach((p, v) => { if (v > line) over += p; else if (v < line) under += p; else push += p; });
  return { over, under, push };
}
const pOver = (d: MlbDist | undefined, line: number) =>
  d ? (d.lines.find((l) => l.line === line)?.over ?? lineFromPmf(d.pmf, line).over) : null;

export function MlbSim() {
  const slate = useQuery({ queryKey: ["mlb-slate"], queryFn: () => api.mlbSlate(10) });
  const [pk, setPk] = useState<number | null>(null);
  const games = slate.data?.games ?? [];
  useEffect(() => { if (pk == null && games.length) setPk(games[0].game_pk); }, [games, pk]);
  const game = games.find((g) => g.game_pk === pk) ?? null;

  return (
    <div className="space-y-5">
      <PageTitle title="MLB Game Sim" subtitle="plate-appearance Monte Carlo · every prop from the same simulated games" />
      {slate.error && <Msg tone="error">{(slate.error as Error).message}</Msg>}
      {slate.isLoading && <Msg>Loading the slate…</Msg>}
      {slate.data && games.length === 0 && <Msg>No MLB games scheduled in the next 10 days.</Msg>}
      {games.length > 0 && (
        <div className="flex gap-2 overflow-x-auto pb-1">
          {games.map((g) => (
            <button key={g.game_pk} onClick={() => setPk(g.game_pk)}
              className={clsx("shrink-0 text-left rounded-lg border px-3 py-2 text-xs",
                g.game_pk === pk ? "border-violet/60 bg-violet/15" : "border-glass hover:bg-white/5")}>
              <div className="font-medium text-sm">{g.away || "TBD"} @ {g.home || "TBD"}</div>
              <div className="text-gray-500">{g.date} · {g.game_type === "POST" ? "postseason" : g.status}</div>
              <div className="text-gray-400 mt-0.5">{g.away_probable?.name ?? "TBD"} vs {g.home_probable?.name ?? "TBD"}</div>
            </button>
          ))}
        </div>
      )}
      {game && <GameRun key={game.game_pk} game={game} />}
      <Validation />
    </div>
  );
}

function StarterPick({ team, value, onChange }: { team: string; value: string; onChange: (v: string) => void }) {
  const list = useQuery({
    queryKey: ["mlb-starters", team],
    queryFn: () => api.playerProjections("mlb", "pitcher_k", { teams: team, limit: 15, min_games: 1 }),
    enabled: !!team,
  });
  return (
    <select value={value} onChange={(e) => onChange(e.target.value)}
      className="bg-surface border border-glass rounded-md px-2 py-1 text-sm text-gray-100">
      <option value="">pick {team} starter…</option>
      {(list.data?.projections ?? []).map((p) => <option key={p.player_id} value={p.player_id}>{p.name} ({p.sample_games} GS)</option>)}
    </select>
  );
}

function GameRun({ game }: { game: MlbSlateGame }) {
  const [awaySp, setAwaySp] = useState("");
  const [homeSp, setHomeSp] = useState("");
  const [n, setN] = useState("100000");
  const [run, setRun] = useState(0);
  const needAway = !game.away_probable;
  const needHome = !game.home_probable;
  const ready = (!needAway || !!awaySp) && (!needHome || !!homeSp);
  const sim = useQuery({
    queryKey: ["mlb-sim", game.game_pk, awaySp, homeSp, n, run],
    queryFn: () => api.mlbSimulate(game.game_pk, { n: Number(n) || 100000, away_sp: awaySp || undefined, home_sp: homeSp || undefined }),
    enabled: run > 0 && ready,
    staleTime: Infinity,
  });
  return (
    <div className="space-y-4">
      <Card className="flex flex-wrap items-center gap-3 text-sm">
        <div>
          <div className="font-medium">{game.away} @ {game.home}</div>
          <div className="text-xs text-gray-500">{game.venue} · {new Date(game.start).toLocaleString()} · lineups {game.lineups_posted.away && game.lineups_posted.home ? "posted" : "not posted yet (previous game's order is used)"}</div>
        </div>
        <div className="ml-auto flex flex-wrap items-center gap-2 text-xs text-gray-400">
          {needAway ? <StarterPick team={game.away} value={awaySp} onChange={setAwaySp} /> : <span>{game.away_probable?.name}</span>}
          <span>vs</span>
          {needHome ? <StarterPick team={game.home} value={homeSp} onChange={setHomeSp} /> : <span>{game.home_probable?.name}</span>}
          sims <Input type="number" value={n} onChange={setN} className="w-24" />
          <button disabled={!ready || sim.isFetching} onClick={() => setRun((r) => r + 1)}
            className="px-3 py-1.5 rounded-md bg-violet/80 hover:bg-violet text-white disabled:opacity-40">
            {sim.isFetching ? "Simulating…" : "Run simulation"}
          </button>
        </div>
      </Card>
      {!ready && <Msg>No starter announced for {needAway && !awaySp ? game.away : game.home} yet — pick the expected starter to simulate.</Msg>}
      {sim.error && <Msg tone="error">{(sim.error as Error).message}</Msg>}
      {sim.data && <SimResult sim={sim.data} />}
    </div>
  );
}

function SimResult({ sim }: { sim: MlbSim }) {
  const s = sim.simulation;
  const t = sim.team;
  return (
    <div className="space-y-4">
      <Card className="space-y-3">
        <div className="grid grid-cols-2 md:grid-cols-6 gap-4">
          <Stat label={`${sim.game.away} win`} value={formatPct(t.away_win)} />
          <Stat label={`${sim.game.home} win`} value={formatPct(t.home_win)} />
          <Stat label="Total runs (mean · median)" value={`${t.total.mean.toFixed(2)} · ${t.total.median}`} />
          <Stat label={`${sim.game.away} runs`} value={sim.away.runs.mean.toFixed(2)} />
          <Stat label={`${sim.game.home} runs`} value={sim.home.runs.mean.toFixed(2)} />
          <Stat label={`${sim.game.home} -1.5`} value={formatPct(t.home_runline_minus_1_5)} />
        </div>
        <div className="flex flex-wrap gap-2 text-xs">
          {t.totals.filter((l) => l.line % 1 !== 0).map((l) => (
            <span key={l.line} className="px-2 py-1 rounded border border-glass font-mono">
              o{l.line} {formatPct(l.over)} · u {formatPct(l.under)}
            </span>
          ))}
        </div>
        <div className="text-[11px] text-gray-500 leading-relaxed">
          <span className={clsx("px-1.5 py-0.5 rounded mr-2", s.production_grade ? "bg-emerald/20 text-emerald-light" : "bg-amber-500/20 text-amber-300")}>
            {s.production_grade ? "production run" : "research run (<100k)"}
          </span>
          {s.valid.toLocaleString()} valid of {s.requested.toLocaleString()} · {s.rejected} rejected · {s.parameter_draws} posterior draws ·
          seed {s.seed} · data before {s.data_cutoff} · params {s.provenance.sim_params} / {s.provenance.baserunning ?? "no baserunning table"} ·
          {" "}{(s.sim_ms / 1000).toFixed(1)}s · {sim.game.away}: {sim.inputs.away.lineup_source}; {sim.game.home}: {sim.inputs.home.lineup_source}
        </div>
        {sim.flags.length > 0 && <div className="text-xs text-amber-300">{sim.flags.join(" · ")}</div>}
      </Card>
      <div className="grid xl:grid-cols-2 gap-4">
        <SideCard side={sim.away} label={`${sim.game.away} (away)`} />
        <SideCard side={sim.home} label={`${sim.game.home} (home)`} />
      </div>
      <Card className="text-[11px] text-gray-500 space-y-1">
        <div className="text-gray-400">{s.note}</div>
        <ul className="list-disc pl-4">{sim.limitations.map((l) => <li key={l}>{l}</li>)}</ul>
      </Card>
    </div>
  );
}

function SideCard({ side, label }: { side: MlbSide; label: string }) {
  const [sel, setSel] = useState<{ kind: "bat" | "sp"; idx: number }>({ kind: "sp", idx: 0 });
  const sp = side.starter.stats;
  const selected = sel.kind === "sp" ? { name: side.starter.name, stats: sp, labels: PIT_STAT_LABEL } :
    { name: side.batters[sel.idx]?.name, stats: side.batters[sel.idx]?.stats ?? {}, labels: BAT_STAT_LABEL };
  return (
    <Card className="space-y-3">
      <div className="flex items-baseline gap-2">
        <h2 className="font-medium">{label}</h2>
        <span className="text-xs text-gray-500">runs {side.runs.mean.toFixed(2)} (80%: {side.runs.p10}–{side.runs.p90})</span>
      </div>
      <button onClick={() => setSel({ kind: "sp", idx: 0 })}
        className={clsx("w-full text-left rounded-md border px-3 py-2 text-xs", sel.kind === "sp" ? "border-violet/50 bg-violet/10" : "border-glass hover:bg-white/5")}>
        <div className="font-medium text-sm">SP {side.starter.name}</div>
        <div className="font-mono text-gray-400 mt-0.5">
          K {sp.k.mean.toFixed(2)} · outs {sp.outs.mean.toFixed(1)} · H {sp.h.mean.toFixed(2)} · ER {sp.er.mean.toFixed(2)} · BB {sp.bb.mean.toFixed(2)} · BF {sp.bf.mean.toFixed(1)}
        </div>
      </button>
      <div className="overflow-x-auto -mx-2">
        <Table head={<tr><Th>#</Th><Th>Batter</Th><Th className="text-right">PA</Th><Th className="text-right">H</Th>{BAT_PROPS.map((p) => <Th key={p.key + p.line} className="text-right">{p.label}</Th>)}</tr>}>
          {side.batters.map((b, i) => (
            <tr key={b.player_id} onClick={() => setSel({ kind: "bat", idx: i })}
              className={clsx("border-t border-glass cursor-pointer", sel.kind === "bat" && sel.idx === i ? "bg-violet/15" : "hover:bg-white/[0.03]")}>
              <Td className="text-gray-500">{b.slot}</Td>
              <Td><span className="font-medium">{b.name}</span> <span className="text-[10px] text-gray-500">{b.position}</span></Td>
              <Td className="text-right font-mono">{b.stats.pa.mean.toFixed(2)}</Td>
              <Td className="text-right font-mono">{b.stats.h.mean.toFixed(2)}</Td>
              {BAT_PROPS.map((p) => <Td key={p.key + p.line} className="text-right font-mono">{formatPct(pOver(b.stats[p.key], p.line), 0)}</Td>)}
            </tr>
          ))}
        </Table>
      </div>
      {selected.name && <PropDetail name={selected.name} stats={selected.stats} labels={selected.labels} />}
    </Card>
  );
}

function PropDetail({ name, stats, labels }: { name: string; stats: Record<string, MlbDist>; labels: Record<string, string> }) {
  const keys = Object.keys(labels).filter((k) => stats[k]);
  const [stat, setStat] = useState(keys[0]);
  const d = stats[stat] ?? stats[keys[0]];
  const [lineText, setLineText] = useState("");
  useEffect(() => { setLineText(""); }, [stat, name]);
  if (!d) return null;
  const line = lineText === "" ? d.lines[0]?.line ?? 0.5 : Number(lineText);
  const p = lineFromPmf(d.pmf, line);
  return (
    <div className="space-y-2 border-t border-glass pt-3">
      <div className="flex flex-wrap items-center gap-2 text-xs text-gray-400">
        <span className="font-medium text-gray-200 text-sm mr-1">{name}</span>
        <select value={stat} onChange={(e) => setStat(e.target.value)} className="bg-surface border border-glass rounded-md px-2 py-1 text-xs text-gray-100">
          {keys.map((k) => <option key={k} value={k}>{labels[k]}</option>)}
        </select>
        line <Input type="number" value={lineText} onChange={setLineText} placeholder={String(line)} className="w-16" />
        <span className="font-mono">over {formatPct(p.over)} · under {formatPct(p.under)}{p.push > 0 ? ` · push ${formatPct(p.push)}` : ""}</span>
      </div>
      <PmfChart pmf={d.pmf} line={line} />
      <div className="text-[11px] text-gray-500 font-mono">
        mean {d.mean.toFixed(2)} · median {d.median} · sd {d.sd.toFixed(2)} · 80% {d.p10}–{d.p90} · 95% {d.p025}–{d.p975}
        {d.lines.length > 0 && <> · MC error ±{formatPct(Math.max(...d.lines.map((l) => l.mcse)), 2)}</>}
      </div>
    </div>
  );
}

function PmfChart({ pmf, line }: { pmf: number[]; line: number }) {
  let last = 0;
  pmf.forEach((p, i) => { if (p >= 0.002) last = i; });
  const shown = pmf.slice(0, Math.max(last + 1, 2));
  const max = Math.max(...shown, 0.01);
  const W = 28, H = 90;
  return (
    <svg viewBox={`0 0 ${shown.length * W} ${H + 16}`} className="w-full" style={{ maxHeight: 130 }} role="img" aria-label="simulated distribution">
      {shown.map((p, v) => {
        const h = (p / max) * H;
        const fill = v > line ? "#10B981" : v < line ? "#EF4444" : "#6B7280";
        return (
          <g key={v}>
            <title>{`${v}: ${(p * 100).toFixed(1)}%`}</title>
            <rect x={v * W + 3} y={H - h} width={W - 6} height={Math.max(h, 1)} rx={2} fill={fill} opacity={0.85} />
            <text x={v * W + W / 2} y={H + 12} textAnchor="middle" fontSize={9} className="fill-gray-500">{v}</text>
          </g>
        );
      })}
    </svg>
  );
}

// --------------------------------------------------------------------------
// Validation: latest stored walk-forward backtest
// --------------------------------------------------------------------------

function Validation() {
  const bt = useQuery({ queryKey: ["mlb-backtests"], queryFn: () => api.mlbBacktests() });
  const runs = bt.data?.runs ?? [];
  const [season, setSeason] = useState<number | null>(null);
  const r: MlbBacktest | undefined = runs.find((x) => x.season === season) ?? runs[0];
  return (
    <Card className="space-y-3">
      <div className="flex flex-wrap items-center gap-2">
        <h2 className="font-medium">Does it work? Walk-forward validation</h2>
        {runs.length > 1 && <Pills options={runs.map((x) => ({ key: String(x.season), label: String(x.season) }))} value={String(r?.season)} onChange={(v) => setSeason(Number(v))} />}
      </div>
      {bt.error && <Msg tone="error">{(bt.error as Error).message}</Msg>}
      {!bt.isLoading && runs.length === 0 && <Msg>No backtest stored yet. Run GET /api/projection-service/backtest/mlb/sim?season=2026.</Msg>}
      {r && (
        <>
          <div className="text-xs text-gray-500">
            {r.games_simulated} games ({r.window[0]} → {r.window[1]}) · {r.sims_per_game.toLocaleString()} sims each (research setting) ·
            params {r.provenance.sim_params} · run {new Date(r.run_at).toLocaleString()}
          </div>
          <div className="overflow-x-auto -mx-2">
            <Table head={<tr><Th>Market</Th><Th className="text-right">n</Th><Th className="text-right">Hit rate</Th><Th className="text-right">Model Brier</Th><Th className="text-right">Baseline Brier</Th><Th className="text-right">Diff ± SE</Th><Th className="text-right">Calib. error</Th></tr>}>
              {[...r.team, ...r.batter, ...r.pitcher].map((m) => <BinaryRow key={m.market} m={m} />)}
            </Table>
          </div>
          <div className="text-[11px] text-gray-500">{r.method} Baselines: {r.baselines.player}; team: {r.baselines.team}. {r.notes[0]}</div>
        </>
      )}
    </Card>
  );
}

function BinaryRow({ m }: { m: MlbBinaryReport }) {
  if (!m.n) return null;
  const d = m.brier_diff_model_minus_baseline;
  const sig = Math.abs(d) >= 2 * m.brier_diff_se;
  return (
    <tr className="border-t border-glass">
      <Td>{m.market}</Td>
      <Td className="text-right font-mono">{m.n}</Td>
      <Td className="text-right font-mono">{formatPct(m.base_rate)}</Td>
      <Td className="text-right font-mono">{m.model.brier.toFixed(4)}</Td>
      <Td className="text-right font-mono text-gray-400">{m.baseline.brier.toFixed(4)}</Td>
      <Td className={clsx("text-right font-mono", d < 0 && sig ? "text-emerald-light" : d > 0 && sig ? "text-crimson-light" : "text-gray-400")}>
        {d > 0 ? "+" : ""}{d.toFixed(4)} ± {m.brier_diff_se.toFixed(4)}
      </Td>
      <Td className="text-right font-mono">{m.model.ece.toFixed(3)}</Td>
    </tr>
  );
}
