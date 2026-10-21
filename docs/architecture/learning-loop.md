# Adaptive learning loop

Status: first version, 2026-10-20. Code: `apps/projection-service/src/learning.rs`
plus the per-sport adapters (`slate`, `predict`, `outcomes`, `evaluate`) in
`nfl_game.rs`, `mlb.rs`, `nba_game.rs` and `nhl_game.rs`. Schema:
`infrastructure/postgres/migrations/0015_learning_loop.sql`. UI: `/models`
(Model Intelligence).

## The loop

    ingest results ─▶ score frozen forecasts ─▶ freeze the next slate
          ▲                                            │
          └──── engines rebuild player / team state ◀──┘
                from all data before each game

1. **Freeze (before the game).** `POST /learning/{sport}/freeze` runs the
   champion model's production prediction, at least 100,000 simulated games
   with 200 parameter draws. It stores:
   * the full response, plus its SHA-256 hash, in `prediction_ledger`;
   * every scoreable binary forecast in `prediction_forecasts`. Each forecast
     has the model probability and a baseline probability computed only from
     data before the game.

   A game is never frozen after its scheduled start. The database also
   rejects a row whose `frozen_at` is not before `game_start`. When a source
   has no start time (NFL), the game must be on a later calendar day.
2. **Score (after the game).** `POST /learning/{sport}/score` writes each
   forecast's outcome exactly once. A forecast is void when:
   * the player did not play (props are conditional on playing);
   * the result lands exactly on a market line (a push).
3. **State update.** The engines don't keep a mutable state that could drift.
   Every prediction rebuilds each player's and team's estimates from all
   games before the game date: recency-weighted rates shrunk toward league
   priors, which is a conjugate-style posterior. Ingesting a result therefore
   updates every later prediction. League parameters are refit per season
   (`POST /fit/{sport}...`).
4. **Cycle.** `POST /learning/{sport}/cycle` does steps 1–3 in order: ingest
   the last three days, score, then freeze today's and tomorrow's slates.

## Engines

The registry key is the engine; each has its own champion, ledger rows and
record.

| Key | Engine | Markets |
|---|---|---|
| `nfl`, `mlb`, `nba`, `nhl` | the play / plate-appearance / possession / event simulators | game markets and player props |
| `nfl_team`, `nba_team`, `nhl_team` | team-points model (`team_engine.rs`) | the same sport's game markets, same lines and baselines |

The team-points engine is the Projections page's log-linear Poisson model,
refit for every game on that sport's games strictly before the game date,
with a per-game recency weight. It has no players or lineups, so it freezes
once per game (lineup mode `none`). Its tunables are `halflife_days`, `ridge`
and `lookback_days`.

## Autopilot (`autopilot.rs`)

