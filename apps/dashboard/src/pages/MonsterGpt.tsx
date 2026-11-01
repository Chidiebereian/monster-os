import { useCallback, useEffect, useRef, useState } from "react";
import { Link, useSearchParams } from "react-router-dom";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  AlertTriangle, ArrowRight, Bot, Brain, Check, ChevronDown, Circle, Copy, Database, ExternalLink, Loader2, MessageSquarePlus, PanelLeft, RotateCw, Send, Share2, Sparkles,
  Square, ThumbsDown, ThumbsUp, Trash2, Wifi, WifiOff, X,
} from "lucide-react";
import clsx from "clsx";
import { GptLocks } from "../components/GptLocks";
import {
  GptSocket, gptApi, newRequestId, type ConnState, type GptCard, type GptModel, type GptSource, type GptStatus, type GptUsage, type ServerMsg, type StoredMessage,
} from "../lib/gpt";

// MonsterGPT: plain-English betting assistant over the monster-gpt WebSocket.
// The bet card and sources are Monster OS data (built in code from AI Picks
// and the Games hub); the streamed text is the model's explanation of them.

interface Turn {
  request_id: string;
  question: string;
  statuses: GptStatus[];
  card?: GptCard;
  sources?: GptSource[];
  text: string;
  done: boolean;
  model?: string | null;
  usage?: GptUsage;
  cancelled?: boolean;
  error?: string;
  rewriting?: boolean;
  /** ms timestamps for the research timer */
  started?: number;
  ended?: number;
  message_id?: string | null;
  feedback?: number | null;
}

const PILLS = [
  "What are the best bets today?",
  "Give me 3 safe bets for tonight, I have $200",
  "Any plus-money NBA props?",
  "Make me a 3-leg parlay",
  "Remember my bankroll is $500",
  "How are your picks doing?",
  "Explain no-vig fair odds in one paragraph",
];

const odds = (p: number | null | undefined) => (p == null ? "—" : p > 0 ? `+${p}` : `${p}`);
const pct = (x: number) => `${(x * 100).toFixed(0)}%`;
const usd = (x: number | null | undefined) => (x == null ? "—" : `$${x >= 100 ? x.toFixed(0) : x.toFixed(2).replace(/\.00$/, "")}`);
const time = (iso: string | null | undefined) => (iso ? new Date(iso).toLocaleString(undefined, { weekday: "short", hour: "numeric", minute: "2-digit" }) : "");

function fromStored(ms: StoredMessage[]): Turn[] {
  const out: Turn[] = [];
  for (const m of ms) {
    if (m.role === "user") out.push({ request_id: m.request_id ?? m.id, question: m.content, statuses: [], text: "", done: false });
    else {
      const t = out.find((x) => x.request_id === m.request_id) ?? out[out.length - 1];
      if (!t) continue;
      Object.assign(t, { text: m.content, card: m.card ?? undefined, sources: m.sources ?? undefined, model: m.model, usage: m.usage ?? undefined, cancelled: m.cancelled, done: true,
        message_id: m.id, feedback: m.feedback ?? null });
    }
  }
  return out.map((t) => ({ ...t, done: true }));
}

