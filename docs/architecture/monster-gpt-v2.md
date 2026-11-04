# MonsterGPT v2: general sports intelligence (Phase 1 design)

Status: design, 2026-11-03. Builds on `monster-gpt.md` (M1-M6, all done) and the Phase 0
audit (`%LOCALAPPDATA%\MonsterOS\reports\phase0\phase0-report.md`). Nothing here is
implemented yet unless it says "exists".

Goal: MonsterGPT answers general sports questions (history, standings, player and team
stats, matchups, simulations, model reliability), not only bets, and every number
in an answer is traceable to a stored record or a model run, with a status saying which.

## 1. Decisions

| Decision | Choice | Why |
| --- | --- | --- |
| Stack | Keep Rust (actix-web) services and React + Vite dashboard | Working, tested (305 Rust tests); the master plan says reuse compatible infrastructure. No FastAPI or Next.js. |
| New services | None. Facts are served by a new `knowledge` module in projection-service; orchestration, verification and citations stay in monster-gpt | projection-service already owns the data and the engines; one more process costs RAM this machine does not have (about 4 GB free) |
| Who computes numbers | Code only. Tools return facts; the model writes words around them | Existing rule ("numbers come from tools, words come from the model"); extended here to per-claim status |
| Citations | Attached by the server, not written by the model | A 3B model is unreliable at producing citation tags; matching each number in a sentence to the fact that contains it is deterministic and testable |
| Intent routing | Rule-based router stays primary; the model may classify only when the rules return `General`, with a JSON schema and a fixed label set | Deterministic and tested now (75 tests); measured on the golden set before the model fallback is switched on |
| Vectors | Keep `real[]` + brute-force cosine | 297 chunks; pgvector only above about 50K chunks (needs an image change and approval) |
| Model | qwen2.5:3b on the 4 GB GPU, provider interface unchanged | Measured 51 tok/s; larger models do not fit |
| Paid odds feeds | The Odds API and SportsGameOdds are paid plans (confirmed by the owner 2026-11-03). They stay **optional**: every workflow except market-price questions works without them, and the registry records the cost | Master plan 2.1 forbids paid *mandatory* dependencies and asks for unavoidable costs to be documented |
| New sports | Not before the source registry has a reviewed entry for that sport's data | Master plan 6; no sport is shown as supported without a working pipeline |

## 2. Architecture

```
Dashboard  MonsterGpt.tsx  (chat, @ picker, research panel, claims + citations, Locks)
   │ WebSocket protocol v2 (v1 frames unchanged, new frames added)   REST: /entities/search
   ▼
monster-gpt :8085
   ├─ router.rs        intents (20 categories, §5) + entities (§4) + filters → Plan
   ├─ planner.rs  NEW  Plan = ordered tool calls with typed args; one plan per intent mix
   ├─ tools.rs         registry v2 (§6): typed args/results, timeouts, audit, cache
   ├─ facts.rs    NEW  Fact records from tool results (§7.1), ids F1..Fn per answer
   ├─ card.rs          bet card (exists)
   ├─ orchestrator.rs  plan → tools → facts → evidence block → stream → verify
   ├─ verify.rs   NEW  (grows from validate.rs/judge.rs) sentence → claims → status,
   │                   citations, conflicts, staleness; no-answer policy (§7.4)
   ├─ rag.rs           hybrid retrieval (exists) + document kind metadata
   ├─ store.rs         conversations, messages, tool audit, claims (migration 0042)
   └─ ws.rs            protocol v2 frames (§8)
        │ HTTP (read-only, localhost)
        ▼
projection-service :8121
   ├─ knowledge.rs NEW  /kb/* : standings, results, champions, player/team stats,
   │                    head-to-head, splits, with as_of + source + row counts
   ├─ entities.rs  NEW  canonical ids + aliases + resolver (/entities/*)
   ├─ {nba,nfl,nhl,mlb}_game simulate (exists: seed, n ≥ 100k, mcse, out/in overrides)
   ├─ validation, picks, hub, learning (exist)
   └─ ingest (exists) + source registry rows (migration 0041)
        ▼
Postgres :5434  (sports_games, player_game_logs, nhl_games_api, prediction_ledger, ...)
```

