import { useCallback, useEffect, useRef, useState } from "react";
import { Link, useSearchParams } from "react-router-dom";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { AlertTriangle, Bot, Brain, Check, ChevronDown, Database, ExternalLink, Loader2, MessageSquarePlus, PanelLeft, Send, Sparkles, Square, Trash2, User, Wifi, WifiOff, X } from "lucide-react";
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
      Object.assign(t, { text: m.content, card: m.card ?? undefined, sources: m.sources ?? undefined, model: m.model, usage: m.usage ?? undefined, cancelled: m.cancelled, done: true });
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
        patch(m.request_id, (t) => ({ ...t, done: true, model: m.model, usage: m.usage, cancelled: m.cancelled }));
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
    setTurns((ts) => [...ts.filter((t) => t.request_id !== request_id), { request_id, question, statuses: [], text: "", done: false }]);
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
          {turns.map((t) => <TurnView key={t.request_id} t={t} onRetry={() => send(t.question, undefined, t.request_id)} />)}
        </div>

        <form onSubmit={(e) => { e.preventDefault(); send(input); }} className="mt-3 flex items-center gap-2 border border-glass rounded-lg bg-surface px-3 py-2">
          <input value={input} onChange={(e) => setInput(e.target.value)} placeholder='e.g. "3 safe NHL props tonight, I have $200"' maxLength={8000}
            className="flex-1 bg-transparent text-sm focus:outline-none text-gray-100 placeholder:text-gray-600" aria-label="Message MonsterGPT" />
          {active ? (
            <button type="button" onClick={() => sock.current?.cancel(active.request_id)} className="inline-flex items-center gap-1.5 px-2.5 py-1.5 rounded-md bg-crimson/20 text-crimson-light text-xs hover:bg-crimson/30">
              <Square size={12} />Stop</button>
          ) : (
            <button type="submit" disabled={!input.trim()} className="p-1.5 rounded-md bg-violet/20 text-violet-light disabled:opacity-40 hover:bg-violet/30" aria-label="Send"><Send size={16} /></button>
          )}
        </form>
      </div>
    </div>
  );
}