export function MonsterGpt() {
  const [turns, setTurns] = useState<Turn[]>([]);
  const [input, setInput] = useState("");
  const [conv, setConv] = useState<string | null>(null);
  const [conn, setConn] = useState<ConnState>("connecting");
  const [models, setModels] = useState<GptModel[]>([]);
  const [model, setModel] = useState<string | null>(null);
  const [sidebar, setSidebar] = useState(false);
  const [params, setParams] = useSearchParams();
  const sock = useRef<GptSocket | null>(null);
  const scroller = useRef<HTMLDivElement>(null);
  const convRef = useRef<string | null>(null);
  const qc = useQueryClient();
  convRef.current = conv;

  const convs = useQuery({ queryKey: ["gpt-conversations"], queryFn: gptApi.conversations, retry: 1 });
  const active = turns.find((t) => !t.done);

  const patch = useCallback((rid: string, f: (t: Turn) => Turn) => setTurns((ts) => ts.map((t) => (t.request_id === rid ? f(t) : t))), []);

  const onMsg = useCallback((m: ServerMsg) => {
    switch (m.type) {
      case "ready": setModels(m.models); setModel((cur) => cur ?? m.models[0]?.id ?? null); break;
      case "ack":
        if (m.conversation_id && !convRef.current) { setConv(m.conversation_id); qc.invalidateQueries({ queryKey: ["gpt-conversations"] }); }
        break;
      case "status": patch(m.request_id, (t) => {
        const rest = t.statuses.filter((s) => !(s.stage === m.stage && s.name === m.name));
        return { ...t, statuses: [...rest, { stage: m.stage, name: m.name, state: m.state, detail: m.detail, ms: m.ms }] };
      }); break;
      case "card": patch(m.request_id, (t) => ({ ...t, card: m.card })); break;
      case "sources": patch(m.request_id, (t) => ({ ...t, sources: m.sources })); break;
      case "delta": patch(m.request_id, (t) => ({ ...t, text: t.text + m.text })); break;
      case "reset": patch(m.request_id, (t) => ({ ...t, text: "", rewriting: true })); break;
      case "done":
        patch(m.request_id, (t) => ({ ...t, done: true, ended: Date.now(), model: m.model, usage: m.usage, cancelled: m.cancelled, message_id: m.message_id }));
        qc.invalidateQueries({ queryKey: ["gpt-conversations"] });
        qc.invalidateQueries({ queryKey: ["gpt-memory"] });
        break;
      case "error":
        if (m.request_id) patch(m.request_id, (t) => ({ ...t, error: m.message, done: ["busy", "rate_limited", "conversation", "in_progress"].includes(m.code) ? true : t.done }));
        break;
    }
  }, [patch, qc]);

  useEffect(() => {
    const s = new GptSocket(onMsg, setConn);
    sock.current = s;
    return () => s.close();
  }, [onMsg]);

  // a dropped connection can't resume a stream: mark it so the user can resend
  useEffect(() => {
    if (conn === "reconnecting") setTurns((ts) => ts.map((t) => (t.done ? t : { ...t, done: true, error: "Connection dropped mid-answer. Resend to get a fresh one (a finished answer is replayed, not regenerated)." })));
  }, [conn]);

  useEffect(() => { scroller.current?.scrollTo({ top: scroller.current.scrollHeight }); }, [turns]);

  function send(q: string, context?: unknown, request_id = newRequestId()) {
    const question = q.trim();
    if (!question || active || !sock.current) return;
    setInput("");
    setTurns((ts) => [...ts.filter((t) => t.request_id !== request_id), { request_id, question, statuses: [], text: "", done: false, started: Date.now() }]);
    sock.current.send({ request_id, conversation_id: conv, text: question, context, model });
  }

  // Arriving from a matchup page: ?q=...&sport=&game= sends the question with that game's brief.
  useEffect(() => {
    const q = params.get("q");
    if (!q) return;
    const sport = params.get("sport");
    const game = params.get("game");
    setParams({}, { replace: true });
    (async () => {
      let context: unknown;
      if (sport && game) {
        try {
          const r = await fetch(`/api/projection-service/hub/${sport}/game/${game}/brief`);
          if (r.ok) context = { game: await r.json() };
        } catch { /* ask without it */ }
      }
      send(q, context);
    })();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  async function openConv(id: string) {
    if (active) return;
    setConv(id);
    setSidebar(false);
    try { setTurns(fromStored((await gptApi.messages(id)).messages)); } catch (e) { setTurns([]); console.error(e); }
  }
  function newChat() { if (active) return; setConv(null); setTurns([]); setSidebar(false); }
  async function remove(id: string) {
    await gptApi.remove(id).catch(() => undefined);
    if (id === conv) newChat();
    qc.invalidateQueries({ queryKey: ["gpt-conversations"] });
  }

  return (
    <div className="flex h-[calc(100vh-7rem)] gap-4">
      {/* conversations */}
      <aside className={clsx("w-60 shrink-0 flex-col gap-2 border-r border-glass pr-3", sidebar ? "flex fixed inset-y-0 left-0 z-40 bg-obsidian p-3 w-72" : "hidden lg:flex")}>
        <div className="flex items-center gap-2">
          <button onClick={newChat} disabled={!!active} className="flex-1 inline-flex items-center gap-2 rounded-md border border-glass px-3 py-2 text-sm text-gray-200 hover:bg-white/5 disabled:opacity-40">
            <MessageSquarePlus size={15} />New chat</button>
          {sidebar && <button onClick={() => setSidebar(false)} className="p-2 text-gray-400" aria-label="Close conversations"><X size={16} /></button>}
        </div>
        <MemoryPanel />
        <div className="text-[10px] uppercase tracking-wider text-gray-500 mt-2">Conversations</div>
        <div className="flex-1 overflow-y-auto space-y-0.5">
          {convs.isError && <div className="text-xs text-gray-500">Couldn't load conversations.</div>}
          {convs.data?.conversations.length === 0 && <div className="text-xs text-gray-500">No conversations yet.</div>}
          {convs.data?.conversations.map((c) => (
            <div key={c.id} className={clsx("group flex items-center rounded-md", c.id === conv ? "bg-white/10" : "hover:bg-white/5")}>
              <button onClick={() => openConv(c.id)} className="flex-1 min-w-0 text-left px-2 py-1.5">
                <div className="text-sm text-gray-200 truncate">{c.title}</div>
                <div className="text-[10px] text-gray-500">{new Date(c.updated_at).toLocaleString(undefined, { month: "short", day: "numeric", hour: "numeric", minute: "2-digit" })}</div>
              </button>
              <button onClick={() => remove(c.id)} className="p-1.5 text-gray-600 opacity-0 group-hover:opacity-100 hover:text-crimson-light" aria-label="Delete conversation"><Trash2 size={13} /></button>
            </div>))}
        </div>
      </aside>

      <div className="flex-1 min-w-0 flex flex-col">
        <div className="flex items-center gap-2 mb-3">
          <button onClick={() => setSidebar(true)} className="lg:hidden p-1.5 rounded-md border border-glass text-gray-400" aria-label="Conversations"><PanelLeft size={15} /></button>
          <Bot className="text-violet" size={20} />
          <h1 className="text-lg font-semibold">MonsterGPT</h1>
          <span className="hidden sm:inline text-sm text-gray-500">ask in plain English · bets come from Monster OS data</span>
          <div className="ml-auto flex items-center gap-2">
            {models.length > 1 && (
              <select value={model ?? ""} onChange={(e) => setModel(e.target.value)} disabled={!!active} className="bg-surface border border-glass rounded-md px-2 py-1 text-xs text-gray-300" aria-label="Model">
                {models.map((m) => <option key={m.id} value={m.id}>{m.id}</option>)}
              </select>)}
            <span className={clsx("inline-flex items-center gap-1 text-[11px]", conn === "open" ? "text-emerald-light" : conn === "closed" ? "text-gray-500" : "text-amber-400")}>
              {conn === "open" ? <Wifi size={12} /> : <WifiOff size={12} />}{conn === "open" ? "live" : conn}</span>
          </div>
        </div>

        <div ref={scroller} className="flex-1 overflow-auto space-y-5 pr-1">
          {turns.length === 0 && (
            <div className="text-gray-500 text-sm space-y-4">
              <div>
                <h2 className="text-2xl font-semibold text-gray-100">What Are We Betting On Today?</h2>
                <p>Ask in plain English: a sport, how many bets, your bankroll, how safe. Follow up with "what about hockey?" or "make it a parlay". The bets come from AI Picks; MonsterGPT explains them.</p>
              </div>
              <GptLocks onAsk={(q) => send(q)} />
              <div className="flex flex-wrap gap-2">
                {PILLS.map((p) => (
                  <button key={p} onClick={() => send(p)} className="flex items-center gap-1.5 px-3 py-1.5 rounded-full border border-glass text-xs text-gray-300 hover:bg-white/5">
                    <Sparkles size={12} className="text-violet-light" /> {p}
                  </button>))}
              </div>
            </div>
          )}
          {turns.map((t, i) => (
            <TurnView key={t.request_id} t={t} last={i === turns.length - 1} busy={!!active}
              onRetry={() => send(t.question, undefined, t.request_id)}
              onAsk={(q, context) => send(q, context)}
              onRate={async (rating) => {
                if (!t.message_id) return;
                const next = t.feedback === rating ? 0 : rating;
                patch(t.request_id, (x) => ({ ...x, feedback: next || null }));
                await gptApi.feedback(t.message_id, next).catch(() => patch(t.request_id, (x) => ({ ...x, feedback: t.feedback ?? null })));
              }} />))}
        </div>

        <form onSubmit={(e) => { e.preventDefault(); send(input); }}
          className="mt-3 flex items-center gap-2 rounded-xl border border-amber-400/25 bg-surface/90 px-4 py-3 shadow-[0_0_24px_-8px_rgba(139,92,246,0.45)] focus-within:border-violet/50">
          <input value={input} onChange={(e) => setInput(e.target.value)} placeholder="Ask MonsterGPT anything to get the best picks." maxLength={8000}
            className="flex-1 bg-transparent text-sm focus:outline-none text-gray-100 placeholder:text-amber-200/50" aria-label="Message MonsterGPT" />
          {active ? (
            <button type="button" onClick={() => sock.current?.cancel(active.request_id)} title="Stop"
              className="relative h-8 w-8 rounded-full text-gray-300 hover:text-crimson-light" aria-label="Stop">
              <Loader2 size={30} className="absolute inset-0 m-auto animate-spin text-gray-500" strokeWidth={1.25} />
              <Square size={10} className="absolute inset-0 m-auto fill-current" /></button>
          ) : (
            <button type="submit" disabled={!input.trim()} className="h-8 w-8 inline-flex items-center justify-center rounded-full bg-violet/25 text-violet-light disabled:opacity-40 hover:bg-violet/35" aria-label="Send"><Send size={15} /></button>
          )}
        </form>
      </div>
    </div>
  );
}

const STEP_LABELS = ["Understanding your question", "Checking Monster picks, projections, and prices", "Building a clear answer"];

/** Which of the three research steps are done, from the status frames. */
function steps(t: Turn): { labels: string[]; done: number } {
  const labels = [...STEP_LABELS];
  if (t.statuses.some((s) => s.stage === "retrieval")) labels[1] = "Searching Monster OS docs and data";
  if (t.statuses.some((s) => s.stage === "route" && s.name === "memory")) labels[1] = "Updating your saved memory";
  if (t.done) return { labels, done: 3 };
  const routed = t.statuses.some((s) => s.stage === "route");
  const gathered = !!t.card || !!t.text || t.statuses.some((s) => s.stage === "model");
  return { labels, done: gathered ? 2 : routed ? 1 : 0 };
}

function useElapsed(t: Turn) {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    if (t.done) return;
    const id = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(id);
  }, [t.done]);
  if (!t.started) return null;
  return Math.max(0, Math.round(((t.done ? t.ended ?? now : now) - t.started) / 1000));
}

/** Questions worth asking next, built from the card (no model call). */
function followUps(t: Turn): { q: string; context?: unknown }[] {
  const c = t.card;
  if (c && c.bets.length > 0) {
    const top = c.bets[0];
    const ctx = { bet: { pick: top.pick, game: top.game, sport: top.sport, price: top.price, book: top.book, p_model: top.p_model, p_market: top.p_market,
      edge: top.edge, grade: top.grade, tier: top.tier, why: top.why, risks: top.risks } };
    const out: { q: string; context?: unknown }[] = [{ q: `What could make ${top.pick} lose?`, context: ctx }];
    if (c.parlays.length > 0) out.push({ q: "No parlay, straight bets", context: undefined }, { q: "Make it a safer parlay", context: undefined });
    else {
      if (c.bets.length > 1) out.push({ q: "Make it a parlay", context: undefined });
      out.push({ q: c.filters.risk === "safe" ? "Only plus-money picks" : "Only the safest picks", context: undefined });
    }
    return out;
  }
  if (c) return [{ q: c.filters.day === "tomorrow" ? "What about today?" : "What about tomorrow?" }, { q: "Show me every sport" }, { q: "How are your picks doing?" }];
  return [];
}

/** "Pick: Cale Makar Under 1.5 Points @ DraftKings -150" over the answer. */
function Headline({ card }: { card: GptCard }) {
  const p = card.parlays[0];
  if (p) return (
    <div className="mb-2 text-[15px] font-medium text-violet-light">
      {card.parlays.length > 1 ? `${card.parlays.length} parlays` : `Parlay: ${p.legs.length} legs`} <span className="font-mono">{odds(p.american)}</span>
      {p.stake_usd != null && <span className="text-gray-300 font-normal"> · {usd(p.stake_usd)} stake pays {usd(p.payout_usd)}</span>}
    </div>);
  const b = card.bets[0];
  if (!b) return null;
  return (
    <div className="mb-2 text-[15px] font-medium text-violet-light">
      {card.bets.length > 1 ? "Top pick" : "Pick"}: {b.pick} <span className="text-gray-400 font-normal">@</span>{" "}
      {b.link ? <a href={b.link} target="_blank" rel="noreferrer" className="inline-flex items-center gap-1 underline decoration-violet/40 underline-offset-2 hover:decoration-violet-light">
        <ExternalLink size={13} />{b.book} {odds(b.price)}</a> : <span>{b.book} {odds(b.price)}</span>}
    </div>);
}

function TurnView({ t, last, busy, onRetry, onAsk, onRate }: {
  t: Turn; last: boolean; busy: boolean; onRetry: () => void; onAsk: (q: string, context?: unknown) => void; onRate: (rating: number) => void;
}) {
  const [details, setDetails] = useState(false);
  const [showCard, setShowCard] = useState<boolean | null>(null);
  const [copied, setCopied] = useState<"copy" | "share" | null>(null);
  const elapsed = useElapsed(t);
  const st = steps(t);
  const issues = t.usage?.judge?.issues ?? [];
  const unsupported = t.usage?.unsupported_numbers ?? [];
  const validation = t.statuses.find((s) => s.stage === "validate");
  const checked = t.done && (validation?.state === "completed" || (t.usage?.judge && issues.length === 0 && unsupported.length === 0));
  const researching = !t.done && !t.text;
  const cardOpen = showCard ?? (!!t.card && (t.card.parlays.length > 0 || !t.text));
  const plain = () => {
    const b = t.card?.bets[0];
    return `${b ? `Pick: ${b.pick} @ ${b.book} ${odds(b.price)}\n\n` : ""}${t.text}`;
  };
  async function copy(kind: "copy" | "share") {
    const text = plain();
    try {
      if (kind === "share" && navigator.share) { await navigator.share({ title: "MonsterGPT", text }); return; }
      await navigator.clipboard.writeText(text);
      setCopied(kind);
      window.setTimeout(() => setCopied(null), 1500);
    } catch { /* share sheet dismissed or clipboard blocked */ }
  }
  const icon = "p-1.5 rounded-md text-gray-500 hover:text-gray-200 hover:bg-white/5 disabled:opacity-30";
  return (
    <div className="space-y-3">
      <div className="flex justify-end">
        <div className="max-w-[70ch] rounded-2xl border border-violet/25 bg-violet/15 px-4 py-3 text-[15px] text-gray-100 whitespace-pre-wrap">{t.question}</div>
      </div>
      <div className="flex gap-3">
        <div className="shrink-0 h-9 w-9 rounded-full bg-gradient-to-br from-violet to-violet/40 ring-2 ring-violet/30 flex items-center justify-center"><Bot size={17} className="text-white" /></div>
        <div className="flex-1 min-w-0 space-y-3">
          {researching ? (
            <div className="max-w-md rounded-xl border border-glass bg-surface/80 p-4 shadow-[0_0_30px_-12px_rgba(139,92,246,0.5)]">
              <div className="flex items-center gap-2.5 mb-3">
                <span className="h-7 w-7 rounded-md bg-violet/20 flex items-center justify-center"><Sparkles size={14} className="text-violet-light" /></span>
                <span className="font-medium text-gray-100">Researching your answer</span>
                {elapsed != null && <span className="ml-auto text-xs text-gray-500 tabular-nums">{elapsed}s</span>}
              </div>
              <ol className="space-y-1">
                {st.labels.map((label, i) => (
                  <li key={label} className={clsx("flex items-start gap-2.5 rounded-md px-2 py-1.5 text-sm", i === st.done ? "bg-white/5 text-gray-100 font-medium" : i < st.done ? "text-gray-400" : "text-gray-600")}>
                    {i < st.done ? <Check size={15} className="mt-0.5 shrink-0 text-emerald-light" /> : i === st.done ? <Loader2 size={15} className="mt-0.5 shrink-0 animate-spin text-violet-light" /> : <Circle size={15} className="mt-0.5 shrink-0" />}
                    {label}
                  </li>))}
              </ol>
              {t.statuses.some((s) => s.stage === "model" && s.state === "queued") && <div className="mt-2 text-xs text-amber-400">Waiting for a free model slot…</div>}
            </div>
          ) : (
            <button onClick={() => setDetails(!details)} className="inline-flex items-center gap-1.5 rounded-md bg-white/[0.03] px-2.5 py-1 text-xs text-gray-500 hover:text-gray-300">
              {t.done ? <Check size={12} className="text-emerald-light" /> : <Loader2 size={12} className="animate-spin text-violet-light" />}
              {t.done ? `Research complete · 3 steps${elapsed != null ? ` · ${elapsed}s` : ""}` : `Writing the answer${elapsed != null ? ` · ${elapsed}s` : ""}`}
              <ChevronDown size={12} className={clsx(details && "rotate-180")} />
            </button>
          )}
          {details && !researching && (
            <div className="flex flex-wrap gap-1.5">{t.statuses.map((s) => (
              <span key={`${s.stage}:${s.name}`} title={s.detail} className={clsx("inline-flex items-center gap-1 rounded-full border px-2 py-0.5 text-[11px]",
                s.state === "failed" ? "border-crimson/40 text-crimson-light" : "border-glass text-gray-400")}>
                {s.state === "failed" ? <AlertTriangle size={11} /> : <Check size={11} />}
                {s.stage === "route" ? s.detail : s.stage === "judge" ? (s.name === "repair" ? "fixed the draft" : "rewrote the draft") : s.stage === "validate" ? "checked against the data" : s.name}
                {s.ms != null && s.stage !== "route" && s.stage !== "validate" && <span className="text-gray-600">{s.ms < 1000 ? `${s.ms}ms` : `${(s.ms / 1000).toFixed(1)}s`}</span>}
              </span>))}
              {t.usage?.ttft_ms != null && <span className="text-[11px] text-gray-500 px-1">first word {(t.usage.ttft_ms / 1000).toFixed(1)}s{t.usage.tokens_per_s != null ? ` · ${t.usage.tokens_per_s} tok/s` : ""}{t.model ? ` · ${t.model}` : ""}</span>}
            </div>)}

          {(t.text || (t.card && t.done)) && (
            <div className="rounded-xl border border-glass bg-surface/70 px-4 py-3.5">
              {t.card && <Headline card={t.card} />}
              {t.text && (
                <div className="text-[15px] text-gray-200 whitespace-pre-wrap leading-relaxed">
                  <Rich text={t.text} />{!t.done && <span className="inline-block w-1.5 h-4 bg-violet-light/70 animate-pulse align-text-bottom ml-0.5" />}
                </div>)}
              {t.done && (issues.length > 0 || unsupported.length > 0) && (
                <div className="mt-2 flex gap-1.5 text-[11px] text-amber-400"><AlertTriangle size={12} className="shrink-0 mt-0.5" />
                  <span>The checker still flags: {[...issues.map((i) => i.detail), ...(unsupported.length && !issues.some((i) => i.code === "unsupported_numbers") ? [`numbers not in the data: ${unsupported.join(", ")}`] : [])].join("; ")}. The bet card is the source of truth.</span></div>)}
              {checked && t.text && (
                <div className="mt-2 text-[11px] text-emerald-light/80">Checked against Monster OS data{t.usage?.judge?.repaired ? " · fixed the draft" : ""}{t.usage?.judge?.revised ? " · rewritten once" : ""}{t.usage?.judge?.scrubbed ? ` · removed ${t.usage.judge.scrubbed} unverifiable sentence${t.usage.judge.scrubbed === 1 ? "" : "s"}` : ""}</div>)}
              {t.cancelled && <div className="mt-2 text-[11px] text-amber-400">Stopped.</div>}
              {t.card && (t.card.bets.length > 0 || t.card.notes.length > 0) && (
                <div className="mt-3">
                  <button onClick={() => setShowCard(!cardOpen)} className="inline-flex items-center gap-1 text-xs text-violet-light hover:underline">
                    <Database size={12} />{cardOpen ? "Hide" : "Show"} the bet card{t.card.bets.length ? ` · ${t.card.bets.length} pick${t.card.bets.length === 1 ? "" : "s"}` : ""}
                    <ChevronDown size={12} className={clsx(cardOpen && "rotate-180")} /></button>
                  {cardOpen && <div className="mt-2"><BetCard card={t.card} sources={t.sources} /></div>}
                </div>)}
            </div>
          )}
          {t.sources && t.sources.length > 0 && !t.card && t.done && <Sources sources={t.sources} />}

          {t.done && t.text && (
            <div className="flex items-center gap-0.5">
              <button className={icon} onClick={() => copy("copy")} title={copied === "copy" ? "Copied" : "Copy"} aria-label="Copy">{copied === "copy" ? <Check size={15} /> : <Copy size={15} />}</button>
              <button className={icon} onClick={() => copy("share")} title={copied === "share" ? "Copied to share" : "Share"} aria-label="Share">{copied === "share" ? <Check size={15} /> : <Share2 size={15} />}</button>
              <button className={clsx(icon, t.feedback === 1 && "text-emerald-light")} disabled={!t.message_id} onClick={() => onRate(1)} title="Good answer" aria-label="Good answer" aria-pressed={t.feedback === 1}><ThumbsUp size={15} /></button>
              <button className={clsx(icon, t.feedback === -1 && "text-crimson-light")} disabled={!t.message_id} onClick={() => onRate(-1)} title="Bad answer" aria-label="Bad answer" aria-pressed={t.feedback === -1}><ThumbsDown size={15} /></button>
              <button className={icon} disabled={busy} onClick={() => onAsk(t.question)} title="Ask again" aria-label="Ask again"><RotateCw size={15} /></button>
            </div>)}

          {t.error && (
            <div className="flex items-center gap-2 text-xs text-crimson-light"><AlertTriangle size={13} />{t.error}
              <button onClick={onRetry} className="underline text-gray-300">Resend</button></div>)}

          {last && t.done && !t.error && followUps(t).length > 0 && (
            <div className="pt-1">
              <div className="text-sm font-medium text-gray-300 mb-1.5">Follow-up</div>
              <div className="space-y-1">
                {followUps(t).map((f) => (
                  <button key={f.q} disabled={busy} onClick={() => onAsk(f.q, f.context)}
                    className="flex w-full items-start gap-2.5 rounded-lg px-2 py-2 text-left text-sm text-gray-300 hover:bg-white/5 disabled:opacity-40">
                    <ArrowRight size={15} className="mt-0.5 shrink-0 text-gray-500" />{f.q}</button>))}
              </div>
            </div>)}
        </div>
      </div>
    </div>
  );
}

/** Model text with **bold** rendered; everything else stays plain. */
function Rich({ text }: { text: string }) {
  const parts = text.split(/(\*\*[^*\n]+\*\*)/g);
  return <>{parts.map((p, i) => (p.startsWith("**") && p.endsWith("**") && p.length > 4 ? <strong key={i} className="text-gray-100">{p.slice(2, -2)}</strong> : <span key={i}>{p}</span>))}</>;
}

function MemoryPanel() {
  const [open, setOpen] = useState(false);
  const qc = useQueryClient();
  const q = useQuery({ queryKey: ["gpt-memory"], queryFn: gptApi.memory, retry: 1 });
  const items = q.data?.items ?? [];
  return (
    <div className="rounded-md border border-glass">
      <button onClick={() => setOpen(!open)} className="w-full flex items-center gap-2 px-3 py-2 text-sm text-gray-300 hover:bg-white/5">
        <Brain size={14} className="text-violet-light" />Memory<span className="ml-auto text-[11px] text-gray-500">{items.length}</span>
        <ChevronDown size={12} className={clsx("text-gray-500", open && "rotate-180")} /></button>
      {open && (
        <div className="px-3 pb-2 space-y-1">
          {items.length === 0 && <div className="text-[11px] text-gray-500">Nothing saved. Say "remember my bankroll is $500" or "from now on only NHL props".</div>}
          {items.map((i) => (
            <div key={i.id} className="group flex items-center gap-1.5 text-xs">
              <span className="text-gray-500">{i.kind === "note" ? "note" : i.key}</span>
              <span className="text-gray-200 truncate flex-1" title={i.value}>{i.key === "bankroll" ? `$${i.value}` : i.value}</span>
              <button onClick={async () => { await gptApi.forget(i.id).catch(() => undefined); qc.invalidateQueries({ queryKey: ["gpt-memory"] }); }}
                className="text-gray-600 hover:text-crimson-light" aria-label={`Forget ${i.key}`}><X size={12} /></button>
            </div>))}
          {items.length > 0 && <div className="text-[10px] text-gray-500 pt-1">Used when a request doesn't say otherwise.</div>}
        </div>)}
    </div>
  );
}

function Sources({ sources }: { sources: GptSource[] }) {
  const [open, setOpen] = useState(false);
  return (
    <div className="text-[11px] text-gray-500">
      <button onClick={() => setOpen(!open)} className="inline-flex items-center gap-1 hover:text-gray-300"><Database size={11} />{sources.length} source{sources.length === 1 ? "" : "s"}<ChevronDown size={11} className={clsx(open && "rotate-180")} /></button>
      {open && <ul className="mt-1 space-y-0.5">{sources.map((s) => (
        <li key={s.id}><span className="font-mono text-gray-400">[{s.id}]</span> {s.label} · fetched {new Date(s.fetched_at).toLocaleTimeString([], { hour: "numeric", minute: "2-digit", second: "2-digit" })}</li>))}</ul>}
    </div>
  );
}

function BetCard({ card, sources }: { card: GptCard; sources?: GptSource[] }) {
  const [open, setOpen] = useState<number | null>(null);
  const picksSrc = sources?.find((s) => s.tool === "picks.today");
  return (
    <div className="rounded-lg border border-violet/30 bg-gradient-to-b from-violet/[0.06] to-transparent overflow-hidden">
      <div className="flex flex-wrap items-center gap-2 px-4 py-2.5 border-b border-glass">
        <Database size={13} className="text-violet-light" />
        <span className="text-sm font-medium">{card.title}</span>
        <span className="ml-auto text-[10px] uppercase tracking-wider text-gray-500">From Monster OS data{picksSrc ? ` · AI Picks [${picksSrc.id}] · ${new Date(picksSrc.fetched_at).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })}` : ""}</span>
      </div>
      {card.bets.length > 0 && (
        <div className="divide-y divide-glass/60">
          {card.bets.map((b, i) => (
            <div key={b.id} className="px-4 py-2.5">
              <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
                <span className="text-xs text-gray-500 w-4">{i + 1}</span>
                <span className="rounded bg-white/5 text-gray-400 text-[10px] px-1.5 py-0.5 uppercase">{b.sport}</span>
                <span className="font-medium text-sm text-gray-100">{b.pick}</span>
                <span className="text-xs text-gray-500">{b.game} · {time(b.starts_at)}</span>
                <span className="ml-auto inline-flex items-center gap-2">
                  <span className="rounded bg-emerald/15 text-emerald-light text-[11px] px-1.5 py-0.5 font-medium" title={b.tier}>{b.grade}</span>
                  {b.link ? <a href={b.link} target="_blank" rel="noreferrer" className="inline-flex items-center gap-1 font-mono text-sm text-violet-light hover:underline">{odds(b.price)} {b.book}<ExternalLink size={11} /></a>
                    : <span className="font-mono text-sm text-violet-light">{odds(b.price)} <span className="text-gray-400 font-sans text-xs">{b.book}</span></span>}
                </span>
              </div>
              <div className="flex flex-wrap gap-x-4 gap-y-0.5 mt-1 pl-7 text-[11px] text-gray-400">
                <span>model {pct(b.p_model)} vs market {pct(b.p_market)}</span>
                <span className="text-emerald-light">edge +{(b.edge * 100).toFixed(1)} pts</span>
                {!card.staking && <span>stake {b.stake_usd != null ? `${usd(b.stake_usd)} to win ${usd(b.to_win_usd)}` : `${(b.stake_frac * 100).toFixed(1)}% of bankroll`}</span>}
                {b.playable_price != null && <span>playable to {odds(b.playable_price)}</span>}
                <span className="capitalize">{b.tier}</span>
                {(b.why || b.risks.length > 0) && <button onClick={() => setOpen(open === b.id ? null : b.id)} className="text-violet-light hover:underline">{open === b.id ? "hide" : "why + risks"}</button>}
              </div>
              {open === b.id && (
                <div className="mt-1.5 pl-7 text-xs text-gray-300 space-y-1">
                  {b.why && <div>{b.why}</div>}
                  {b.risks.map((r) => <div key={r} className="flex gap-1.5 text-amber-400/90"><AlertTriangle size={11} className="shrink-0 mt-0.5" />{r}</div>)}
                  <Link to={`/picks?id=${b.id}`} className="text-violet-light hover:underline">Open in AI Picks</Link>
                </div>)}
            </div>))}
        </div>
      )}
      {(card.parlays ?? []).map((p, i) => (
        <div key={i} className="px-4 py-2.5 border-t border-glass text-sm">
          <div className="flex flex-wrap items-baseline gap-x-2">
            <span className="font-medium">{card.parlays.length > 1 ? `Parlay ${i + 1} · ` : ""}{p.legs.length}-leg parlay</span>
            <span className="font-mono text-violet-light">{odds(p.american)}</span>
            <span className="text-xs text-gray-400">model {(p.p_model * 100).toFixed(1)}% · EV {p.ev >= 0 ? "+" : ""}{(p.ev * 100).toFixed(0)}%</span>
          </div>
          <div className="text-xs text-gray-300 mt-1">{p.selections.join(" + ")}</div>
          <div className="flex flex-wrap gap-x-4 mt-1 text-xs">
            {p.stake_usd != null ? <>
              <span>Total parlay stake <span className="font-medium text-gray-100">{usd(p.stake_usd)}</span></span>
              <span>potential payout <span className="font-medium text-gray-100">{usd(p.payout_usd)}</span></span>
              <span>potential profit <span className="font-medium text-emerald-light">{usd(p.to_win_usd)}</span></span>
            </> : <span className="text-gray-400">Unstaked recommendation: say an amount ("put $20 on it") to see the payout.</span>}
          </div>
          <div className="text-[11px] text-gray-500 mt-0.5">{p.note} <Link to="/parlays" className="text-violet-light hover:underline">Parlay Builder</Link></div>
        </div>))}
      {card.staking && card.parlays.length > 0 && card.staking.stake_per_parlay != null && (
        <div className="px-4 py-2 border-t border-glass text-xs text-gray-400">
          {card.staking.basis === "requested" ? "Stake you asked for" : "Sized from your bankroll"}: {usd(card.staking.stake_per_parlay)}{card.staking.number_of_parlays > 1 ? ` per parlay × ${card.staking.number_of_parlays} = ${usd(card.staking.total_exposure)} total exposure` : ""}
          {card.staking.bankroll != null ? ` · bankroll ${usd(card.staking.bankroll)}` : ""}
        </div>)}
      {(!card.staking && card.total_stake_usd != null && card.bets.length > 0) && (
        <div className="px-4 py-2 border-t border-glass text-xs text-gray-400">Total stake {usd(card.total_stake_usd)} · model-expected profit {usd(card.expected_profit_usd)}</div>)}
      {card.notes.length > 0 && <div className="px-4 py-2 border-t border-glass text-[11px] text-gray-500 space-y-0.5">{card.notes.map((n) => <div key={n}>{n}</div>)}</div>}
      {sources && sources.length > 0 && <div className="px-4 py-2 border-t border-glass"><Sources sources={sources} /></div>}
    </div>
  );
}
