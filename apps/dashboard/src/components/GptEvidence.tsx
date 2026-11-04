import { forwardRef, useEffect, useId, useRef, useState, type KeyboardEvent, type ReactNode } from "react";
import { AlertTriangle, Check, ChevronDown, Cpu, Loader2, User, Users, X } from "lucide-react";
import clsx from "clsx";
import { searchEntities, type ClaimStatus, type EntityHit, type GptCitation, type GptClaim, type GptEntity, type GptJob } from "../lib/gpt";

// MonsterGPT v2 evidence: every sentence carries a verification status and citations
// (monster-gpt verify.rs). Verified sentences read as plain text with footnote numbers;
// everything else is marked so a reader can tell a record from a model estimate.

export const STATUS: Record<ClaimStatus, { label: string; mark: string; dot: string; plain?: boolean }> = {
  verified: { label: "Verified", mark: "", dot: "bg-emerald-light", plain: true },
  cross_verified: { label: "Cross-verified", mark: "", dot: "bg-emerald-light", plain: true },
  model_estimated: { label: "Model estimate", mark: "underline decoration-dashed decoration-violet-light/70 underline-offset-4", dot: "bg-violet-light" },
  hypothetical: { label: "Hypothetical", mark: "underline decoration-dashed decoration-sky-400/80 underline-offset-4", dot: "bg-sky-400" },
  stale: { label: "May be out of date", mark: "underline decoration-dotted decoration-amber/80 underline-offset-4", dot: "bg-amber" },
  partially_verified: { label: "Partly verified", mark: "underline decoration-wavy decoration-amber/80 underline-offset-4", dot: "bg-amber" },
  conflicting: { label: "Sources disagree", mark: "underline decoration-wavy decoration-crimson-light underline-offset-4", dot: "bg-crimson-light" },
  unverified: { label: "Not verified", mark: "underline decoration-wavy decoration-crimson-light underline-offset-4", dot: "bg-crimson-light" },
};

/** Footnote number per source, in order of first citation. */
export function footnotes(claims: GptClaim[]): { order: GptCitation[]; num: Map<string, number> } {
  const num = new Map<string, number>();
  const order: GptCitation[] = [];
  for (const c of claims) for (const ci of c.citations) {
    if (!num.has(ci.source)) { num.set(ci.source, order.length + 1); order.push(ci); }
  }
  return { order, num };
}

/** The answer with each claimed sentence marked by status and followed by its footnotes. */
export function ClaimedText({ text, claims, render, prefix }: { text: string; claims: GptClaim[]; render: (s: string) => ReactNode; prefix: string }) {
  const { num } = footnotes(claims);
  const out: ReactNode[] = [];
  let at = 0;
  [...claims].sort((a, b) => a.sentence_ix - b.sentence_ix).forEach((c, i) => {
    const idx = text.indexOf(c.text, at);
    if (idx < 0) return;
    if (idx > at) out.push(<span key={`p${i}`}>{render(text.slice(at, idx))}</span>);
    const st = STATUS[c.status] ?? STATUS.unverified;
    const notes = [...new Set(c.citations.map((x) => num.get(x.source)).filter((n): n is number => n != null))];
    const tip = `${st.label}${c.issues.length ? ` · ${c.issues.join("; ")}` : ""}`;
    out.push(
      <span key={`c${i}`} className={clsx(st.mark)} title={tip}>
        {render(c.text)}
        {!st.plain && <span className="sr-only"> ({st.label})</span>}
        {notes.map((n) => <sup key={n}><a href={`#fn-${prefix}-${n}`} className="ml-0.5 text-[10px] text-violet-light no-underline hover:underline" aria-label={`source ${n}`}>[{n}]</a></sup>)}
      </span>);
    at = idx + c.text.length;
  });
  if (at < text.length) out.push(<span key="tail">{render(text.slice(at))}</span>);
  return <>{out}</>;
}

