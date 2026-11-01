-- Publication holds and research isolation (docs/architecture/validation.md).
--
-- publication_holds: an append-only log of decisions to hide an engine x market
-- from public AI Picks, independent of its evidence tier. The latest decision
-- per engine x market wins; a hold is lifted only by a later 'release' row with
-- a named decider (scripts/ops/release_hold.py). There is no HTTP path that
-- writes this table. Evidence (pick_evidence) is not touched: the snapshot
-- column records the evidence as it stood when the decision was made.
BEGIN;

CREATE TABLE IF NOT EXISTS publication_holds (
    id                bigserial PRIMARY KEY,
    engine            text        NOT NULL,
    market            text        NOT NULL,
    action            text        NOT NULL CHECK (action IN ('suppress', 'release')),
    reason            text        NOT NULL,
    decided_by        text        NOT NULL,
    decided_at        timestamptz NOT NULL DEFAULT now(),
    evidence_snapshot jsonb       NOT NULL DEFAULT '[]'::jsonb
);
CREATE INDEX IF NOT EXISTS publication_holds_key ON publication_holds (engine, market, decided_at DESC, id DESC);

CREATE OR REPLACE FUNCTION publication_holds_append_only() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION 'publication_holds is append-only: record a new decision instead (id %)', OLD.id;
END $$ LANGUAGE plpgsql;
DROP TRIGGER IF EXISTS publication_holds_immutable ON publication_holds;
CREATE TRIGGER publication_holds_immutable BEFORE UPDATE OR DELETE ON publication_holds
    FOR EACH ROW EXECUTE FUNCTION publication_holds_append_only();
DROP TRIGGER IF EXISTS publication_holds_no_truncate ON publication_holds;
CREATE TRIGGER publication_holds_no_truncate BEFORE TRUNCATE ON publication_holds
    FOR EACH STATEMENT EXECUTE FUNCTION ml_no_truncate();

-- engine x market currently hidden from public AI Picks
CREATE OR REPLACE VIEW publication_holds_active AS
SELECT engine, market, reason, decided_by, decided_at FROM (
    SELECT DISTINCT ON (engine, market) engine, market, action, reason, decided_by, decided_at
    FROM publication_holds ORDER BY engine, market, decided_at DESC, id DESC) d
WHERE action = 'suppress';

-- The five markets whose evidence was refreshed by research backtests run in
-- normal mode on 2026-11-01 (ml-research.md, "Production side effect").
INSERT INTO publication_holds (engine, market, action, reason, decided_by, evidence_snapshot)
SELECT v.engine, v.market, 'suppress',
       'evidence refreshed ahead of schedule by research backtests run in normal mode on 2026-11-01; '
       || 'hidden until an explicit release decision (Phase 2A audit)',
       'phase-2a-audit',
       COALESCE((SELECT jsonb_agg(to_jsonb(e) - 'detail' || jsonb_build_object('w', e.detail->'w', 'engine_version', e.detail->'engine_version'))
                 FROM pick_evidence e WHERE e.engine = v.engine AND e.market = v.market), '[]'::jsonb)
FROM (VALUES ('nhl_team', 'team_total_away'), ('nhl_team', 'team_total_home'), ('nba_team', 'moneyline_1h'),
             ('nhl_team', 'moneyline_1h'), ('nhl_team', 'team_total_away_1h')) AS v(engine, market)
WHERE NOT EXISTS (SELECT 1 FROM publication_holds h WHERE h.engine = v.engine AND h.market = v.market);

-- Research jobs (scripts/ml) connect as monster_research: they can read
-- everything and add rows to the research tables, nothing else. A research
-- script that tried to touch production evidence, picks or publication holds
-- fails with "permission denied". The role is created without a credential;
-- scripts/ops/rotate_research_password.py sets one and stores the URL in the
-- git-ignored .env (scripts/dbenv.py reads it).
DO $$ BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'monster_research') THEN
        CREATE ROLE monster_research LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE;
    END IF;
END $$;
GRANT CONNECT ON DATABASE monsteros TO monster_research;
GRANT USAGE ON SCHEMA public TO monster_research;
GRANT SELECT ON ALL TABLES IN SCHEMA public TO monster_research;
GRANT INSERT ON ml_examples, ml_experiments, feature_snapshots TO monster_research;
GRANT USAGE ON SEQUENCE ml_examples_id_seq, ml_experiments_id_seq, feature_snapshots_id_seq TO monster_research;

COMMIT;
