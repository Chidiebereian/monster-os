// MonsterGPT client: session token (REST through the gateway), WebSocket
// protocol v1 (Vite proxies /ws/monster-gpt to monster-gpt /ws), and the
// conversation endpoints. See docs/architecture/monster-gpt.md.

export interface GptSource { id: string; tool: string; label: string; url: string; fetched_at: string }
export interface GptBet {
  id: number; sport: string; kind: string; game: string; starts_at: string | null; pick: string; market: string; line: number | null; side: string;
  price: number; book: string; link: string | null; p_model: number; p_market: number; edge: number; ev: number; grade: string; tier: string;
  stake_frac: number; stake_usd: number | null; to_win_usd: number | null; playable_price: number | null; why: string; risks: string[]; source: string;
}
export interface GptParlay {
  legs: number[]; selections: string[]; decimal_odds: number; american: number; p_model: number; ev: number;
  stake_usd: number | null; payout_usd: number | null; to_win_usd: number | null; note: string;
}
/** basis: "requested" = the user named the stake, "bankroll" = sized from their stated bankroll, "unstaked" = no amount given */
export interface GptStaking {
  basis: "requested" | "bankroll" | "unstaked"; parlay_stake: number | null; bankroll: number | null;
  stake_per_parlay: number | null; number_of_parlays: number; total_exposure: number | null;
}
export interface GptCard {
  title: string; generated_at: string; bets: GptBet[]; total_stake_usd: number | null; expected_profit_usd: number | null; notes: string[];
  /** one wager each; legs are listed in `bets` and carry no stake of their own */
  parlays: GptParlay[];
  staking: GptStaking | null;
  available: Record<string, number>;
  filters: { sport: string | null; kind: string | null; day: string | null; count: number | null; bankroll: number | null; risk: string | null; odds: string | null; parlay: boolean;
    parlay_stake?: number | null; legs?: number | null; parlays?: number | null; unclear_amount?: number | null };
}
export interface GptStatus { stage: "route" | "tool" | "retrieval" | "model" | "validate" | "judge"; name: string; state: "started" | "queued" | "completed" | "failed"; detail?: string; ms?: number }
export interface GptUsage {
  ttft_ms?: number | null; total_ms?: number; completion_tokens?: number; tokens_per_s?: number | null; cancelled?: boolean; error?: string;
  unsupported_numbers?: string[]; judge?: { revised: boolean; scrubbed?: number; issues: { code: string; detail: string; serious: boolean }[] }; memory?: unknown; context?: { used_est: number; budget: number; dropped_history_turns: number; evidence_truncated: boolean };
}
export interface GptModel { id: string; provider: string; num_ctx: number }

export type ServerMsg =
  | { type: "ready"; session: string; owner: string; models: GptModel[]; limits: Record<string, number> }
  | { type: "ack"; request_id: string; conversation_id: string | null; duplicate: boolean }
  | ({ type: "status"; request_id: string } & GptStatus)
  | { type: "card"; request_id: string; card: GptCard }
  | { type: "sources"; request_id: string; sources: GptSource[] }
  | { type: "delta"; request_id: string; seq: number; text: string }
  | { type: "reset"; request_id: string; reason: string }
  | { type: "done"; request_id: string; message_id: string | null; model: string | null; cancelled: boolean; usage: GptUsage }
  | { type: "error"; request_id: string | null; code: string; message: string }
  | { type: "pong" };

const BASE = "/api/monster-gpt";
const TOKEN_KEY = "monsterGpt.token";

async function newToken(): Promise<string> {
  const r = await fetch(`${BASE}/session`, { method: "POST", headers: { "content-type": "application/json" }, body: "{}" });
  if (!r.ok) throw new Error(`session: ${r.status} ${await r.text().catch(() => "")}`);
  const t = (await r.json()).token as string;
  try { sessionStorage.setItem(TOKEN_KEY, t); } catch { /* storage blocked */ }
  return t;
}
export async function token(fresh = false): Promise<string> {
  if (!fresh) { try { const t = sessionStorage.getItem(TOKEN_KEY); if (t) return t; } catch { /* storage blocked */ } }
  return newToken();
}

