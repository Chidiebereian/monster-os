import { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import clsx from "clsx";
import { api, type PlayerProjection } from "../lib/api";
import { normalPdf, probOver } from "../lib/stats";
import { americanToImpliedProb, formatPct } from "../lib/odds";
import { Card, Input, Msg, PageTitle, Pills, Stat, Table, Td, Th } from "../components/ui";

// AI Picks & Projections. Filter sport -> stat -> player, then see that player's
// model distribution (from projection-service's Bayesian posteriors, drawn by
// the simulation-engine at n=100k) against a draggable line. Every number is
// model-driven — the UI never invents a projection.

type StatDef = { key: string; label: string; model: string; unit: string };
type SportDef = { sport: string; label: string; stats: StatDef[] };

const nflStat = (key: string, label: string, unit: string): StatDef => ({ key, label, unit, model: `nfl-${key}-v1` });
const PROJ_SPORTS: SportDef[] = [
  {
    sport: "nfl",
    label: "NFL",
    stats: [
      nflStat("pass_yds", "Passing Yards", "yds"),
      nflStat("pass_td", "Passing TDs", "TDs"),
      nflStat("pass_att", "Pass Attempts", "att"),
      nflStat("completions", "Completions", "comp"),
      nflStat("interceptions", "Interceptions", "INT"),
      nflStat("rush_yds", "Rushing Yards", "yds"),
      nflStat("carries", "Carries", "att"),
      nflStat("rush_td", "Rushing TDs", "TDs"),
      nflStat("rec_yds", "Receiving Yards", "yds"),
      nflStat("receptions", "Receptions", "rec"),
      nflStat("targets", "Targets", "tgt"),
      nflStat("rec_td", "Receiving TDs", "TDs"),
    ],
  },
  { sport: "nba", label: "NBA", stats: [] },
  { sport: "mlb", label: "MLB", stats: [] },
  { sport: "nhl", label: "NHL", stats: [] },
];

export function Projections() {
  const [sportKey, setSportKey] = useState("nfl");
  const [statKey, setStatKey] = useState("pass_td");
  const [tab, setTab] = useState<"player" | "game">("player");

  const sport = PROJ_SPORTS.find((s) => s.sport === sportKey) ?? PROJ_SPORTS[0];
  const stat = sport.stats.find((s) => s.key === statKey) ?? sport.stats[0];

  function pickSport(k: string) {
    setSportKey(k);
    const next = PROJ_SPORTS.find((s) => s.sport === k);
    setStatKey(next?.stats[0]?.key ?? "");
  }

  return (
    <div className="space-y-5">
      <PageTitle
        title="Projections"
        subtitle="model distribution vs the line · trained on decades of real games"
        right={
          <Pills
            options={[{ key: "player", label: "Player projections" }, { key: "game", label: "Game projections" }]}
            value={tab}
            onChange={(v) => setTab(v as "player" | "game")}
          />
        }
      />

      <div className="flex flex-wrap items-center gap-3">
        <span className="text-xs text-gray-500 uppercase tracking-wide">Sport</span>
        <Pills options={PROJ_SPORTS.map((s) => ({ key: s.sport, label: s.label }))} value={sportKey} onChange={pickSport} />
        {sport.stats.length > 0 && (
          <>
            <span className="text-xs text-gray-500 uppercase tracking-wide ml-2">Stat</span>
            <Pills options={sport.stats.map((s) => ({ key: s.key, label: s.label }))} value={statKey} onChange={setStatKey} />
          </>
        )}
      </div>

      {tab === "game" ? (
        <Msg>Game projections arrive with the team models (Elo / Bradley-Terry, Phase 2). Player projections are live now.</Msg>
      ) : sport.stats.length === 0 || !stat ? (
        <Msg>{sport.label} player models are in the pipeline. NFL passing TDs is live now — pick NFL above.</Msg>
      ) : (
        <PlayerProjections sport={sport.sport} stat={stat} />
      )}
    </div>
  );
}

function PlayerProjections({ sport, stat }: { sport: string; stat: StatDef }) {
  const [search, setSearch] = useState("");
  const [selected, setSelected] = useState<string | null>(null);

  const list = useQuery({
    queryKey: ["projections", sport, stat.key, search],
    queryFn: () => api.playerProjections(sport, stat.key, { limit: 150, min_games: 16, q: search || undefined }),
  });

  const rows = list.data?.projections ?? [];
  // default-select the top projection once data arrives
  useEffect(() => {
    if (rows.length && (!selected || !rows.some((r) => r.player_id === selected))) {
      setSelected(rows[0].player_id);
    }
  }, [rows, selected]);

  const current = rows.find((r) => r.player_id === selected) ?? null;

  return (
    <div className="grid lg:grid-cols-[minmax(0,360px)_1fr] gap-5 items-start">
      {/* player list */}
      <Card className="space-y-3">
        <div className="flex items-center gap-2">
          <h2 className="font-medium text-sm">{stat.label}</h2>
          <span className="text-xs text-gray-500 ml-auto">{list.data?.count ?? 0} players</span>
        </div>
        <Input value={search} onChange={setSearch} placeholder="Search player…" className="w-full" />
        {list.isLoading && <Msg>Loading projections…</Msg>}
        {list.error && <Msg tone="error">{(list.error as Error).message}</Msg>}
        {!list.isLoading && rows.length === 0 && <Msg>No players match.</Msg>}
        {rows.length > 0 && (
          <div className="max-h-[520px] overflow-auto -mx-2">
            <Table head={<tr><Th>Player</Th><Th className="text-right">Proj</Th><Th className="text-right">Recent</Th></tr>}>
              {rows.map((p) => (
                <tr
                  key={p.player_id}
                  onClick={() => setSelected(p.player_id)}
                  className={clsx(
                    "border-t border-glass cursor-pointer",
                    p.player_id === selected ? "bg-violet/15" : "hover:bg-white/[0.03]",
                  )}
                >
                  <Td>
                    <div className="font-medium leading-tight">{p.name}</div>
                    <div className="text-[10px] text-gray-500">{p.team ?? "—"} · {p.sample_games}g</div>
                  </Td>
                  <Td className="text-right font-mono text-violet-light">{p.proj_mean.toFixed(2)}</Td>
                  <Td className="text-right font-mono text-gray-400">{p.recent_mean?.toFixed(2) ?? "—"}</Td>
                </tr>
              ))}
            </Table>
          </div>
        )}
      </Card>

      {/* distribution for the selected player */}
      {current ? <Distribution sport={sport} stat={stat} player={current} /> : <Card><Msg>Select a player.</Msg></Card>}
    </div>
  );
}

function Distribution({ sport, stat, player }: { sport: string; stat: StatDef; player: PlayerProjection }) {
  const mean = player.proj_mean;
  const sd = Math.max(player.proj_sd, 0.05);
  const [line, setLine] = useState(roundHalf(mean));
  const [marketOdds, setMarketOdds] = useState("-110");

  // reset the line when the player changes
  useEffect(() => setLine(roundHalf(mean)), [player.player_id, mean]);

  const sim = useQuery({
    queryKey: ["sim-model", stat.model, player.player_id],
    queryFn: () => api.simulateModel({ model_id: stat.model, parameter: `${stat.key}:${player.player_id}`, n: 100000 }),
  });
  const detail = useQuery({
    queryKey: ["proj-detail", sport, stat.key, player.player_id],
    queryFn: () => api.playerProjection(sport, stat.key, player.player_id),
  });

  const pOver = probOver(line, mean, sd);
  const marketImplied = americanToImpliedProb(Number(marketOdds) || 0);
  const edge = pOver - marketImplied;

  return (
    <div className="space-y-4">
      <Card className="space-y-4">
        <div className="flex items-baseline gap-3 flex-wrap">
          <h2 className="font-medium">{player.name}</h2>
          <span className="text-xs text-gray-500">{player.team ?? "—"} · {stat.label} · {player.sample_games} games</span>
          <span className="ml-auto text-xs text-gray-500">
            career <span className="font-mono text-gray-300">{player.career_mean?.toFixed(2)}</span> ·
            recent <span className="font-mono text-gray-300"> {player.recent_mean?.toFixed(2)}</span>
          </span>
        </div>

        <div className="grid md:grid-cols-4 gap-4 items-end">
          <Stat label="Projection (mean)" value={`${mean.toFixed(2)} ${stat.unit}`} />
          <label className="text-xs text-gray-500">Line ({stat.unit})<Input type="number" value={line} onChange={(v) => setLine(Number(v) || 0)} className="w-full mt-1" /></label>
          <label className="text-xs text-gray-500">Market odds (Over)<Input type="number" value={marketOdds} onChange={setMarketOdds} className="w-full mt-1" /></label>
          <Stat label="Edge (Over)" value={`${edge >= 0 ? "+" : ""}${formatPct(edge)}`} tone={edge >= 0 ? "pos" : "neg"} />
        </div>

        <Curve mean={mean} sd={sd} line={line} />

        <div>
          <div className="flex justify-between text-xs text-gray-500 mb-1">
            <span>Line: <span className="text-gray-200 font-mono">{line.toFixed(1)}</span> {stat.unit}</span>
            <span>drag to recompute</span>
          </div>
          <input
            type="range"
            min={Math.max(0, mean - 3 * sd)}
            max={mean + 3 * sd}
            step={(6 * sd) / 200}
            value={line}
            onChange={(e) => setLine(Number(e.target.value))}
            className="w-full accent-violet"
          />
        </div>

        <div className="grid grid-cols-2 md:grid-cols-4 gap-4 pt-3 border-t border-glass">
          <Stat label={`P(Over ${line.toFixed(1)})`} value={formatPct(pOver)} tone="pos" />
          <Stat label="P(Under)" value={formatPct(1 - pOver)} tone="neg" />
          <Stat label="Market implied" value={formatPct(marketImplied)} />
          <Stat label="Proj SD" value={sd.toFixed(2)} />
        </div>
      </Card>

      <div className="grid md:grid-cols-2 gap-4">
        <Card>
          <h3 className="font-medium text-sm mb-2">Monte Carlo (n=100k, model posterior)</h3>
          {sim.isLoading && <Msg>Simulating…</Msg>}
          {sim.error && <Msg tone="error">{(sim.error as Error).message}</Msg>}
          {sim.data && (
            <div className="grid grid-cols-2 sm:grid-cols-4 gap-3">
              <Stat label="Mean" value={sim.data.mean.toFixed(2)} />
              <Stat label="Median" value={sim.data.median.toFixed(2)} />
              <Stat label="p10" value={sim.data.p10.toFixed(2)} />
              <Stat label="p90" value={sim.data.p90.toFixed(2)} />
            </div>
          )}
        </Card>
        <Card>
          <h3 className="font-medium text-sm mb-2">Recent form</h3>
          {detail.isLoading && <Msg>Loading…</Msg>}
          {detail.data && detail.data.recent_games.length > 0 ? (
            <div className="flex flex-wrap gap-1.5">
              {detail.data.recent_games.slice(0, 16).map((g, i) => {
                const v = Number(g.value ?? 0);
                const label = Number.isInteger(v) ? v : Math.round(v);
                return (
                  <div
                    key={i}
                    title={`${g.season} wk ${g.week} vs ${g.opponent}: ${v} ${stat.unit}`}
                    className={clsx(
                      "min-w-7 h-7 px-1.5 rounded flex items-center justify-center text-xs font-mono",
                      v >= mean ? "bg-emerald/25 text-emerald-light" : v > 0 ? "bg-white/5 text-gray-300" : "bg-crimson/15 text-crimson-light",
                    )}
                  >
                    {label}
                  </div>
                );
              })}
            </div>
          ) : (
            !detail.isLoading && <Msg>No recent games.</Msg>
          )}
        </Card>
      </div>
    </div>
  );
}

function roundHalf(x: number) {
  return Math.round(x * 2) / 2;
}

function Curve({ mean, sd, line }: { mean: number; sd: number; line: number }) {
  const W = 640;
  const H = 180;
  const lo = Math.max(0, mean - 3.5 * sd);
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
      {over.length > 1 && (
        <polygon points={`${lineX},${H} ${over.join(" ")} ${x(hi)},${H}`} fill="rgba(16,185,129,0.18)" />
      )}
      <polyline points={pts.join(" ")} fill="none" stroke="#8B5CF6" strokeWidth={2} />
      <line x1={lineX} y1={6} x2={lineX} y2={H} stroke="#F59E0B" strokeWidth={1.5} strokeDasharray="4 3" />
      <line x1={x(mean)} y1={14} x2={x(mean)} y2={H} stroke="rgba(255,255,255,0.25)" strokeWidth={1} />
      <text x={x(mean)} y={11} fill="rgba(255,255,255,0.4)" fontSize="9" textAnchor="middle">proj</text>
      <text x={lineX} y={11} fill="#F59E0B" fontSize="9" textAnchor="middle">line</text>
    </svg>
  );
}
