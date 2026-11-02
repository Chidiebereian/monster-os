# Research orchestration (PROPOSAL, not implemented)

Status: design for review (2026-11-01, revised for Phase 2B). Nothing in this document is built; deployment waits for the governance review. It
adds a research lane to the existing learning autopilot
(`projection-service/src/autopilot.rs`); it does not add a scheduler.

## 1. What exists and is reused

| Piece | Today | Reuse |
|---|---|---|
| Scheduler | autopilot loops in projection-service (10 min tick, slow loop, `due(job, minutes)` / `mark(job)` on `learning_jobs`) | the research lane is one more `due()`-gated step in the slow loop |
| Job state | `learning_jobs` (last run / ok / detail per job name), `learning_audit` (append-only actions) | kept for production jobs; research tasks get their own table (below) |
| Research work | `scripts/ml/{dataset,features,experiment,backtest_runner}.py`, research-mode backtests (`mode=research`) | executed by the lane, unchanged |
| Isolation | `monster_research` role, explicit backtest mode, publication holds, research gate | the lane runs every research task under the research role |
| Monitoring | `/learning/autopilot`, Models tab, logs in `%LOCALAPPDATA%/MonsterOS/logs` | a "Research" panel and `/research/tasks` read endpoint |
| Live updates | gateway WebSocket (`:8090`) used by MonsterGPT | optional: task state changes published on the existing socket, no polling loop |

## 2. Table `research_tasks` (migration 00xx, proposed)

```sql
CREATE TABLE research_tasks (
    task_id             bigserial PRIMARY KEY,
    idempotency_key     text        NOT NULL UNIQUE,   -- task_type + sha256(parameters + data_fingerprint + code_version)
    task_type           text        NOT NULL CHECK (task_type IN
                          ('dataset_snapshot','feature_snapshot','backtest_batch','experiment_suite','experiment','integrity_check','drift_report','shadow_score')),
    parameters          jsonb       NOT NULL DEFAULT '{}',
    data_fingerprint    text,                          -- input dataset version (ml_examples / feature_snapshots high-water marks)
    code_version        text        NOT NULL,          -- git short hash of the scripts that will run
    model_version       text,                          -- for shadow_score / experiment of a named candidate
    status              text        NOT NULL DEFAULT 'queued' CHECK (status IN
                          ('queued','leased','running','succeeded','failed','retry_wait','cancelled','deferred_resources','dead')),
    retry_count         int         NOT NULL DEFAULT 0,
    max_retries         int         NOT NULL DEFAULT 3 CHECK (max_retries BETWEEN 0 AND 10),
    not_before          timestamptz NOT NULL DEFAULT now(),   -- backoff / scheduling
    created_at          timestamptz NOT NULL DEFAULT now(),
    started_at          timestamptz,
    completed_at        timestamptz,
    worker_id           text,
    lease_expires_at    timestamptz,
    heartbeat_at        timestamptz,
    checkpoint_reference text,                         -- e.g. path of backtest_nba.json, or ml_examples high-water mark
    output_reference    text,                          -- e.g. ml_experiments.id list, feature row counts
    error_details       jsonb,
    resource_class      text        NOT NULL DEFAULT 'light' CHECK (resource_class IN ('light','medium','heavy')),
    cancel_requested    boolean     NOT NULL DEFAULT false,
    depends_on          bigint[]    NOT NULL DEFAULT '{}',          -- tasks whose outputs this one reads
    CHECK (status <> 'leased'  OR (worker_id IS NOT NULL AND lease_expires_at IS NOT NULL)),
    CHECK (status <> 'running' OR started_at IS NOT NULL),
    CHECK (status NOT IN ('succeeded','failed','dead','cancelled') OR completed_at IS NOT NULL)
);
CREATE INDEX research_tasks_ready ON research_tasks (not_before) WHERE status IN ('queued','retry_wait','deferred_resources');
CREATE INDEX research_tasks_leases ON research_tasks (lease_expires_at) WHERE status IN ('leased','running');
CREATE TABLE research_task_events (                -- append-only audit of every transition
    id bigserial PRIMARY KEY, task_id bigint NOT NULL REFERENCES research_tasks, at timestamptz NOT NULL DEFAULT now(),
    from_status text, to_status text NOT NULL, worker_id text, detail jsonb NOT NULL DEFAULT '{}');
```

