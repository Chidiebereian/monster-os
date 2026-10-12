#!/usr/bin/env bash
set -e
SP="$1"; PG="monster-os-postgres-1"
run() { docker exec -e PGPASSWORD=monster "$PG" psql -U monster -d monsteros -v ON_ERROR_STOP=1 "$@"; }
run -c "DROP TABLE IF EXISTS stg_g; CREATE UNLOGGED TABLE stg_g (game_id text, season int, game_date text, game_type text, home text, away text, hs text, as_ text, status text);"
docker exec -i -e PGPASSWORD=monster "$PG" psql -U monster -d monsteros -v ON_ERROR_STOP=1 \
  -c "\copy stg_g FROM STDIN WITH (FORMAT csv)" < "$SP/nba_games.csv"
run -c "INSERT INTO sports_games (sport,game_id,season,week,game_type,game_date,home_team,away_team,home_score,away_score,status) \
        SELECT 'nba',game_id,season,game_date,game_type,NULLIF(game_date,'')::date,home,away, \
               NULLIF(hs,'')::int, NULLIF(as_,'')::int, status FROM stg_g \
        ON CONFLICT (sport,game_id) DO UPDATE SET season=EXCLUDED.season, game_type=EXCLUDED.game_type, \
          game_date=EXCLUDED.game_date, home_team=EXCLUDED.home_team, away_team=EXCLUDED.away_team, \
          home_score=EXCLUDED.home_score, away_score=EXCLUDED.away_score, status=EXCLUDED.status, updated_at=now();"
run -tAc "SELECT status,count(*) FROM sports_games WHERE sport='nba' GROUP BY status; SELECT 'completed_scored',count(*) FROM sports_games WHERE sport='nba' AND home_score IS NOT NULL;"
run -c "DROP TABLE stg_g;"