## 3. Data contracts (new migrations)

### 0040 `entities`

```sql
CREATE TABLE entities (
  entity_id   text PRIMARY KEY,           -- 'pl:nba:000123', 'tm:nhl:TOR', 'gm:nfl:2026_10_KC_BUF'
  kind        text NOT NULL CHECK (kind IN ('player','team','game','league','venue','coach')),
  sport       text NOT NULL,
  name        text NOT NULL,              -- display name
  team_id     text,                       -- current team for players (entity_id)
  active_from date, active_to date,
  attrs       jsonb NOT NULL DEFAULT '{}' -- position, jersey, city, conference ...
);
CREATE TABLE entity_ids (                 -- source identifiers, many per entity
  entity_id text REFERENCES entities, source text, source_id text,
  PRIMARY KEY (source, source_id));
CREATE TABLE entity_aliases (             -- names, abbreviations, historical names
  entity_id text REFERENCES entities, alias text, norm text, valid_from date, valid_to date,
  weight real NOT NULL DEFAULT 1);
CREATE INDEX ON entity_aliases (norm);
```

Built from `sports_players` (13,456), `espn_athletes` (917), team tables and
`hitrates::canon` team codes. Built by a deterministic job and safe to rebuild; the ids
are stable because they are derived from the primary source id, not from row order.

### 0041 `data_sources`

```sql
CREATE TABLE data_sources (
  source_id text PRIMARY KEY, name text, url text, sports text[], data text[],
  access text CHECK (access IN ('official_public','open_data','paid_api','undocumented_public')),
  licence text, terms_url text, terms_reviewed_at date, terms_notes text,
  attribution text, rate_limit text, cost text, priority int,   -- 1 official ... 5 archive
  status text CHECK (status IN ('approved','review_pending','restricted','retired')));
```

Facts carry `source_id`. A source in `restricted` or `review_pending` status can still
be read for existing internal use, but MonsterGPT shows its facts with the attribution
and never reproduces raw records in bulk. Initial rows (terms are **not yet reviewed**;
reviewing them is a Phase 3 task):

| source_id | Data | Access | Cost |
| --- | --- | --- | --- |
| espn_site | schedules, scores, injuries, live, standings | undocumented_public | free |
| nflverse | NFL play-by-play, rosters, schedules (incl. postseason) | open_data (licence per repo) | free |
| nhl_api | NHL games, shots, box scores, playoffs | undocumented_public | free |
| mlb_statsapi | MLB games and stats | official_public (terms restrict use) | free |
| nba_stats | NBA stats, referees | official_public (terms restrict use) | free |
| moneypuck | NHL shot data | open_data with attribution | free |
| the_odds_api | market prices | paid_api | paid plan, 9 credits/day budget |
| sportsgameodds | market prices, props | paid_api | paid plan, 2,500 entities/month |
| polymarket, kalshi | prediction-market prices | official_public | free |

### 0042 `gpt_claims`

```sql
CREATE TABLE gpt_claims (
  id bigserial PRIMARY KEY, message_id text NOT NULL, sentence_ix int NOT NULL,
  text text NOT NULL, status text NOT NULL,       -- §7.2
  facts jsonb NOT NULL,                           -- [{fact_id, value, source_id, as_of, ref}]
  issues jsonb NOT NULL DEFAULT '[]', created_at timestamptz NOT NULL DEFAULT now());
```

Write-once (same trigger pattern as `feature_snapshots`). It makes every past answer
auditable: which sentence rested on which record.

### 0043 `gpt_sim_runs`

