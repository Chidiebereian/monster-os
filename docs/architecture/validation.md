# Validation engine

One scorecard for every sport (NBA, NHL, NFL, MLB), engine (simulators and team
models), game market and player prop. It lives in projection-service
(`src/validation.rs`); AI Picks and MonsterGPT both read it, so neither one
invents probabilities, edges or verdicts on its own.

```
data (ESPN, Odds API, SportsGameOdds, stats)  ->  engines (sims, team models)
        -> prediction_ledger / pick_line_evals / ai_picks
        -> validation engine (GET /validation/scorecard)
        -> AI Picks (grades, qualification, Models tab) + MonsterGPT (diag.rs)
```

## Sources (all sports, same code)

| Source | Table | Measures |
|---|---|---|
| forecasts | `prediction_forecasts` x `prediction_ledger` | engine vs its naive baseline: Brier, log loss, calibration error, reliability bins |
| lines | `pick_line_evals` | the raw model at real lines: calibration, Brier vs the de-vigged market |
| picks | `ai_picks` | realised 1-unit results: ROI, hit rate, CLV, max drawdown, Sharpe, Sortino, profit factor |
| backtest | `pick_evidence` | walk-forward seasons vs closing lines or the baseline (via `picks::tier`) |

Every comparison has a 95% bootstrap interval that resamples whole games
(lines of one game move together), so a small or lucky sample cannot look
proven. CLV is one metric among these, not the gate.

## Two grades, kept apart

- **Model quality** (per engine x market): the track-record tier sets the band
  (proven A, beats its baseline B, unproven C, losing D); calibration error at
  real lines sets the step (<=2% top, <=4% middle, else bottom). `I` = fewer
  than 200 settled lines and no backtest.
- **Opportunity** (per pick): EV at the current price (A+ >= 10%, ... C > 0, F <= 0).

Market status: `verified` (beat the market), `model_only` (beats the baseline
only), `no_signal`, `losing`, `insufficient_data`.

Pick qualification (`/picks` adds `model_quality`, `opportunity_grade`,
`ev_now`, `qualification`; filter with `?qualification=`): `verified_edge`,
`watchlist`, `high_uncertainty` (model-only engine with a > 12-point edge, or
no signal), `no_value` (EV <= 0 now), `insufficient_data`, `suspended` (started,
price gone, or quarantined feed).

## Feed quarantine (migration 0034)

For each sport x market x book x side, the scorecard compares the probability
the published prices imply with how often those bets won. A feed is quarantined
when n >= 40, the hit rate beats the implied probability by 8+ points and
z > 4. A real edge moves the hit rate a few points; a gap like that means the
quotes belong to another line or market. Quarantined feeds are skipped when
picks are priced, their open picks are voided, and their settled picks are left
out of evidence, the track record and ROI. Separately, with 3+ books at one
line, a quote more than 15 points (no-vig) from the median is dropped
(`picks::OUTLIER_GAP`).

First run (2026-11-01): SportsGameOdds NHL points at ESPN BET (614 bets,
implied 21%, won 37%) and FanDuel (240 bets, implied 33%, won 64%) were
quarantined; no other feed in any sport was flagged. Those quotes had made NHL
points the only "verified" market (+45% ROI on 1,561 picks). Without them NHL
points is 707 picks at -11.4% ROI (95% -18.4% to -4.6%) and losing to the
market; no market in any sport is currently verified against the market. The
`lines` evidence row for NHL points still includes those quotes in its consensus
(pick_line_evals keeps no book), so it ages out as new lines settle.

Open picks follow the evidence each picks run: quarantined-feed picks are
voided and a lower tier re-caps the grade (never raises it). The published
price, probabilities, line, timestamp and ledger forecast never change
(`prediction_ledger` is write-once by trigger); a re-capped pick keeps its
published grade and tier in a `regraded` risk flag.

## Milestone: corrected baseline (2026-11-01)

No market in any sport is verified as beating the market. Future engine
versions are compared against this baseline, not the earlier inflated NHL
numbers.

