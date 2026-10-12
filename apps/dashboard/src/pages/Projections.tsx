import { useEffect, useState } from "react";
import { keepPreviousData, useQuery } from "@tanstack/react-query";
import clsx from "clsx";
import { api, type PlayerProjection, type SimHistogram } from "../lib/api";
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
        sportKey === "nfl" ? (
          <GameProjections />
        ) : (
          <Msg>{sport.label} team model is in the pipeline. NFL game projections are live now.</Msg>
        )
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
  const [gameId, setGameId] = useState("");

  const games = useQuery({
    queryKey: ["games", sport, "scheduled"],
    queryFn: () => api.games(sport, { status: "scheduled", limit: 400 }),
    enabled: sport === "nfl",
  });
  // upcoming games sorted by date, soonest first
  const upcoming = [...(games.data?.games ?? [])]
    .filter((g) => g.home && g.away)
    .sort((a, b) => (a.date ?? "").localeCompare(b.date ?? ""));
  const game = upcoming.find((g) => g.game_id === gameId);
  const teams = game ? `${game.away},${game.home}` : undefined;
  // when a game is picked, restrict to current-roster players (last season near now)
  const currentSeason = Math.max(0, ...upcoming.map((g) => g.season ?? 0));
  const minLastSeason = game && currentSeason ? currentSeason - 1 : undefined;

  const list = useQuery({
    queryKey: ["projections", sport, stat.key, search, teams, minLastSeason],
    queryFn: () =>
      api.playerProjections(sport, stat.key, {
        limit: 150,
        min_games: 16,
        q: search || undefined,
        teams,
        min_last_season: minLastSeason,
      }),
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
        {sport === "nfl" && (
          <select
            value={gameId}
            onChange={(e) => setGameId(e.target.value)}
            className="w-full bg-surface border border-glass rounded-md px-2 py-1 text-sm text-gray-100 focus:outline-none focus:border-violet/50"
          >
            <option value="">All games · all players</option>
            {upcoming.map((g) => (
              <option key={g.game_id} value={g.game_id}>
                {g.away} @ {g.home}{g.week ? ` · wk ${g.week}` : ""}{g.date ? ` · ${g.date}` : ""}
              </option>
            ))}
          </select>
        )}
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
  const [line, setLine] = useState(roundHalf(mean));
  const [marketOdds, setMarketOdds] = useState("-110");

  // reset the line when the player changes
  useEffect(() => setLine(roundHalf(mean)), [player.player_id, mean]);

  const base = {
    model_id: stat.model,
    parameter: `${stat.key}:${player.player_id}`,
    family: player.family ?? undefined,
    dispersion: player.dispersion ?? undefined,
    n: 100000,
  };

  // Stable histogram of the real simulated distribution (one draw per player).
  const hist = useQuery({
    queryKey: ["sim-hist", stat.model, player.player_id],
    queryFn: () => api.simulateModel({ ...base, hist: 64 }),
  });
  // Exact Over/Under/Push at the (rounded) line, from the backend draws.
  const qline = roundHalf(line);
  const prob = useQuery({
    queryKey: ["sim-prob", stat.model, player.player_id, qline],
    queryFn: () => api.simulateModel({ ...base, line: qline }),
    placeholderData: keepPreviousData,
  });
  const detail = useQuery({
    queryKey: ["proj-detail", sport, stat.key, player.player_id],
    queryFn: () => api.playerProjection(sport, stat.key, player.player_id),
  });

  const pOver = prob.data?.p_over ?? 0;
  const pUnder = prob.data?.p_under ?? 0;
  const pPush = prob.data?.p_push ?? 0;
  const marketImplied = americanToImpliedProb(Number(marketOdds) || 0);
  const edge = pOver - marketImplied;
  const s = hist.data;
  const sd = Math.max(player.proj_sd, 0.05);

  return (
    <div className="space-y-4">
      <Card className="space-y-4">
        <div className="flex items-baseline gap-3 flex-wrap">
          <h2 className="font-medium">{player.name}</h2>
          <span className="text-xs text-gray-500">{player.team ?? "—"} · {stat.label} · {player.sample_games} games</span>
          <span className="ml-auto text-xs text-gray-500">
            career <span className="font-mono text-gray-300">{player.career_mean?.toFixed(2)}</span> ·
            recent <span className="font-mono text-gray-300"> {player.recent_mean?.toFixed(2)}</span>
            <span className="ml-2 px-1.5 py-0.5 rounded bg-white/5 text-[10px]">{s?.family ?? player.family}</span>
          </span>
        </div>

        <div className="grid md:grid-cols-4 gap-4 items-end">
          <Stat label="Projection (mean)" value={`${mean.toFixed(2)} ${stat.unit}`} />
          <label className="text-xs text-gray-500">Line ({stat.unit})<Input type="number" value={line} onChange={(v) => setLine(Number(v) || 0)} className="w-full mt-1" /></label>
          <label className="text-xs text-gray-500">Market odds (Over)<Input type="number" value={marketOdds} onChange={setMarketOdds} className="w-full mt-1" /></label>
          <Stat label="Edge (Over)" value={`${edge >= 0 ? "+" : ""}${formatPct(edge)}`} tone={edge >= 0 ? "pos" : "neg"} />
        </div>

        <HistCurve hist={s?.hist} line={line} loading={hist.isLoading} />

        <div>
          <div className="flex justify-between text-xs text-gray-500 mb-1">
            <span>Line: <span className="text-gray-200 font-mono">{line.toFixed(1)}</span> {stat.unit}</span>
            <span>drag to recompute · exact from 100k draws</span>
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
          <Stat label={`P(Over ${qline.toFixed(1)})`} value={formatPct(pOver)} tone="pos" />
          <Stat label="P(Under)" value={formatPct(pUnder)} tone="neg" />
          <Stat label="P(Push)" value={pPush > 0.0005 ? formatPct(pPush) : "—"} />
          <Stat label="Market implied" value={formatPct(marketImplied)} />
        </div>
      </Card>

      <div className="grid md:grid-cols-2 gap-4">
        <Card>
          <h3 className="font-medium text-sm mb-2">Monte Carlo (n=100k, {s?.family ?? "model"} posterior)</h3>
          {hist.isLoading && <Msg>Simulating…</Msg>}
          {hist.error && <Msg tone="error">{(hist.error as Error).message}</Msg>}
          {s && (
            <div className="grid grid-cols-2 sm:grid-cols-4 gap-3">
              <Stat label="Mean" value={s.mean.toFixed(2)} />
              <Stat label="Median" value={s.median.toFixed(2)} />
              <Stat label="p10" value={s.p10.toFixed(2)} />
              <Stat label="p90" value={s.p90.toFixed(2)} />
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

// Game projections from the team model (log-linear Poisson, 100k simulation):
// win probabilities, projected score, spread and total with distributions.
function GameProjections() {
  const ratings = useQuery({ queryKey: ["team-ratings"], queryFn: () => api.teamRatings("nfl") });
  const teams = (ratings.data?.teams ?? []).map((t) => t.team).sort();
  const [home, setHome] = useState<string>("");
  const [away, setAway] = useState<string>("");
  const [spread, setSpread] = useState("-2.5");
  const [total, setTotal] = useState("47.5");

  useEffect(() => {
    if (teams.length && !home) {
      setHome(teams.includes("BUF") ? "BUF" : teams[0]);
      setAway(teams.includes("KC") ? "KC" : teams[1] ?? teams[0]);
    }
  }, [teams, home]);

  const m = useQuery({
    queryKey: ["matchup", home, away, spread, total],
    queryFn: () => api.matchup("nfl", home, away, { spread: Number(spread) || undefined, total: Number(total) || undefined, n: 100000 }),
    enabled: Boolean(home && away && home !== away),
    placeholderData: keepPreviousData,
  });
  const d = m.data;

  const teamSelect = (value: string, onChange: (v: string) => void, label: string) => (
    <label className="text-xs text-gray-500">
      {label}
      <select value={value} onChange={(e) => onChange(e.target.value)} className="w-full mt-1 bg-surface border border-glass rounded-md px-2 py-1 text-sm text-gray-100 focus:outline-none focus:border-violet/50">
        {teams.map((t) => <option key={t} value={t}>{t}</option>)}
      </select>
    </label>
  );

  return (
    <div className="space-y-4">
      <Card className="space-y-4">
        <div className="grid grid-cols-2 md:grid-cols-4 gap-4 items-end">
          {teamSelect(away, setAway, "Away")}
          {teamSelect(home, setHome, "Home")}
          <label className="text-xs text-gray-500">Home spread<Input type="number" value={spread} onChange={setSpread} className="w-full mt-1" /></label>
          <label className="text-xs text-gray-500">Total<Input type="number" value={total} onChange={setTotal} className="w-full mt-1" /></label>
        </div>

        {ratings.isLoading && <Msg>Loading team ratings…</Msg>}
        {home === away && <Msg tone="error">Pick two different teams.</Msg>}
        {m.error && <Msg tone="error">{(m.error as Error).message}</Msg>}

        {d && (
          <>
            <div className="flex items-center gap-4 flex-wrap">
              <div className="text-lg font-semibold">
                {d.away} <span className="font-mono text-gray-300">{d.proj_away_points.toFixed(1)}</span>
                <span className="text-gray-600 mx-2">@</span>
                {d.home} <span className="font-mono text-gray-300">{d.proj_home_points.toFixed(1)}</span>
              </div>
              <div className="ml-auto text-xs text-gray-500">100k sim · log-linear Poisson</div>
            </div>

            {/* win probability bar */}
            <div>
              <div className="flex justify-between text-xs mb-1">
                <span className="text-emerald-light">{d.away} {formatPct(d.away_win_prob)}</span>
                <span className="text-gray-500">tie {formatPct(d.tie_prob)}</span>
                <span className="text-violet-light">{d.home} {formatPct(d.home_win_prob)}</span>
              </div>
              <div className="h-3 rounded overflow-hidden flex bg-surface border border-glass">
                <div className="bg-emerald/60" style={{ width: `${d.away_win_prob * 100}%` }} />
                <div className="bg-gray-600" style={{ width: `${d.tie_prob * 100}%` }} />
                <div className="bg-violet/60" style={{ width: `${d.home_win_prob * 100}%` }} />
              </div>
            </div>

            <div className="grid grid-cols-2 md:grid-cols-4 gap-4 pt-2 border-t border-glass">
              <Stat label={`${d.home} win`} value={formatPct(d.home_win_prob)} tone="pos" />
              <Stat label="Proj spread (home)" value={d.proj_spread.toFixed(1)} />
              <Stat label="Proj total" value={d.proj_total.toFixed(1)} />
              <Stat label="Cover / Over" value={`${d.cover_prob != null ? formatPct(d.cover_prob) : "—"} / ${d.over_prob != null ? formatPct(d.over_prob) : "—"}`} />
            </div>
          </>
        )}
      </Card>

      {d && (
        <div className="grid md:grid-cols-2 gap-4">
          <Card>
            <h3 className="font-medium text-sm mb-1">Margin ({d.home} − {d.away})</h3>
            <div className="text-[11px] text-gray-500 mb-1">median {d.median_margin} · [{d.margin_p10}, {d.margin_p90}] · line at home spread</div>
            <HistCurve hist={d.margin_hist} line={-(Number(spread) || 0)} loading={false} />
          </Card>
          <Card>
            <h3 className="font-medium text-sm mb-1">Total points</h3>
            <div className="text-[11px] text-gray-500 mb-1">median {d.median_total} · [{d.total_p10}, {d.total_p90}] · line at total</div>
            <HistCurve hist={d.total_hist} line={Number(total) || 0} loading={false} />
          </Card>
        </div>
      )}
    </div>
  );
}

// Draws the real simulated predictive distribution (histogram density from the
// backend's 100k draws). Bins right of the line are shaded as the Over region.
function HistCurve({ hist, line, loading }: { hist?: SimHistogram; line: number; loading: boolean }) {
  const W = 640;
  const H = 180;
  if (!hist) {
    return <div className="h-44 flex items-center justify-center text-xs text-gray-600">{loading ? "Simulating…" : "No distribution"}</div>;
  }
  const bins = hist.density.length;
  const maxD = Math.max(...hist.density, 1e-9);
  const barW = W / bins;
  const span = hist.hi - hist.lo || 1;
  const xAt = (v: number) => ((v - hist.lo) / span) * W;
  const lineX = Math.max(0, Math.min(W, xAt(line)));

  return (
    <svg viewBox={`0 0 ${W} ${H}`} className="w-full h-44">
      {hist.density.map((d, i) => {
        const h = (d / maxD) * (H - 18);
        const center = hist.lo + (i + 0.5) * hist.bin_width;
        const isOver = center >= line;
        return (
          <rect
            key={i}
            x={i * barW + 0.5}
            y={H - h}
            width={Math.max(0.5, barW - 1)}
            height={h}
            fill={isOver ? "rgba(16,185,129,0.55)" : "rgba(139,92,246,0.5)"}
          />
        );
      })}
      <line x1={lineX} y1={2} x2={lineX} y2={H} stroke="#F59E0B" strokeWidth={1.5} strokeDasharray="4 3" />
      <text x={Math.min(lineX, W - 14)} y={10} fill="#F59E0B" fontSize="9" textAnchor="middle">line</text>
    </svg>
  );
}
