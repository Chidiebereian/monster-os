# NFL play simulator model card — `nfl-play-sim-v1`

Status as of 2026-10-19. **Experimental.** It plays realistic, fully
consistent football games and powers the Game Broadcast view. As a betting
predictor it does **not** beat closing lines or simple player baselines in
walk-forward testing (numbers below). Every number here was produced by the
code in this repository against real data.

## Data

nflverse play-by-play (CC-BY 4.0), 2024, 2025 and 2026 through 2026-10-18:
111,113 plays across 662 games, stored in `nfl_plays`.

Ingest check: season totals rebuilt from the stored plays match the official
nflverse weekly player stats exactly for the 2025 leaders. Passing yards
match for Stafford, Goff, Prescott, Maye, Darnold and Lawrence. Rushing yards
match for Cook, Henry, Taylor, Robinson and Achane. Receiving yards and
receptions match for Smith-Njigba, Nacua, Pickens and Chase.

The only differences:

* Pass attempts run 0–4 lower, because spikes are a separate play type.
* St. Brown's receiving yards are 11 off (1,412 vs 1,401), likely a lateral.

## Confirmed lineups and alternate realities

Each game's skill-position lineup comes from:

* **Roster:** the nflverse weekly roster for that week. A player must be
  status ACT on that team; players on reserve, practice squad or another
  team are inactive.
* **Injury report:** the official report for that week. Out means inactive.
* **Uncertain players:** Questionable and Doubtful players play with a fitted
  probability.
* **Probabilities:** the chance a listed player touches the ball, relative to
  players listed without a designation. The rates are fitted from earlier
  seasons' reports (add-one smoothed), with no look-ahead. For 2026 games:
  Questionable 0.65, Doubtful 0.01.
* **Draws:** in each of the 200 parameter draws of a 100,000-game run, every
  uncertain player is drawn active or inactive.

Shares and substitutes:

* A player's target and carry share is his recency-weighted share of the
  team's work in the games he played. It is shrunk toward 0 by a prior worth
  two team games, so one-game cameos stay small.
* Inactive players' work goes to the active players in proportion to their
  shares.
* An inactive starting QB is replaced by the backup: the next available QB by
  dropbacks, or an active roster QB at league-average rates if none has snaps.

Player props are reported conditional on the player being active, the way
sportsbooks void inactive players, and each prop shows the player's share of
active games.

Alternate realities let you force any player out or active, or choose either
starting QB. The scenario rebuilds the inputs, re-runs the 100,000-game
ensemble and a new broadcast, and is shown against the confirmed lineup.

* API: `GET /nfl/simulate/{game}?out=ID,ID&in=ID&away_qb=ID&home_qb=ID`.
* Session body: `{ out: [...], in: [...], away_qb, home_qb }`.
* Production predictions always use at least 100,000 games; the server
  enforces the floor.

Limits:

* Final inactives (90 minutes before kickoff) may differ from the injury
  report.
* Defensive injuries are not modelled.

## What one simulated play involves

| Step | Source (fitted per season from play-by-play) |
|---|---|
| Kneel / last-play FG / 4th-down go-punt-FG | empirical 4th-down decision table by field position x distance x trailing late; kneel and end-of-half rules |
| Nullifying penalty (false start, holding…) | empirical rate per snap; yards, side and automatic first downs sampled from real penalties |
| Pass or run | league pass rate by down x distance x score x time cell, shifted by the team's pass-rate-over-expected (empirical-Bayes shrunk) |
| Dropback: sack / scramble / throw | QB x defence via odds ratios against the league, by down-and-distance group, home-field factor |
| Target | recency-weighted target shares (half-life 3 team games), Dirichlet posterior per batch |
| Interception, completion | team rate = QB x defence; receiver catch rate redistributes it within the team (no double counting) |
| Yards | real league gains for the same field band (10 bands) x down-and-distance group (4), scaled by the player's shrunk yards ratio and the defence's shrunk ratio |
| Fumbles, return touchdowns, return field position | empirical rates and next-snap field position after kicks and turnovers |
| Live-ball penalty after the play | empirical rate and distribution |
| Clock | empirical snap-to-snap seconds by situation (normal / hurry-up / protecting a lead) x outcome (in bounds / out of bounds / incomplete / score-turnover); two-minute warning |
| Kicks | league FG logistic in distance, XP rate, two-point decision by score margin and success rate |

The full rule set also covers quarters, halftime, overtime (2025+ rule: both
teams possess, then sudden death; regular season can end tied), safeties and
free kicks.

Pooling strengths are fitted per season, not hand-set:

* QB, receiver and defence rates use beta-binomial method of moments.
* Mean yards and team pass tendency use the variance ratio (σ² / τ²).

Leakage control:

* League parameters for a regular-season game in season S come from season
  S−1.
* Team and player inputs use only plays before the game date, from seasons
  S−1 and S.

## Simulated vs real games (2025, 200 games, 20 simulated games each)

