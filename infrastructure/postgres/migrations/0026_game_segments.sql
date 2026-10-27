-- First-segment scores of finished games, for the 1st-half markets (moneyline,
-- spread, total and team totals). The segment is the sport's standard "first
-- half" market: NBA and NFL the 1st half (Q1+Q2), NHL the 1st period, MLB the
-- first five innings. game_id is the id sports_games and the simulators use
-- (NBA ESPN event id, NHL api game id, NFL nflverse game_id, MLB gamePk).
CREATE TABLE IF NOT EXISTS game_segments (
    sport       text        NOT NULL,
    game_id     text        NOT NULL,
    home_1h     integer     NOT NULL,
    away_1h     integer     NOT NULL,
    source      text        NOT NULL,
    updated_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (sport, game_id)
);
