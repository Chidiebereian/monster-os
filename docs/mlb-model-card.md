# MLB model card — `mlb-pa-sim-v1` + `mlb-{stat}-v1`

Status as of 2026-10-19. Every number below was produced by the code in this
repository against real MLB data; nothing is illustrative.

## What exists

| Piece | Where | Status |
|---|---|---|
| Ingest: schedule, final scores, per-game box scores (2024–2026) | `projection-service/src/mlb.rs` `POST /ingest/mlb` | implemented, tested, data spot-checked |
| Empirical baserunning table from play-by-play (800 games / season) | same, `baserunning:{season}` | implemented, tested |
| Per-stat empirical-Bayes count models (11 batter, 8 starter props) | `POST /fit/mlb`, `mlb-{stat}-v1` | implemented; served by Projections, Hit Rates, Cheat Sheets, simulation-engine |
| Plate-appearance game simulator | `projection-service/src/mlb_sim.rs` | implemented, invariant-tested, walk-forward validated |
| Simulate a game (100k default, stored with seed + inputs) | `GET /mlb/simulate/{gamePk}`, `GET /mlb/sims/{id}` | implemented |
| Slate (probable pitchers, lineup status) | `GET /mlb/slate` | implemented |
| Walk-forward backtest vs baselines | `GET /backtest/mlb/sim`, `GET /mlb/backtests` | implemented, results below |
| Dashboard | `/mlb-sim` page; MLB in Projections / Hit Rates / Cheat Sheets | implemented |
| Sportsbook MLB prop IDs | `market.rs` `sgo_stat` | **unverified** (SGO rate-limited when added) |

## Data

MLB Stats API (`statsapi.mlb.com`, no key; MLB Advanced Media copyright —
personal, non-commercial use, not redistributed). Schedule + box scores for
regular season and postseason; suspended/resumed games de-duplicated by
gamePk. Stored in `player_game_logs` (`sport='mlb'`, `week = gamePk`) and
`sports_games`. Ingest is idempotent (upserts; already-stored games skipped).

Validation of ingest: season totals summed from stored game logs match the
official season stats exactly for Judge 2024, Ohtani 2025, Soto 2026 (hitting)
and Skubal 2025, Skenes 2026 (pitching) — PA/H/HR/TB/R/RBI/BB/K/SB and
BF/outs/K/H/ER/BB/HR/GS.

## Simulator

Each simulated game plays nine innings (extra innings with the automatic runner
from 2020; walk-offs end the game; the home team skips the bottom of the ninth
when ahead). Every plate appearance ends in K / BB / HBP / 1B / 2B / 3B / HR /
ball-in-play out, drawn from

    q_c ∝ batter_c · pitcher_c / league_c · env_c      (multinomial odds ratio)

* **Player rates** — recency-weighted counts from games *before the game date*,
  partially pooled toward the league per outcome with beta-binomial strengths
  chosen on year-over-year prediction (falls back to within-season method of
  moments). Each batch of games redraws every player's rates from a Dirichlet
  posterior (parameter uncertainty). Starters use only their starts once they
  have 100+ BF as a starter.
* **Environment** — home/away factors and park factors (home park vs road,
  shrunk by estimated reliability across parks), from one season.
* **Baserunning** — runner advancement, double plays, sac flies and RBI credit
  sampled from the empirical (event, bases, outs) table; forced moves for
  BB/HBP/HR/K.
* **Workload** — starter exits after a batters-faced count drawn from his last
  10 starts (league distribution at weight 1 start; tuned on 2025, validated on
  2026); team-aggregate bullpen afterwards.
* **Substitutions** — a starting batter is replaced before his next PA with a
  hazard by PA number estimated Kaplan–Meier style from lineup-slot usage
  (starters take 95.5% of their slot's PAs); replacements bat with league
  bench rates and are not credited to the starter.
* **Parameters** for a regular-season game come from the previous season; for
  a postseason game, from the same (completed) season.

Every player stat comes from the same simulated games, so hits, total bases,
runs, RBI, pitcher strikeouts/outs and the score are mutually consistent.
Tested invariants: H = 1B+2B+3B+HR, TB = 1B+2·2B+3·3B+4·HR, batter runs sum to
team runs, RBI ≤ runs, runs charged to pitchers = runs scored, slot PAs follow
the batting order, 3 outs per away half-inning, no ties, reproducible seeds,
P(over)+P(under)+P(push) = 1, no push on half lines.

Monte Carlo error (`mcse`) is reported per line; it measures sampling precision
only, not model accuracy.

## Validation — walk-forward, 2026 (held out)

299 regular-season games, May–Sept 2026, 5,000 sims each (research setting),
actual starting lineups and starters (known pregame), player data strictly
before each game, simulator parameters from 2025. Baselines: each player's own
prior frequency (up to 162 games, add-one smoothed); team: 2025 league rates.
Brier: lower is better; difference = model − baseline.

| Market | n | Model Brier | Baseline | Diff ± SE |
|---|---|---|---|---|
| Home win | 299 | 0.2371 | 0.2491 | −0.0120 ± 0.0035 |
| Total over 8.5 | 299 | 0.2476 | 0.2510 | −0.0034 ± 0.0038 |
| Batter 1+ hits | 5,382 | 0.2346 | 0.2388 | −0.0042 ± 0.0011 |
| Batter 2+ total bases | 5,382 | 0.2242 | 0.2276 | −0.0034 ± 0.0008 |
| Batter 1+ HR | 5,382 | 0.1013 | 0.1027 | −0.0014 ± 0.0004 |
| Batter 2+ H+R+RBI | 5,382 | 0.2442 | 0.2483 | −0.0041 ± 0.0011 |
| Batter 1+ K | 5,382 | 0.2258 | 0.2317 | −0.0059 ± 0.0011 |
| Starter K over own-average line | 598 | 0.2419 | 0.2443 | −0.0024 ± 0.0048 |
| Starter outs over own-average line | 598 | 0.2459 | 0.2471 | −0.0012 ± 0.0056 |

Reading it honestly: batter markets and home win beat the baselines by more
than 3 standard errors; totals and starter props are not distinguishable from
the baselines. Starter strikeouts carry a +0.26 K level bias in 2026 (−0.09 in
2025), consistent with 2026 pitchers striking out fewer batters than their own
history (22.0% vs 22.6% from prior starts) — season-level drift the model does
not yet track. No historical sportsbook prices are stored, so none of this is a
betting-ROI claim.

## Known limitations / not built yet

* Plate-appearance level, not pitch level; no handedness/platoon splits.
* No stolen-base attempts in the simulator (SB props use the per-stat model).
* Starter exit is a workload draw, not pitch count or in-game performance.
* Bullpen is a team aggregate; no reliever availability/leverage.
* No weather, roof, umpire inputs; park factors are single-season.
* Earned runs = runs × league earned share (no error reconstruction).
* Lineups for unposted games are the team's previous batting order.
* MLB sportsbook stat IDs unverified; no stored historical prop prices.
* The Chapter 18 run-event (1/2/3/4-run) team model is not implemented; game
  lines currently come from the plate-appearance simulator.

## Reproduce

    POST /api/projection-service/ingest/mlb?from=2024&to=2026&pbp_games=800   # per season for pbp tables
    POST /api/projection-service/fit/mlb
    GET  /api/projection-service/backtest/mlb/sim?season=2026&games=300&n=5000&draws=25
    GET  /api/projection-service/mlb/simulate/{gamePk}?n=100000&seed=42
