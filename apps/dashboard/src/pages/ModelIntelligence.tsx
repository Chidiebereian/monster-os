import { useMemo, useState, type ReactNode } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import clsx from "clsx";
import { engineLabel, learningApi, verdict, type EvalReport, type Evidence, type MarketScore, type RegistryRow, type Tunables } from "../lib/learning";
import { formatPct } from "../lib/odds";
import { Card, Input, Msg, PageTitle, Pills, Stat, Table, Td, Th } from "../components/ui";

const ENGINES = ["nfl", "mlb", "nba", "nhl", "nfl_team", "nba_team", "nhl_team"];
const SPORTS = [
  { key: "all", label: "All" },
  { key: "nfl", label: "NFL" },
  { key: "mlb", label: "MLB" },
  { key: "nba", label: "NBA" },
  { key: "nhl", label: "NHL" },
];

// Model Intelligence: the adaptive learning loop made visible. Every number here
// comes from the projection service — frozen pregame predictions scored after
// the game, walk-forward backtests, and the registry's promotion evidence.
export function ModelIntelligence() {
  const [sport, setSport] = useState("all");
  const s = sport === "all" ? undefined : sport;
  return (
    <div className="space-y-6">
      <PageTitle title="Model Intelligence" subtitle="frozen predictions · postgame scoring · gated promotion and rollback" right={<Pills options={SPORTS} value={sport} onChange={setSport} />} />
      <AutopilotPanel />
      <Registry sport={s} />
      <LiveRecord sport={s} />
      <Backtests sport={s} />
      <Ledger sport={s} />
      <Audit sport={s} />
    </div>
  );
}

function Btn({ children, onClick, disabled }: { children: ReactNode; onClick: () => void; disabled?: boolean }) {
  return (
    <button onClick={(e) => { e.stopPropagation(); onClick(); }} disabled={disabled}
      className="px-2 py-1 text-xs rounded-md bg-violet/20 text-violet-light border border-violet/30 hover:bg-violet/30 disabled:opacity-40 disabled:cursor-not-allowed">
      {children}
    </button>
  );
}

function PairedLines({ paired }: { paired: EvalReport["paired"] }) {
  return (
    <>
      {Object.entries(paired).map(([m, p]) => (
        <div key={m} className="font-mono">{m}: {p.brier_diff_candidate_minus_champion >= 0 ? "+" : ""}{p.brier_diff_candidate_minus_champion.toFixed(4)} ± {p.se.toFixed(4)} (z {p.z})</div>
      ))}
    </>
  );
}

function ReportLine({ label, r }: { label: string; r: EvalReport }) {
  return (
    <div>
      {label} {r.season} · {r.games} games · <span className={r.gate.pass ? "text-emerald-light" : "text-crimson-light"}>{r.gate.pass ? "passed" : "failed"}</span>
      {r.gate.improves.length > 0 && <> · better: {r.gate.improves.join(", ")}</>}
      {r.gate.degrades.length > 0 && <> · worse: {r.gate.degrades.join(", ")}</>}
    </div>
  );
}

/** Evidence from a manual evaluation, or the autopilot's search → holdout → live trail. */
function EvidenceView({ ev, notes }: { ev: Evidence; notes: string | null }) {
  if (ev.search) {
    return (
      <div className="space-y-0.5">
        <div>{notes ?? ev.change} · vs {ev.champion}</div>
        <ReportLine label="search" r={ev.search} />
        {ev.holdout && <ReportLine label="holdout" r={ev.holdout} />}
        {ev.live && <div>live: {ev.live.games} paired games{ev.live.shadow_better.length ? ` · better: ${ev.live.shadow_better.join(", ")}` : ""}{ev.live.shadow_worse.length ? ` · worse: ${ev.live.shadow_worse.join(", ")}` : ""}</div>}
        <details><summary className="cursor-pointer text-gray-500">paired differences</summary><PairedLines paired={(ev.holdout ?? ev.search).paired} /></details>
      </div>
    );
  }
  return (
    <div className="space-y-0.5">
      <div>vs {ev.champion} · {ev.season} · {ev.games} games · <span className={ev.gate.pass ? "text-emerald-light" : "text-crimson-light"}>{ev.gate.pass ? "gate passed" : "gate failed"}</span></div>
      {ev.paired && <PairedLines paired={ev.paired} />}
    </div>
  );
}

