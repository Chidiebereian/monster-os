-- Production governance (docs/architecture/governance.md).
--
-- 1. Publication state. Evidence (pick_evidence tiers) and publication are
--    separate: tiers may change automatically, publication only by an
--    authenticated decision or a predefined safety rule. The decision log is
--    the existing append-only publication_holds table; the latest decision per
--    engine x market is the state:
--      publish / release -> published   default feed when the pick's tier is eligible (proven / baseline)
--      leans_only        -> leans_only  only with "include unproven" (any tier)
--      suppress / suspend-> suspended   never public
--      (no decision)     -> unpublished never public; a new market starts here
-- 2. promotion_requests: the only way a production model changes. The binding
--    (what, which version, against which champion and which validation
--    artifact) is fixed at creation; status moves only along the lifecycle;
--    every change is copied to promotion_request_events (append-only).
-- 3. research_backtest_skips: games a research backtest could not forecast,
--    so resumable runs count them once instead of retrying them forever.
BEGIN;

-- ---------------------------------------------------------------- publication
ALTER TABLE publication_holds DROP CONSTRAINT IF EXISTS publication_holds_action_check;
ALTER TABLE publication_holds ADD CONSTRAINT publication_holds_action_check
    CHECK (action IN ('suppress', 'release', 'publish', 'leans_only', 'suspend'));
-- how the decision was authorised: migration, seed (cut-over snapshot), admin_token, safety_rule
ALTER TABLE publication_holds ADD COLUMN IF NOT EXISTS auth_method text NOT NULL DEFAULT 'migration';

CREATE OR REPLACE VIEW publication_state AS
SELECT engine, market,
       CASE action WHEN 'publish' THEN 'published' WHEN 'release' THEN 'published' WHEN 'leans_only' THEN 'leans_only' ELSE 'suspended' END AS status,
       reason, decided_by, auth_method, decided_at
FROM (SELECT DISTINCT ON (engine, market) engine, market, action, reason, decided_by, auth_method, decided_at
      FROM publication_holds ORDER BY engine, market, decided_at DESC, id DESC) d;

CREATE OR REPLACE VIEW publication_holds_active AS
SELECT engine, market, reason, decided_by, decided_at FROM publication_state WHERE status = 'suspended';

-- ---------------------------------------------------------------- promotion requests
CREATE TABLE IF NOT EXISTS promotion_requests (
    request_id            bigserial PRIMARY KEY,
    idempotency_key       text        NOT NULL,          -- unique among open requests (index below)
    kind                  text        NOT NULL CHECK (kind IN ('engine', 'parlay')),
    engine                text        NOT NULL,          -- registry key (nba, nhl_team, ...) or 'parlay'
    action                text        NOT NULL CHECK (action IN ('promote', 'rollback')),
    target_environment    text        NOT NULL DEFAULT 'production' CHECK (target_environment = 'production'),
    candidate_model_id    text        NOT NULL,          -- engine:version
    candidate_version     text        NOT NULL,
    champion_version      text        NOT NULL,          -- the champion this request replaces
    dataset_fingerprint   text,
    validation_report_ref text        NOT NULL,
    artifact_hash         text        NOT NULL,          -- sha256 of the validation artifact at proposal
    calibration           jsonb,
    market_comparison     jsonb,
    status                text        NOT NULL DEFAULT 'PROPOSED' CHECK (status IN
                            ('PROPOSED', 'PENDING_APPROVAL', 'APPROVED', 'EXECUTED', 'REJECTED', 'CANCELLED', 'EXPIRED', 'FAILED')),
    proposed_by           text        NOT NULL,
    proposed_at           timestamptz NOT NULL DEFAULT now(),
    expires_at            timestamptz NOT NULL DEFAULT now() + interval '72 hours',
    approver              text,                          -- authenticated principal, never client-supplied
    approved_at           timestamptz,
    decision_reason       text,
    executed_at           timestamptz,
    failure               text,
    last_actor            text,                          -- who made the latest transition (copied to the event)
    CHECK (status NOT IN ('APPROVED', 'EXECUTED') OR (approver IS NOT NULL AND approved_at IS NOT NULL)),
    CHECK (status <> 'EXECUTED' OR executed_at IS NOT NULL),
    CHECK (status NOT IN ('REJECTED', 'CANCELLED', 'EXPIRED', 'FAILED') OR decision_reason IS NOT NULL OR failure IS NOT NULL)
);
ALTER TABLE promotion_requests ADD COLUMN IF NOT EXISTS last_actor text;  -- for databases that created the table before this column
-- the same proposal (same candidate, champion and artifact) is only open once; once decided it may be filed again
ALTER TABLE promotion_requests DROP CONSTRAINT IF EXISTS promotion_requests_idempotency_key_key;
CREATE UNIQUE INDEX IF NOT EXISTS promotion_requests_idempotent_open ON promotion_requests (idempotency_key)
    WHERE status IN ('PROPOSED', 'PENDING_APPROVAL', 'APPROVED');
-- one open request per model line at a time
CREATE UNIQUE INDEX IF NOT EXISTS promotion_requests_one_open ON promotion_requests (kind, engine)
    WHERE status IN ('PROPOSED', 'PENDING_APPROVAL', 'APPROVED');
CREATE INDEX IF NOT EXISTS promotion_requests_status ON promotion_requests (status, expires_at);

