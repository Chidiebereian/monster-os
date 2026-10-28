# Parlay Builder

Multi-leg tickets for NBA, NFL, NHL and MLB. The tool works across moneylines, spreads, totals, team totals, 1st-half / 1st-period / first-5 markets and player props. It has two modes: auto-generated boards and a custom builder. Tickets are graded, and a controlled loop learns the dependence weight.

Code:
- `apps/projection-service/src/parlay.rs` holds the math, the slate, generation and the HTTP endpoints.
- `apps/projection-service/src/parlay_store.rs` holds recording, grading, the track record and learning.
- The per-simulation hooks are `*_sim::sample` and `*_game::parlay_samples`.
- The page is `apps/dashboard/src/pages/ParlayBuilder.tsx`.
- Migration `0027_parlays.sql` creates the tables.

## What it reuses

| Need | Source |
|---|---|
| Lines and prices | `market_lines` (the AI Picks feeds: ESPN/DraftKings, The Odds API, SportsGameOdds). The freshest quote per book is used, default maximum age 180 min. |
| Joint outcomes | The champion engines (`model_registry`): `nba_sim`, `nhl_sim`, `nfl_sim`, `mlb_sim`, built by each `*_game::build` with the champion config. Each sport keeps its own lineups, injuries and posterior parameter draws (50 per slate game). The lineup mode comes from the latest frozen forecast in `prediction_ledger`. |
| Leg probabilities | The AI Picks blend (`blend::weights` / `blend::apply`). It combines the engine, the player's own frequency and the de-vigged market, with weights fitted on settled lines and backtests (`pick_evidence`). |
| Settlement | `learning::game_outcomes`, `picks::prop_actuals` and `picks::result_of`: the same rules as AI Picks. 1st-segment scores come from `game_segments`. |
| Automation | `autopilot.rs`, background loop: the `parlays:{sport}` job runs every 30 min and `parlays:learn` runs daily. |

## Joint probability

1. **Simulation.** Each slate game is simulated once: 10,000 games for NBA, NHL and MLB, 4,000 for NFL (setting `parlay_sims_{sport}`). Every leg of that game is graded against the same simulated games, giving win / loss / push / void per simulation. A void means the player was inactive in that simulation. Simulations are cached per game for 60 min (NFL 240). The cache is re-run when the champion version or lineup mode changes. Seeds derive from (sport, game, version, lineup), so runs are reproducible.
2. **Marginals.** The simulator's own marginals lose to closing lines, so each leg's level is its blended probability.
3. **Recentering.** On each slate, every over/under market with 8 or more lines is recentred. The mean logit gap between the blend and the market over the slate's overs is removed. A model that leans the same way on nearly every line of a market has a bias, not value everywhere. Players keep their relative ranking. Setting `parlay_recenter = 0` turns this off. The gaps are shown on the page.
4. **Tilting.** The simulated games of one game's legs are re-weighted to those marginals by iterative proportional fitting. This is the maximum-entropy tilt, and it keeps each leg's push/void share. The dependence stays the simulator's.
5. **Dependence weight.** `p_dep` is P(all win) on the tilted simulations. `p_indep` is the product of the same marginals. The ticket probability is `p = w·p_dep + (1−w)·p_indep`, where `w` is the dependence weight in force (`parlay_models`).
6. **Across games.** Legs in different games are independent (separate simulations), so their probabilities multiply. No slate-wide factor is modelled.

Diagnostics on every ticket:
- per-leg market, blend, simulator and tilted P(win)
- per-game joint vs independent
- pairwise P(both), lift and P(B|A)
- the joint under raw simulator marginals and under market marginals (the model range)
- Monte Carlo standard error from the Kish effective sample size

## Price, EV, fair odds

- **One book.** A ticket is priced at one book that quotes every leg; it picks the best product of prices.
- **Cross-game tickets.** These pay the product of the leg prices, and the price is marked **verified**.
- **Same-game tickets.** Books reprice these for correlation, and no feed carries SGP prices. The estimate is the product of leg prices × `Π(market marginals) / P(joint at market marginals)`. That is the discount a book applies if it knows the dependence as well as the simulator. The estimate is never above the product, and it is labelled **estimated**. Without this discount, stacking correlated legs shows large fake EVs.
- **Payout.** A losing leg loses the ticket. A pushed or void leg drops out. E[payout] is taken over the full joint distribution of leg patterns. EV = E[payout] − 1 per unit. Fair odds = 1 / P(all legs win).
- **Conservative EV** is a screen, not a guarantee. It computes E[payout] with marginals halfway between the blend and the market, then subtracts 2 Monte Carlo standard errors.

## Generation

