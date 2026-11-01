# MonsterGPT: audit, architecture and plan

Goal: type plain English ("what should I bet tonight with $200, nothing crazy")
and get bets for the day that come from Monster OS's own models and prices,
with the numbers traceable to their source. Longer term: our own GPT-style
model, RAG over Monster OS knowledge, and a real-time chat protocol.

## 1. Repository audit (2026-10-31)

| Area | What exists | Reuse |
| --- | --- | --- |
| `apps/monster-gpt` (Rust, actix-web 4.4.1, :8085) | `POST /ask`: one blocking OpenAI-compatible call (temp 0.2, 180 s timeout), optional non-sensitive fallback endpoint, BM25 over `research/` (15 chunks), RAG pulls from feature-store, model-registry, projection-service `/hub/brief` and `/picks`; derived-facts block (fair odds, edge); sensitive-context guard; 11 tests | Keep: BM25 index, derived facts, sensitivity guard, fallback rules. Replace the blocking call with a streaming provider interface. |
| LLM runtime | `monster-os-ollama-1` (Docker, **no GPU device request**). Models: `qwen2.5:7b` (Q4_K_M, 4.7 GB), `qwen2.5:0.5b`. 7B runs on CPU at about 6 tokens/s; a full `/ask` answer currently exceeds the 180 s timeout and fails. | Use as the "open-weight" provider. GPU passthrough is a separate, opt-in change (container restart). |
| Data/tools | projection-service: `/picks` (graded, staked, playable-price AI picks), `/picks/record`, `/picks/edges`, `/hub/{sport}/schedule`, `/hub/{sport}/game/{id}` and `/brief`, `/hub/brief?q=`, `/hub/{sport}/game/{id}/live`, `/parlays`; odds-service `/screen`; whale-tracker; model-registry | Wrap as typed, read-only tools. No new data sources needed for the first slice. |
| Gateway (:8090) | HTTP reverse proxy `/api/{service}/...`; no WebSocket upgrade support; **no authentication anywhere in the stack** (local, single-user) | WebSocket goes dashboard → Vite proxy → monster-gpt directly in dev. The gateway gets upgrade pass-through in a later milestone. |
| Dashboard | React + Vite + TanStack Query; `pages/MonsterGpt.tsx` (non-streaming chat, Locks carousel, `?q` auto-send with a game brief) | Extend in place; no new frontend. |
| Database | Postgres 16 (:5434), numbered SQL migrations (`infrastructure/postgres/migrations`, latest 0029) | Conversations, messages and audit in a new migration. pgvector is **not** installed; see 6. |
| Python | 3.13, torch 2.8.0 **CPU build**, numpy, pytest, tokenizers, tiktoken, fastapi, websockets | Scratch-model curriculum and training live in Python. |
| Hardware | RTX A1000 Laptop, 4 GB VRAM; 16 GB RAM (about 3 GB free while the stack runs); C: 35 GB free | Scratch "Micro" model (1–10M params) trains on CPU. Larger local models need the GPU passthrough and stay at or below 3B to fit in 4 GB. |

Risks: low free RAM (Docker has wedged before); CPU-only inference latency;
the CPU-only torch build (a CUDA build is a 2.5 GB download); no auth, so the
first auth layer must not break the existing dashboard.

## 2. Architecture

```
Dashboard (MonsterGpt.tsx)
   │  WebSocket  /ws/monster-gpt  (Vite proxy → :8085/ws), protocol v1
   ▼
monster-gpt (Rust)
   ├─ ws.rs            session token + origin check, heartbeats, limits, cancel,
   │                   bounded outbound queue, request-id idempotency
   ├─ orchestrator.rs  route → tools/retrieval → bet card → context → stream
   ├─ router.rs        plain English + previous turn → typed intent & filters
   ├─ tools.rs         typed read-only tool contracts (timeouts, audit, sources)
   │     ├─ picks.today / picks.record / picks.explain  → projection-service
   │     ├─ hub.brief / hub.slate / hub.live            → projection-service
   │     └─ research.search (BM25 now; hybrid RAG in M4)
   ├─ card.rs          deterministic bet card: ranking, stakes, playable prices
   ├─ llm.rs           provider interface: ollama (native, num_ctx) | openai-compatible
   │                   (future: scratch model server) + context budget manager
   ├─ store.rs         conversations, messages, per-turn state, tool audit (Postgres)
   └─ metrics          connections, TTFT, tokens/s, tool latency, cancels, errors
```

