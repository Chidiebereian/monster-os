-- Adaptive learning loop: frozen pregame predictions, postgame scoring,
-- a model registry with gated promotion / rollback, and an audit trail.
--
-- Integrity rules enforced in the database, not only in code:
--   * a ledger row is immutable once written (no UPDATE, no DELETE);
--   * a forecast's prediction fields never change; its outcome is written once;
--   * at most one champion per sport.

CREATE TABLE IF NOT EXISTS model_registry (
    sport        TEXT        NOT NULL,
    version      TEXT        NOT NULL,             -- e.g. nhl-event-sim-v1+cfg.3f2a9c1e
    engine       TEXT        NOT NULL,             -- engine MODEL_VERSION in code
    config       JSONB       NOT NULL DEFAULT '{}', -- hyper-parameter overlay (empty = engine defaults)
    status       TEXT        NOT NULL CHECK (status IN ('candidate', 'champion', 'retired', 'rejected')),
    parent       TEXT,                              -- version it was derived from
    evidence     JSONB,                             -- validation report behind the last status change
    notes        TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    promoted_at  TIMESTAMPTZ,
    retired_at   TIMESTAMPTZ,
    PRIMARY KEY (sport, version)
);
CREATE UNIQUE INDEX IF NOT EXISTS model_registry_one_champion ON model_registry (sport) WHERE status = 'champion';

CREATE TABLE IF NOT EXISTS prediction_ledger (
    id             BIGSERIAL   PRIMARY KEY,
    sport          TEXT        NOT NULL,
    game_id        TEXT        NOT NULL,
    game_date      DATE        NOT NULL,
    game_start     TIMESTAMPTZ,                    -- scheduled start when the source has one
    frozen_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    model_version  TEXT        NOT NULL,           -- registry version that produced it
    engine         TEXT        NOT NULL,
    lineup_mode    TEXT        NOT NULL,
    seed           BIGINT      NOT NULL,
    n_sims         INTEGER     NOT NULL,
    data_cutoff    DATE        NOT NULL,           -- inputs strictly before this date + current-day injury/lineup feeds
    payload_sha256 TEXT        NOT NULL,
    payload        JSONB       NOT NULL,           -- the full prediction exactly as served
    UNIQUE (sport, game_id, model_version, lineup_mode),
    CHECK (game_start IS NULL OR frozen_at < game_start)
);
CREATE INDEX IF NOT EXISTS prediction_ledger_sport_date ON prediction_ledger (sport, game_date);

CREATE TABLE IF NOT EXISTS prediction_forecasts (
    ledger_id     BIGINT      NOT NULL REFERENCES prediction_ledger (id),
    market        TEXT        NOT NULL,            -- home_win, total_over_5_5, sog_over_avg, ...
    subject       TEXT        NOT NULL,            -- 'game' or a player id
    name          TEXT        NOT NULL,
    stat          TEXT        NOT NULL,            -- outcome statistic; y = actual > line
    line          DOUBLE PRECISION NOT NULL,
    p_model       DOUBLE PRECISION NOT NULL,
    p_base        DOUBLE PRECISION NOT NULL,
    baseline      TEXT        NOT NULL,
    push_void     BOOLEAN     NOT NULL DEFAULT FALSE,  -- actual == line voids (a push at a market line)
    -- written once, after the game
    actual        DOUBLE PRECISION,
    y             SMALLINT,
    void_reason   TEXT,                            -- e.g. player did not play (props are conditional on playing)
    scored_at     TIMESTAMPTZ,
    PRIMARY KEY (ledger_id, market, subject)
);

CREATE TABLE IF NOT EXISTS learning_audit (
    id       BIGSERIAL   PRIMARY KEY,
    at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    sport    TEXT        NOT NULL,
    action   TEXT        NOT NULL,   -- freeze, score, register, evaluate, promote, reject, rollback, refit
    version  TEXT,
    detail   JSONB       NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS learning_audit_sport_at ON learning_audit (sport, at DESC);

CREATE OR REPLACE FUNCTION ledger_immutable() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION 'prediction_ledger rows are immutable (id %)', OLD.id;
END $$ LANGUAGE plpgsql;
DROP TRIGGER IF EXISTS ledger_immutable ON prediction_ledger;
CREATE TRIGGER ledger_immutable BEFORE UPDATE OR DELETE ON prediction_ledger
    FOR EACH ROW EXECUTE FUNCTION ledger_immutable();

CREATE OR REPLACE FUNCTION forecast_write_once() RETURNS trigger AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'prediction_forecasts rows cannot be deleted';
    END IF;
    IF (NEW.market, NEW.subject, NEW.stat, NEW.line, NEW.p_model, NEW.p_base, NEW.baseline, NEW.push_void)
       IS DISTINCT FROM (OLD.market, OLD.subject, OLD.stat, OLD.line, OLD.p_model, OLD.p_base, OLD.baseline, OLD.push_void) THEN
        RAISE EXCEPTION 'frozen forecast fields cannot change';
    END IF;
    IF OLD.scored_at IS NOT NULL THEN
        RAISE EXCEPTION 'forecast already scored';
    END IF;
    RETURN NEW;
END $$ LANGUAGE plpgsql;
DROP TRIGGER IF EXISTS forecast_write_once ON prediction_forecasts;
CREATE TRIGGER forecast_write_once BEFORE UPDATE OR DELETE ON prediction_forecasts
    FOR EACH ROW EXECUTE FUNCTION forecast_write_once();
