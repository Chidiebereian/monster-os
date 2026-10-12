-- Team model: schedule/results with scores and (historical) market lines, plus
-- a small key/value store for the fitted global model parameters. Team
-- offensive/defensive ratings reuse the existing team_components table
-- (component = 'points_off' / 'points_def').
ALTER TABLE sports_games ADD COLUMN IF NOT EXISTS home_score  INT;
ALTER TABLE sports_games ADD COLUMN IF NOT EXISTS away_score  INT;
ALTER TABLE sports_games ADD COLUMN IF NOT EXISTS spread_line DOUBLE PRECISION; -- market home spread (negative = home favored)
ALTER TABLE sports_games ADD COLUMN IF NOT EXISTS total_line  DOUBLE PRECISION; -- market total points
ALTER TABLE sports_games ADD COLUMN IF NOT EXISTS game_type   TEXT;            -- REG / POST

-- Global fitted parameters per team model (home advantage, intercept, NB dispersion).
CREATE TABLE IF NOT EXISTS team_model_params (
    model_id TEXT NOT NULL,
    key      TEXT NOT NULL,
    value    DOUBLE PRECISION NOT NULL,
    as_of    TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (model_id, key)
);