Rule of the answer: **numbers come from tools, words come from the model.**
The bet card (picks, prices, books, stakes, grades, timestamps) is built in
code from tool results and sent as structured data. The model only writes the
explanation, and it receives those numbers as fenced evidence. If the model is
slow or down, the card still arrives. The UI labels the parts "From Monster OS
data" and "MonsterGPT's explanation".

## 3. Milestones

Each milestone ends with tests run and results reported.

**M1: Plain-English daily bets (vertical slice).** *This change.*
- Router: sport, day, prop vs game lines, count ("top 3"), bankroll ("$200"),
  risk ("safe" → proven/baseline tiers and B+ or better; "aggressive" → C+ allowed),
  odds preference ("plus money", "favorites"), parlay request, follow-ups that
  inherit the previous turn's filters ("what about hockey?", "only props").
- Tools: picks.today, picks.record, hub.brief, hub.slate, with timeouts, an
  audit row per call, and sources with fetched-at timestamps.
- Bet card: one pick per game/player, playable-now check, Kelly stake from the
  pick's suggested fraction × bankroll, total risk, expected profit.
- Streaming provider interface (Ollama native with `num_ctx`, OpenAI-compatible)
  with a token-budget context manager and history truncation.
- WebSocket protocol v1 (section 4) with cancel, heartbeats, reconnect,
  idempotent request ids, size and rate limits, and a one-generation-per-connection
  plus global LLM concurrency limit.
- Conversations persisted (migration 0030); conversation list in the UI.
- UI: streaming text, tool-status chips, bet card, sources and timestamps,
  Stop, reconnect state.
- Acceptance: "best bets today" returns a card in under 3 s from tool data and
  streams an explanation; a follow-up "only NHL props" narrows the card;
  cancel stops generation; unit tests plus a WebSocket end-to-end test pass.

**M2: Scratch-model curriculum (Python, `models/monster-gpt/`).** Math (gradient
descent, sigmoid/ReLU, softmax, cross-entropy, linear regression), NN from
scratch (neuron, backprop, MLP, init, numerical gradient check), PyTorch
foundations (autograd, LayerNorm/BatchNorm/RMSNorm, training loop, diagnostics,
dead-ReLU detector, digit classifier), NLP (embeddings, preprocessing,
sentiment baseline, positional encodings, masking), attention/transformer
blocks. Acceptance: pytest suite with gradient checks and shape tests passes.

**M3: Monster GPT Micro.** Byte-level BPE (round-trip and Unicode tests),
dataset/loader with deterministic splits and provenance, decoder-only GPT (4
layers, 4 heads, 256–512 context, 1–10M params), training with LR/grad-norm/
throughput logging, checkpoint save/load/resume, generation, KV cache (tested
equal to uncached), real grouped-query attention (tested against an MHA
reference), memory estimator, sequence-length benchmarks (512→4K as CPU allows).
Served behind the same provider interface as a third backend. Honest scope:
a toy that validates the pipeline. It will not answer betting questions; the
open-weight model does that.

**M4: RAG.** Ingestion adapters (docs/, research/, model cards, pick writeups,
methodology), normalization, structure-aware chunking, stable doc/chunk ids,
content hashes for change detection, deletion propagation; embedding interface
(local model), storage decision (section 6), hybrid BM25 + vector retrieval,
metadata and permission filters, thresholds, dedupe, context-aware selection;
evaluation set (precision/recall/MRR, citation correctness, unsupported-claim
rate, latency, freshness, deletion, injection-in-document tests). Live values
(odds, scores, injuries) stay tool calls, never indexed facts.

**M5: Memory and long context.** Conversation summaries, user-approved
preferences (bankroll, risk, books), per-request memory retrieval, deletion;
long-context evaluation of the scratch model.

**M6: Hardening.** Gateway WebSocket pass-through, auth beyond the local
session token if the app ever leaves localhost, load tests (backpressure,
connection limits), admin training/eval dashboard, benchmark regression gates.

### Status (2026-10-31)

| Milestone | State | Where |
| --- | --- | --- |
| M1 plain-English bets | done | `apps/monster-gpt/src/{router,card,tools,orchestrator,ws}.rs` |
| M2 curriculum | done, 46 pytest tests | `models/monster-gpt/monster_gpt/{math,nn,nlp}` |
| M3 Micro | built; training resumable (see `models/monster-gpt/README.md`) | `models/monster-gpt/` |
| M4 RAG | done | `rag.rs`, migration 0031, `tests/rag_eval.json` |
| M5 memory | done (preferences, notes, summaries) | `memory.rs`, `/memory` |
| M6 gateway pass-through | done; load tests and admin dashboard not started | `apps/api-gateway/src/ws.rs` |

