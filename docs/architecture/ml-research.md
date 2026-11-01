# ML research engine (v1)

Market-aware residual learning on an immutable dataset, judged by the
validation engine. The question every experiment answers is "does Monster know
something the market did not price?", not "can it predict games?". The
validation baseline of 2026-11-01 (validation.md) is the control: a model has
to beat the market out of sample before anything else.

## Dataset (migration 0035, `ml_examples`, write-once)

| source | from | rows (2026-11-01) | market baseline (`p_base`) |
|---|---|---|---|
| live | `scripts/ml/dataset.py`: every settled line AI Picks priced, features rebuilt from the `market_lines` quote history as of the decision time | 16,030 (NBA 10,975 / 85 games, NHL 5,048 / 75, MLB 7), Oct 21 - Nov 1 | de-vigged consensus at decision time, quarantined feeds left out |
| backtest | `evidence.rs` walk-forward backtests now store every per-game forecast instead of only the aggregate | NFL 10,195 / 1,035 games (2022-26), NBA 21,975 / 4,050, NHL 36,185 / 4,724 (team models) | closing spread / total / moneyline (`comparison='market'`) or the naive model (`'baseline'`) |

Live features (all known before the game): engine vs market log-odds gap,
naive-model gap, movement from the opening consensus, books quoting, hours to
start, hold, line, market. The consensus at the close and the best real over /
under prices at decision time are stored for CLV and ROI. Rows are inserted
once, after settlement; an UPDATE or DELETE raises. Closing NBA lines exist only
for the 2023 and 2026 seasons; NFL has 2022-2026.

## Experiments (`scripts/ml/experiment.py`, results in `ml_experiments`, append-only)

Models on the same out-of-sample rows: `market`, `engine_raw`, `blend` (what AI
Picks uses: w fitted by Brier on the training folds), and three residual
learners that start from logit(market): elastic-net logistic with the market as
offset, LightGBM (`init_score`), XGBoost (`base_margin`). Folds are
chronological: live by the game's start date (ET; every row of a game shares a
fold, and a training game must have finished 4 hours before the test fold's
first decision), backtests by season.
Targets: `outcome`, and `movement` (live: will the consensus move toward the
over / home side before the close, a CLV model judged on its own).

Metrics: Brier, log loss, calibration error, paired Brier difference vs the
market with a game-clustered bootstrap (95% and 99%), folds better, and for
live data flat-stake ROI at the best real price (EV >= 2%) with a clustered
interval and the CLV of those bets.

Promotion gate (best of the three learned models): 99% interval of the Brier
difference below 0, calibration error within 1 point of the market's, better
in at least 60% of folds, at least 300 test rows and 30 games. Passing makes a
model a `shadow_candidate` only; nothing changes what AI Picks publishes.
Otherwise `inconclusive`, `rejected` (99% interval above 0) or
`insufficient_data`. `scripts/ml/test_experiment.py` checks on synthetic data
that the learners find a real signal and gain nothing from noise.

```
python scripts/ml/dataset.py              # snapshot newly settled lines (idempotent)
python scripts/ml/experiment.py --suite   # standard research set
python scripts/ml/experiment.py --sport nba --markets pa,pra --target movement
```

## First results (2026-11-01)

No experiment reached shadow candidate.

- Game lines vs closing lines, whole seasons: NFL spread / total / moneyline
  (775-794 test games) and NBA spread / total / moneyline (979): every learned
  model is inconclusive or worse; the production blend ties the market (its
  fitted weight is ~0); the raw engines are significantly worse than the market
  (Brier +0.010 to +0.025).
- NBA props, all markets live (7,385 rows, 52 games): inconclusive; raw engine
  significantly worse than the market.
- NHL props live (3,166 rows): rejected, learned models worse than the market.
- NBA points + assists: 145 test rows over 7 games, insufficient data (the
  +17.5% ROI on the scorecard is not testable yet). NBA PRA and NHL shots on
  goal: inconclusive.
- Line movement (CLV) for NBA props: inconclusive (XGBoost -0.009, interval
  includes 0); NHL: insufficient data.

Read: with today's features the market already contains what the engines know.
The leverage is in new information the market may price late (injuries and
lineups at decision time, usage and minutes, opponent suppression), more live
seasons, and decision times earlier than ~2 hours before start (movement has
little room left by then).

