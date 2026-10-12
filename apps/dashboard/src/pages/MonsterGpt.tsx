import { useRef, useState } from "react";
import { Bot, Send, User, Sparkles } from "lucide-react";
import clsx from "clsx";
import { api } from "../lib/api";

// Module (MonsterGPT): conversational research assistant over monster-gpt /ask.
// The LLM explains and interprets; it never produces raw predictions — the
// backend auto-retrieves evidence (RAG) and the answer flags it.
interface Msg {
  role: "user" | "assistant";
  text: string;
  retrieved?: boolean;
  model?: string;
}

const PILLS = [
  "Is there value on the Yankees ML? NYY in MLB",
  "Explain no-vig fair odds in one paragraph",
  "What model does MonsterOS use for MLB run totals?",
  "How should I size a bet with a 6% edge?",
];

export function MonsterGpt() {
  const [messages, setMessages] = useState<Msg[]>([]);
  const [input, setInput] = useState("");
  const [busy, setBusy] = useState(false);
  const scroller = useRef<HTMLDivElement>(null);

  async function send(q: string) {
    const question = q.trim();
    if (!question || busy) return;
    setInput("");
    setMessages((m) => [...m, { role: "user", text: question }]);
    setBusy(true);
    try {
      const res = await api.ask(question);
      setMessages((m) => [...m, { role: "assistant", text: res.answer, retrieved: res.retrieved, model: res.model }]);
    } catch (e) {
      setMessages((m) => [...m, { role: "assistant", text: `Error: ${(e as Error).message}` }]);
    } finally {
      setBusy(false);
      requestAnimationFrame(() => scroller.current?.scrollTo({ top: scroller.current.scrollHeight }));
    }
  }

  return (
    <div className="flex flex-col h-[calc(100vh-7rem)]">
      <div className="flex items-center gap-2 mb-3">
        <Bot className="text-violet" size={20} />
        <h1 className="text-lg font-semibold">MonsterGPT</h1>
        <span className="text-sm text-gray-500">research assistant · auto-retrieves evidence</span>
      </div>

      <div ref={scroller} className="flex-1 overflow-auto space-y-4 pr-1">
        {messages.length === 0 && (
          <div className="text-gray-500 text-sm space-y-3">
            <p>Ask about value, edges, models, or market flow. Answers are grounded in retrieved evidence.</p>
            <div className="flex flex-wrap gap-2">
              {PILLS.map((p) => (
                <button key={p} onClick={() => send(p)} className="flex items-center gap-1.5 px-3 py-1.5 rounded-full border border-glass text-xs text-gray-300 hover:bg-white/5">
                  <Sparkles size={12} className="text-violet-light" /> {p}
                </button>
              ))}
            </div>
          </div>
        )}
        {messages.map((m, i) => (
          <div key={i} className={clsx("flex gap-3", m.role === "user" && "justify-end")}>
            {m.role === "assistant" && <div className="shrink-0 h-7 w-7 rounded-md bg-violet/15 flex items-center justify-center"><Bot size={15} className="text-violet-light" /></div>}
            <div className={clsx("max-w-[70ch] rounded-lg px-4 py-3 text-sm whitespace-pre-wrap", m.role === "user" ? "bg-violet/15 text-gray-100" : "bg-surface border border-glass text-gray-200")}>
              {m.text}
              {m.role === "assistant" && (m.model || m.retrieved != null) && (
                <div className="mt-2 pt-2 border-t border-glass text-[11px] text-gray-500">
                  {m.model} {m.retrieved ? "· evidence retrieved" : "· no evidence"}
                </div>
              )}
            </div>
            {m.role === "user" && <div className="shrink-0 h-7 w-7 rounded-md bg-white/5 flex items-center justify-center"><User size={15} className="text-gray-400" /></div>}
          </div>
        ))}
        {busy && <div className="text-sm text-gray-500 flex items-center gap-2"><Bot size={15} className="text-violet-light animate-pulse" /> thinking… (local model on CPU, ~1 min)</div>}
      </div>

      <form
        onSubmit={(e) => { e.preventDefault(); send(input); }}
        className="mt-3 flex items-center gap-2 border border-glass rounded-lg bg-surface px-3 py-2"
      >
        <input
          value={input}
          onChange={(e) => setInput(e.target.value)}
          placeholder="Ask MonsterGPT…"
          disabled={busy}
          className="flex-1 bg-transparent text-sm focus:outline-none text-gray-100 placeholder:text-gray-600"
        />
        <button type="submit" disabled={busy || !input.trim()} className="p-1.5 rounded-md bg-violet/20 text-violet-light disabled:opacity-40 hover:bg-violet/30">
          <Send size={16} />
        </button>
      </form>
    </div>
  );
}
