import { formatPct } from "../../lib/odds";
import type { SegmentBlock } from "../../lib/api";

type Summary = { mean: number; q10?: number; q90?: number; p10?: number; p90?: number };

/** First segment (1st half; NHL 1st period; MLB first 5 innings) and team totals
 *  from a simulator ensemble: `h1` and `score` in the payload (segments.rs). */
export function SegmentPanel({ teams, h1, score }: { teams: [string, string]; h1?: SegmentBlock | null; score?: { away?: Summary; home?: Summary } | null }) {
  const range = (s?: Summary) => {
    if (!s) return "";
    const lo = s.q10 ?? s.p10;
    const hi = s.q90 ?? s.p90;
    return lo != null && hi != null ? ` (80%: ${lo}–${hi})` : "";
  };
  if (!h1 && !score) return null;
  return (
    <div className="grid [&>*]:min-w-0 grid-cols-2 gap-3 text-[13px]">
      {h1 && h1.n > 0 && (
        <div className="rounded-md border border-glass p-2">
          <div className="text-gray-500 text-[11px] capitalize">{h1.label} ({h1.tag})</div>
          <div className="font-mono">{teams[0]} {h1.away.mean.toFixed(1)} – {teams[1]} {h1.home.mean.toFixed(1)}</div>
          <div className="text-gray-400">
            {teams[1]} leads {formatPct(h1.win.home)} · {teams[0]} {formatPct(h1.win.away)} · tied {formatPct(h1.win.tie)}
          </div>
          <div className="text-gray-400">total {h1.total.mean.toFixed(1)}{range(h1.total)}</div>
        </div>
      )}
      {score && (score.away || score.home) && (
        <div className="rounded-md border border-glass p-2">
          <div className="text-gray-500 text-[11px]">Team totals</div>
          {score.away && <div className="font-mono">{teams[0]} {score.away.mean.toFixed(1)}<span className="text-gray-500">{range(score.away)}</span></div>}
          {score.home && <div className="font-mono">{teams[1]} {score.home.mean.toFixed(1)}<span className="text-gray-500">{range(score.home)}</span></div>}
        </div>
      )}
    </div>
  );
}