const ago = (iso: string) => {
  const m = Math.round((Date.now() - new Date(iso.replace(" ", "T")).getTime()) / 60000);
  return m < 1 ? "just now" : m < 60 ? `${m} min ago` : m < 48 * 60 ? `${Math.round(m / 60)} h ago` : `${Math.round(m / 1440)} d ago`;
};

/** The unattended loop: what runs when, what it last did, shadows on live trial. */
function AutopilotPanel() {
  const qc = useQueryClient();
  const q = useQuery({ queryKey: ["learning-autopilot"], queryFn: () => learningApi.autopilot(), refetchInterval: 60_000 });
  const refresh = () => ["learning-autopilot", "learning-registry", "learning-summary", "learning-ledger", "learning-audit"].forEach((k) => qc.invalidateQueries({ queryKey: [k] }));
  const pause = useMutation({ mutationFn: (p: boolean) => learningApi.setPaused(p), onSuccess: refresh });
  const tick = useMutation({ mutationFn: () => learningApi.runTick(), onSuccess: refresh });
  const d = q.data;
  const tickJob = d?.jobs.find((j) => j.job === "tick");
  return (
    <div className="space-y-3">
      <div className="flex items-center gap-3 flex-wrap">
        <h2 className="font-medium">Autopilot</h2>
        {d && (
          <span className={clsx("rounded px-1.5 py-0.5 text-[11px]", !d.enabled ? "bg-white/5 text-gray-400" : d.paused ? "bg-amber/15 text-amber" : "bg-emerald/15 text-emerald-light")}>
            {!d.enabled ? "disabled (LEARNING_AUTOPILOT=off)" : d.paused ? "paused" : "running"}
          </span>
        )}
        {tickJob && <span className="text-xs text-gray-500">last cycle {ago(tickJob.last_run)}{tickJob.ok ? "" : " · with errors"}</span>}
        <div className="ml-auto flex items-center gap-2">
          {d?.enabled && <Btn disabled={pause.isPending} onClick={() => pause.mutate(!d.paused)}>{d.paused ? "Resume" : "Pause"}</Btn>}
          <Btn disabled={tick.isPending || !d?.enabled} onClick={() => tick.mutate()}>{tick.isPending ? "Running…" : "Run a cycle now"}</Btn>
        </div>
      </div>
      {q.error && <Msg tone="error">{(q.error as Error).message}</Msg>}
      {tick.data && <div className="text-xs text-gray-400">Cycle: froze {tick.data.frozen} predictions, scored {tick.data.forecasts_scored} forecasts{tick.data.errors.length ? `, ${tick.data.errors.length} errors (see the jobs below)` : ""}.</div>}
      {d && (
        <div className="grid gap-3 lg:grid-cols-2">
          <Card className="space-y-1 text-xs text-gray-400">
            {Object.entries(d.schedule).filter(([k]) => k !== "tick_minutes").map(([k, v]) => (
              <div key={k}><span className="text-gray-300 capitalize">{k}:</span> {String(v)}</div>
            ))}
            <div className="text-gray-500">Every {d.schedule.tick_minutes} minutes. Every action is in the audit trail below.</div>
          </Card>
          <Card className="space-y-2">
            <div className="text-sm font-medium">Shadows on live trial</div>
            {!d.shadows.length && <div className="text-xs text-gray-500">None. A tuned change becomes a shadow after passing its search and holdout seasons; it is then frozen next to the champion on live games.</div>}
            {d.shadows.map((sh) => (
              <div key={sh.engine + sh.version} className="text-xs space-y-0.5 border-t border-glass pt-1">
                <div><span className="text-gray-300">{engineLabel(sh.engine)}</span> · <span className="font-mono">{sh.version}</span> · {sh.role}</div>
                {sh.notes && <div className="text-gray-500">{sh.notes}</div>}
                <div className="text-gray-400">{sh.live_games} / {sh.needed} paired live games vs {sh.champion}
                  {sh.shadow_better.length > 0 && <span className="text-emerald-light"> · better: {sh.shadow_better.join(", ")}</span>}
                  {sh.shadow_worse.length > 0 && <span className="text-crimson-light"> · worse: {sh.shadow_worse.join(", ")}</span>}
                </div>
              </div>
            ))}
          </Card>
        </div>
      )}
      {d && d.jobs.length > 0 && (
        <Table head={<tr><Th>Job</Th><Th>Last run</Th><Th>Status</Th><Th>Result</Th></tr>}>
          {d.jobs.map((j) => (
            <tr key={j.job} className="border-t border-glass align-top">
              <Td className="font-mono text-xs">{j.job}</Td>
              <Td className="text-xs text-gray-400 whitespace-nowrap">{ago(j.last_run)}</Td>
              <Td><span className={clsx("rounded px-1.5 py-0.5 text-[11px]", j.ok ? "bg-emerald/15 text-emerald-light" : "bg-crimson/15 text-crimson-light")}>{j.ok ? "ok" : "error"}</span></Td>
              <Td className="text-[11px] text-gray-500 font-mono max-w-xl truncate">{jobSummary(j.detail)}</Td>
            </tr>
          ))}
        </Table>
      )}
    </div>
  );
}

