# NBA possession simulator model card — `nba-poss-sim-v4`

Status as of 2026-10-21. **Experimental.** Every number was
produced by the code in this repository against real data.

## Data

ESPN's public site API, through `POST /ingest/nba/espn`:

* **Coverage:** 2024-25 and 2025-26 regular seasons and playoffs (about
  2,600 completed games), the 2026-27 preseason, and the upcoming schedule.
* **Box scores:** 72,505 player rows with FG, 3PT, FT, OREB, DREB, AST, TO,
  STL, BLK, PF and minutes, plus starter flags and DNP reasons.
* **Shot charts:** 482,708 field-goal attempts with court coordinates.
* **Lines:** pregame spread and total from ESPN's pickcenter.

## Model

**Possessions.** Possessions alternate. The player who ends each one is
chosen by usage among the five on the floor. The outcome is one of:

* a turnover (with a league steal share),
* a two-shot free-throw trip,
* a 2- or 3-point attempt. Make probability is shooter × defence odds ratio
  against the league, with a home shooting edge.

After the shot or trip:

* Made shots are assisted at the league rate; the assister is chosen by
  assist rate.
* Misses are blocked at the league rate, then rebounded by the offence or the
  defence. Team OREB and DREB factors set which side gets it; player
  rebounding rates pick the rebounder.

**Player and team rates.**

* Player rates are recency-weighted (half-life 20 games) and shrunk to the
  league.
* Pooling strengths are fitted per season by beta-binomial method of moments.
* Pace is a team's recent possessions per 48, shrunk to the league; it sets
  possession length. Possessions are counted the standard way (an offensive
  rebound continues the same one), so one drawn length covers a possession's
  second chances.
