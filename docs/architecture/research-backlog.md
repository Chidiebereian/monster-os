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
| R7 | NFL key-number margin shape | started 2026-11-02 | the engines' margin distribution reaches real key-number frequencies (|margin| = 3: real 14.7%, team-points engine 8.7%, play simulator 9.9%; `scripts/ml/margin_shape.py`) and spread / moneyline calibration is re-tested walk-forward against the closing line, beside the unchanged champion |
| R8 | NHL puck line and 2-goal margins | started 2026-11-02 | puck-line (+/-1.5) forecasts are in the NHL backtest; the simulator's 2+ goal share matches real games (real 59.3%, simulator 55.5%, team-goals engine 55.1%) and the puck line is calibrated over full seasons |

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
