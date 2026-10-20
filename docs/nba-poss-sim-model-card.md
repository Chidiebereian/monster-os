# NBA possession simulator model card — `nba-poss-sim-v1`

Status as of 2026-10-19. **Experimental, first version.** Every number was
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
  possession length.

**Rotation.**

* Each player's minutes are drawn from his last 10 regular-season or playoff
  games.
* Minutes are allocated in priority order to 240.
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

## Walk-forward validation (2025-26, 149 games spread from December)

Each game is simulated 2,000 times with 20 parameter draws, using 2024-25
league parameters and data strictly before tip-off. Brier diff = model −
baseline (negative is better).

| Market | Projected lineup | Actual lineup (hindsight) | Baseline |
|---|---|---|---|
| Home win | +0.0334 ± 0.0091 | +0.0273 ± 0.0089 | closing spread |
| Home cover | +0.0167 ± 0.0091 | +0.0119 ± 0.0085 | 0.5 at the line |
| Total over | +0.0301 ± 0.0111 | +0.0344 ± 0.0113 | 0.5 at the line |
| Points over own avg | −0.0052 ± 0.0038 | **−0.0398 ± 0.0035** | player's prior frequency |
| Rebounds over own avg | +0.0108 ± 0.0039 | −0.0067 ± 0.0041 | player's prior frequency |
| Assists over own avg | +0.0026 ± 0.0036 | **−0.0203 ± 0.0035** | player's prior frequency |
| Threes over own avg | +0.0048 ± 0.0036 | **−0.0105 ± 0.0032** | player's prior frequency |

Possessions per game: 204.4 simulated vs 203.9 actual. Total points bias is
+1.2. Mean absolute error is 14.9, against 12.7 for the closing total.

**Reading it honestly:**

* The market is clearly better on sides and totals.
* With projected lineups, props are about even with each player's history;
  rebounds are slightly worse.
* Knowing who actually plays and starts makes props clearly better, so the
  value is in confirmed lineups.

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
    GET  /api/projection-service/backtest/nba/sim?season=2026&games=150&n=2000&draws=20&lineup=projected
    GET  /api/projection-service/nba/simulate/401909088?n=100000&lineup=confirmed
    POST /api/projection-service/sim/sessions  {"sport":"nba","game_id":"401909088","lineup":"confirmed"}