function TurnView({ t, onRetry }: { t: Turn; onRetry: () => void }) {
  const modelStatus = t.statuses.find((s) => s.stage === "model");
  const validation = t.statuses.find((s) => s.stage === "validate");
  const issues = t.usage?.judge?.issues ?? [];
  const unsupported = t.usage?.unsupported_numbers ?? [];
  const checked = t.done && (validation?.state === "completed" || (t.usage?.judge && issues.length === 0 && unsupported.length === 0));
  return (
    <div className="space-y-3">
      <div className="flex gap-3 justify-end">
        <div className="max-w-[70ch] rounded-lg px-4 py-2.5 text-sm bg-violet/15 text-gray-100 whitespace-pre-wrap">{t.question}</div>
        <div className="shrink-0 h-7 w-7 rounded-md bg-white/5 flex items-center justify-center"><User size={15} className="text-gray-400" /></div>
      </div>
      <div className="flex gap-3">
        <div className="shrink-0 h-7 w-7 rounded-md bg-violet/15 flex items-center justify-center"><Bot size={15} className="text-violet-light" /></div>
        <div className="flex-1 min-w-0 space-y-3">
          {t.statuses.length > 0 && (
            <div className="flex flex-wrap gap-1.5">{t.statuses.filter((s) => s.stage !== "validate" && !(s.stage === "judge" && s.state === "started" && t.done)).map((s) => (
              <span key={`${s.stage}:${s.name}`} title={s.detail} className={clsx("inline-flex items-center gap-1 rounded-full border px-2 py-0.5 text-[11px]",
                s.state === "failed" ? "border-crimson/40 text-crimson-light" : s.state === "completed" ? "border-glass text-gray-400" : "border-violet/40 text-violet-light")}>
                {s.state === "completed" ? <Check size={11} /> : s.state === "failed" ? <AlertTriangle size={11} /> : <Loader2 size={11} className="animate-spin" />}
                {s.stage === "route" ? s.detail : s.stage === "model" ? `${s.name}${s.state === "queued" ? " (queued)" : ""}` : s.stage === "judge" ? "fixing the draft" : s.name}
                {s.ms != null && s.stage !== "route" && <span className="text-gray-600">{s.ms < 1000 ? `${s.ms}ms` : `${(s.ms / 1000).toFixed(1)}s`}</span>}
              </span>))}</div>
          )}
          {t.card && <BetCard card={t.card} sources={t.sources} />}
          {(t.text || (!t.done && modelStatus)) && (
            <div className="rounded-lg bg-surface border border-glass px-4 py-3">
              <div className="text-[10px] uppercase tracking-wider text-gray-500 mb-1.5 flex items-center gap-1.5"><Bot size={11} />MonsterGPT's explanation{t.model ? ` · ${t.model}` : ""}</div>
              <div className="text-sm text-gray-200 whitespace-pre-wrap leading-relaxed">
                <Rich text={t.text} />{!t.done && <span className="inline-block w-1.5 h-4 bg-violet-light/70 animate-pulse align-text-bottom ml-0.5" />}
                {!t.text && !t.done && <span className="text-gray-500">{t.rewriting ? "The checker found problems in the draft; rewriting it…" : modelStatus?.state === "queued" ? "Waiting for a free model slot…" : "Writing…"}</span>}
              </div>
              {t.done && (issues.length > 0 || unsupported.length > 0) && (
                <div className="mt-2 flex gap-1.5 text-[11px] text-amber-400"><AlertTriangle size={12} className="shrink-0 mt-0.5" />
                  <span>The checker still flags: {[...issues.map((i) => i.detail), ...(unsupported.length && !issues.some((i) => i.code === "unsupported_numbers") ? [`numbers not in the data: ${unsupported.join(", ")}`] : [])].join("; ")}. The card above is the source of truth.</span></div>)}
              {t.done && (
                <div className="mt-2 pt-2 border-t border-glass text-[11px] text-gray-500 flex flex-wrap gap-x-3">
                  {t.cancelled && <span className="text-amber-400">stopped</span>}
                  {t.usage?.ttft_ms != null && <span>first word {(t.usage.ttft_ms / 1000).toFixed(1)}s</span>}
                  {t.usage?.tokens_per_s != null && <span>{t.usage.tokens_per_s} tok/s</span>}
                  {t.usage?.context && <span>context ~{t.usage.context.used_est}/{t.usage.context.budget} tokens{t.usage.context.dropped_history_turns ? ` · ${t.usage.context.dropped_history_turns} old turns dropped` : ""}</span>}
                  {checked && <span className="text-emerald-light">checked against the data{t.usage?.judge?.revised ? " (rewritten once to fix the draft)" : ""}{t.usage?.judge?.scrubbed ? ` · removed ${t.usage.judge.scrubbed} sentence${t.usage.judge.scrubbed === 1 ? "" : "s"} it couldn't verify` : ""}</span>}
                </div>)}
            </div>
          )}
          {t.sources && t.sources.length > 0 && !t.card && <Sources sources={t.sources} />}
          {t.error && (
            <div className="flex items-center gap-2 text-xs text-crimson-light"><AlertTriangle size={13} />{t.error}
              <button onClick={onRetry} className="underline text-gray-300">Resend</button></div>)}
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
                <span>stake {b.stake_usd != null ? `${usd(b.stake_usd)} to win ${usd(b.to_win_usd)}` : `${(b.stake_frac * 100).toFixed(1)}% of bankroll`}</span>
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
      {card.parlay && (
        <div className="px-4 py-2.5 border-t border-glass text-sm">
          <span className="font-medium">{card.parlay.legs.length}-leg parlay</span> <span className="font-mono text-violet-light">{odds(card.parlay.american)}</span>
          <span className="text-xs text-gray-400"> · model {(card.parlay.p_model * 100).toFixed(1)}% · EV {card.parlay.ev >= 0 ? "+" : ""}{(card.parlay.ev * 100).toFixed(0)}%{card.parlay.stake_usd != null ? ` · stake ${usd(card.parlay.stake_usd)} to win ${usd(card.parlay.to_win_usd)}` : ""}</span>
          <div className="text-[11px] text-gray-500 mt-0.5">{card.parlay.note} <Link to="/parlays" className="text-violet-light hover:underline">Parlay Builder</Link></div>
        </div>)}
      {(card.total_stake_usd != null && card.bets.length > 0) && (
        <div className="px-4 py-2 border-t border-glass text-xs text-gray-400">Total stake {usd(card.total_stake_usd)} · model-expected profit {usd(card.expected_profit_usd)}</div>)}
      {card.notes.length > 0 && <div className="px-4 py-2 border-t border-glass text-[11px] text-gray-500 space-y-0.5">{card.notes.map((n) => <div key={n}>{n}</div>)}</div>}
      {sources && sources.length > 0 && <div className="px-4 py-2 border-t border-glass"><Sources sources={sources} /></div>}
    </div>
  );
}
