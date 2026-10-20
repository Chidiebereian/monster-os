import clsx from "clsx";
import type { LineupEntry, NflEnsemble, Scenario, SimTeamInputs } from "../../lib/sim";
import { teamColor } from "../../lib/sim";
import { formatPct } from "../../lib/odds";

// Confirmed lineups + alternate realities. The lineup (who is active, injury
// designations, probabilities to play) comes from the backend; this panel only
// lets the user override it and asks the backend to re-simulate.

type Choice = "auto" | "in" | "out";

function choiceOf(s: Scenario, id: string): Choice {
  return s.out.includes(id) ? "out" : s.in.includes(id) ? "in" : "auto";
}
function setChoice(s: Scenario, id: string, c: Choice): Scenario {
  const out = s.out.filter((x) => x !== id);
  const inn = s.in.filter((x) => x !== id);
  if (c === "out") out.push(id);
  if (c === "in") inn.push(id);
  return { ...s, out, in: inn };
}

function Badge({ e }: { e: LineupEntry }) {
  if (e.report === "Out") return <span className="rounded bg-crimson/25 px-1.5 text-[10px] text-crimson-light">OUT</span>;
  if (e.report === "Doubtful") return <span className="rounded bg-crimson/15 px-1.5 text-[10px] text-crimson-light">D</span>;
  if (e.report === "Questionable") return <span className="rounded bg-amber/20 px-1.5 text-[10px] text-amber">Q</span>;
  if (e.p_active === 0) return <span className="rounded bg-white/10 px-1.5 text-[10px] text-gray-400">INACTIVE</span>;
  return null;
}

