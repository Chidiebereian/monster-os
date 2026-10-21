-- Learning autopilot: shadow models, job bookkeeping and a pause switch.
--
-- A candidate that passes its search and held-out evaluations becomes a
-- `shadow`: it is frozen next to the champion on live games until it has
-- enough paired live forecasts to be promoted. After a promotion, the old
-- champion also stays a `shadow` (promoted_at set) so the new champion can be
-- rolled back automatically if its live record falls behind.

DO $$
DECLARE c text;
BEGIN
    FOR c IN SELECT conname FROM pg_constraint
             WHERE conrelid = 'model_registry'::regclass AND contype = 'c' AND pg_get_constraintdef(oid) LIKE '%status%'
    LOOP
        EXECUTE format('ALTER TABLE model_registry DROP CONSTRAINT %I', c);
    END LOOP;
END $$;
ALTER TABLE model_registry ADD CONSTRAINT model_registry_status_check
    CHECK (status IN ('candidate', 'shadow', 'champion', 'retired', 'rejected'));

-- When each recurring job last ran (survives restarts, so jobs are not repeated).
CREATE TABLE IF NOT EXISTS learning_jobs (
    job       TEXT        PRIMARY KEY,    -- e.g. ingest:nba, tune:nhl_team
    last_run  TIMESTAMPTZ NOT NULL,
    last_ok   BOOLEAN     NOT NULL,
    detail    JSONB       NOT NULL DEFAULT '{}'
);

CREATE TABLE IF NOT EXISTS learning_settings (
    key        TEXT        PRIMARY KEY,   -- 'autopilot' -> {"paused": false}
    value      JSONB       NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
