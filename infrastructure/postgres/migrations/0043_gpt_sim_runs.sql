-- Simulations requested from chat (docs/architecture/monster-gpt-v2.md, 3 and 6).
-- Kept apart from prediction_ledger, which holds the champions' frozen, scored forecasts.
-- A run records everything needed to reproduce it (engine, model version, seed, n,
-- input hash, overrides). Once finished, failed or cancelled it is never changed.
BEGIN;

CREATE TABLE IF NOT EXISTS gpt_sim_runs (
    run_id        text PRIMARY KEY,
    sport         text NOT NULL,
    game_id       text NOT NULL,
    engine        text NOT NULL,
    model_version text,
    seed          bigint NOT NULL,
    n             int NOT NULL CHECK (n BETWEEN 1 AND 1000000),
    inputs_sha256 text,
    overrides     jsonb NOT NULL DEFAULT '{}',   -- {out:[...], in:[...]}: set means hypothetical
    status        text NOT NULL DEFAULT 'queued' CHECK (status IN ('queued', 'running', 'done', 'failed', 'cancelled')),
    result        jsonb,
    error         text,
    requested_by  text NOT NULL,                 -- conversation owner
    message_id    text,
    created_at    timestamptz NOT NULL DEFAULT now(),
    finished_at   timestamptz
);
CREATE INDEX IF NOT EXISTS gpt_sim_runs_owner_idx ON gpt_sim_runs (requested_by, created_at DESC);

CREATE OR REPLACE FUNCTION gpt_sim_runs_final() RETURNS trigger AS $$
BEGIN
    IF OLD.status IN ('done', 'failed', 'cancelled') THEN
        RAISE EXCEPTION 'gpt_sim_runs % is finished and cannot change', OLD.run_id;
    END IF;
    IF TG_OP = 'UPDATE' AND (NEW.seed <> OLD.seed OR NEW.n <> OLD.n OR NEW.overrides <> OLD.overrides
                             OR NEW.game_id <> OLD.game_id OR NEW.engine <> OLD.engine) THEN
        RAISE EXCEPTION 'gpt_sim_runs %: the inputs of a run cannot change', OLD.run_id;
    END IF;
    RETURN CASE WHEN TG_OP = 'DELETE' THEN OLD ELSE NEW END;
END $$ LANGUAGE plpgsql;
DROP TRIGGER IF EXISTS gpt_sim_runs_final ON gpt_sim_runs;
CREATE TRIGGER gpt_sim_runs_final BEFORE UPDATE OR DELETE ON gpt_sim_runs FOR EACH ROW EXECUTE FUNCTION gpt_sim_runs_final();

COMMIT;