| Per game, both teams | Simulated | Actual |
|---|---|---|
| Scrimmage plays (no penalty) | 124.5 | 118.9 |
| Dropbacks | 74.8 | 71.3 |
| Completions | 42.5 | 40.5 |
| Sacks / scrambles | 4.69 / 3.73 | 4.68 / 3.77 |
| Pass yards / rush yards | 459 / 213 | 443 / 204 |
| Interceptions / fumbles lost | 1.45 / 0.85 | 1.35 / 0.80 |
| Offensive TDs | 4.41 | 4.72 |
| FG attempts / made | 4.01 / 3.37 | 4.01 / 3.40 |
| Punts | 8.00 | 7.09 |
| 3rd-down conversions | 9.85 of 25.8 | 9.71 of 24.6 |
| Pass rate, Q1–Q3, downs 1–3 | 0.59 | 0.59 |
| Possessions | 20.7 | 19.4 |
| Points | 42.5 | 45.9 |

First-down rates by down and distance match closely. For example, 1st & 10 is
19.8% in both, and 3rd & 7–10 is 31.8% vs 31.9%. Red-zone TD rate is 55% vs
58%.

**Known residual:** the simulator runs about 4% too many plays and about one
extra possession a game, and scores about 3 points too few. The gap is mostly
extra punts and fewer return TDs. It is reported here rather than hidden
behind an ad-hoc scaling factor.

## Walk-forward validation

Each game is simulated 2,000 times with 20 parameter draws, using:

* the previous season's parameters;
* plays strictly before the game date;
* that week's roster and injury report, which are published before kickoff.

Brier score: lower is better; difference = model − baseline.

**2025 season** (199 games spread through weeks 3–18; parameters
`nfl:sim:2024`)

| Market | n | Model | Baseline | Diff ± SE | Baseline used |
|---|---|---|---|---|---|
| Home win | 199 | 0.2360 | 0.2118 | +0.0242 ± 0.0072 | closing spread |
| Home cover | 198 | 0.2758 | 0.2500 | +0.0258 ± 0.0106 | 0.5 |
| Total over | 199 | 0.2836 | 0.2500 | +0.0336 ± 0.0105 | 0.5 |
| QB pass yds over own avg | 427 | 0.2510 | 0.2525 | −0.0016 ± 0.0111 | player's prior frequency |
| Receiving yds over own avg | 2,087 | 0.2380 | 0.2450 | −0.0071 ± 0.0037 | player's prior frequency |
| Receptions over own avg | 2,087 | 0.2383 | 0.2461 | −0.0077 ± 0.0041 | player's prior frequency |
| Rushing yds over own avg | 665 | 0.2317 | 0.2483 | −0.0166 ± 0.0076 | player's prior frequency |
| Anytime TD | 2,865 | 0.1724 | 0.1807 | −0.0084 ± 0.0023 | player's prior TD-game frequency |

Totals: mean absolute error 11.1 vs 10.0 for the closing total; bias −3.2
points.

**Confirmed lineups helped the player props.** Without them (roster =
players used in the last 3 games), the 2025 Brier differences were:

| Prop | Without lineups | With lineups |
|---|---|---|
| Rushing yards | +0.0066 | −0.0166 |
| Receiving yards | +0.0010 | −0.0071 |
| Receptions | +0.0017 | −0.0077 |
| Anytime TD | −0.0052 | −0.0084 |

Anytime TD and rushing yards are now more than 2 SE better than each player's
own history. Game lines are unchanged.

**2026 to date** (45 games, weeks 3–6; parameters `nfl:sim:2025`): home win
Brier 0.221 vs 0.227, total over 0.281 vs 0.250, rushing yards 0.222 vs
0.246, totals bias −5.9 points. The sample is too small to separate these
from the baselines.

**Reading it honestly:**

* The closing market is clearly better for sides and totals.
* With confirmed lineups, player props modestly beat each player's own
  history in 2025.
* No historical prop prices are stored, so none of this is an ROI claim.

## Limitations / not modelled

* No pass depth (air yards), so the field shows only direction for
  incompletions.
* No individual player tracking. The field draws no player dots.
* No timeouts as decisions; their clock effect is inside the empirical times.
* No onside kicks, injuries or in-game QB changes, and no weather or roof.
* Final inactives and in-game injuries are not modelled, and neither are defensive injuries.
* Coaching tendencies are captured only through team pass rate over expected.
* The two-point decision is league-level.
* Snap-to-snap gaps over 60 s (about 1.6 a game in real data) are excluded
  from the clock samples.

## Reproduce

    POST /api/projection-service/ingest/nfl/pbp?seasons=2024,2025,2026
    POST /api/projection-service/fit/nfl/sim
    GET  /api/projection-service/backtest/nfl/sim?season=2025&games=200&n=2000&draws=20
    GET  /api/projection-service/nfl/simulate/2026_06_WAS_SF?n=100000&seed=42
    POST /api/projection-service/sim/sessions   {"sport":"nfl","game_id":"2026_06_WAS_SF","seed":42}