Runs inside the service. `GET /learning/autopilot` shows the schedule, the
last run of every job and the shadows' live progress. `POST
/learning/autopilot?paused=true|false` pauses or resumes it. `POST
/learning/autopilot/run?job=tick` (or `job=tune&engine=nba`) runs a job now.
`LEARNING_AUTOPILOT=off` keeps it from starting.

Every 10 minutes:

1. **Ingest.** NBA and NHL every 30 minutes, MLB and NFL every 6 hours: the
   last three days and the next two.
2. **Score** every engine's finished games.
3. **Guard** shadows (below).
4. **Freeze** the champion and every shadow:
   * projected lineups when the game starts within 36 hours;
   * confirmed lineups within 90 minutes of the start;
   * NFL has no start times, so it is frozen on the days before, with
     confirmed lineups the day before.

**Tuning**, once a day per engine between 08:00 and 14:00 UTC:

1. **Propose.** Change one tunable by ×1.5 or ÷1.5, within its bounds. The
   choice is deterministic per engine and day, and versions already tried are
   skipped.
2. **Search.** Champion and candidate run on 300 games of the older of the
   two latest seasons with 200+ testable games (2,000 sims × 20 draws each).
   The search uses the promotion gate above.
3. **Holdout.** The same comparison on the latest season's most recent 300
   games. A market that improved in the search must improve again by 2 SE,
   and no market may get worse by 2 SE. This guards against the lucky winners
   that come from trying many changes.
4. **Shadow.** A passing candidate becomes a `shadow`. Only one fresh shadow
   per engine is on trial at a time.

**Guard:**

* **Promote.** A fresh shadow is promoted once it has 50 paired live games
  (NFL: 30) with no market worse than the champion by 2 SE. Paired means the
  same games, lineup mode, subjects and lines.
* **Reject early.** A shadow that is 2 SE worse on any market after half that
  many games is rejected.
* **Rollback reference.** After a promotion the old champion stays a shadow
  (`promoted_at` set).
  * If it beats the new champion by 2 SE on any market, the new champion is
    rolled back and marked rejected.
  * After twice the trial length without that, it is retired.

Job runs are kept in `learning_jobs`; the pause switch is in
`learning_settings` (migration 0017). Every action is in the audit trail:
`register`, `auto_reject`, `auto_shadow`, `shadow_reject`, `auto_promote`,
`auto_rollback`, `watch_end` and `freeze`.

**Running unattended.** The autopilot only runs while the service does.
`scripts/windows/run-projection-service.ps1` is a supervisor:
* it waits for Postgres;
* it loads `.env` into its own process;
* it restarts the service if it exits;
* it logs to `%LOCALAPPDATA%\MonsterOS\logs`.

`scripts/windows/install-projection-service-task.ps1` registers it as a
logon task, and has to be run by the user.

## Integrity rules (enforced by the database)

* `prediction_ledger` rows cannot be updated or deleted (trigger).
* A forecast's prediction fields cannot change, and its outcome can be
  written only once (trigger).
* At most one champion per sport: a deferrable exclusion constraint, checked
  at the end of each statement so the one-statement swap in promote and
  rollback works whatever order the rows are stored in (migration 0016).
* `GET /learning/ledger/{id}` recomputes the payload hash and reports
  `hash_verified`.

## Model registry

A model version is an engine (code, `MODEL_VERSION`) plus a configuration
overlay of declared hyperparameters (half-lives and prior strengths). Each
engine declares its knobs once with defaults and bounds (the `tuning!`
macro). `GET /learning/registry` lists them, and registration rejects unknown
keys and out-of-range values.

| Step | Endpoint | Rule |
|---|---|---|
| register | `POST /learning/{sport}/candidates {"config": {...}}` | config must differ from the champion |
| evaluate | `POST /learning/{sport}/evaluate?version=&season=&games=` | champion and candidate simulate the **same historical games with the same seeds**, inputs strictly before each game; paired Brier differences per market |
| promote | `POST /learning/{sport}/promote?version=` | only if the stored evaluation was against the current champion and passed the gate; a failed candidate is marked `rejected` |
| rollback | `POST /learning/{sport}/rollback` | restores the most recently retired champion on the current engine |

**Engine upgrades.** When the code's `MODEL_VERSION` changes, the next
registry call retires the champion and installs the same configuration under
a version that names the new engine (audit action `engine_upgrade`). Ledger
rows therefore always name the code that produced them. Candidates evaluated
against the old champion must be evaluated again.

**Promotion gate:**
* at least one market improves by ≥ 2 standard errors of the paired
  difference;
* no market gets worse by ≥ 2 SE;
* only markets with at least 200 paired forecasts count.

Every register, evaluate, promote, reject, rollback, freeze and score is
written to `learning_audit`.

## Shared definitions

The walk-forward backtests and the ledger use the same `Forecast` and
`Outcomes` code, so a market's backtest Brier score and its live Brier score
measure the same thing.

**NFL, NBA, NHL.** The backtest calls the sport's `evaluate` per game.

**MLB.** The backtest keeps its richer report format, which the MLB page
reads. Its adapters use the same market definitions.

## Known limits

* **Hyperparameters only.** Candidates are hyperparameter overlays. Changes to
  the engine itself still need code, a new `MODEL_VERSION`, and a backtest.
* **Narrow search.** The autopilot changes one tunable at a time by a fixed
  factor; it is a slow coordinate search, not a full optimiser.
* **Live baselines.**
  * NBA/NFL market baselines use the line stored at freeze time, not the
    closing line.
  * NHL and MLB have no stored betting lines, so their game baselines are
    league rates.
* **Unattended only while the service runs.** The autopilot lives in the
  service process; install the logon task to keep it running across restarts.
* **League parameters.** Per-season league fits (`/fit/{sport}/sim`) and the
  MLB prop models are not refit automatically.
* **Live sample size.** Live records start small, and a market needs a few
  hundred resolved forecasts before its standard error allows a verdict.