/** Status counts, the issues behind any sentence that is not verified, and the numbered sources. */
export function ClaimPanel({ claims, idPrefix }: { claims: GptClaim[]; idPrefix: string }) {
  const [open, setOpen] = useState(false);
  const counts = new Map<ClaimStatus, number>();
  for (const c of claims) counts.set(c.status, (counts.get(c.status) ?? 0) + 1);
  const flagged = claims.filter((c) => !STATUS[c.status]?.plain);
  const { order } = footnotes(claims);
  if (claims.length === 0) return null;
  return (
    <div className="mt-3 border-t border-glass pt-2 text-[11px] text-gray-400 space-y-1.5">
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
        <span className="text-gray-500">Checked {claims.length} sentence{claims.length === 1 ? "" : "s"}:</span>
        {[...counts.entries()].map(([s, n]) => (
          <span key={s} className="inline-flex items-center gap-1"><span className={clsx("h-1.5 w-1.5 rounded-full", STATUS[s]?.dot)} aria-hidden />{STATUS[s]?.label ?? s} {n}</span>))}
        {(flagged.length > 0 || order.length > 0) && (
          <button onClick={() => setOpen(!open)} aria-expanded={open} className="inline-flex items-center gap-0.5 text-violet-light hover:underline">
            {open ? "Hide" : "Show"} sources and checks<ChevronDown size={11} className={clsx(open && "rotate-180")} /></button>)}
      </div>
      {open && (
        <>
          {flagged.length > 0 && (
            <ul className="space-y-0.5">
              {flagged.map((c) => (
                <li key={c.sentence_ix} className="flex gap-1.5"><span className={clsx("mt-1 h-1.5 w-1.5 shrink-0 rounded-full", STATUS[c.status]?.dot)} aria-hidden />
                  <span><span className="text-gray-300">{STATUS[c.status]?.label}:</span> “{c.text.length > 120 ? `${c.text.slice(0, 117)}…` : c.text}”
                    {c.issues.length > 0 && <span className="text-gray-500"> · {c.issues.join("; ")}</span>}</span></li>))}
            </ul>)}
          {order.length > 0 && (
            <ol className="space-y-0.5">
              {order.map((ci, i) => (
                <li key={ci.source} id={`fn-${idPrefix}-${i + 1}`} className="scroll-mt-24">
                  <span className="font-mono text-gray-500">[{i + 1}]</span> {ci.label}
                  <span className="text-gray-500"> · {ci.source_id}{ci.as_of ? ` · as of ${ci.as_of.slice(0, 10)}` : ""} · retrieved {new Date(ci.retrieved_at).toLocaleString(undefined, { month: "short", day: "numeric", hour: "numeric", minute: "2-digit" })}</span>
                  {ci.stale && <span className="text-amber"> · may be out of date</span>}
                </li>))}
            </ol>)}
        </>)}
    </div>
  );
}

/** The players and teams the question resolved to. */
export function EntityChips({ entities }: { entities: GptEntity[] }) {
  if (entities.length === 0) return null;
  return (
    <div className="flex flex-wrap items-center gap-1.5 text-[11px]" aria-label="Players and teams in this answer">
      {entities.map((e) => {
        const team = e.team_id?.split(":")[2];
        return (
          <span key={e.entity_id} className="inline-flex items-center gap-1 rounded-full border border-glass px-2 py-0.5 text-gray-300" title={e.entity_id}>
            {e.entity_id.startsWith("tm:") ? <Users size={11} className="text-gray-500" /> : <User size={11} className="text-gray-500" />}
            {e.name ?? e.mention ?? e.entity_id}{team ? <span className="text-gray-500">{team}</span> : null}
            {e.sport && <span className="uppercase text-gray-600">{e.sport}</span>}
          </span>);
      })}
    </div>
  );
}

/** A simulation job's progress. */
export function JobLine({ jobs }: { jobs: GptJob[] }) {
  const j = jobs[jobs.length - 1];
  if (!j) return null;
  const busy = j.state === "queued" || j.state === "running";
  return (
    <div className={clsx("inline-flex items-center gap-1.5 rounded-md px-2.5 py-1 text-xs", j.state === "failed" ? "bg-crimson/10 text-crimson-light" : "bg-white/[0.03] text-gray-400")} role="status" aria-live="polite">
      {busy ? <Loader2 size={12} className="animate-spin text-violet-light" /> : j.state === "done" ? <Check size={12} className="text-emerald-light" /> : j.state === "failed" ? <AlertTriangle size={12} /> : <X size={12} />}
      <Cpu size={12} className="text-gray-500" />
      Simulation {j.state === "queued" ? "queued" : j.state === "running" ? "running" : j.state}{j.detail ? ` · ${j.detail}` : ""}
    </div>
  );
}

export interface Picked { mention: string; entity_id: string }

