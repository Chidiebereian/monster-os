import clsx from "clsx";
import { formatPct } from "../../lib/odds";

// Projected vs confirmed vs actual starters, and which one the engine plays.
// All three lists come from the backend; this only lays them side by side.

export type LineupMode = "projected" | "confirmed" | "actual";
export interface StarterPerson { id: string; name: string; pos?: string; p_active?: number }
export interface StarterRow { label: string; projected?: StarterPerson | null; confirmed?: StarterPerson | null; actual?: StarterPerson | null }
export interface StarterTeam { abbr: string; rows: StarterRow[] }

const MODES: { key: LineupMode; label: string; help: string }[] = [
  { key: "projected", label: "Projected", help: "recent usage / previous lineup only" },
  { key: "confirmed", label: "Confirmed", help: "official roster, injury report, posted lineup" },
  { key: "actual", label: "Actual", help: "who really played (after the game)" },
];

function Cell({ p, base, highlight }: { p?: StarterPerson | null; base?: StarterPerson | null; highlight: boolean }) {
  if (p === undefined) return <td className="px-2 py-1 text-gray-600">—</td>;
  if (p === null) return <td className="px-2 py-1 text-gray-600">—</td>;
  const changed = base && base.id !== p.id;
  return (
    <td className={clsx("px-2 py-1", highlight && "bg-amber/10")}>
      <span className={clsx("truncate", changed ? "text-amber" : "text-gray-200")}>{p.name}</span>
      {p.p_active !== undefined && p.p_active < 1 && <span className="ml-1 text-[10px] text-amber">plays {formatPct(p.p_active, 0)}</span>}
    </td>
  );
}

export function StartersCompare({ teams, mode, onMode, available, busy }: {
  teams: StarterTeam[]; mode: LineupMode; onMode: (m: LineupMode) => void;
  available: Record<LineupMode, boolean>; busy: boolean;
}) {
  return (
    <div className="space-y-3">
      <div className="flex flex-wrap items-center gap-2 text-[12px]">
        <span className="text-gray-400">Engine plays:</span>
        <div className="flex overflow-hidden rounded-md border border-glass">
          {MODES.map((m) => (
            <button key={m.key} disabled={!available[m.key] || busy} onClick={() => onMode(m.key)} title={available[m.key] ? m.help : "not available for this game yet"}
              className={clsx("px-3 py-1", mode === m.key ? "bg-amber/20 text-amber" : "text-gray-400 hover:bg-white/5", (!available[m.key] || busy) && "opacity-40 cursor-not-allowed")}>
              {m.label}
            </button>
          ))}
        </div>
        <span className="text-gray-500">{MODES.find((m) => m.key === mode)?.help}{busy ? " · re-simulating…" : ""}</span>
      </div>
      <div className="grid gap-4 md:grid-cols-2 [&>*]:min-w-0">
        {teams.map((t) => (
          <div key={t.abbr} className="overflow-x-auto rounded-md border border-glass">
            <table className="w-full text-[12px]">
              <thead className="bg-surface text-[10px] uppercase tracking-wide text-gray-500">
                <tr><th className="px-2 py-1 text-left">{t.abbr}</th>
                  {MODES.map((m) => <th key={m.key} className={clsx("px-2 py-1 text-left", mode === m.key && "text-amber")}>{m.label}</th>)}</tr>
              </thead>
              <tbody>
                {t.rows.map((r) => (
                  <tr key={r.label} className="border-t border-white/5">
                    <td className="px-2 py-1 text-gray-500">{r.label}</td>
                    <Cell p={r.projected} highlight={mode === "projected"} />
                    <Cell p={r.confirmed} base={r.projected} highlight={mode === "confirmed"} />
                    <Cell p={r.actual} base={r.confirmed ?? r.projected} highlight={mode === "actual"} />
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        ))}
      </div>
      <div className="text-[11px] text-gray-500">Amber names differ from the column to their left. "Actual" uses the game's own data, so it is hindsight: use it to see what the engine would have said with the real lineup.</div>
    </div>
  );
}
