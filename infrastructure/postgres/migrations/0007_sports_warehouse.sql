-- MonsterOS sports data warehouse (cross-sport).
--
-- Mirrors the per-sport warehouse shape used by professional stacks (players,
-- teams, schedule, player/team game logs, projections) with a `sport`
-- discriminator so NFL/NBA/MLB/NHL share one data model instead of 30 tables
-- per sport. Richer per-sport feeds (play_by_play, play_participants, lineups,
-- injuries, officials, venues, standings, moneypuck_* skater/goalie/team logs,
-- projected_starting_goalies, projected_power_play_units, edge_shot_profiles)
-- are added by later migrations as each feed's ingestion lands — every table we
-- create is backed by real ingested data, never an empty placeholder.

-- The player dimension. `player_id` is the upstream source id (e.g. nflverse
-- gsis_id, nba_api personId, MLB StatsAPI id, NHL api id).
CREATE TABLE IF NOT EXISTS sports_players (
    sport        TEXT NOT NULL,
    player_id    TEXT NOT NULL,
    display_name TEXT NOT NULL,
    position     TEXT,
    team         TEXT,                        -- most-recent team
    first_season INT,
    last_season  INT,
    games        INT,                          -- games we hold logs for
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (sport, player_id)
);
CREATE INDEX IF NOT EXISTS idx_sports_players_name ON sports_players (sport, display_name);
CREATE INDEX IF NOT EXISTS idx_sports_players_team ON sports_players (sport, team);

-- The schedule / game dimension.
CREATE TABLE IF NOT EXISTS sports_games (
    sport      TEXT NOT NULL,
    game_id    TEXT NOT NULL,
    season     INT,
    week       TEXT,
    game_date  DATE,
    home_team  TEXT,
    away_team  TEXT,
    status     TEXT,                           -- scheduled | final
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (sport, game_id)
);
CREATE INDEX IF NOT EXISTS idx_sports_games_date ON sports_games (sport, game_date);

-- One row per player per game. The full stat line is JSONB so it fits every
-- sport; the common dimensions we filter/model on are promoted to columns.
CREATE TABLE IF NOT EXISTS player_game_logs (
    sport     TEXT NOT NULL,
    player_id TEXT NOT NULL,
    season    INT  NOT NULL,
    week      TEXT NOT NULL,                   -- week / game sequence (string = flexible)
    game_date DATE,
    team      TEXT,
    opponent  TEXT,
    stats     JSONB NOT NULL,                  -- {"passing_tds":3,"passing_yards":315,...}
    PRIMARY KEY (sport, player_id, season, week)
);
CREATE INDEX IF NOT EXISTS idx_pgl_player ON player_game_logs (sport, player_id);

-- Model projections per (player, stat, game). mean/sd define the predictive
-- distribution the simulation-engine draws from; dispersion carries the
-- Negative-Binomial overdispersion so we can draw the true count law later.
-- market_line / market_implied come from the baseline market when one matches.
CREATE TABLE IF NOT EXISTS player_projections (
    sport          TEXT NOT NULL,
    player_id      TEXT NOT NULL,
    stat           TEXT NOT NULL,              -- 'passing_tds'
    game_id        TEXT NOT NULL DEFAULT '',   -- '' = next-game / general projection
    model_id       TEXT,
    proj_mean      DOUBLE PRECISION NOT NULL,
    proj_sd        DOUBLE PRECISION NOT NULL,
    dispersion     DOUBLE PRECISION,           -- NB size param (Gamma shape alpha)
    sample_games   INT,
    career_mean    DOUBLE PRECISION,
    recent_mean    DOUBLE PRECISION,
    market_line    DOUBLE PRECISION,
    market_implied DOUBLE PRECISION,
    as_of          TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (sport, player_id, stat, game_id)
);
CREATE INDEX IF NOT EXISTS idx_pproj_stat ON player_projections (sport, stat, proj_mean);