/** The composer: typing "@" and a few letters suggests players and teams by canonical id. */
export const EntityInput = forwardRef<HTMLInputElement, {
  value: string; onChange: (v: string) => void; onPick: (p: Picked) => void; placeholder: string; disabled?: boolean;
}>(function EntityInput({ value, onChange, onPick, placeholder, disabled }, ref) {
  const [hits, setHits] = useState<EntityHit[]>([]);
  const [sel, setSel] = useState(0);
  const [query, setQuery] = useState<string | null>(null);
  /** where the "@query" being completed sits in the value: [start, end) */
  const [span, setSpan] = useState<[number, number] | null>(null);
  const caretTo = useRef<number | null>(null);
  const inner = useRef<HTMLInputElement | null>(null);
  // put the caret after an inserted name once React has rendered the new value
  useEffect(() => {
    if (caretTo.current != null && inner.current) { inner.current.setSelectionRange(caretTo.current, caretTo.current); caretTo.current = null; }
  }, [value]);
  const listId = useId();
  useEffect(() => {
    if (!query || query.length < 2) { setHits([]); return; }
    const ac = new AbortController();
    const t = window.setTimeout(() => { searchEntities(query, ac.signal).then((h) => { setHits(h); setSel(0); }).catch(() => undefined); }, 180);
    return () => { window.clearTimeout(t); ac.abort(); };
  }, [query]);
  function sync(v: string, caret: number | null) {
    const end = caret ?? v.length;
    const m = /@([^@\n]{0,40})$/.exec(v.slice(0, end));
    setQuery(m ? m[1].trimStart() : null);
    setSpan(m ? [end - m[0].length, end] : null);
  }
  function pick(h: EntityHit) {
    if (!span || value[span[0]] !== "@") return;
    const [start, end] = span;
    const insert = `${h.name} `;
    caretTo.current = start + insert.length;
    onChange(value.slice(0, start) + insert + value.slice(end));
    onPick({ mention: h.name, entity_id: h.entity_id });
    setQuery(null); setSpan(null); setHits([]);
    inner.current?.focus();
  }
  function onKey(e: KeyboardEvent<HTMLInputElement>) {
    if (hits.length === 0 || query == null) return;
    if (e.key === "ArrowDown") { e.preventDefault(); setSel((s) => (s + 1) % hits.length); }
    else if (e.key === "ArrowUp") { e.preventDefault(); setSel((s) => (s - 1 + hits.length) % hits.length); }
    else if (e.key === "Enter" || e.key === "Tab") { e.preventDefault(); pick(hits[sel]); }
    else if (e.key === "Escape") { e.preventDefault(); setQuery(null); setHits([]); }
  }
  const open = query != null && hits.length > 0;
  return (
    <div className="relative flex-1 min-w-0">
      {open && (
        <ul id={listId} role="listbox" aria-label="Players and teams"
          className="absolute bottom-full left-0 mb-3 w-full max-w-md rounded-lg border border-glass bg-obsidian/95 p-1 shadow-xl backdrop-blur z-30">
          {hits.map((h, i) => (
            <li key={h.entity_id} id={`${listId}-${i}`} role="option" aria-selected={i === sel}
              onMouseDown={(e) => { e.preventDefault(); pick(h); }} onMouseEnter={() => setSel(i)}
              className={clsx("flex cursor-pointer items-center gap-2 rounded-md px-2 py-1.5 text-sm", i === sel ? "bg-violet/20 text-gray-100" : "text-gray-300")}>
              {h.kind === "team" ? <Users size={13} className="text-gray-500" /> : <User size={13} className="text-gray-500" />}
              <span className="truncate">{h.name}</span>
              <span className="ml-auto shrink-0 text-[11px] text-gray-500">{h.team_id ? `${h.team_id.split(":")[2]} · ` : ""}{h.sport.toUpperCase()}{h.active ? "" : " · inactive"}</span>
            </li>))}
        </ul>)}
      <input
        ref={(el) => { inner.current = el; if (typeof ref === "function") ref(el); else if (ref) ref.current = el; }}
        value={value} disabled={disabled} placeholder={placeholder} maxLength={8000}
        onChange={(e) => { onChange(e.target.value); sync(e.target.value, e.target.selectionStart); }}
        onKeyDown={onKey} onBlur={() => window.setTimeout(() => setHits([]), 120)}
        role="combobox" aria-expanded={open} aria-controls={listId} aria-autocomplete="list"
        aria-activedescendant={open ? `${listId}-${sel}` : undefined} aria-label="Message MonsterGPT (type @ to pick a player or team)"
        className="w-full bg-transparent text-sm focus:outline-none text-gray-100 placeholder:text-amber-200/50" />
    </div>
  );
});