function jobSummary(d: Record<string, unknown>): string {
  if ("outcome" in d) return [d.outcome, d.change].filter(Boolean).join(" · ");
  if ("frozen" in d) return `froze ${d.frozen}, scored ${d.forecasts_scored}${Array.isArray(d.errors) && d.errors.length ? `, ${d.errors.length} errors` : ""}`;
  if ("url" in d) return `${d.ok ? "ok" : "failed"} · ${String(d.url).replace(/^https?:\/\/[^/]+/, "")}`;
  return JSON.stringify(d).slice(0, 160);
}

function VerdictTag({ m }: { m: MarketScore }) {
  const v = verdict(m.brier_diff, m.brier_diff_se);
  return (
    <span className={clsx("rounded px-1.5 py-0.5 text-[11px]",
      v === "beats" && "bg-emerald/15 text-emerald-light", v === "worse" && "bg-crimson/15 text-crimson-light", v === "even" && "bg-white/5 text-gray-400")}>
      {v === "beats" ? "beats baseline" : v === "worse" ? "worse than baseline" : "no clear difference"}
    </span>
  );
}

function MarketTable({ markets }: { markets: Record<string, MarketScore> }) {
  const rows = Object.entries(markets);
  if (!rows.length) return <div className="text-xs text-gray-500">No scored forecasts yet.</div>;
  return (
    <Table head={<tr><Th>Market</Th><Th>n</Th><Th>Brier model</Th><Th>Brier baseline</Th><Th>Diff ± SE</Th><Th>Verdict</Th><Th>Baseline</Th></tr>}>
      {rows.map(([k, m]) => (
        <tr key={k} className="border-t border-glass">
          <Td className="font-mono text-xs">{k}</Td>
          <Td className="font-mono text-xs">{m.n.toLocaleString()}</Td>
          <Td className="font-mono text-xs">{m.brier_model.toFixed(4)}</Td>
          <Td className="font-mono text-xs">{m.brier_baseline.toFixed(4)}</Td>
          <Td className="font-mono text-xs">{m.brier_diff >= 0 ? "+" : ""}{m.brier_diff.toFixed(4)} ± {m.brier_diff_se.toFixed(4)}</Td>
          <Td><VerdictTag m={m} /></Td>
          <Td className="text-xs text-gray-500">{m.baseline}</Td>
        </tr>
      ))}
    </Table>
  );
}

