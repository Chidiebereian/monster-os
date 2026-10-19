-- MLB game simulator: fitted parameters (league outcome rates, partial-pooling
-- strengths, home/park environment factors, starter workload distribution, and
-- the empirical baserunning transition table) and every simulation run kept
-- with its seed, inputs and model version so a prediction can be reproduced.
-- Player-game box-score lines live in player_game_logs (sport = 'mlb').
CREATE TABLE IF NOT EXISTS mlb_model_params (
    key       TEXT PRIMARY KEY,
    params    JSONB NOT NULL,
    source    TEXT,
    fitted_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS mlb_sim_runs (
    sim_id        TEXT PRIMARY KEY,
    game_pk       TEXT NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    model_version TEXT NOT NULL,
    seed          BIGINT NOT NULL,
    n_requested   INT NOT NULL,
    n_valid       INT NOT NULL,
    n_rejected    INT NOT NULL,
    data_cutoff   DATE NOT NULL,
    inputs        JSONB NOT NULL,
    result        JSONB NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_mlb_sim_runs_game ON mlb_sim_runs (game_pk, created_at DESC);
