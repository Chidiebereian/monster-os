-- NBA game data from ESPN's public site API (scoreboard + game summary):
-- games (with the pregame line), full player box scores incl. starters and
-- DNP reasons, and every field-goal attempt with its court coordinates.

CREATE TABLE IF NOT EXISTS nba_games_espn (
    espn_id     TEXT PRIMARY KEY,
    season      INTEGER NOT NULL,      -- ESPN season year (2026 = 2025-26)
    season_type INTEGER NOT NULL,      -- 1 pre, 2 regular, 3 post
    game_date   DATE NOT NULL,
    start_time  TIMESTAMPTZ,
    away        TEXT NOT NULL,
    home        TEXT NOT NULL,
    away_score  INTEGER,
    home_score  INTEGER,
    completed   BOOLEAN NOT NULL DEFAULT FALSE,
    spread      DOUBLE PRECISION,      -- pickcenter spread (home perspective, negative = home favoured)
    over_under  DOUBLE PRECISION,
    periods     INTEGER,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_nba_games_espn_date ON nba_games_espn (game_date);

CREATE TABLE IF NOT EXISTS nba_box_espn (
    espn_id   TEXT NOT NULL,
    team      TEXT NOT NULL,
    player_id TEXT NOT NULL,
    name      TEXT NOT NULL,
    pos       TEXT,
    starter   BOOLEAN NOT NULL DEFAULT FALSE,
    dnp       BOOLEAN NOT NULL DEFAULT FALSE,
    reason    TEXT,
    min       DOUBLE PRECISION, pts INTEGER, fgm INTEGER, fga INTEGER, fg3m INTEGER, fg3a INTEGER,
    ftm INTEGER, fta INTEGER, oreb INTEGER, dreb INTEGER, ast INTEGER, tov INTEGER, stl INTEGER, blk INTEGER, pf INTEGER,
    PRIMARY KEY (espn_id, player_id)
);
CREATE INDEX IF NOT EXISTS idx_nba_box_espn_player ON nba_box_espn (player_id);
CREATE INDEX IF NOT EXISTS idx_nba_box_espn_team ON nba_box_espn (team);

CREATE TABLE IF NOT EXISTS nba_shots_espn (
    espn_id   TEXT NOT NULL,
    seq       INTEGER NOT NULL,
    player_id TEXT NOT NULL,
    team      TEXT,
    period    INTEGER,
    x         DOUBLE PRECISION,        -- feet across the court (0-50)
    y         DOUBLE PRECISION,        -- feet from the baseline (0-47)
    made      BOOLEAN NOT NULL,
    value     INTEGER NOT NULL,        -- 2 or 3
    PRIMARY KEY (espn_id, seq)
);
CREATE INDEX IF NOT EXISTS idx_nba_shots_player ON nba_shots_espn (player_id);
