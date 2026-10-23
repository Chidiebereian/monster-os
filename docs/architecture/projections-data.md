# Projections page data

The Projections page, Hit Rates and the prop matcher read the shared tables
`sports_players`, `player_game_logs`, `player_projections` and `sports_games`.
These tables used to be filled once by bulk ingests (stats.nba.com, MoneyPuck,
nflverse) and then went stale. By 2026-10-22 NBA logs stopped at 2026-04-12 and
NHL logs at 2026-10-12. Players were listed on teams they had left (Darius
Garland on CLE instead of LAC, Paul George on PHI instead of BOS).

`proj_sync.rs` now rebuilds them from the warehouses the simulators already keep
current. The learning autopilot runs it (job `sync:{sport}`: every 6 h for NBA,
NHL and NFL; daily for MLB). To run it by hand:

    POST /api/projection-service/sync/projections?sport=nba          (runs on the server, returns at once)
    POST /api/projection-service/sync/projections?sport=nba&wait=true (blocks, returns the summary)

| Sport | Game logs | Current team | Games list |
|---|---|---|---|
| NBA | ESPN box scores (`nba_box_espn`), regular season | ESPN team rosters (30 calls) | `nba_games_espn` |
| NHL | NHL API box scores (`nhl_box_api`), skaters and goalies | NHL API `roster/{team}/current` | `nhl_games_api`, plus the season schedule |
| NFL | nflverse weekly files for this season and last (`/ingest/nfl?sources=current&fit=false`) | nflverse weekly rosters, each team's latest week | nflverse `games.csv` (`/ingest/nfl/teams`) |
| MLB | its own warehouse | box scores | — (refit only) |

- **Teams:** a player's team is the live roster. When every team's roster came
  back, a player on none of them has no team (free agent or retired), so they
  drop out of game filters. If a roster fetch fails, the latest box score's team
  stands.
- **Projections:** each run refits every player with the same empirical-Bayes,
  recency-weighted model the ingests used. The fit streams the logs one player
  at a time, so the full history never has to fit in memory.
- **Team models:** NBA, NHL and NFL team models are refitted on the same run.

## NBA ids

The page now uses ESPN athlete ids: the ids the simulator, AI Picks, recorded
lines and headshots use. The two id spaces overlap numerically, so old rows
are recognised by their stats.nba.com game ids (`00…`), not by player id.

On the first run, all old NBA rows moved to sport `nba_legacy`: 357k logs,
1,776 players, 16,776 projections and 18,166 games. Nothing was deleted. Games
before ESPN coverage (2019-10-22) were then copied back under ESPN ids for
players whose name matches exactly one player on each side (113k logs). So
careers still reach back to 2012.

## Lines

`GET /market/{sport}/props/{stat}` and `GET /market/{sport}/games` now read
`market_lines`. AI Picks records ESPN/DraftKings, The Odds API and SportsGameOdds
quotes there every 20 minutes, so the page spends no provider budget. Each line
is the main line (the one most books quote), priced at DraftKings when it quotes
it, otherwise at the first book that does. `?live=true` still calls
SportsGameOdds directly (2,500 entities a month).
