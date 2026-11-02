# Research backlog (tracked deliverables)

Open research items after the Phase 2A audit (2026-11-01). Each is separate:
none of their partial results are part of the Phase 2A findings in
`ml-research.md`, and none can change AI Picks (research role, research-mode
backtests, publication holds; see `validation.md`).

| # | Deliverable | Status | Done when |
|---|---|---|---|
| R1 | NBA player-prop experiments on research-only forecasts | ready (R3 complete), not run | `experiment.py --sport nba --source backtest --markets pts,reb,...` recorded; every record labelled "(vs naive model, no market prices)" and verdict at most inconclusive (enforced in code and by test) |
| R2 | NFL simulator backtest | not started | `backtest_runner.py --engine nfl` complete (research mode); per-season row counts in this table |
| R3 | NBA simulator backtest | complete (2026-11-01): 4,132 games = 4,123 stored (1,463,194 rows) + 9 without an evaluation (the All-Star games: 2023 1, 2024 1, 2025 3, 2026 4), 0 duplicates; per season stored 1,030 / 1,031 / 1,031 / 1,031 (2023-2026) | `backtest_runner.py --engine nba` reports complete; stored games = season games with a forecast |
| R4 | Historical ESPN injury reports | not started | a snapshotter stores every ESPN injury report as published (observed_at = fetch time) into `feature_snapshots` (quality observed); no back-fill of the past |
| R5 | NFL injury / depth-chart timestamps | not started | a source with publication times is found, or the weekly tables are stored as quality 'inferred' and kept out of experiments |
| R6 | NHL starting-goalie confirmation | not started | a confirmed-starter source captured before puck drop; today `goalie_confirmed` is always 0 at freeze |
| R7 | NFL key-number margin shape | step 1 done 2026-11-02, negative: re-pricing the team-points engine's own projection with real key-number margins (walk-forward from 2010, `scripts/ml/r7_keynumbers.py`) made spread prices worse on 1,016 games (Brier 0.2766 vs engine 0.2741, +0.0025 +/- 0.0013; key-number lines +0.0047 +/- 0.0019); both lose to the closing line (0.2500). The error is the projected margin, not its shape; production already gives this engine weight 0 for NFL spread / moneyline / total. Step 2 done 2026-11-02 (`scripts/ml/r7_ratings.py`, 2,936 games 2015-2026, walk-forward): game-by-game ratings beat the team-points engine on its own 1,016 backtest games (margin Elo -0.0108 +/- 0.0027 Brier, least squares -0.0070 +/- 0.0021) but not the closing line (Elo +0.0098 +/- 0.0020; blended weight fitted on earlier seasons drifts to 0). Better projections for game pages and MonsterGPT, no betting edge on NFL sides |
| R9 | AI Picks track record audit | found 2026-11-02 | settled picks total +792 flat units only because of quarantined feeds (+810, ROI 88-96%: fake); without them -18.2 units on 3,382 picks. The proven tier is -82.8 units (ROI -12.5%), almost all NHL player points at non-quarantined books (638 picks, -85.0, stated edge +6.3 pts); that market is already leans_only. Closing-line test (2,916 picks with a same-line close): 38.8% beat the close, average CLV +0.0011, but the expected value of the price taken at the closing fair probability is -5.3% (baseline -4.3%, proven -6.3%); 9.3% of picks are +EV at the close. Done when closing-fair EV by tier and market is a standing report, tiers and publication react to it, and the default feed shows only markets with positive closing-fair EV over a meaningful sample | the engines' margin distribution reaches real key-number frequencies (|margin| = 3: real 14.7%, team-points engine 8.7%, play simulator 9.9%; `scripts/ml/margin_shape.py`) and spread / moneyline calibration is re-tested walk-forward against the closing line, beside the unchanged champion |
| R8 | NHL puck line and 2-goal margins | step 1 done 2026-11-02 (`scripts/ml/r8_puckline.py`, 3,563 games, walk-forward): home -1.5 priced from the team-goals engine's own goal expectations beats the earlier-season base rate (Brier 0.2164 vs 0.2228, -0.0063 +/- 0.0017) but underprices it (mean 31.0% vs actual 33.4%); rescaling by the real 2-goal share fixes the mean, not the Brier (+0.0001 +/- 0.0006). Step 2 (`scripts/ml/r8_market.py`, 89 settled games with stored quotes since 2026-10-21): the event simulator prices the -1.5 favourite at 31.6% vs market 36.2% and actual 36.0%; Brier simulator 0.2262 vs market 0.2179 (+0.0082 +/- 0.0075, not yet significant). The published NHL spread market (engine nhl) blends the simulator at w 0.94, fitted against the naive baseline, not the market; 34 of its 40 picks are +1.5 underdogs (17-16, -6.47 units). Publication decision raised with the owner | puck-line (+/-1.5) forecasts are in the NHL backtest; the simulator's 2+ goal share matches real games (real 59.3%, simulator 55.5%, team-goals engine 55.1%) and the puck line is calibrated over full seasons |

## R3 progress

Resumable and bounded: `python scripts/ml/backtest_runner.py --engine nba`
(25 games per request, stops below 1.5 GB free memory, checkpoint and log in
`%LOCALAPPDATA%/MonsterOS/logs/research/backtest_nba.{json,log}`). Each game is
committed on its own; a stopped run resumes after the stored games.

## Rules for every item

- Research mode only (`mode=research`, `monster_research` role).
- Never reconstruct the past from present-day data; unknown timing = 'missing'
  or 'inferred', and inferred values stay out of experiments.
- Props without historical prices are compared with the naive model only and can
  never be a shadow candidate.
- Results go to `ml_experiments`; nothing is promoted automatically.