CREATE TABLE IF NOT EXISTS promotion_request_events (
    id          bigserial PRIMARY KEY,
    request_id  bigint      NOT NULL REFERENCES promotion_requests(request_id),
    at          timestamptz NOT NULL DEFAULT now(),
    from_status text,
    to_status   text        NOT NULL,
    actor       text        NOT NULL,
    detail      jsonb       NOT NULL DEFAULT '{}'::jsonb
);

CREATE OR REPLACE FUNCTION promotion_request_guard() RETURNS trigger AS $$
DECLARE ok boolean;
BEGIN
    IF TG_OP = 'DELETE' THEN RAISE EXCEPTION 'promotion_requests are never deleted (request %)', OLD.request_id; END IF;
    IF (NEW.idempotency_key, NEW.kind, NEW.engine, NEW.action, NEW.target_environment, NEW.candidate_model_id, NEW.candidate_version,
        NEW.champion_version, NEW.dataset_fingerprint, NEW.validation_report_ref, NEW.artifact_hash, NEW.proposed_by, NEW.proposed_at)
       IS DISTINCT FROM
       (OLD.idempotency_key, OLD.kind, OLD.engine, OLD.action, OLD.target_environment, OLD.candidate_model_id, OLD.candidate_version,
        OLD.champion_version, OLD.dataset_fingerprint, OLD.validation_report_ref, OLD.artifact_hash, OLD.proposed_by, OLD.proposed_at) THEN
        RAISE EXCEPTION 'the binding of promotion request % cannot change; file a new request', OLD.request_id;
    END IF;
    IF OLD.approver IS NOT NULL AND NEW.approver IS DISTINCT FROM OLD.approver THEN
        RAISE EXCEPTION 'the approver of promotion request % cannot change', OLD.request_id;
    END IF;
    IF NEW.status <> OLD.status THEN
        ok := (OLD.status, NEW.status) IN (
            ('PROPOSED', 'PENDING_APPROVAL'), ('PROPOSED', 'CANCELLED'), ('PROPOSED', 'EXPIRED'),
            ('PENDING_APPROVAL', 'APPROVED'), ('PENDING_APPROVAL', 'REJECTED'), ('PENDING_APPROVAL', 'CANCELLED'), ('PENDING_APPROVAL', 'EXPIRED'),
            ('APPROVED', 'EXECUTED'), ('APPROVED', 'FAILED'));
        IF NOT ok THEN RAISE EXCEPTION 'promotion request %: % -> % is not allowed', OLD.request_id, OLD.status, NEW.status; END IF;
        INSERT INTO promotion_request_events (request_id, from_status, to_status, actor, detail)
        VALUES (NEW.request_id, OLD.status, NEW.status, COALESCE(NEW.last_actor, 'unknown'),
                jsonb_build_object('reason', NEW.decision_reason, 'failure', NEW.failure));
    ELSIF ROW(NEW.*) IS DISTINCT FROM ROW(OLD.*) THEN
        RAISE EXCEPTION 'promotion request % can only change status', OLD.request_id;
    END IF;
    RETURN NEW;
END $$ LANGUAGE plpgsql;
DROP TRIGGER IF EXISTS promotion_requests_guard ON promotion_requests;
CREATE TRIGGER promotion_requests_guard BEFORE UPDATE OR DELETE ON promotion_requests FOR EACH ROW EXECUTE FUNCTION promotion_request_guard();
DROP TRIGGER IF EXISTS promotion_requests_no_truncate ON promotion_requests;
CREATE TRIGGER promotion_requests_no_truncate BEFORE TRUNCATE ON promotion_requests FOR EACH STATEMENT EXECUTE FUNCTION ml_no_truncate();

CREATE OR REPLACE FUNCTION promotion_request_created() RETURNS trigger AS $$
BEGIN
    INSERT INTO promotion_request_events (request_id, from_status, to_status, actor, detail)
    VALUES (NEW.request_id, NULL, NEW.status, NEW.proposed_by, jsonb_build_object('artifact_hash', NEW.artifact_hash, 'candidate', NEW.candidate_version,
            'champion', NEW.champion_version, 'action', NEW.action));
    RETURN NEW;
END $$ LANGUAGE plpgsql;
DROP TRIGGER IF EXISTS promotion_requests_created ON promotion_requests;
CREATE TRIGGER promotion_requests_created AFTER INSERT ON promotion_requests FOR EACH ROW EXECUTE FUNCTION promotion_request_created();

CREATE OR REPLACE FUNCTION promotion_events_append_only() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION 'promotion_request_events is append-only (id %)', OLD.id;
END $$ LANGUAGE plpgsql;
DROP TRIGGER IF EXISTS promotion_request_events_immutable ON promotion_request_events;
CREATE TRIGGER promotion_request_events_immutable BEFORE UPDATE OR DELETE ON promotion_request_events FOR EACH ROW EXECUTE FUNCTION promotion_events_append_only();
DROP TRIGGER IF EXISTS promotion_request_events_no_truncate ON promotion_request_events;
CREATE TRIGGER promotion_request_events_no_truncate BEFORE TRUNCATE ON promotion_request_events FOR EACH STATEMENT EXECUTE FUNCTION ml_no_truncate();

-- ---------------------------------------------------------------- research backtest skips
CREATE TABLE IF NOT EXISTS research_backtest_skips (
    engine         text        NOT NULL,
    engine_version text        NOT NULL,
    season         integer     NOT NULL,
    game_id        text        NOT NULL,
    reason         text        NOT NULL,
    at             timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (engine, engine_version, game_id)
);

COMMIT;