async function authed<T>(path: string, init: RequestInit = {}, retry = true): Promise<T> {
  const t = await token();
  const r = await fetch(`${BASE}${path}`, { ...init, headers: { ...(init.headers ?? {}), authorization: `Bearer ${t}`, accept: "application/json" } });
  if (r.status === 401 && retry) { await token(true); return authed<T>(path, init, false); }
  if (!r.ok) throw new Error(`${r.status}: ${(await r.text().catch(() => "")).slice(0, 300)}`);
  return (r.status === 204 ? undefined : await r.json()) as T;
}

export interface ConversationRow { id: string; title: string; updated_at: string; messages: number }
export interface StoredMessage {
  id: string; request_id: string | null; role: "user" | "assistant"; content: string; card: GptCard | null; sources: GptSource[] | null;
  model: string | null; usage: GptUsage | null; cancelled: boolean; created_at: string;
}
export interface MemoryItem { id: number; kind: "preference" | "note"; key: string; value: string; updated_at: string }
export const gptApi = {
  memory: () => authed<{ items: MemoryItem[] }>("/memory"),
  forget: (id: number) => authed<void>(`/memory/${id}`, { method: "DELETE" }),
  conversations: () => authed<{ conversations: ConversationRow[] }>("/conversations"),
  messages: (id: string) => authed<{ messages: StoredMessage[] }>(`/conversations/${id}`),
  remove: (id: string) => authed<void>(`/conversations/${id}`, { method: "DELETE" }),
};

export type ConnState = "connecting" | "open" | "reconnecting" | "closed";

/** One WebSocket with hello/auth, heartbeat and exponential-backoff reconnect. */
export class GptSocket {
  private ws: WebSocket | null = null;
  private attempts = 0;
  private stopped = false;
  private pingTimer: number | undefined;
  private queue: string[] = [];
  models: GptModel[] = [];

  constructor(private onMsg: (m: ServerMsg) => void, private onState: (s: ConnState) => void) { this.connect(); }

  private url() { return `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/ws/monster-gpt`; }

  private async connect(freshToken = false) {
    if (this.stopped) return;
    this.onState(this.attempts === 0 ? "connecting" : "reconnecting");
    let t: string;
    try { t = await token(freshToken); } catch { return this.retry(); }
    const ws = new WebSocket(this.url());
    this.ws = ws;
    let authFailed = false;
    ws.onopen = () => ws.send(JSON.stringify({ v: 1, type: "hello", token: t }));
    ws.onmessage = (ev) => {
      let m: ServerMsg;
      try { m = JSON.parse(ev.data); } catch { return; }
      if (m.type === "ready") {
        this.attempts = 0;
        this.models = m.models;
        this.onState("open");
        window.clearInterval(this.pingTimer);
        this.pingTimer = window.setInterval(() => this.raw({ v: 1, type: "ping" }), 25_000);
        for (const q of this.queue.splice(0)) ws.send(q);
      }
      if (m.type === "error" && m.code === "unauthorized") authFailed = true;
      this.onMsg(m);
    };
    ws.onclose = () => {
      window.clearInterval(this.pingTimer);
      if (this.ws === ws) this.ws = null;
      if (!this.stopped) this.retry(authFailed);
    };
  }

  private retry(freshToken = false) {
    if (this.stopped) return;
    this.onState("reconnecting");
    const delay = Math.min(15_000, 500 * 2 ** this.attempts) * (0.75 + Math.random() * 0.5);
    this.attempts += 1;
    window.setTimeout(() => this.connect(freshToken), delay);
  }

  private raw(o: object) {
    const s = JSON.stringify(o);
    if (this.ws?.readyState === WebSocket.OPEN) this.ws.send(s); else this.queue.push(s);
  }

  send(m: { request_id: string; conversation_id?: string | null; text: string; context?: unknown; model?: string | null }) {
    this.raw({ v: 1, type: "message", ...m, conversation_id: m.conversation_id ?? undefined, model: m.model ?? undefined });
  }
  cancel(request_id: string) { this.raw({ v: 1, type: "cancel", request_id }); }
  close() { this.stopped = true; window.clearInterval(this.pingTimer); this.ws?.close(); this.onState("closed"); }
}

export const newRequestId = () => `r_${Date.now().toString(36)}${Math.random().toString(36).slice(2, 8)}`;
