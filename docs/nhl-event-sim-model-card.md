# NHL event simulator model card — `nhl-event-sim-v1`

Status as of 2026-10-20. **Experimental, first version.** Every number was
produced by the code in this repository against real data.

## Data

The NHL's public API (`api-web.nhle.com`), via `POST /ingest/nhl/api`:

* **Coverage:** 2024-25 and 2025-26 regular seasons and playoffs, plus 2026-27
  to date (about 2,750 completed games).
* **Shot attempts:** 354,690 (goal, shot on goal, missed, blocked). Each has
  rink coordinates normalised so the shooter attacks the net at x = +89 ft,
  plus shot type, shooter, goalie in net, assists and strength state (EV, PP,
  SH, empty net, skater counts).
* **Box scores:** 120,231 player rows (TOI, G, A, SOG, hits, PIM, blocks) and
  goalie lines, including starters.
* **Penalties:** each with its duration.
* **Ingest checks:** 100% of shooters belong to the attributed team for every
  attempt type. An early bug assumed blocked shots belong to the blocking
  team; the API attributes them to the shooting team, and it is fixed.

## League fit (per season, `POST /fit/nhl/sim`)

| Parameter | 2024-25 | 2025-26 |
|---|---|---|
| EV / PP / SH attempts per 60 | 61.6 / 64.1 / 9.0 | 60.0 / 63.1 / 8.5 |
| Goal per shot on goal | 0.101 | 0.105 |
| xG coefficient per foot of distance | −0.055 | −0.053 |
| Shootout conversion | 0.333 | 0.331 |
| Goalie pulled when trailing 1–2 at 17:00 of the 3rd | 0.91 | 0.88 |
| Empty-net goals per unblocked attempt | 0.56 | 0.51 |

The expected-goals model is a logistic regression on shots on goal. Its
inputs are distance, angle, a power-play flag and shot type (wrist is the
baseline). Tip-ins and deflections score more; backhands and wraparounds
score less.

Power-play and shorthanded minutes per game are estimated from penalty
minutes, with power-play goals ending minors early.

## Model

The game runs in continuous time. Attempts and penalties are competing
Poisson processes in each state. The state is the strength for each team
(power plays, 3-on-3 overtime, goalie pulled), and each state change cuts
the draw.

**Team rates.**

* Each team has offence and defence multipliers by strength vs the league.
  The defence is indexed by its own strength, so a power play meets the
  opposing penalty kill.
* Rates are recency-weighted (half-life 20 games) and shrunk.

**Attempts and goals.**

* Each attempt is blocked, missed or on goal, using league fractions and the
  team's block factor.
* The shooter is picked by his shrunk share of the team's attempts at that
  strength in the games he played, so power-play units come from the data.
* The location is drawn from his real attempts.
* Goal probability on a shot on goal is the xG at that location × shooter
  finishing (goals vs xG, shrunk) × the goalie factor (goals against vs xG
  faced over 450 days, shrunk).
* Assists are drawn by assist share. Empty-net and 6-on-5 rates are the
  league's.

**Rules.** A power-play goal ends the minor. Regular-season overtime is 3-on-3
for five minutes, then a shootout at the league conversion rate. Playoff
overtime is 20-minute 5-on-5 sudden-death periods.

**Uncertainty.** Each of the 200 draws in a 100,000-game run perturbs team,
finishing and goalie factors, and draws availability. Player props are
conditional on playing.

**Lineups.** The engine plays one source:

| Source | Skaters | Goalie |
|---|---|---|
| projected | played in the last 5 games | started the team's most recent game |
| confirmed | projected + ESPN's current injury report (matched to NHL players by name) | as projected |
| actual | who played, from the game's box score (hindsight) | the actual starter |

The confirmed mapping from injury status to chance of playing is a set of
documented priors, not fitted.

**Speed.** 100,000 games take about 0.8 s.

## Walk-forward validation (2025-26, 200 games after the first 150)

Each game is simulated 2,000 times with 20 parameter draws, using 2024-25
league parameters and data strictly before puck drop. Brier diff = model −
baseline (negative is better).

| Market | Projected | Actual (hindsight) | Baseline |
|---|---|---|---|
| Home win | **−0.0128 ± 0.0056** | **−0.0169 ± 0.0054** | league home rate 0.535 |
| Total over 5.5 | +0.0110 ± 0.0041 | +0.0064 ± 0.0040 | league rate 0.53 |
| Skater SOG over own average | +0.0033 ± 0.0014 | +0.0005 ± 0.0012 | player's prior frequency |
| Skater 1+ point | +0.0029 ± 0.0010 | +0.0007 ± 0.0009 | player's prior frequency |
| Goalie saves over own average | −0.0070 ± 0.0106 | −0.0111 ± 0.0067 | goalie's prior frequency |

Shots on goal per game: 54.6 simulated vs 55.8 actual. Goals per game: bias
−0.46, mostly because 2025-26 converted 10.5% of shots on goal against 10.1%
in the training season.

No NHL closing lines are stored, so sides and totals are compared with
league base rates, not the market.

**Reading it honestly:**

* Win probabilities beat a constant home-rate baseline.
* Totals are slightly low and worse than the base rate.
* Skater props are only about even with each player's own history.

## Limitations

* No explicit lines, shifts or deployment matchups; shooter shares stand in.
* No coincidental minors, fights or delayed penalties.
* Goalie-pull times come from the first 6-skater attempt, about 10 seconds
  after the actual pull.
* Strength minutes are estimated from penalties.
* There is no goalie-confirmation feed; "confirmed" uses the projected
  starter.

## Reproduce

    POST /api/projection-service/ingest/nhl/api?from=2025-10-07&to=2026-06-25
    POST /api/projection-service/fit/nhl/sim
    GET  /api/projection-service/backtest/nhl/sim?season=20252026&games=200&lineup=projected
    GET  /api/projection-service/nhl/simulate/2026020144?n=100000
    POST /api/projection-service/sim/sessions  {"sport":"nhl","game_id":"2026020144"}
