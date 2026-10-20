-- NFL play-by-play warehouse (nflverse pbp, one row per snap/kick) + the
-- sport-agnostic broadcast simulation sessions (one seeded game, its ordered
-- authoritative event log) and fitted simulator parameters.

CREATE TABLE IF NOT EXISTS nfl_plays (
    game_id      TEXT    NOT NULL,
    play_id      INTEGER NOT NULL,
    season       INTEGER NOT NULL,
    week         INTEGER NOT NULL,
    season_type  TEXT    NOT NULL,
    game_date    DATE    NOT NULL,
    home_team    TEXT    NOT NULL,
    away_team    TEXT    NOT NULL,
    posteam      TEXT,
    defteam      TEXT,
    qtr          INTEGER,
    qsr          INTEGER,            -- quarter seconds remaining at the snap
    yardline_100 INTEGER,            -- yards to the opponent's goal line
    down         INTEGER,
    ydstogo      INTEGER,
    score_diff   INTEGER,            -- posteam minus defteam, before the play
    play_type    TEXT,               -- pass / run / punt / field_goal / kickoff / extra_point / qb_kneel / qb_spike / no_play
    yards        INTEGER,
    sack         BOOLEAN NOT NULL DEFAULT FALSE,
    scramble     BOOLEAN NOT NULL DEFAULT FALSE,
    interception BOOLEAN NOT NULL DEFAULT FALSE,
    complete     BOOLEAN NOT NULL DEFAULT FALSE,
    fumble_lost  BOOLEAN NOT NULL DEFAULT FALSE,
    touchdown    BOOLEAN NOT NULL DEFAULT FALSE,
    td_team      TEXT,
    safety       BOOLEAN NOT NULL DEFAULT FALSE,
    out_of_bounds BOOLEAN NOT NULL DEFAULT FALSE,
    penalty      BOOLEAN NOT NULL DEFAULT FALSE,
    two_point    BOOLEAN NOT NULL DEFAULT FALSE,
    fg_result    TEXT,
    xp_result    TEXT,
    two_pt_result TEXT,
    kick_distance INTEGER,
    air_yards    INTEGER,
    location     TEXT,               -- pass_location or run_location (left/middle/right)
    passer_id    TEXT, passer_name   TEXT,
    rusher_id    TEXT, rusher_name   TEXT,
    receiver_id  TEXT, receiver_name TEXT,
    kicker_id    TEXT, kicker_name   TEXT,
    PRIMARY KEY (game_id, play_id)
);
CREATE INDEX IF NOT EXISTS idx_nfl_plays_season ON nfl_plays (season, game_date);
CREATE INDEX IF NOT EXISTS idx_nfl_plays_posteam ON nfl_plays (posteam, game_date);

CREATE TABLE IF NOT EXISTS sim_model_params (
    key        TEXT PRIMARY KEY,     -- e.g. 'nfl:sim:2025'
    params     JSONB NOT NULL,
    source     TEXT,
    fitted_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS sim_sessions (
    session_id    TEXT PRIMARY KEY,
    sport         TEXT NOT NULL,
    game_id       TEXT NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    model_version TEXT NOT NULL,
    seed          BIGINT NOT NULL,
    data_cutoff   DATE,
    meta          JSONB NOT NULL,    -- teams, rosters, inputs, provenance
    events        JSONB NOT NULL,    -- ordered authoritative event log
    final         JSONB NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_sim_sessions_game ON sim_sessions (sport, game_id, created_at DESC);

-- penalty detail (penalties are simulated as events)
ALTER TABLE nfl_plays ADD COLUMN IF NOT EXISTS penalty_team TEXT;
ALTER TABLE nfl_plays ADD COLUMN IF NOT EXISTS penalty_yards INTEGER;
ALTER TABLE nfl_plays ADD COLUMN IF NOT EXISTS first_down_penalty BOOLEAN NOT NULL DEFAULT FALSE;
