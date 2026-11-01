-- Kickoff times for games whose source table has dates only (NFL sports_games).
-- Filled from the ESPN scoreboard when a slate is built. prediction_ledger rows
-- are immutable, so forecasts frozen without a start read it from here instead.
CREATE TABLE IF NOT EXISTS game_kickoffs (
    sport      text        NOT NULL,
    game_id    text        NOT NULL,
    starts_at  timestamptz NOT NULL,
    source     text        NOT NULL DEFAULT 'espn',
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (sport, game_id)
);