Judgement (added beyond the plan). Every model answer goes through
`judge.rs`: numbers not in the evidence, names not in the evidence, over/under
flips against the card, guarantee language, "whole bankroll" claims when the
card risks under half, and a missing risk mention. A serious issue triggers one
rewrite with the issues listed (`reset` frame, then the new text). If names or
numbers are still unsupported, `judge::scrub` removes those sentences (and a
removed bet heading's sub-points) and resends the cleaned text. A missing risk
mention gets one deterministic sentence. A bet request whose card is empty is
answered without a model ("nothing open matches; open now: NHL 25").

Measured (`apps/monster-gpt/tests/eval_answers.py`, 10 questions, GPU):

| | qwen2.5:3b | qwen2.5:0.5b |
| --- | --- | --- |
| median first token | 0.4–0.6 s | 0.4 s |
| tokens/s | ~51 | ~130–150 |
| drafts flagged by the checker | 5/10 | 5/10 |
| final answers still flagged, before scrub + name fixes | 5/10 | 6/10 |
| final answers still flagged, after | 1/10 | 0–2/10 |

RAG at k=3 on 16 labelled queries plus 2 off-topic ones: hybrid P@3 0.61,
R@3 0.89, MRR 0.86; BM25 MRR 0.82; vector MRR 0.83; off-topic queries return
nothing. Warm search about 0.3 s including the embedding.

Known limits: "a 3-leg parlay with $50" reads $50 as the bankroll, not the
parlay stake; the checker cannot judge reasoning quality, only facts against
the evidence; the 7B model doesn't fit beside the 3B on a 4 GB GPU with the
RAM this machine has free.

## 4. WebSocket protocol v1

All frames are JSON `{ "v": 1, "type": ..., ... }`.

Client → server
- `hello { token, resume? }`, with the token from `POST /session` (localhost origin only)
- `message { request_id, conversation_id?, text, context? }`, at most 8 KB
- `cancel { request_id }`
- `ping`

Server → client
- `ready { session, model, limits }`
- `ack { request_id, conversation_id, message_id, duplicate }`
- `status { request_id, stage: route|tool|retrieval|model|judge|validate, name, state: started|queued|completed|failed, detail?, ms? }`
- `card { request_id, card }`, with structured bets and their sources
- `sources { request_id, sources: [{ id, label, url, fetched_at }] }`
- `delta { request_id, seq, text }`
- `reset { request_id, reason }`: discard the text streamed so far; the
  checker is replacing it (rewrite or cleanup). Deltas after it are the answer.
- `done { request_id, message_id, model, cancelled, usage: { ttft_ms, total_ms, completion_tokens, tokens_per_s } }`
- `error { request_id?, code, message }`
- `pong`

Limits: one generation per connection, 2 concurrent generations overall (the
model is CPU-bound), 20 messages/min per connection, 64-frame outbound queue,
ping every 20 s, connection closed after 60 s without traffic. A request_id
seen before replays the stored answer instead of generating twice. A stream cut
by a disconnect is not resumed; the client is told to resend.

## 5. Compute

| Job | Where | Estimate |
| --- | --- | --- |
| qwen2.5:7b chat (before) | Ollama, CPU | 8.8 tok/s; first token up to 52 s |
| qwen2.5:3b chat (default) | Ollama on the RTX A1000 (`infrastructure/docker-compose.gpu.yml`) | ~51 tok/s; first token ~0.5 s warm; kept warm every 20 min |
| AI Picks writeups | projection-service, same `LLM_MODEL` | 3B now; the 7B took over a minute each and starved the GPU and RAM |
| Monster GPT Micro (M3) | CPU torch | 5M params, 512 ctx: minutes per epoch on a few MB of text; under 100 MB of disk per run |
| RAG index (M4) | Postgres | thousands of chunks; a few hundred MB at most |

## 6. Storage decision for RAG (M4)

Corpus size today is small (about 15 research chunks, 20 architecture docs,
several thousand pick writeups). Postgres is already running, and pgvector
would avoid a new service, but it needs a different image (`pgvector/pgvector:pg16`).
Until then, vectors are stored as `real[]` in Postgres with brute-force cosine
in the service, which is fine below about 50K chunks. Revisit at that size.