function Registry({ sport }: { sport?: string }) {
  const qc = useQueryClient();
  const q = useQuery({ queryKey: ["learning-registry", sport], queryFn: () => learningApi.registry(sport) });
  const [err, setErr] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);
  const refresh = () => { qc.invalidateQueries({ queryKey: ["learning-registry"] }); qc.invalidateQueries({ queryKey: ["learning-audit"] }); };
  const act = useMutation({
    mutationFn: async (a: { kind: "evaluate" | "promote" | "rollback"; row: RegistryRow }) => {
      if (a.kind === "evaluate") return learningApi.evaluate(a.row.sport, a.row.version);
      // the dashboard only files a request; production changes need an admin approval (scripts/ops/governance.py)
      if (a.kind === "promote") return learningApi.requestPromotion(a.row.sport, a.row.version);
      return learningApi.requestRollback(a.row.sport);
    },
    onSuccess: (v) => {
      setErr(null);
      if (v && typeof v === "object" && "request_id" in v) setNote(`Promotion request #${v.request_id} is ${v.status ?? "open"}: approve it with scripts/ops/governance.py (admin token required).`);
      refresh();
    },
    onError: (e) => { setErr((e as Error).message); refresh(); },
  });
  const rows = q.data?.registry ?? [];
  const hasRetired = (s: string) => rows.some((r) => r.sport === s && (r.status === "retired" || r.status === "shadow") && r.promoted_at);
  return (
    <div className="space-y-3">
      <h2 className="font-medium">Model registry</h2>
      {q.isLoading && <Msg>Loading registry…</Msg>}
      {q.error && <Msg tone="error">{(q.error as Error).message}</Msg>}
      {err && <Msg tone="error">{err}</Msg>}
      {note && <Msg>{note}</Msg>}
      {act.isPending && <Msg>{act.variables?.kind === "evaluate" ? "Running a paired walk-forward evaluation (champion vs candidate on the same games and seeds)… this takes several minutes." : "Working…"}</Msg>}
      {rows.length > 0 && (
        <Table head={<tr><Th>Sport</Th><Th>Version</Th><Th>Status</Th><Th>Config</Th><Th>Evidence</Th><Th>Actions</Th></tr>}>
          {rows.map((r) => (
            <tr key={r.sport + r.version} className="border-t border-glass align-top">
              <Td className="text-xs whitespace-nowrap">{engineLabel(r.sport)}</Td>
              <Td className="font-mono text-xs">{r.version}{r.engine !== r.current_engine && <div className="text-amber text-[11px]">engine now {r.current_engine}</div>}</Td>
              <Td><span className={clsx("rounded px-1.5 py-0.5 text-[11px]",
                r.status === "champion" ? "bg-violet/20 text-violet-light" : r.status === "candidate" ? "bg-amber/15 text-amber" : r.status === "shadow" ? "bg-emerald/15 text-emerald-light" : r.status === "rejected" ? "bg-crimson/15 text-crimson-light" : "bg-white/5 text-gray-400")}>{r.status}</span></Td>
              <Td className="font-mono text-[11px] text-gray-400">{Object.keys(r.config).length ? Object.entries(r.config).map(([k, v]) => `${k}=${v}`).join(", ") : "engine defaults"}</Td>
              <Td className="text-[11px] text-gray-400 max-w-md">
                {r.evidence ? <EvidenceView ev={r.evidence} notes={r.notes} />
                  : r.status === "champion" ? r.notes ?? "" : r.notes ?? "not evaluated"}
              </Td>
              <Td className="whitespace-nowrap space-x-1">
                {r.status === "candidate" && <Btn disabled={act.isPending} onClick={() => act.mutate({ kind: "evaluate", row: r })}>Evaluate</Btn>}
                {(r.status === "candidate" || r.status === "shadow") && !r.promoted_at && r.evidence?.gate.pass && <Btn disabled={act.isPending} onClick={() => act.mutate({ kind: "promote", row: r })}>Promote</Btn>}
                {r.status === "champion" && hasRetired(r.sport) && <Btn disabled={act.isPending} onClick={() => act.mutate({ kind: "rollback", row: r })}>Roll back</Btn>}
              </Td>
            </tr>
          ))}
        </Table>
      )}
      {q.data && sport && <CandidateForm sport={sport} tunables={q.data.tunables} onDone={refresh} />}
      <div className="text-[11px] text-gray-500">
        Promotion gate: a candidate replaces the champion only if, on the same historical games and seeds, at least one market improves by ≥2 standard errors and none gets worse by ≥2 standard errors. Promotion without passing is refused. The autopilot adds two steps: the improvement must repeat on the latest season's games it was not tuned on, then hold up as a live shadow.
      </div>
    </div>
  );
}