- NHL points: the historical edge came from mislabelled SportsGameOdds quotes;
  clean picks are -11.4% ROI (95% -18.4% to -4.6%, n=707), losing.
- NBA points + assists: +17.5% ROI (95% +4.9% to +30.6%, n=155) on published
  picks but no market-beating track record yet: promising, needs more evidence.
- NHL shots on goal: +5.2% ROI (95% -2.5% to +13.1%, n=491): not established.
- NFL game lines: no advantage over closing lines in 4 backtested seasons
  (about 950 games; fitted market weight 0).
- NFL player props (passing, rushing, receiving yards, receptions): beat their
  naive baseline in backtests; research categories, not verified edges, and no
  live prop lines yet.

## Where it shows

- AI Picks: "Model X · Price Y" chip and a qualification badge on every pick, a
  status filter, EV sort, a Models tab (scorecard, reliability, forecasts vs
  baseline, quarantined feeds), and an empty state that lists each market's
  status for the sport.
- MonsterGPT: questions about the models ("why isn't Monster publishing NFL
  spreads?", "which NBA models are calibrated?", "how good are the NHL player
  props models?") route to `Intent::Model`, call `model.validation`, and are
  answered from the scorecard in code (`diag.rs`), with no language model.

## Not built yet (from the 2026-11-01 plan)

- Distributional accuracy (CRPS, interval coverage) from the ledger PMFs.
- Testing other market-blend weights / uncertainty-dependent weights out of
  sample (the current blend fits w by Brier; it is never lowered to manufacture
  an edge).
- A provider prop-name taxonomy beyond the per-provider maps in `lines.rs`, and
  quota scheduling that spends Odds API / SGO credits on markets with validated
  models first (NFL props: pass/rush/rec yards, receptions beat their baseline).
- Conformal intervals, regime detection, ensembles, Bayesian player models.
- Pushing pick and validation changes over the gateway WebSocket.

## Publication holds (migration 0037)

`publication_holds` is an append-only log of decisions to hide an engine x
market from public AI Picks, independent of its evidence tier;
`publication_holds_active` is the latest decision per pair. Held picks are still
built, priced and settled, and the evidence underneath is untouched (each
decision stores a snapshot of it). Public reads leave them out: `/picks`,
`/picks/{id}`, `/picks/meta` (which lists the holds) and the Games hub; internal
callers pass `include_held=true`, where held picks show as "suspended". The
scorecard marks held markets ("on hold" in AI Picks > Models). A hold is lifted
only by a new `release` decision with a named decider, a reason and `--confirm`
(`scripts/ops/release_hold.py`); there is no HTTP path that writes holds.

Current holds (2026-11-01, `phase-2a-audit`): nhl_team team_total_away,
team_total_home, moneyline_1h, team_total_away_1h; nba_team moneyline_1h. Their
evidence was refreshed ahead of schedule by research backtests run in normal
mode (ml-research.md).

## Research isolation

- Research scripts (`scripts/ml`) connect as `monster_research`: SELECT on
  everything, INSERT on `ml_examples`, `ml_experiments`, `feature_snapshots`
  only. Writing evidence, picks, holds, the ledger or the registry fails with
  "permission denied".
- Manual backtests need an explicit mode: `POST /picks/evidence/run` without
  `mode=production` or `mode=research` is refused (400). Research mode is a
  separate code path with no `pick_evidence` writes, bounded (`max_games`) and
  resumable, one batch per engine at a time.
- `scripts/ci/research_checks.py` runs the migration, ML, integrity, Rust,
  dashboard (and optionally end-to-end) checks and fails closed.

## Credentials for scripts

No database credential is stored in tracked files. Scripts read `ML_DATABASE_URL`
(research role) and `OPS_DATABASE_URL` (owner, for operations such as
publication decisions) from the environment or the git-ignored `.env`
(`scripts/dbenv.py`); a missing variable stops the script with the command that
fixes it. `monster_research` is created without a password (migration 0037);
`python scripts/ops/rotate_research_password.py` sets a random one and writes
`ML_DATABASE_URL` to `.env` without printing it. Run it again to rotate. The
gate's `secrets` step scans tracked, staged and new files and the git history.

