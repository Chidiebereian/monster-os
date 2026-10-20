-- NHL game data from the NHL's public API (api-web.nhle.com): games, player
-- box scores (incl. goalie starters), every shot attempt with rink
-- coordinates and strength state, and penalties.

CREATE TABLE IF NOT EXISTS nhl_games_api (
    game_id     BIGINT PRIMARY KEY,
    season      INTEGER NOT NULL,      -- 20252026
    game_type   INTEGER NOT NULL,      -- 1 pre, 2 regular, 3 playoffs
    game_date   DATE NOT NULL,
    start_time  TIMESTAMPTZ,
    away        TEXT NOT NULL,
    home        TEXT NOT NULL,
    away_score  INTEGER,
    home_score  INTEGER,
    completed   BOOLEAN NOT NULL DEFAULT FALSE,
    last_period TEXT,                  -- REG / OT / SO
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_nhl_games_api_date ON nhl_games_api (game_date);

CREATE TABLE IF NOT EXISTS nhl_box_api (
    game_id   BIGINT NOT NULL,
    team      TEXT NOT NULL,
    player_id BIGINT NOT NULL,
    name      TEXT NOT NULL,
    pos       TEXT NOT NULL,           -- C L R D G
    toi_sec   INTEGER,
    goals INTEGER, assists INTEGER, sog INTEGER, hits INTEGER, pim INTEGER, blocks INTEGER, pp_goals INTEGER,
    starter   BOOLEAN NOT NULL DEFAULT FALSE,   -- goalies
    saves INTEGER, shots_against INTEGER, goals_against INTEGER,
    PRIMARY KEY (game_id, player_id)
);
CREATE INDEX IF NOT EXISTS idx_nhl_box_api_player ON nhl_box_api (player_id);

CREATE TABLE IF NOT EXISTS nhl_shots_api (
    game_id   BIGINT NOT NULL,
    event_id  INTEGER NOT NULL,
    team      TEXT NOT NULL,
    shooter   BIGINT,
    goalie    BIGINT,
    period    INTEGER NOT NULL,
    period_type TEXT,
    sec       INTEGER NOT NULL,        -- seconds elapsed in the period
    kind      TEXT NOT NULL,           -- goal / sog / missed / blocked
    strength  TEXT NOT NULL,           -- EV / PP / SH / EN (shooting team's view)
    skaters   TEXT,                    -- e.g. 5v4
    x DOUBLE PRECISION, y DOUBLE PRECISION,   -- normalised: attacking the net at x = 89
    shot_type TEXT,
    assist1 BIGINT, assist2 BIGINT,
    empty_net BOOLEAN NOT NULL DEFAULT FALSE,
    PRIMARY KEY (game_id, event_id)
);
CREATE INDEX IF NOT EXISTS idx_nhl_shots_shooter ON nhl_shots_api (shooter);

CREATE TABLE IF NOT EXISTS nhl_penalties_api (
    game_id  BIGINT NOT NULL,
    event_id INTEGER NOT NULL,
    team     TEXT NOT NULL,            -- penalised team
    player   BIGINT,
    period   INTEGER NOT NULL,
    sec      INTEGER NOT NULL,
    duration INTEGER NOT NULL,         -- minutes
    kind     TEXT,
    PRIMARY KEY (game_id, event_id)
);
