-- Decision-time feature store for ML research (scripts/ml/features.py,
-- docs/architecture/ml-research.md). Every value says when the event is
-- (`event_at`), when it was observed (`observed_at`), when it could first have
-- been used (`available_at`), where it came from and how trustworthy it is.
-- Experiments join a feature only if `available_at` <= the example's decision
-- time. A value that cannot be shown to have been available is stored as
-- quality 'missing' (value NULL), never reconstructed from later data.
--   quality  observed   recorded at the time (engine inputs at freeze, odds snapshots)
--            derived    computed from data that was public before available_at (schedules)
--            inferred   timing assumed from a rule, not recorded (kept apart in experiments)
--            missing    no evidence it was available
BEGIN;

CREATE TABLE IF NOT EXISTS feature_snapshots (
    id           bigserial PRIMARY KEY,
    sport        text        NOT NULL,
    game_id      text        NOT NULL,
    subject      text        NOT NULL,          -- player id, 'team:<abbr>', or 'line:<market>|<subject>|<line>'
    feature_set  text        NOT NULL,          -- availability | lineup | minutes | market_horizon | schedule
    name         text        NOT NULL,
    value        double precision,
    value_text   text,
    event_at     timestamptz,
    observed_at  timestamptz NOT NULL,
    available_at timestamptz NOT NULL,
    source       text        NOT NULL,
    quality      text        NOT NULL CHECK (quality IN ('observed', 'derived', 'inferred', 'missing')),
    extractor    text        NOT NULL,
    inserted_at  timestamptz NOT NULL DEFAULT now(),
    CHECK (quality <> 'missing' OR value IS NULL),
    UNIQUE (sport, game_id, subject, feature_set, name, source, observed_at)
);
CREATE INDEX IF NOT EXISTS feature_snapshots_asof ON feature_snapshots (sport, game_id, subject, available_at);

CREATE OR REPLACE FUNCTION feature_snapshots_write_once() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION 'feature_snapshots rows are immutable (id %)', OLD.id;
END $$ LANGUAGE plpgsql;
DROP TRIGGER IF EXISTS feature_snapshots_immutable ON feature_snapshots;
CREATE TRIGGER feature_snapshots_immutable BEFORE UPDATE OR DELETE ON feature_snapshots
    FOR EACH ROW EXECUTE FUNCTION feature_snapshots_write_once();
DROP TRIGGER IF EXISTS feature_snapshots_no_truncate ON feature_snapshots;
CREATE TRIGGER feature_snapshots_no_truncate BEFORE TRUNCATE ON feature_snapshots
    FOR EACH STATEMENT EXECUTE FUNCTION ml_no_truncate();

COMMIT;
