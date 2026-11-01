-- ML research engine (scripts/ml, docs/architecture/ml-research.md).
--
-- ml_examples: one immutable, timestamped training example per forecast and
-- line. Features are only what was known at `decision_at` (before the game);
-- the outcome `y` is filled once, at settlement. Rows are never updated or
-- deleted, so experiments stay reproducible and no later model can rewrite the
-- history it is judged on.
--   source 'live'      a line AI Picks priced (pick_line_evals + market_lines history)
--   source 'backtest'  a walk-forward backtest forecast (evidence.rs), with the
--                      closing line (game markets) or the naive model (props) as p_base
BEGIN;

CREATE TABLE IF NOT EXISTS ml_examples (
    id             bigserial PRIMARY KEY,
    source         text        NOT NULL CHECK (source IN ('live', 'backtest')),
    sport          text        NOT NULL,
    engine         text        NOT NULL,
    engine_version text        NOT NULL,
    season         integer,
    game_id        text        NOT NULL,
    game_date      date,
    kind           text        NOT NULL CHECK (kind IN ('game', 'prop')),
    market         text        NOT NULL,
    subject        text        NOT NULL,
    line           double precision,
    comparison     text        NOT NULL,          -- 'market' | 'baseline': what p_base is
    decision_at    timestamptz,                   -- when the features were known
    starts_at      timestamptz,
    p_model        double precision NOT NULL,     -- engine probability of the over / home side
    p_base         double precision,              -- comparison probability (market or naive)
    features       jsonb       NOT NULL DEFAULT '{}'::jsonb,
    y              double precision,              -- 1 over/home won, 0 lost (pushes are not stored)
    inserted_at    timestamptz NOT NULL DEFAULT now(),
    UNIQUE (source, engine, engine_version, game_id, market, subject, line)
);
CREATE INDEX IF NOT EXISTS ml_examples_sel ON ml_examples (sport, market, source);

CREATE OR REPLACE FUNCTION ml_examples_write_once() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION 'ml_examples rows are immutable (id %)', OLD.id;
END $$ LANGUAGE plpgsql;
DROP TRIGGER IF EXISTS ml_examples_immutable ON ml_examples;
CREATE TRIGGER ml_examples_immutable BEFORE UPDATE OR DELETE ON ml_examples
    FOR EACH ROW EXECUTE FUNCTION ml_examples_write_once();

-- ml_experiments: append-only results of every walk-forward experiment, with the
-- dataset fingerprint and code version that produced them. `verdict` is the
-- promotion gate's decision; nothing here changes what AI Picks publishes.
CREATE TABLE IF NOT EXISTS ml_experiments (
    id           bigserial PRIMARY KEY,
    created_at   timestamptz NOT NULL DEFAULT now(),
    name         text        NOT NULL,
    sport        text        NOT NULL,
    markets      text[]      NOT NULL,
    source       text        NOT NULL,
    target       text        NOT NULL,            -- 'outcome' | 'movement'
    dataset_hash text        NOT NULL,
    n            integer     NOT NULL,
    n_test       integer     NOT NULL,
    folds        integer     NOT NULL,
    best_model   text,
    verdict      text        NOT NULL CHECK (verdict IN ('shadow_candidate', 'inconclusive', 'rejected', 'insufficient_data')),
    results      jsonb       NOT NULL,            -- per model: metrics, intervals, per-fold
    gate         jsonb       NOT NULL,            -- the rule and why it passed / failed
    code_version text        NOT NULL
);
CREATE INDEX IF NOT EXISTS ml_experiments_recent ON ml_experiments (sport, created_at DESC);

CREATE OR REPLACE FUNCTION ml_experiments_append_only() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION 'ml_experiments rows are append-only (id %)', OLD.id;
END $$ LANGUAGE plpgsql;
DROP TRIGGER IF EXISTS ml_experiments_immutable ON ml_experiments;
CREATE TRIGGER ml_experiments_immutable BEFORE UPDATE OR DELETE ON ml_experiments
    FOR EACH ROW EXECUTE FUNCTION ml_experiments_append_only();

-- TRUNCATE skips row triggers: refuse it too.
CREATE OR REPLACE FUNCTION ml_no_truncate() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION '% is append-only; TRUNCATE is refused', TG_TABLE_NAME;
END $$ LANGUAGE plpgsql;
DROP TRIGGER IF EXISTS ml_examples_no_truncate ON ml_examples;
CREATE TRIGGER ml_examples_no_truncate BEFORE TRUNCATE ON ml_examples FOR EACH STATEMENT EXECUTE FUNCTION ml_no_truncate();
DROP TRIGGER IF EXISTS ml_experiments_no_truncate ON ml_experiments;
CREATE TRIGGER ml_experiments_no_truncate BEFORE TRUNCATE ON ml_experiments FOR EACH STATEMENT EXECUTE FUNCTION ml_no_truncate();

COMMIT;