function CandidateForm({ sport, tunables, onDone }: { sport: string; tunables: Tunables; onDone: () => void }) {
  const spec = tunables[sport] ?? {};
  const [vals, setVals] = useState<Record<string, string>>({});
  const [msg, setMsg] = useState<string | null>(null);
  const config = useMemo(() => {
    const c: Record<string, number> = {};
    for (const [k, v] of Object.entries(vals)) { const x = Number(v); if (v !== "" && Number.isFinite(x) && x !== spec[k]?.default) c[k] = x; }
    return c;
  }, [vals, spec]);
  const reg = useMutation({
    mutationFn: () => learningApi.register(sport, config, "registered from Model Intelligence"),
    onSuccess: (r) => { setMsg(`Registered ${r.version} as a candidate. Evaluate it to collect promotion evidence.`); setVals({}); onDone(); },
    onError: (e) => setMsg((e as Error).message),
  });
  return (
    <Card className="space-y-2">
      <div className="text-sm font-medium">New candidate ({sport.toUpperCase()})</div>
      <div className="grid gap-2 sm:grid-cols-2">
        {Object.entries(spec).map(([k, s]) => (
          <label key={k} className="text-xs text-gray-400 flex items-center gap-2">
            <Input type="number" value={vals[k] ?? ""} placeholder={String(s.default)} onChange={(v) => setVals((o) => ({ ...o, [k]: v }))} className="w-24" />
            <span><span className="font-mono text-gray-300">{k}</span> — {s.note} <span className="text-gray-600">[{s.min}–{s.max}]</span></span>
          </label>
        ))}
      </div>
      <div className="flex items-center gap-3">
        <Btn disabled={!Object.keys(config).length || reg.isPending} onClick={() => reg.mutate()}>Register candidate</Btn>
        {msg && <span className="text-xs text-gray-400">{msg}</span>}
      </div>
    </Card>
  );
}

function LiveRecord({ sport }: { sport?: string }) {
  const qc = useQueryClient();
  const q = useQuery({ queryKey: ["learning-summary", sport], queryFn: () => learningApi.summary(sport) });
  const [msg, setMsg] = useState<string | null>(null);
  const run = useMutation({
    mutationFn: async (kind: "freeze" | "score") => {
      const sports = sport ? ENGINES.filter((e) => e === sport || e === `${sport}_team`) : ENGINES;
      const out: string[] = [];
      for (const s of sports) {
        if (kind === "freeze") {
          const r = await learningApi.freeze(s);
          out.push(`${engineLabel(s)}: froze ${r.frozen.length}, skipped ${r.skipped.length}`);
        } else {
          const r = await learningApi.score(s);
          out.push(`${engineLabel(s)}: scored ${r.games_scored} games (${r.forecasts_scored} forecasts, ${r.forecasts_void} void), ${r.games_pending} pending`);
        }
      }
      return out.join(" · ");
    },
    onSuccess: (m) => { setMsg(m); qc.invalidateQueries({ queryKey: ["learning-summary"] }); qc.invalidateQueries({ queryKey: ["learning-ledger"] }); qc.invalidateQueries({ queryKey: ["learning-audit"] }); },
    onError: (e) => setMsg((e as Error).message),
  });
  const d = q.data;
  return (
    <div className="space-y-3">
      <div className="flex items-center gap-3 flex-wrap">
        <h2 className="font-medium">Live record — frozen before the game, scored after</h2>
        <div className="ml-auto flex items-center gap-2">
          <Btn disabled={run.isPending} onClick={() => run.mutate("freeze")}>Freeze today's games</Btn>
          <Btn disabled={run.isPending} onClick={() => run.mutate("score")}>Score finished games</Btn>
        </div>
      </div>
      {run.isPending && <Msg>Running 100,000-game predictions for each game that has not started…</Msg>}
      {msg && <div className="text-xs text-gray-400">{msg}</div>}
      {q.isLoading && <Msg>Loading…</Msg>}
      {q.error && <Msg tone="error">{(q.error as Error).message}</Msg>}
      {d && (
        <>
          <div className="flex gap-6 flex-wrap">
            {d.counts.map((c) => (
              <Card key={c.sport} className="flex gap-6">
                <Stat label={`${engineLabel(c.sport)} predictions`} value={c.predictions} />
                <Stat label="forecasts resolved" value={`${c.resolved.toLocaleString()} / ${c.forecasts.toLocaleString()}`} />
                <Stat label="void" value={c.void.toLocaleString()} />
              </Card>
            ))}
            {!d.counts.length && <div className="text-xs text-gray-500">No frozen predictions yet. Freeze a slate before the games start; they are scored once final.</div>}
          </div>
          {d.live.map((l) => (
            <div key={l.sport + l.model_version} className="space-y-1">
              <div className="text-xs text-gray-400">{engineLabel(l.sport)} · <span className="font-mono">{l.model_version}</span></div>
              <MarketTable markets={l.markets} />
            </div>
          ))}
          <div className="text-[11px] text-gray-500">{d.note} Live samples start small: until a market has a few hundred resolved forecasts, the standard error is too wide to call.</div>
        </>
      )}
    </div>
  );
}