## Integrity (audit, 2026-11-01)

- `ml_examples` and `ml_experiments` refuse UPDATE, DELETE and TRUNCATE
  (row and statement triggers); migration 0035 runs in one transaction, so a
  re-run never leaves a window without the guards. It applies cleanly to an
  empty database (0001-0035) and to one already at 0034.
- `dataset.py` inserts with ON CONFLICT DO NOTHING on (source, engine, engine
  version, game, market, subject, line): a re-run adds only newly settled lines.
- An experiment whose name, dataset fingerprint and code version are already
  recorded is not stored again. LightGBM and XGBoost run single-threaded with
  fixed seeds; two suite runs give identical output.
- Each record keeps the dataset fingerprint, code version, model settings,
  feature definitions, per-fold train / test ranges with row and game counts,
  metrics with 95% / 99% intervals, verdict and reason, and caveats: best-of-3
  selection (99% intervals), about 15 experiments per suite without family-wise
  correction, backtest game lines deciding at the close (no ROI claimed), pushes
  and voids left out.
- The first v1 run assigned live folds by decision date; 65 games had lines
  decided on two days, so some of a game's rows could train a model tested on
  that same game. Folds now follow the game's start date with an assertion that
  no game is in both partitions. Every verdict was unchanged.
- Nothing in AI Picks reads the ML tables; services only read the results
  (validation.rs, diag.rs) and the backtest worker only adds examples. No
  endpoint or chat path can start training.

## Where it shows

- `GET /validation/experiments` and the scorecard's `experiments` / `dataset`.
- AI Picks > Models: "ML research" table (verdict, engine raw / blend / best
  learned vs market, ROI).
