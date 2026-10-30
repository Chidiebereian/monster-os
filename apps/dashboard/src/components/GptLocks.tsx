// "MonsterGPT Locks": today's sharpest open AI picks across sports as a
// paged carousel on the MonsterGPT home (grade, model vs market probability,
// edge, the pick analysis and a book link).
import { useState } from "react";
import { Link } from "react-router-dom";
import { useQuery } from "@tanstack/react-query";
import clsx from "clsx";
import { Bot, ChevronLeft, ChevronRight, ExternalLink, Loader2, Share2 } from "lucide-react";
import { picksApi, splitWriteup, type Pick } from "../lib/picks";
import { bookName, fmtOdds, usableLink } from "../lib/hub";

const PER_PAGE = 3;

export function GptLocks({ onAsk }: { onAsk: (q: string) => void }) {
  const [page, setPage] = useState(0);
  const [copied, setCopied] = useState<number | null>(null);
  const q = useQuery({
    queryKey: ["gpt-locks"],
    queryFn: () => picksApi.list({ status: "open", sort: "edge", limit: 30 }),
    refetchInterval: 5 * 60_000,
  });
  // proven/baseline first, then grade, then edge; one pick per subject
  const seen = new Set<string>();
  const picks = (q.data?.picks ?? [])
    .filter((p) => p.status === "open" && (p.tier === "proven" || p.tier === "baseline"))
    .sort((a, b) => (a.tier === b.tier ? 0 : a.tier === "proven" ? -1 : 1) || a.grade.localeCompare(b.grade) || b.edge - a.edge)
    .filter((p) => { const k = `${p.game_id}:${p.subject}`; if (seen.has(k)) return false; seen.add(k); return true; })
    .slice(0, 30);
  const pages = Math.max(1, Math.ceil(picks.length / PER_PAGE));
  const view = picks.slice(page * PER_PAGE, page * PER_PAGE + PER_PAGE);

  const share = async (p: Pick) => {
    const text = `MonsterGPT lock (${p.grade}): ${p.label} ${fmtOdds(p.price)} at ${p.book_title} - ${p.game_label}. Model ${(p.p_model * 100).toFixed(1)}% vs market ${(p.p_market * 100).toFixed(1)}% (edge ${(p.edge * 100).toFixed(1)}%).`;
    try { await navigator.clipboard.writeText(text); setCopied(p.id); setTimeout(() => setCopied(null), 1500); } catch { /* clipboard blocked */ }
  };

  return (
    <div className="rounded-xl border border-violet/25 bg-gradient-to-b from-violet/[0.08] to-transparent p-4 space-y-3">
      <div className="flex items-center gap-3">
        <div className="h-9 w-9 rounded-lg bg-violet/20 flex items-center justify-center"><Bot size={18} className="text-violet-light" /></div>
        <div><div className="font-semibold">MonsterGPT Locks</div><div className="text-xs text-gray-500">Today's sharpest plays</div></div>
        <div className="ml-auto flex items-center gap-2">
          <Link to="/picks" className="px-3 py-1.5 rounded-md bg-white/5 text-xs text-gray-200 hover:bg-white/10">All Picks</Link>
          <Link to="/projections" className="px-3 py-1.5 rounded-md bg-white/5 text-xs text-gray-200 hover:bg-white/10">All Projections</Link>
          <button disabled={page === 0} onClick={() => setPage(page - 1)} className="p-1.5 rounded-md border border-glass text-gray-400 disabled:opacity-30"><ChevronLeft size={14} /></button>
          <span className="text-xs text-gray-400 font-mono">{picks.length ? page + 1 : 0}/{picks.length ? pages : 0}</span>
          <button disabled={page >= pages - 1} onClick={() => setPage(page + 1)} className="p-1.5 rounded-md border border-glass text-gray-400 disabled:opacity-30"><ChevronRight size={14} /></button>
        </div>
      </div>
      {q.isLoading && <div className="text-sm text-gray-500 py-6 text-center"><Loader2 size={14} className="inline animate-spin mr-2" />Loading locks…</div>}
      {q.data && picks.length === 0 && (
        <div className="text-sm text-gray-500 py-6 text-center">No proven or baseline picks are open right now. Picks publish through the day as games get within ~40 hours; unproven ones stay in AI Picks.</div>
      )}
      <div className="grid md:grid-cols-3 gap-3">
        {view.map((p) => {
          const link = usableLink(p.link);
          const { case_for } = splitWriteup(p.writeup);
          return (
            <div key={p.id} className="rounded-lg border border-glass bg-surface/80 p-4 flex flex-col gap-3">
              <div className="flex items-center gap-2">
                {p.headshot ? <img src={p.headshot} className="w-9 h-9 rounded-full object-cover bg-white/5" alt="" /> : <div className="w-9 h-9 rounded-full bg-white/5 flex items-center justify-center text-[10px] text-gray-400">{p.sport.toUpperCase()}</div>}
                <span className="rounded bg-emerald/15 text-emerald-light text-[11px] px-1.5 py-0.5 font-medium">{p.grade}</span>
                <span className="rounded bg-white/5 text-gray-400 text-[10px] px-1.5 py-0.5 uppercase">{p.sport}</span>
                <span className="ml-auto font-mono text-sm text-violet-light">{fmtOdds(p.price)}</span>
              </div>
              <div>
                <div className="font-semibold truncate" title={p.kind === "prop" ? p.subject_name : p.game_label}>{p.kind === "prop" ? p.subject_name : p.game_label}</div>
                <div className="text-sm text-gray-200">{p.label}</div>
                <div className="text-[11px] text-gray-500 truncate">{p.game_label}{p.starts_at && ` · ${new Date(p.starts_at).toLocaleString(undefined, { month: "numeric", day: "numeric", hour: "numeric", minute: "2-digit" })}`}</div>
              </div>
              <div className="grid grid-cols-[1fr_auto] gap-2 text-xs border-t border-glass pt-2">
                <div><div className="text-gray-500">Model probability / Market probability</div>
                  <div className="font-mono text-gray-100">{(p.p_model * 100).toFixed(1)}% / {(p.p_market * 100).toFixed(1)}%</div></div>
                <div className="text-right"><div className="text-gray-500">Edge</div><div className="font-mono text-emerald-light">+{(p.edge * 100).toFixed(1)}%</div></div>
              </div>
              <div className="flex-1">
                <div className="text-[10px] uppercase tracking-wider text-violet-light font-medium">Pick analysis</div>
                <div className="text-xs text-gray-400 line-clamp-3">{case_for}</div>
              </div>
              <div className="flex gap-2">
                {link ? (
                  <a href={link} target="_blank" rel="noreferrer" className="flex-1 inline-flex items-center justify-center gap-1.5 rounded-md border border-violet/40 bg-violet/10 px-2 py-1.5 text-xs text-violet-light hover:bg-violet/20">
                    {bookName(p.book)} {fmtOdds(p.price)}<ExternalLink size={11} /></a>
                ) : <span className="flex-1 inline-flex items-center justify-center rounded-md border border-glass px-2 py-1.5 text-xs text-gray-300">{p.book_title} {fmtOdds(p.price)}</span>}
                <button onClick={() => onAsk(`Explain the ${p.label} pick for ${p.game_label}: why does the model like it and what could make it wrong?`)}
                  className="rounded-md border border-glass px-2 py-1.5 text-xs text-gray-300 hover:bg-white/5" title="Ask MonsterGPT"><Bot size={13} /></button>
                <button onClick={() => share(p)} className={clsx("rounded-md border border-glass px-2 py-1.5 text-xs hover:bg-white/5", copied === p.id ? "text-emerald-light" : "text-gray-300")} title="Copy to share">
                  <Share2 size={13} /></button>
              </div>
            </div>);
        })}
      </div>
    </div>
  );
}
