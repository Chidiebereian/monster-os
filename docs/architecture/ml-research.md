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

## Next

Injury / lineup / usage features captured at decision time; an earlier decision
time for the movement target; storing the NFL and NBA simulators' backtests
(player props vs the naive model); CRPS on the ledger PMFs; Bayesian player
models; a shadow runner that scores candidates live beside AI Picks.