- MonsterGPT: ML questions ("does the XGBoost residual model beat the market on
  NFL spreads?", "show me the NBA ML experiments") route to `Intent::Model` and
  are answered from the stored results (`diag.rs`); training runs only in
  `scripts/ml`, never from chat.

## Phase 2A: decision-time enrichment (2026-11-01, research only)

### Gap analysis

| Information | What exists | Known at decision time? |
|---|---|---|
| Injuries / availability, NBA + NHL | every frozen forecast's `payload.inputs`: `p_active`, ESPN injury-report status line, "not used in last 5" | yes: `prediction_ledger.frozen_at`, projected or confirmed lineup, write-once (since 2026-10-20) |
| Raw ESPN injury reports | fetched live at confirmed freezes, never stored | only going forward, if a snapshotter is added |
| NFL injuries / depth charts | `nfl_injuries`, `nfl_roster_weekly` (weekly, no timestamps) | not provable: at best 'inferred' (final report before kickoff); not used |
| Lineups / starters | NBA starter flags, NHL projected goalie and its source, projected vs confirmed freeze | yes, at freeze. NHL "goalie confirmed" is always 0 at freeze: no confirmation is captured before games |
| Expected minutes / TOI | `min_mean` (NBA), `toi_mean` (NHL) in the inputs | yes, at freeze |
| Workload | games played and last-5 status in the inputs; box scores (`nba_box_espn`, `nhl_box_api`) are post-game | box scores only lagged to earlier games (not built yet) |
| Coaching / rotation | NFL coach and QB names in `nfl_game_info`; nothing for NBA / NHL | NFL only (not built yet) |
| Rest / back-to-backs | schedules (`nba_games_espn`, `nhl_games_api`; NFL rest in `nfl_game_info`) | yes, derived |
| Odds at several horizons | hourly ESPN snapshots in `market_lines` since 2026-10-21. 6 h before: every game line, 98% of props; 12 h: most game lines, 59% of props; 24 h: 16% of props | yes |
| Historical prop prices (before 2026-10-21) | none | no: never reconstructed |

### Feature store (migration 0036, `feature_snapshots`, write-once)

One row per value: `event_at`, `observed_at`, `available_at`, `source`,
`quality` (observed / derived / inferred / missing; missing has no value),
`extractor`. `scripts/ml/features.py` fills it (re-runs add nothing):

- `engine_inputs` (NBA, NHL; available at `frozen_at`): per player `p_active`,
  `injury_listed`, `status`, `starter`, `lineup_confirmed`, `team`,
  `exp_minutes`, `games_recent`; per team `out_count`, `out_minutes` (regulars
  not expected to play and their expected minutes), `injury_listed_count`, NHL
  `goalie_confirmed`. 173,255 rows.
- `market_horizon`: each live example's line consensus 24 / 12 / 6 / 2 h before
  the start (quarantined books left out); 58,490 of 121,864 are 'missing'.
- `schedule` (NBA, NHL): `rest_days`, `back_to_back` (derived; available at the
  team's previous game). 12,660 rows.

Experiments attach features with `asof()`: the latest value whose
`available_at` is at or before the example's decision time, for the same game
and subject; 'inferred' and 'missing' are never used. Tests cover later
snapshots being ignored, ties, and no borrowing across games or players.
Horizon experiments (`--horizon 6|12`) replace the market with the consensus at
the horizon, set the decision time to start - h, and drop the engine and naive
probabilities, which may have been updated after the horizon.

### Feature ablation (`--ablation`: v1, +availability, +lineup, +minutes, +schedule, all; identical eligible rows)

Each arm is judged against the market and, paired on the same test rows,
against the v1 arm (99% game-bootstrap). Results (code v1.2 / v1.2.1):

| Data | Rows (games) | vs market, every arm | Any arm better than v1 at 99%? |
|---|---|---|---|
| NBA props + game lines, outcome | 7,385 (52) | inconclusive | no; availability and all-features significantly worse for the elastic net |
| NHL, outcome | 3,166 (47) | v1 to minutes rejected; schedule and all inconclusive | no |
| NBA game lines, outcome | 740 (52) | inconclusive | no |
| NHL game lines, outcome | 212 (28) | insufficient data | no |
| NBA movement 6 h -> close | 3,786 (52) | inconclusive: LightGBM on market-only features beats the base rate (-0.0091, 99% upper -0.0002) but its calibration error is 3.1% vs 1.6% | no; availability and all-features worse |
| NBA movement 12 h | 1,086 (42) | inconclusive | no |
| NHL movement 6 h / 12 h | 1,935 (47) / 1,183 (42) | inconclusive | no |

Read: the information the engines already use (availability, expected
minutes, lineups, rest) does not add anything the market has not priced, at
least on two weeks of live data; added features mostly add noise. 120 paired
comparisons from correlated data, not family-wise corrected. The one thing
worth watching is short-horizon line movement from market-only information.

### Player-prop backtests (NBA / NFL simulators)

`evidence.rs` now has a research-only mode (`POST
/picks/evidence/run?engine=...&research_only=true`): it stores every per-game
forecast in `ml_examples` and does not write `pick_evidence`, so AI Picks tiers
cannot move. These rows compare the simulator with the naive model only: no
historical prop prices exist and none are made up. Experiments on them are
labelled "(vs naive model, no market prices)" and can never be a shadow
candidate.

### Phase 2A audit (2026-11-01)

- Five markets whose evidence the normal-mode backtests refreshed are under a
  publication hold (validation.md); their evidence is unchanged.
- Research and production are separated three ways: the `monster_research`
  database role, an explicit `mode` on manual backtests (research mode has no
  evidence code path), and holds that sit beside the evidence.
- `scripts/ml/backtest_runner.py`: bounded, resumable research backtests with a
  free-memory floor and a checkpoint; `scripts/ml/test_integrity.py` (24 tests
  against the live database and service) and `scripts/ci/research_checks.py`.
- Open items: `research-backlog.md`.

### Production side effect (found in this phase)

The v1 build ran the team-model backtests in normal mode to fill the dataset.
That also refreshed their `pick_evidence` rows ahead of the 30-day schedule:
`nhl_team` team totals moved from unproven to baseline, and 16 first-half /
team-total markets got evidence for the first time (`nba_team` 1H moneyline,
`nhl_team` 1H moneyline and 1H away team total as baseline; the rest
unproven). Baseline picks are shown by default (capped B+). No team-model pick
was open at the time. The research-only mode above exists so this cannot
happen again.

## Next

Saving raw ESPN injury reports as they are published (a snapshotter); lagged
workload from box scores; NFL coaching / QB context; running the NFL simulator
backtest in research mode; more live weeks (two weeks of data is thin); CRPS on
the ledger PMFs; Bayesian player models; a shadow runner that scores candidates
live beside AI Picks.