* Lengths are Gamma(4), capped at 3–24 s. Both the cap and each period's
  cut-short last possession add possessions, so the Gamma scale is solved so
  that the expected count equals pace (`dur_scale`; renewal-theory count with
  the capped length's exact moments).

**Rotation.**

* Each player's minutes are drawn from his last 10 regular-season or playoff
  games.
* **Minutes budget (v4).** When the active players' projections add up to more
  than 240, the projected starters keep 85% protection (`starter_protect`).
  Players outside the top 12 (`rotation_depth`) share 12 garbage-time minutes
  (`deep_bench_minutes`). The rest of the rotation is scaled to the minutes
  that remain. After a preseason game, the projected starters are the five who
  started most of the team's recent games.
* Minutes are then allocated in priority order to 240.
* A greedy 2-minute stint schedule decides who is on the floor. Starters open
  the 1st and 3rd quarters.
* Overtime goes to the heaviest-minute players.

**Uncertainty and ensemble.**

* Each of the 200 draws in a 100,000-game run redraws every rate from its
  posterior and draws availability.
* Player props are conditional on playing.

**Lineups** (the engine plays one source):

| Source | What it uses |
|---|---|
| projected | players used in the team's last 5 games; last game's starters, or after a preseason game the top five by projected minutes |
| confirmed | projected, plus ESPN's current injury report for upcoming games |
| actual | the game's own box score: who played and who started (hindsight) |

The confirmed mapping from injury status to chance of playing is a set of
documented priors, not fitted: Out 0, Doubtful 0.15, Questionable 0.5,
Day-To-Day 0.6, Probable 0.9. ESPN's endpoint is current-only, so no history
exists yet to fit from.

## Walk-forward validation (full 2025-26 season, 1,031 games)

Each game is simulated 2,000 times with 20 parameter draws, using league
parameters from earlier seasons and data strictly before tip-off. Brier
diff = model − baseline (negative is better), ± one standard error of the
paired difference.

| Market | Projected lineup | Actual lineup (hindsight) | Baseline |
|---|---|---|---|
| Home win | +0.0148 ± 0.0038 | +0.0129 ± 0.0038 | closing spread (normal, sd 12) |
| Home cover | +0.0114 ± 0.0036 | +0.0108 ± 0.0035 | 0.5 at the line |
| Total over | +0.0216 ± 0.0040 | +0.0204 ± 0.0040 | 0.5 at the line |
| Points over own avg | +0.0003 ± 0.0014 | **−0.0285 ± 0.0013** | player's prior frequency |
| Rebounds over own avg | +0.0115 ± 0.0015 | −0.0038 ± 0.0015 | player's prior frequency |
| Assists over own avg | +0.0069 ± 0.0014 | **−0.0140 ± 0.0013** | player's prior frequency |
| Threes over own avg | +0.0030 ± 0.0013 | **−0.0092 ± 0.0011** | player's prior frequency |

Possessions per game: 202.3 simulated vs 203.1 actual. Total points bias is
−0.9. Mean absolute error is 15.6, against 13.9 for the closing total.

**v1 → v2.** v1 shipped with a possession-count bias: 212.1 simulated vs
203.1 actual possessions per game, and totals 10.2 points too high. The total-over
Brier difference was +0.064. v2 calibrates possession length (above); the
other markets barely moved. Test: `possessions_match_pace`, which fails on
v1 (98.4 possessions per 48 at pace 92).

**Reading it honestly:**

* The market is clearly better on sides and totals.
* With projected lineups, props are about even with each player's history;
  rebounds and assists are slightly worse.
* Knowing who actually plays and starts makes props clearly better, so the
  value is in confirmed lineups.

## v2 → v4: the minutes budget

v2 handed the 240 minutes out heaviest projection first. In opening week every
preseason participant counts as active, so projections added up to about 420
minutes. The ninth player got almost nothing: a starting center (Poeltl, 19.5
projected) was simulated at 2 minutes. AI Picks then priced his unders as
locks.

The fix was measured with a per-player minutes diagnostic added to
`GET /backtest/nba/sim`:

* minutes MAE, over players who played or were expected to;
* rotation misses: played 15+ minutes but projected under 8;
* starter bias: projected-starter simulated minus actual minutes.

Each window is 56–60 games, 1,000 simulations, projected lineups:

| Window | MAE v2 → v4 | Misses per game v2 → v4 | Starter bias v2 → v4 |
|---|---|---|---|
| Opening week 2025-26 | 10.38 → 9.32 | 3.07 → 2.26 | −1.78 → −0.28 |
| Opening week 2024-25 | 8.89 → 7.88 | 2.57 → 2.20 | −0.46 → +0.61 |
| Mid-January 2026 | 7.69 → 7.36 | 1.77 → 0.98 | −0.99 → −0.33 |
| Early March 2025 | 8.54 → 8.13 | 1.75 → 0.93 | −2.28 → −1.50 |

Scaling the whole rotation, starters included, was tried first. It fixed the
misses but under-projected starters by 3–5 minutes, and starters are the
subjects of most props. `nba-poss-sim-v3` was that interim build; it froze one
evening of forecasts (2026-10-21) before v4 replaced it. The four settings are
tunable, so the autopilot's tuner can move them.

## Limitations

* No fouls, foul trouble, end-game fouling, and-1s or timeouts.
* No lineup positions, and no in-game injuries or blowout rotations.
* Shot locations are display only: drawn from the shooter's real chart, but
  make probability is by shot type, not location.
* Injury-status probabilities are priors.
* For past games, "confirmed" equals projected, because the injury feed is
  current-only.

## Reproduce

    POST /api/projection-service/ingest/nba/espn?from=2025-10-21&to=2026-06-25
    POST /api/projection-service/fit/nba/sim
    GET  /api/projection-service/backtest/nba/sim?season=2026&games=100000&n=2000&draws=20&lineup=projected
    GET  /api/projection-service/nba/simulate/401909088?n=100000&lineup=confirmed
    POST /api/projection-service/sim/sessions  {"sport":"nba","game_id":"401909088","lineup":"confirmed"}