Chat-requested simulations are **not** written to `prediction_ledger` (that ledger holds
the champion's frozen forecasts and is scored). They go here: `run_id, sport, game_id,
engine, model_version, seed, n, inputs_sha256, overrides (out/in), status (queued |
running | done | failed | cancelled), result jsonb, requested_by, created_at, finished_at`.
Re-running with the same seed and inputs must reproduce the result (tested).

## 4. Entity resolution

Input: a mention ("James", "the Leafs", "Mahomes vs top-5 defenses"), the
conversation's focus (previous entities, sport) and the question's sport words.

1. Normalise (`norm` = lowercase alphanumerics) and look up `entity_aliases.norm`
   (exact), then prefix and token-subset matches, then edit distance ≤ 2 for 6+ letters.
2. Score each candidate: alias weight + same sport as the question (+2) + in focus
   (+3) + active this season (+1) + plays in a game on the slate in question (+1).
3. Exactly one candidate clearly ahead (top score ≥ second + 2) → resolved.
   Otherwise → **ask**: "Which James: LeBron James (LAL) or Jalen James (...)?" with up
   to 4 options; no tool runs on a guess.
4. The @ picker calls `GET /entities/search?q=&sport=&kind=` and inserts the
   `entity_id` directly; picked entities skip steps 1-3.
5. Resolved entities join the conversation focus (`gpt_messages.state`) and are
   inherited by follow-ups until another entity of the same kind replaces them.

Historical names are aliases with validity dates (`Washington Redskins` valid to 2019 and
`Washington Football Team` 2020-2021, both for the Commanders' entity); a question about a
season uses the alias valid in that season. A relocated franchise that the league treats as
new (not a rename) gets a new team entity.

## 5. Intents

The 20 categories from the master plan, grouped by the tool plan they need. A question
can carry several (for example "compare X and Y and run a sim").

| Intent | Tool plan |
| --- | --- |
| general_knowledge, terminology | rag.search (docs + glossary); no numbers unless a tool supplies them |
| historical_research, records | kb.results / kb.champions / kb.player_stats (career windows) |
| current_info, schedules_standings | hub.slate, kb.standings, hub.live |
| game_analysis, matchup | hub.brief + kb.head_to_head + kb.team_stats |
| player_analysis, player_comparison, trends | kb.player_stats (windows, splits) |
| team_analysis, team_comparison | kb.team_stats, kb.standings |
| statistical_research | kb.* + calc.* (§6) |
| injuries_lineups | injuries.current (feature_snapshots rows) + hub.brief lineups |
| news | **not offered** until a licensed news source exists; answer says so |
| prediction_research, player_props | picks.today, projections, model.validation |
| odds_comparison | odds.screen (paid feeds; optional) |
| simulation | sim.run (§6) |
| parlay_research | parlays (exists) |
| model_diagnostics | model.validation + model.diagnostics |
| tactical_analysis | rag.search + kb stats; flagged as commentary, not fact |

Router v2 keeps today's `Intent` enum as the bets path and adds `Research(Vec<Category>)`.
Every category has a set of trigger patterns and tests; anything unmatched is `General`.

## 6. Tool registry v2

Every tool: typed args (validated before any network call), typed result, timeout,
audit row in `gpt_tool_audit`, `source_id` + `as_of` on every fact, 30 s cache (exists).
No tool accepts SQL, code or URLs from the model.

| Tool | Backing | State |
| --- | --- | --- |
| picks.today, picks.record, hub.brief, hub.slate, model.validation | projection-service | exists |
| entities.resolve / entities.search | `/entities/*` (0040) | new |
| kb.standings {sport, season?, as_of?} | computed from `sports_games` finals (W-L, pct, GB, home/away, last 10, streak); NHL points from `nhl_games_api` incl. OT/SO; ESPN standings as a cross-check source | new |
| kb.results {sport, team?, season?, from?, to?, game_type?} | `sports_games`, `nhl_games_api` | new |
| kb.champions {sport, season} | last postseason series/game; NBA/MLB/NHL derivable now; **NFL postseason is not stored** (ingest keeps REG only) → Phase 3 fix | new |
| kb.player_stats {entity, stats[], window: last_n / season / career / dates, split?: home/away/opponent} | `player_game_logs` (stat keys per sport, e.g. NBA pts/reb/ast/fg3a, NHL goals/shots/toi, MLB h/hr/k, NFL passing_yards...). Returns games used (ids, dates), n, mean, median, sd | new |
| kb.team_stats {entity, window, metrics[]} | `sports_games` (points for/against, margins), sport tables (NFL EPA from `nfl_play_metrics`, NHL shots/xG from `nhl_shots_api`) | new |
| kb.head_to_head {a, b, window} | `sports_games` | new |
| injuries.current {sport, team?} | latest `feature_snapshots` espn_injuries rows | new (data exists) |
| sim.run {sport, game, n, seed?, out?, in?} | `/{sport}/simulate/{game}` (exists: n 100k-1M, seed, mcse, overrides) → `gpt_sim_runs` | new tool, async job (§8) |
| calc.odds {american/decimal/fractional} → implied, no-vig (power method), EV | `pricing` code in monster-gpt (derived facts exist) | extend |
| calc.stats {op: mean/median/sd/percentile/ci/ttest/corr, over fact ids} | pure Rust, on numbers already in the answer's facts; never on model text | new |
| odds.screen {sport, market} | odds-service `/screen` (paid feeds) | wrap |
| model.diagnostics {sport, market} | `/validation/scorecard` + `/learning/{sport}/scorecard` with metric definitions (MAE, RMSE, Brier, log loss, calibration bins, n, period) | extend |
| rag.search | exists | exists |
| news.search | none: no licensed free news source identified | **not offered** |

Limits: at most 6 tool calls per answer; sim.run n capped at 1,000,000 and one
simulation job at a time (CPU); kb queries capped at 5,000 games per call.

## 7. Evidence and verification

### 7.1 Facts

Each tool result becomes facts:
`{ fact_id: "F3", kind: observed | computed | model_estimate | hypothetical | reference,
   subject: entity_id, metric, value, unit, window, n, source_id, ref (table + keys or URL),
   as_of, fetched_at, model_version?, seed? }`.
`computed` facts (averages, standings) keep the ids of the rows they were computed from
(`ref.rows`), so a reviewer can recompute them. The evidence block given to the model
lists facts as `[F3] Connor McDavid points per game, last 20 games: 1.45 (n=20, NHL API, as of 2026-11-02)`.

### 7.2 Claim status

The verifier splits the answer into sentences and finds the claims in each: numbers,
entity names, dates and comparisons ("more", "best", "over"). Each number is matched to
facts (same value after rounding, and the subject named in the sentence is the
fact's subject). The sentence status is the weakest claim in it:

| Status | Rule |
| --- | --- |
| VERIFIED | every claim matches an `observed` or `computed` fact from an approved or official source |
| CROSS-VERIFIED | as VERIFIED, and an independent second source agrees (e.g. computed standings = ESPN standings) |
| MODEL-ESTIMATED | the claim matches a `model_estimate` fact (projection, sim, probability); shown with model version and validation status |
| HYPOTHETICAL | matches a fact from a scenario run (overrides set) |
| STALE | matching fact older than its freshness limit (standings 24 h, injuries 2 h, prices 90 min, live 2 min) |
| CONFLICTING | two facts for the same subject + metric + window disagree beyond tolerance; both are shown with source and time |
| PARTIALLY VERIFIED | some claims matched, some did not |
| UNVERIFIED | a number or name with no matching fact |

Unverified sentences are repaired or removed (`judge::repair` / `scrub`, exists), never
shown as fact. Comparisons are checked against the facts' values ("X has more rebounds
than Y" with F1 < F2 is a flip and is corrected, as over/under flips are today).

No confidence percentage is shown. If one is added later it needs a documented
calculation and a calibration check (master plan 9.3).

### 7.3 Citations

Each verified sentence gets the citations of its facts: source name, record title (e.g.
"NHL API boxscore, 2026-10-30 TOR @ BOS"), internal ref or URL as stored, as_of,
retrieved-at. URLs are only those stored with the record; none are generated. The
dashboard shows them as footnotes; `gpt_claims` stores them.

### 7.4 No-answer policy

If the plan's tools return nothing usable, the answer is built in code (as the empty-card
path does today): what was asked, which tools and sources were searched, what each
returned, and that the fact could not be verified. Historical questions with no stored
record never fall back to the model's memory.

## 8. WebSocket protocol v2

v1 frames are unchanged; a v1 client ignores the new ones. New and extended frames:

| Frame | Meaning | Maps to master-plan event |
| --- | --- | --- |
| `status {stage: intent}` with `detail` = categories | intents resolved | intent.resolved |
| `entities {request_id, entities: [{mention, entity_id, name, confidence}]}` | resolution result | entity.resolved |
| `clarify {request_id, question, options}` | ambiguous mention; no tools run | — |
| `status {stage: tool, name}` (exists) | research steps | research.started / source.retrieved |
| `job {request_id, job_id, kind: sim, state, progress?}` | async simulation | simulation.completed |
| `claims {request_id, message_id, claims: [{sentence_ix, status, citations}]}` | after verification | verification.completed |
| `delta`, `reset`, `done`, `error` (exist) | streaming | response.delta / chat.completed / chat.failed |

Recovery: every frame of a finished answer is rebuilt from `gpt_messages` + `gpt_claims`
+ `gpt_sim_runs`; a reconnecting client sends `resume {request_id}` and gets the stored
answer (today: resend). A simulation job keeps running when the socket drops and its
result is attached to the message when done.

## 9. RAG changes

- Add `kind` to `rag_chunks`: methodology, architecture, model_card, research, glossary.
  Retrieval filters by kind per intent (terminology → glossary first).
- A sports glossary (definitions of stats such as TS%, EPA, Corsi, wOBA, xG, with the
  formula Monster OS uses), written in-repo, so terminology answers have a source.
- RAG never supplies live values or sports statistics; a number found only in a
  retrieved chunk is UNVERIFIED unless the chunk is a methodology document about Monster OS
  itself (status VERIFIED, source = the document).
- Chunks flagged `suspicious` (prompt-injection patterns, exists) stay excluded.

## 10. Security

- Tools are read-only and allow-listed; args validated; `entity_id` must exist.
- No model-written SQL, code, URLs or file paths. kb queries are parameterised.
- Retrieved and tool text goes into the prompt as fenced data; instructions inside it
  are ignored (existing system prompt rule and injection filter).
- Resource limits: tool-call cap, sim cap and one sim job, 2 concurrent generations
  (exists), 20 messages/min per connection (exists), kb row caps.
- Sessions stay loopback-only. Before any network exposure (B2), real auth and
  per-user isolation are required; conversations already carry an owner.

## 11. Acceptance: golden question set

`apps/monster-gpt/tests/golden.json`: about 70 questions, 10 per workflow A-G, each with
the expected intents, entities, tools, the facts the answer must contain (computed by
SQL in the test, not typed by hand), forbidden content, and the expected status of key
sentences. Runs against a scratch conversation owner and deletes nothing in production;
the answers it writes are tagged `owner = 'golden'`.

| Metric | Gate |
| --- | --- |
| entity resolution accuracy | ≥ 95%, and 100% of ambiguous cases ask instead of guess |
| tool selection (expected tools called) | ≥ 90% |
| numeric facts correct vs SQL | 100% |
| sentences shown as VERIFIED that are wrong | 0 |
| invented citations | 0 |
| unverifiable questions answered with a number | 0 |
| median first word / full answer | ≤ 1.5 s / ≤ 8 s |

## 12. Implementation order

Ordered by what unlocks the seven workflows. Each step ends with tests and a report.

| Step | Work | Unlocks |
| --- | --- | --- |
| P2 | CI script running all suites (Rust, pytest, tsc); golden-set harness skeleton; migrations 0040-0043 | baseline gates |
| P3a | Source registry rows; entity build job + resolver + `/entities/*`; NFL postseason ingest from nflverse | C, A (NFL) |
| P3b | `knowledge.rs`: standings, results, champions, player/team stats, head-to-head | A, B, C |
| P4 | Router v2 categories, planner, entity focus, clarify frame | all |
| P5 | Facts, verifier, claim statuses, citations, `gpt_claims`, no-answer path | A-G trust |
| P6 | sim.run jobs + `gpt_sim_runs` + scenario overrides; model.diagnostics with definitions | E, F |
| P7 | Dashboard: @ picker, claims and footnotes, research panel, job progress | UI |
| P8 | Golden set to its gates; then new sports by registry review | release |

### Status (2026-11-03)

| Step | State | Where |
| --- | --- | --- |
| P2 | done | `scripts/ci/research_checks.py`, `scripts/ops/migrate.py`, migrations 0040-0043, `apps/monster-gpt/tests/golden.{json,py}` |
| P3a | done; terms reviewed, statuses left pending by the owner | `entities.rs`, `teams.rs` (`stored_game_type`), migration 0044, `data-foundation.md` |
| P3b | done (head-to-head is `kb.results` with an opponent) | `projection-service/src/knowledge.rs`: `/kb/standings`, `/kb/champions`, `/kb/player_stats`, `/kb/results` (+ `upcoming=true`) |
| P4 | done for the research kinds below; the other master-plan categories still go through the existing intents | `monster-gpt/src/research.rs`, `orchestrator.rs` step 1a, `tools.rs` |

Implemented in P4: research questions of four kinds (standings, champions, a player's
games, a team's results) are detected in code, every name is resolved through
`entities.resolve` (an ambiguous one gets "Which James do you mean: ...?" and the reply
picks one), follow-ups that change only the stat or window keep the players
("what about rebounds?"), and the answer is written in code from the tool results.
A matchup whose game is not on today's slate gets its next scheduled game. A
simulation request is told plainly that chat cannot run simulations yet (P6). New
frame: `entities`. Golden set: 15/17 (A-D, F-H all pass; E waits for P6).

P5 (done, `monster-gpt/src/verify.rs`): every tool result becomes facts by kind
(observed, computed, model estimate, hypothetical, reference) with source, record date
and staleness; calculations an answer makes are computed facts with their inputs
("37.7 - 23.4 = 14.3"). Every answer path (research, next game, diagnostics, empty
card, model answers) is split into sentences; each number and name is matched to the
facts holding it and the sentence gets a status (verified, cross_verified,
partially_verified, conflicting when two sources disagree on a subject's value,
unverified, stale, model_estimated, hypothetical) and citations to this turn's
sources only. Questions back to the user and the user's quoted words are not claims.
Stored write-once in `gpt_claims`; sent as a `claims` frame. A factual question that no
record or document answers gets a code-built "I can't verify that" listing what was
searched (no answer from model memory). Golden: 16/18, 74 claims stored, none of the
expected facts outside a verified sentence. Not shown in the dashboard yet (P7).
Cross-verified and conflicting need a second source for the same value; none is
wired yet, so in practice answers are verified or model_estimated.

P6 (done, `monster-gpt/src/sim.rs`, orchestrator `sim_turn`): "run 100,000 simulations
of the next Lakers game", "simulate Edmonton's next game 200,000 times", "simulate the
Nuggets without Jokic". The team's next game comes from `kb.schedule`; named players
are ruled out. Each run is a `gpt_sim_runs` row (seed, n, overrides, model version,
inputs fingerprint `fnv64:...` in `inputs_sha256`, compact result), run as a background
task holding the one simulator slot, with `job` frames (queued, running, done, failed,
cancelled); a dropped connection does not lose the run, cancel marks it cancelled. A
scenario also runs the baseline with the same seed; its numbers are hypothetical and a
change within twice the run-to-run error is called "no measurable effect". Counts are
held to the engines' 100,000-1,000,000.

Monte Carlo error: the engines sample 200 parameter draws per run, so the binomial
error sqrt(p(1-p)/n) (also what the engines' `mcse` fields report) understates the
seed-to-seed spread. Measured 2026-11-03 over six seeds x 100,000 games: sd of the home
win share 0.33 (NBA), 0.37 (NHL), 0.33 (NFL) points, 2.1-2.4x binomial. MonsterGPT
states `sqrt(p(1-p)/n + 1.0e-5)` (`sim::mc_error_pts`), which barely shrinks with more
games; `tests/test_sim_repro.py` (CI step sim-repro) checks same-seed reproduction and
that two seeds agree within it.

Model finding (not changed here): ruling Nikola Jokic out of DEN v TOR (2026-11-04)
moved Denver's simulated win share from 46.3% to 46.0% (seed 5), within noise. The
`out` override reaches the engine (he leaves the player list), so the NBA possession
simulator barely responds to losing a star, presumably because the team ratings carry
his production. Worth a research item before scenario answers are trusted for the NBA.

Model diagnostics now end with definitions of the measures they use (Brier score, log
loss, CLV, ROI, calibration, MAE, RMSE, drawdown, 95% interval, walk-forward), cited as
a reference source. Golden: 19/19 (E1-E3 run real simulations).

Engine Monte Carlo error (2026-11-03, projection-service `mc.rs`): every simulator
histogram now records each parameter-draw batch's count and sum, and a line's `mcse` is
the binomial error times the square root of the design effect estimated from the batch
means (NBA/NHL `nba_sim::Hist`, NFL `nfl_sim::Hist`, MLB `mlb_sim::Hist`). It is display
only (no pick or blend reads it). Measured against reruns: NBA total line 0.0044 vs a
6-seed sd of 0.0047; NHL 0.0046 vs a 16-seed sd of 0.0037; NFL 0.0035 vs 0.0026; the old
binomial figure was 0.0016. Using the mean's design effect for a line probability is an
approximation that runs up to about 35% high, the safe side.

P7 (frontend, `apps/dashboard/src/components/GptEvidence.tsx`, `pages/MonsterGpt.tsx`):
- every sentence of an answer is matched to its claim: verified sentences read as plain
  text with footnote numbers; model estimates (dashed violet), hypotheticals (dashed
  sky), stale (dotted amber), partly verified (wavy amber), conflicting or unverified
  (wavy red) are marked, with the status and issues as a tooltip and in screen-reader
  text; under the answer a status count, the issues, and numbered sources (source,
  as-of date, retrieval time); saved conversations reload their claims;
- the players and teams an answer resolved to are chips above it; a simulation shows
  "Simulation queued / running / done · 11.6 s";
- the composer is a combobox: "@" plus two letters suggests players and teams from
  `/entities/search` (arrow keys, Enter or Tab, Escape); picked names travel as
  canonical ids (`context.entities`) and MonsterGPT uses them instead of resolving
  the name again (research and simulations);
- checked on a scratch stack (`apps/monster-gpt/tests/scratch_stack.py`: scratch
  database, monster-gpt :8185, gateway :8191, dashboard :5191) so no test conversation
  reaches production; at 390 px there is no horizontal page scroll and no chat element
  wider than the viewport.

Citations are scoped to the sentence's subject: when a sentence names a player or team,
a number held by several records ("last 20 games") is cited from the named one's record.

Not yet: news; tactical analysis,
parlay research, odds comparison beyond the existing bet card; injuries as a research
answer; the model-based intent fallback (not needed by the golden set so far).

## 13. Open risks

- Undocumented endpoints (ESPN, NHL) can change or close; the registry marks them and
  facts keep their source so a change is visible.
- Terms of NBA and MLB stats sites may restrict reuse; the review may remove sources.
- 3B model prose quality: the verifier guarantees facts, not good reasoning.
- Disk (13 GB free) and RAM (about 4 GB free) limit everything above; B3/B4 backups are
  still missing.
- News stays unsupported until a licensed source exists.