function TeamLineup({ t, side, draft, setDraft }: {
  t: SimTeamInputs; side: "away" | "home"; draft: Scenario; setDraft: (s: Scenario) => void;
}) {
  const qbs = t.lineup.filter((e) => e.role !== "skill");
  const qbKey = side === "away" ? "away_qb" : "home_qb";
  const chosenQb = draft[qbKey] ?? "";
  const skill = t.lineup.filter((e) => e.role === "skill" && e.pos !== "QB");
  return (
    <div className="space-y-2 min-w-0">
      <div className="flex items-center gap-2">
        <span className="h-3 w-3 rounded-sm" style={{ background: teamColor(t.abbr) }} />
        <span className="font-semibold">{t.abbr}</span>
        <span className="truncate text-[11px] text-gray-500">{t.lineup_source}</span>
      </div>
      <label className="flex items-center gap-2 text-[12px] text-gray-400">
        Starting QB
        <select value={chosenQb} onChange={(ev) => setDraft({ ...draft, [qbKey]: ev.target.value || undefined })}
          className="min-w-0 flex-1 rounded border border-glass bg-surface px-2 py-1 text-[12px] text-gray-100">
          <option value="">Auto — {t.offense.qb}{t.offense.qb_p_active < 1 ? ` (plays ${formatPct(t.offense.qb_p_active, 0)})` : ""}</option>
          {qbs.filter((q) => q.id !== t.offense.qb_id).map((q) => (
            <option key={q.id} value={q.id}>{q.name}{q.report ? ` (${q.report})` : q.p_active === 0 ? ` (${q.status})` : ""}</option>
          ))}
        </select>
      </label>
      <div className="max-h-[360px] overflow-y-auto rounded-md border border-glass">
        <table className="w-full text-[12px]">
          <thead className="sticky top-0 bg-surface text-[10px] uppercase tracking-wide text-gray-500">
            <tr><th className="px-2 py-1 text-left">Player</th><th className="px-1 text-right">Tgt</th><th className="px-1 text-right">Car</th><th className="px-1 text-right">Plays</th><th className="px-2 text-right">Reality</th></tr>
          </thead>
          <tbody>
            {skill.map((e) => {
              const c = choiceOf(draft, e.id);
              const eff = c === "out" ? 0 : c === "in" ? 1 : e.p_active;
              return (
                <tr key={e.id} className={clsx("border-t border-white/5", eff === 0 && "opacity-50")}>
                  <td className="px-2 py-1">
                    <div className="flex items-center gap-1.5 min-w-0">
                      <span className="truncate text-gray-200" title={`${e.status}${e.injury ? ` · ${e.injury}` : ""}${e.practice ? ` · ${e.practice}` : ""}`}>{e.name}</span>
                      <span className="text-[10px] text-gray-500">{e.pos}</span>
                      <Badge e={e} />
                    </div>
                  </td>
                  <td className="px-1 text-right font-mono text-gray-400">{(e.target_share * 100).toFixed(0)}%</td>
                  <td className="px-1 text-right font-mono text-gray-400">{(e.carry_share * 100).toFixed(0)}%</td>
                  <td className={clsx("px-1 text-right font-mono", eff === 0 ? "text-crimson-light" : eff < 1 ? "text-amber" : "text-gray-300")}>{formatPct(eff, 0)}</td>
                  <td className="px-2 py-1">
                    <div className="ml-auto flex w-fit overflow-hidden rounded border border-glass">
                      {(["auto", "in", "out"] as Choice[]).map((k) => (
                        <button key={k} onClick={() => setDraft(setChoice(draft, e.id, k))}
                          className={clsx("px-1.5 py-0.5 text-[10px]", c === k ? (k === "out" ? "bg-crimson/30 text-crimson-light" : k === "in" ? "bg-emerald/25 text-emerald-light" : "bg-white/10 text-gray-200") : "text-gray-500 hover:bg-white/5")}>
                          {k === "auto" ? "Auto" : k === "in" ? "Plays" : "Out"}
                        </button>
                      ))}
                    </div>
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
    </div>
  );
}

export function LineupPanel({ teams, draft, setDraft, applied, onRun, onReset, running, rates }: {
  teams: SimTeamInputs[]; draft: Scenario; setDraft: (s: Scenario) => void; applied: Scenario;
  onRun: () => void; onReset: () => void; running: boolean; rates?: { questionable: number; doubtful: number; source: string };
}) {
  const dirty = JSON.stringify(draft) !== JSON.stringify(applied);
  const n = draft.out.length + draft.in.length + (draft.away_qb ? 1 : 0) + (draft.home_qb ? 1 : 0);
  return (
    <div className="space-y-3">
      <div className="text-[12px] text-gray-400">
        Confirmed lineup from this week's roster and the official injury report. Questionable players play with probability{" "}
        {rates ? formatPct(rates.questionable, 0) : "—"} and Doubtful {rates ? formatPct(rates.doubtful, 0) : "—"} (fitted from earlier
        seasons' reports), so some of the 100,000 simulated games have them inactive. Override anyone to create an alternate reality.
      </div>
      <div className="grid gap-4 md:grid-cols-2 [&>*]:min-w-0">
        {teams.map((t, i) => <TeamLineup key={t.abbr} t={t} side={i === 0 ? "away" : "home"} draft={draft} setDraft={setDraft} />)}
      </div>
      <div className="flex flex-wrap items-center gap-2">
        <button onClick={onRun} disabled={running || !dirty}
          className="rounded-md border border-amber/40 bg-amber/20 px-3 py-1.5 text-sm text-amber disabled:opacity-40">
          {running ? "Simulating 100,000 games…" : `Run this reality${n ? ` (${n} change${n > 1 ? "s" : ""})` : ""}`}
        </button>
        <button onClick={onReset} disabled={running || (n === 0 && !dirty)}
          className="rounded-md border border-glass px-3 py-1.5 text-sm text-gray-300 disabled:opacity-40">Back to confirmed lineup</button>
        {!dirty && n > 0 && <span className="text-[12px] text-emerald-light">This reality is running in the broadcast and prediction.</span>}
      </div>
    </div>
  );
}

/** Scenario vs confirmed-lineup comparison, both from 100k-game ensembles. */
export function ScenarioImpact({ base, alt, teams }: { base?: NflEnsemble; alt?: NflEnsemble; teams: [string, string] }) {
  if (!base || !alt) return null;
  const pct = (x: number) => `${x >= 0 ? "+" : ""}${(x * 100).toFixed(1)} pts`;
  const rows: { name: string; stat: string; a: number; b: number }[] = [];
  const all = (e: NflEnsemble) => [...e.players.away, ...e.players.home];
  const bmap = new Map(all(base).map((p) => [p.id, p]));
  for (const p of all(alt)) {
    const b = bmap.get(p.id);
    for (const stat of ["pass_yds", "rush_yds", "rec_yds", "receptions"]) {
      const sa = p.stats[stat];
      const sb = b?.stats[stat];
      if (sa && (sb || !b)) rows.push({ name: p.name, stat, a: sa.mean, b: sb?.mean ?? 0 });
    }
  }
  const moved = rows.filter((r) => Math.abs(r.a - r.b) >= 1).sort((x, y) => Math.abs(y.a - y.b) - Math.abs(x.a - x.b)).slice(0, 8);
  const label: Record<string, string> = { pass_yds: "pass yds", rush_yds: "rush yds", rec_yds: "rec yds", receptions: "receptions" };
  return (
    <div className="space-y-2 rounded-md border border-amber/30 bg-amber/5 p-3 text-[13px]">
      <div className="font-semibold text-amber">This reality vs the confirmed lineup (100,000 games each)</div>
      <div className="grid grid-cols-3 gap-2 font-mono text-[12px]">
        <div>{teams[1]} win {formatPct(alt.win.home)} <span className="text-gray-500">({pct(alt.win.home - base.win.home)})</span></div>
        <div>Total {alt.total.mean.toFixed(1)} <span className="text-gray-500">({(alt.total.mean - base.total.mean >= 0 ? "+" : "") + (alt.total.mean - base.total.mean).toFixed(1)})</span></div>
        <div>{teams[1]} margin {alt.margin_home.mean.toFixed(1)} <span className="text-gray-500">({(alt.margin_home.mean - base.margin_home.mean >= 0 ? "+" : "") + (alt.margin_home.mean - base.margin_home.mean).toFixed(1)})</span></div>
      </div>
      {moved.length > 0 && (
        <div className="space-y-0.5">
          {moved.map((r) => (
            <div key={`${r.name}${r.stat}`} className="flex justify-between gap-2 text-[12px]">
              <span className="truncate text-gray-300">{r.name} {label[r.stat]}</span>
              <span className="font-mono text-gray-400">{r.b.toFixed(1)} → <span className={r.a > r.b ? "text-emerald-light" : "text-crimson-light"}>{r.a.toFixed(1)}</span></span>
            </div>
          ))}
        </div>
      )}
      <div className="text-[11px] text-gray-500">Player means are conditional on the player being active.</div>
    </div>
  );
}