function Backtests({ sport }: { sport?: string }) {
  const q = useQuery({ queryKey: ["learning-summary", sport], queryFn: () => learningApi.summary(sport) });
  const bts = q.data?.backtests ?? [];
  return (
    <div className="space-y-3">
      <h2 className="font-medium">Walk-forward backtests — data strictly before each game, previous season's league parameters</h2>
      {!bts.length && !q.isLoading && <div className="text-xs text-gray-500">No stored backtests.</div>}
      {bts.map((b) => (
        <div key={b.key} className="space-y-1">
          <div className="text-xs text-gray-400 font-mono">{b.key} · {b.games ?? "?"} games{b.lineup ? ` · ${b.lineup} lineups` : ""} · run {b.run_at.slice(0, 16)}</div>
          {b.markets ? <MarketTable markets={b.markets} /> : <div className="text-xs text-gray-500">This report uses its own format; see the sport page.</div>}
        </div>
      ))}
    </div>
  );
}

function Ledger({ sport }: { sport?: string }) {
  const q = useQuery({ queryKey: ["learning-ledger", sport], queryFn: () => learningApi.ledger(sport) });
  const [open, setOpen] = useState<number | null>(null);
  const rows = q.data?.ledger ?? [];
  return (
    <div className="space-y-3">
      <h2 className="font-medium">Prediction ledger</h2>
      {!rows.length && !q.isLoading && <div className="text-xs text-gray-500">Empty.</div>}
      {rows.length > 0 && (
        <Table head={<tr><Th>Game</Th><Th>Date</Th><Th>Frozen</Th><Th>Start</Th><Th>Model</Th><Th>Resolved</Th><Th>Brier (model / base)</Th><Th>SHA-256</Th></tr>}>
          {rows.map((r) => (
            <tr key={r.id} className={clsx("border-t border-glass cursor-pointer hover:bg-white/5", open === r.id && "bg-white/5")} onClick={() => setOpen(open === r.id ? null : r.id)}>
              <Td className="text-xs"><span className="uppercase text-gray-500 mr-1">{r.sport}</span>{r.away} @ {r.home}</Td>
              <Td className="font-mono text-xs">{r.game_date}</Td>
              <Td className="font-mono text-[11px] text-gray-400">{r.frozen_at.slice(0, 19)}</Td>
              <Td className="font-mono text-[11px] text-gray-400">{r.game_start?.slice(0, 16) ?? "—"}</Td>
              <Td className="font-mono text-[11px]">{r.model_version}</Td>
              <Td className="font-mono text-xs">{r.resolved}/{r.forecasts}</Td>
              <Td className="font-mono text-xs">{r.brier_model != null ? `${r.brier_model.toFixed(3)} / ${r.brier_baseline?.toFixed(3)}` : "—"}</Td>
              <Td className="font-mono text-[11px] text-gray-500">{r.sha256.slice(0, 12)}</Td>
            </tr>
          ))}
        </Table>
      )}
      {open != null && <LedgerDetail id={open} />}
    </div>
  );
}

