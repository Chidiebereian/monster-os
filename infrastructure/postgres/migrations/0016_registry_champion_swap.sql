-- One champion per sport, checked at the end of each statement.
--
-- 0015 enforced it with a partial unique index, which Postgres checks row by
-- row: the single-statement swap used by promote/rollback (old champion ->
-- retired, new -> champion) failed whenever the new row was stored before
-- the old one. A deferrable exclusion constraint has the same meaning but is
-- checked once the statement has finished.

DROP INDEX IF EXISTS model_registry_one_champion;

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'model_registry_one_champion') THEN
        ALTER TABLE model_registry
            ADD CONSTRAINT model_registry_one_champion
            EXCLUDE USING btree (sport WITH =) WHERE (status = 'champion')
            DEFERRABLE INITIALLY IMMEDIATE;
    END IF;
END $$;