1. Filter the legs: category, markets, prop types, games, teams, players, sides, books, minimum leg probability, model-confidence tier, and include-flagged.
2. Build a pool: the best `pool` legs (default 40) by the objective's single-leg proxy. At most a quarter of the pool comes from one game, unless the ticket is same-game only. Locked legs are always kept.
3. Beam search (width 150) adds one leg per step, with exact ticket evaluation at every step. Per-game evaluations are memoized, so cross-game extensions are O(1).
4. Exposure, scope and conflict rules apply at every step:
   - no two selections in one market for one subject
   - maximum legs per game, per player and per team
   - same-game or cross-game scope
5. Final filters apply to whole tickets: odds, probability, EV, conservative EV, lift and model range. Contradictory tickets (P(both) < 0.01%) are dropped.

Objectives:
- conservative EV
- EV
- probability
- longest odds
- shortest odds
- lowest uncertainty

Eligibility guards exclude a leg by default; "include flagged" lets it back in. A leg is excluded when it has:
- no two-way price to de-vig
- the player inactive in more than 50% of simulations
- the blend more than 25 points from the market
- the raw simulator more than 30 points from the market (usually thin or stale player data)
- near-certain lines

## Grading and track record

- **Recording.** Each auto run stores the top 5 tickets by conservative EV for every category (moneyline, spread, total, team total, props, mixed) × 2/3/4 legs, for today and tomorrow. A ticket is stored once per distinct legs+book (unique `ticket_key`), so it is graded once. Saved custom tickets are stored the same way.
- **Immutability.** Prediction-time fields cannot change; a trigger enforces this. `latest_price_decimal` is refreshed until the start, which gives the closing comparison.
- **Settlement.**
  - Results come from the same sources as AI Picks.
  - A player who didn't play voids the leg.
  - A game that is still not final 4 days after its start voids its legs.
  - A losing leg settles the ticket as a loss immediately.
  - Stake is 1 unit per ticket.
- **`/parlays/record`** reports:
  - W/L/P/V, ROI and units
  - Brier and log loss of P(all win) vs the independence baseline and vs the market product (paired)
  - reliability by probability bucket with Wilson intervals
  - price vs close
  - breakdowns by sport, source, leg count, category, odds bucket, dependence bucket and scope

  Calibration uses only tickets whose legs all won or lost. Standard errors are clustered by slate date, because tickets from one slate share legs.

## Controlled learning

The parlay model's free parameter is the dependence weight `w`. The leg levels are learned by the AI Picks loop.

Every day, on settled same-game tickets in time order:
1. `w ∈ {0, .25, .5, .75, 1}` is chosen on the older 70%.
2. It is tested on the newest 30% against the weight in force.
3. It is promoted only with at least 100 test tickets **and** a Brier improvement of more than 2 clustered standard errors. Otherwise it is recorded as rejected.

All versions are kept in `parlay_models`. `POST /parlays/model/rollback` restores the previous champion.

## Endpoints

| Method | Path | Purpose |
|---|---|---|
| GET | `/parlays/slate?sport=&date=&max_age=` | Coverage, games and simulation status, markets, books, model bias |
| GET | `/parlays/legs?sport=&date=&game=&q=&market=&category=` | Legs for the custom builder |
| GET | `/parlays/generate?...` | Ranked tickets (filters above) |
| POST | `/parlays/evaluate` | `{sport, date, legs[], book}`; legs no longer offered are reported, never substituted |
| GET | `/parlays/status` | Simulation cache and jobs |
| POST | `/parlays/cancel?sport=` | Cancel the background simulation job |
| POST | `/parlays/run?sport=` | Run the automated job now |
| GET, POST | `/parlays/tickets` | List / save |
| GET | `/parlays/tickets/{id}` | One ticket |
| POST | `/parlays/tickets/{id}/unsave` | Remove from the saved list |
| GET | `/parlays/record` | Track record |
| GET | `/parlays/validation` | Learning state |
| POST | `/parlays/learn` | Run the dependence-weight test now |
| POST | `/parlays/model/rollback` | Restore the previous champion |

## Limitations

- **Pre-game only.** Legs drop out 5 minutes before the start. There is no live-odds feed.
- **No same-game parlay prices.** No feed carries them, so they are estimated (see above).
- **Sports covered.** There is no WNBA engine in MonsterOS. MLB player props have no line feed, so MLB is game markets only.
- **Cross-game independence.** No slate-wide factor (officiating trends, weather fronts) is modelled.
- **Marginals.** They are only as good as the AI Picks blend. Backtests show the engines don't beat closing lines on their own, and the slate recentering is a guard, not a fix.
- **Selection bias.** Tickets chosen by EV are the ones the model likes most. Their realized results test the model where it is most confident, not a random sample.