`research_tasks` rows change state (unlike the write-once research tables), so
every transition goes through one function that also appends to
`research_task_events` (append-only, same guards as `ml_experiments`). Owner of
both tables: the owner role; the lane writes them through a `SECURITY DEFINER`
function `research_task_transition(task_id, to_status, ...)` so the research
role can move its own tasks but cannot write anything else.

### Field review

All proposed fields are kept. Additions: `not_before` (backoff and scheduling
without sleeping workers), `heartbeat_at` (stale detection independent of the
lease length), `resource_class` (light / medium / heavy), `cancel_requested` and
`depends_on` (lineage between snapshot and experiment tasks). `model_version` is
optional (only candidate-specific tasks).

### State transitions (enforced in `research_task_transition`)

```
queued ─lease→ leased ─start→ running ─ok→ succeeded
   ↑               │              ├─error, retries left→ retry_wait ─(not_before)→ queued
   │               │              ├─error, no retries→ dead (alert)
   │          lease expired       └─memory floor hit→ deferred_resources ─(not_before)→ queued
   └──────────── (stale worker: back to queued, retry_count + 1)
any non-terminal ─cancel→ cancelled
```

Terminal: succeeded, cancelled, dead. Illegal transitions raise.

## 3. Execution

- **Dispatch**: the slow loop, when `due("research:dispatch", 5)`, leases at most
  one task (`SELECT ... FOR UPDATE SKIP LOCKED`, lease 15 min, heartbeat every
  minute). Concurrency: one research task at a time (single worker), and never
  a heavy task while free memory < 1.5 GB or while a production heavy job
  (evidence run, tuner) is in progress → `deferred_resources` with `not_before =
  now() + 15 min`.
- **Isolation**: the task runs as a child process (`python scripts/ml/...`) with
  only `ML_DATABASE_URL` in its environment (research role), a timeout, and its
  output captured to the task log. Backtests go through `mode=research` only.
  The lane has no code path to `pick_evidence`, `model_registry`,
  `publication_holds` or `ai_picks`, and the role could not write them anyway.
- **Idempotency**: `idempotency_key` is unique; scheduling the same work with the
  same data and code is a no-op. Each script is already idempotent (insert-only
  with ON CONFLICT DO NOTHING; experiments skip identical records).
- **Checkpoint / resume**: backtests resume from stored games (already true);
  `checkpoint_reference` points at the runner's checkpoint. A retry continues,
  it never restarts from zero.
- **Retries**: exponential backoff 5, 20, 80 min (`not_before`), at most
  `max_retries`; data-source failures (provider down, budget exhausted) mark the
  task `retry_wait` and **block dependants**: an experiment never runs on a
  dataset whose snapshot task failed (dependency check on `data_fingerprint`).
- **Stale workers**: a lease past `lease_expires_at` or no heartbeat for 5 min
  returns the task to `queued` with `retry_count + 1` and an event row.
- **Scheduling** (configurable rows in the existing `settings` table, not code):
  dataset + feature snapshot daily and after a settlement batch; experiment
  suite when the dataset grew by ≥ 5% or ≥ 500 rows since the last suite
  (fingerprint change), never twice on the same fingerprint; integrity checks
  daily; drift report weekly.

## 4. Promotion governance (built in Phase 2B, see governance.md)

The lane can only *propose*: a `shadow_score` task that finds a qualifying
candidate files a promotion request (`governance::propose`), exactly as the
autopilot guard does. Approval needs an authenticated admin; the lane has no
token, and its database role cannot touch `promotion_requests`,
`model_registry`, `parlay_models` or `publication_holds`.

## 4b. Resource classes and concurrency

