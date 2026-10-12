#!/usr/bin/env bash
# Load the NBA CSVs (from nba_ingest.py) into Postgres: sports_players + player_game_logs.
set -e
SP="$1"
PG="monster-os-postgres-1"
run() { docker exec -e PGPASSWORD=monster "$PG" psql -U monster -d monsteros -v ON_ERROR_STOP=1 "$@"; }

run -c "DROP TABLE IF EXISTS stg_players; CREATE UNLOGGED TABLE stg_players (player_id text, display_name text, position text, team text, first_season int, last_season int, games int);"
run -c "DROP TABLE IF EXISTS stg_logs; CREATE UNLOGGED TABLE stg_logs (player_id text, season int, game_id text, game_date date, team text, opponent text, stats jsonb);"

docker exec -i -e PGPASSWORD=monster "$PG" psql -U monster -d monsteros -v ON_ERROR_STOP=1 \
  -c "\copy stg_players FROM STDIN WITH (FORMAT csv)" < "$SP/nba_players.csv"
docker exec -i -e PGPASSWORD=monster "$PG" psql -U monster -d monsteros -v ON_ERROR_STOP=1 \
  -c "\copy stg_logs FROM STDIN WITH (FORMAT csv)" < "$SP/nba_logs.csv"

run -c "INSERT INTO sports_players (sport,player_id,display_name,position,team,first_season,last_season,games) \
        SELECT 'nba',player_id,display_name,NULLIF(position,''),team,first_season,last_season,games FROM stg_players \
        ON CONFLICT (sport,player_id) DO UPDATE SET display_name=EXCLUDED.display_name, team=EXCLUDED.team, \
          first_season=EXCLUDED.first_season, last_season=EXCLUDED.last_season, games=EXCLUDED.games, updated_at=now();"
run -c "INSERT INTO player_game_logs (sport,player_id,season,week,game_date,team,opponent,stats) \
        SELECT 'nba',player_id,season,game_id,game_date,team,opponent,stats FROM stg_logs \
        ON CONFLICT (sport,player_id,season,week) DO UPDATE SET game_date=EXCLUDED.game_date, team=EXCLUDED.team, \
          opponent=EXCLUDED.opponent, stats=EXCLUDED.stats;"
run -tAc "SELECT 'players',count(*) FROM sports_players WHERE sport='nba'; SELECT 'logs',count(*) FROM player_game_logs WHERE sport='nba'; SELECT 'max_season',max(last_season) FROM sports_players WHERE sport='nba';"
run -c "DROP TABLE stg_players; DROP TABLE stg_logs;"
