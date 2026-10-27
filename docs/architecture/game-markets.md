# Game markets: team totals and the first segment

Every game market MonsterOS prices, for every sport:

| Market key | What it is | Settles on |
|---|---|---|
| `moneyline`, `spread`, `total` | full game | home margin / total |
| `team_total_home`, `team_total_away` | full-game team totals | home / away points |
| `moneyline_1h` | first-segment moneyline (a tie refunds) | 1H margin |
| `spread_1h`, `total_1h` | first-segment spread and total | 1H margin / 1H total |
| `team_total_home_1h`, `team_total_away_1h` | first-segment team totals | 1H home / away points |

The **first segment** is each sport's standard "first half" market:
NBA and NFL the 1st half (Q1+Q2), NHL the 1st period, MLB the first five innings.
Labels use `1H`, `1P` and `F5`. Helpers live in `segments.rs`:
`base_market`, `outcome_key`, `market_label` and `label`/`tag`.

## Data: `game_segments` (migration 0026)

This table holds the first-segment score of every finished game, keyed by the
id that sports_games and the simulators use.

| Sport | Source | Since |
|---|---|---|
| NBA | ESPN scoreboard linescores, fetched one date at a time (ESPN rejects date ranges) | 2019 |
| NFL | the same ESPN source, matched on date and team pair | 2019 |
| NHL | period-1 goals in `nhl_shots_api`, stored under both the NHL api id and the MoneyPuck `mp_…` id that sports_games uses for past seasons | 2019 |
| MLB | MLB StatsAPI schedule with `hydrate=linescore` (one request per season); rows exist only when both teams batted through the 5th | 2019 |

Every row is checked against the final score. The backfill had 0 mismatches:
- NBA: 9,370 games
- NFL: 1,977 games
- NHL: 9,855 games, plus 7,286 MoneyPuck aliases
- MLB: 18,236 games

The autopilot job `segments:{sport}` fills new games every 3 hours. You can
also run it by hand with `POST /ingest/segments?sport=&since=`.

Share of full-game scoring in the first segment, and how often it ends tied:

| Sport | Share of scoring | Tied |
|---|---|---|
| NBA 1H | 50.3% | 2.9% |
| NFL 1H | 50.3% | 7% |
| NHL 1P | 29% | 32% |
| MLB F5 | 56.5% | 15% |

## Engines

- **Simulators** (`nba_sim`, `nhl_sim`, `nfl_sim`, `mlb_sim`) record the score
  at the end of the segment. That happens at halftime, after the 1st period,
  or after five full innings. The ensemble keeps the joint (home, away)
  distribution of that score in `segments::Joint`.
  - Every payload has `h1`, which holds the win/away/tie probabilities, the
    home, away and total distributions, and `margin_home` as a pmf.
  - Every payload also has `score.home|away` for the full-game team totals.
  - NHL now also serves a full-game `margin_home` pmf, so alternate puck lines
    price directly.
  - MLB adds the same top-level keys (`win`, `score`, `total`, `margin_home`,
    `h1`) beside its own `team` block.
  - These changes do not change RNG consumption, so existing results replay
    exactly.
- **Team engines** (`team_engine.rs`) fit a second log-linear model on the
  segment scores. It uses the same fit, recency weights and ridge as the
  full-game model. They simulate independent negative-binomial segment scores
  with no overtime, so ties stand. They serve `h1` and `score` in the same
  shape as the simulators.
- `GET /matchup/{sport}/h1?home=&away=&spread=&total=&home_total=&away_total=`
  is the Projections page's first-segment model, fit on games up to today. The
  full-game `/matchup/{sport}` handlers now take `home_total`/`away_total` and
  return a `team_totals` block. That block comes from the same simulated draws
  as the rest of the matchup.

## Learning loop and evidence

`segments::forecasts` adds forecasts for team totals and the first segment to
every engine. All four sports' outcomes gain `home_pts`, `away_pts` and
`h1_*`, which come from `segments::augment`.

**NBA and NFL** derive their lines from the game's closing line and score them
against 0.5:
- 1H spread = 0.595× the full-game spread for NBA, 0.57× for NFL.
- 1H total = 0.505× the total for NBA, 0.51× for NFL.
- Team totals = (total ± spread)/2, hooked to the half point.

These factors are regressions over 2019–2026 games that have closing lines.

**NHL and MLB** have no stored historical lines, so they use standard lines
against league base rates:
- NHL: 1P total 1.5 (0.54), 1P lead (0.521), 1P team over 0.5 (0.605 home /
  0.583 away), full-game team over 2.5 (0.631 / 0.587). These come from 6,747
  games between 2021-22 and 2026-27.
- MLB: F5 total 4.5 (0.50), F5 lead (0.532), F5 team over 2.5 (0.43 / 0.39),
  team over 4.5 (0.435 / 0.42).

`evidence::pick_market` maps these forecasts to AI Picks markets as
**baseline** evidence. Their lines are derived or league-standard, never a real
quoted line, so the grade cap for these markets is B+. MLB is now a pick
engine; its evidence covers moneyline, total, team totals and F5.

Use `GET /learning/{sport}/scorecard?season=&games=` to check them. It is a
read-only walk-forward scorecard of the champion on every market.

## Lines

SportsGameOdds already returns every market of an event for one entity, so the
extra markets cost nothing. `lines::parse_sgo` reads these odd ids:
- `points-{home|away}-game-ou-*` for the full-game team totals.
- The period `1h` (NHL `1p`) for `-ml-`, `-sp-`, `points-all-…-ou-` and the
  team `-ou-` markets. SGO's MLB `1h` is the first five innings.

ESPN (DraftKings) and The Odds API featured endpoint carry only the full-game
three. The Odds API's period markets need its per-event endpoint, which costs
credits per market, so it is not used.

## Consumers

- **AI Picks** (`picks::price`, `result_of`, `pick_label`, templates) prices,
  pairs, labels and settles all ten markets, for NBA, NHL, NFL and now MLB.
  `sharp.rs` (stale-line edges) and `blend.rs` use the same helpers.
- **`/market/{sport}/games`** (recorded lines) adds `home_total`, `away_total`
  and odds, plus an `h1` block with the same fields.
- **`GET /market/{sport}/screen?market=`** is the Odds Screen and +EV Finder
  board for any of the ten markets, built from recorded multi-book lines. Each
  row has:
  - every book's price at that number, the best price, and the no-vig
    consensus ("fair") with the best price's edge;
  - the champion engine's probability and EV at the best price, where the
    engine has frozen the game.
- **Hit rates** (`/hitrates/{sport}/team/{team}`) take `spread_1h`,
  `total_1h` and `team_total_1h`. They return `win_1h`, `cover_1h`, `over_1h`,
  `team_total_over_1h` and first-segment averages. Only finished games count.
  NHL schedule rows carry 0–0 scores and were previously counted.
- **Cheat sheets** (`/cheatsheet/{sport}/games`) read the recorded lines first,
  so they are free and include team totals and 1H lines. They grade both teams
  on every market.
- **Dashboard pages:**
  - Projections (game tab): full-game and first-segment market tables with
    lines, odds and EV, autofilled from recorded lines.
  - Odds Screen and +EV Finder: market picker; EV against the no-vig fair or
    the model.
  - Hit Rates and Cheat Sheets: the first-segment and team-total columns.
  - Game Broadcast (NFL, NBA, NHL, MLB) and MLB Game Sim: a first-segment and
    team-totals panel.
  - AI Picks: MLB and the new market labels.