function LedgerDetail({ id }: { id: number }) {
  const q = useQuery({ queryKey: ["learning-entry", id], queryFn: () => learningApi.entry(id) });
  const [market, setMarket] = useState("all");
  if (q.isLoading) return <Msg>Loading…</Msg>;
  if (q.error) return <Msg tone="error">{(q.error as Error).message}</Msg>;
  const d = q.data!;
  const markets = ["all", ...Array.from(new Set(d.forecasts.map((f) => f.market)))];
  const fs = d.forecasts.filter((f) => market === "all" || f.market === market);
  return (
    <Card className="space-y-2">
      <div className="flex gap-6 flex-wrap text-xs text-gray-400">
        <span>ledger #{d.id}</span><span>seed {d.seed}</span><span>{d.n_sims.toLocaleString()} simulated games</span><span>data cutoff {d.data_cutoff}</span>
        <span>{d.lineup} lineups</span><span>engine {d.engine}</span>
        <span className={d.hash_verified ? "text-emerald-light" : "text-crimson-light"}>{d.hash_verified ? "payload hash verified" : "payload hash MISMATCH"}</span>
        {d.payload.win && <span>home win {formatPct(d.payload.win.home, 1)}</span>}
      </div>
      <Pills options={markets.map((m) => ({ key: m, label: m }))} value={market} onChange={setMarket} />
      <Table head={<tr><Th>Market</Th><Th>Subject</Th><Th>Line</Th><Th>P(model)</Th><Th>P(baseline)</Th><Th>Actual</Th><Th>Result</Th></tr>}>
        {fs.map((f) => (
          <tr key={f.market + f.subject} className="border-t border-glass">
            <Td className="font-mono text-[11px]">{f.market}</Td>
            <Td className="text-xs">{f.name}</Td>
            <Td className="font-mono text-xs">{f.line}</Td>
            <Td className="font-mono text-xs">{formatPct(f.p_model, 1)}</Td>
            <Td className="font-mono text-xs">{formatPct(f.p_base, 1)}</Td>
            <Td className="font-mono text-xs">{f.actual ?? "—"}</Td>
            <Td className="text-xs">{f.y == null ? <span className="text-gray-500">{f.void_reason ?? "pending"}</span> : f.y ? "over" : "under"}</Td>
          </tr>
        ))}
      </Table>
    </Card>
  );
}

function Audit({ sport }: { sport?: string }) {
  const q = useQuery({ queryKey: ["learning-audit", sport], queryFn: () => learningApi.audit(sport) });
  const rows = q.data?.audit ?? [];
  return (
    <div className="space-y-3">
      <h2 className="font-medium">Audit trail</h2>
      {!rows.length && !q.isLoading && <div className="text-xs text-gray-500">No events yet.</div>}
      {rows.length > 0 && (
        <Table head={<tr><Th>When</Th><Th>Sport</Th><Th>Action</Th><Th>Version</Th><Th>Detail</Th></tr>}>
          {rows.map((r) => (
            <tr key={r.id} className="border-t border-glass">
              <Td className="font-mono text-[11px] text-gray-400">{r.at.slice(0, 19)}</Td>
              <Td className="text-xs whitespace-nowrap">{engineLabel(r.sport)}</Td>
              <Td className="text-xs">{r.action}</Td>
              <Td className="font-mono text-[11px]">{r.version ?? "—"}</Td>
              <Td className="font-mono text-[11px] text-gray-500 max-w-xl truncate">{JSON.stringify(r.detail)}</Td>
            </tr>
          ))}
        </Table>
      )}
    </div>
  );
}