| Class | Examples | Rule |
|---|---|---|
| light | dataset snapshot, feature snapshot, integrity check, report | up to 2 at a time; always allowed |
| medium | experiment suite, drift report | 1 at a time; needs 1.0 GB free and no heavy task running |
| heavy | research-mode simulator backtests, shadow scoring of a full slate | 1 at a time, never beside another heavy or medium task; needs 1.5 GB free and no production heavy job (evidence run, tuner) in progress; otherwise `deferred_resources` |

Dispatch reads free memory (and the production lane's running jobs from
`learning_jobs`) before every lease; a heavy task re-checks between its bounded
batches (as `backtest_runner.py` does today) and checkpoints instead of pushing on.

## 4c. Cancellation

`cancel_requested = true` (through the same transition function). A queued or
deferred task moves to `cancelled` at once; a running task sees the flag at its
next checkpoint (batch boundary), stops cleanly and moves to `cancelled` with its
checkpoint kept, so the work done so far stays usable.

## 4d. Lineage

Each task records `data_fingerprint` (inputs), `code_version` (git hash of the
scripts) and `output_reference`; `depends_on` links an experiment to the snapshot
tasks it reads. An experiment is only dispatched when every dependency
`succeeded` and its fingerprint matches the dataset it will read; a failed or
partial snapshot blocks it (the task stays queued with a reason).

## 4e. Failure notifications

A task reaching `dead`, two consecutive failures of one task type, or a heavy
task deferred for more than 24 h writes a `research_alert` row (append-only)
and shows on the dashboard notification bell; no external service.

## 4f. Rollback of a production change

`python scripts/ops/governance.py request --kind engine --engine <sport>
--action rollback`, then an admin approves it: the previous champion on the
same engine code comes back, recorded like a promotion (governance.md).

## 5. Monitoring and alerts

- `/research/tasks` (read-only) and a Research panel: queue, running task,
  last success per type, deferred-for-resources count, dead tasks, dataset growth.
- Alerts: a `dead` task or two consecutive failures of the same type write a
  `research_alert` audit row and a dashboard banner (existing notification bell);
  no external service.

## 6. Automation inventory (Task 7)

| Need | Status |
|---|---|
| Temporal-leakage regression tests | built (`test_experiment.py`, `test_integrity.py`) |
| Game-level split checks | built (assertion in `experiment.py` + test) |
| Write-once DB tests | built (`test_integrity.py`) |
| Research vs production isolation | built (role, mode, tests); lane would add process-level env isolation |
| Determinism / idempotency checks | built (tests) |
| Feature provenance validation | built (tests) |
| Publication-hold regression tests | built (tests) |
| Resumable backtests + checkpoint + memory floor | built (`backtest_runner.py`) |
| Local CI gate incl. secret scan | built (`scripts/ci/research_checks.py`) |
| Periodic scheduling, dispatch, retries, stale-worker recovery, alerts | proposed (this document) |
| Human approval for production promotion | built: admin-token principals, promotion_requests lifecycle, retired direct endpoints |
| Promotion audit trail | built: `promotion_request_events` (append-only) and `learning_audit` |

## 7. Risks for review

1. **Resolved in Phase 2B:** evidence refreshes no longer change publication; only admin decisions publish (governance.md). Original note: The autopilot's
   evidence jobs (30-day backtests, per-cycle live and line evidence) recompute
   tiers, and tiers decide which picks are visible. This is the validated
   publication rule working as designed, but under a strict reading of "the
   autopilot must not change AI Picks publication status" it is an automatic
   change. Decision needed: keep, or require approval for tier changes that
   unhide a market (e.g. auto-hold any market newly reaching baseline/proven).
2. **Engine-upgrade relabel**: deploying new engine code makes `champion()`
   carry the champion's configuration to the new code (`engine_upgrade`). It is
   triggered by a human deploy, not by the autopilot; documented, unchanged.
3. Resolved in Phase 2B: production changes need an authenticated admin (governance.md).
4. Migration: `research_tasks` is new; no change to existing tables. The
   `SECURITY DEFINER` function must pin `search_path`.
5. Memory: this machine often has 1-2 GB free; heavy tasks will spend long in
   `deferred_resources`. Running the lane at night or moving Postgres/Docker
   memory limits is an operator decision.
