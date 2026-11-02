-- Governance privilege boundary (docs/architecture/governance.md, "Database roles").
--
-- Before this migration every service connected as the database owner (a
-- superuser), so the triggers of 0038 bound the code, not the credential. Now:
--
--   monster          owner / superuser. Migrations and break-glass operations only
--                    (OPS_DATABASE_URL); not used by any running service.
--   monster_app      every service (APP_DATABASE_URL). Ordinary read/write on the
--                    application tables, but it cannot make a model champion, change
--                    a champion, approve / reject / execute a promotion request, write
--                    an approval or execution event, publish or restore a market,
--                    rewrite audit history, or read admin credentials.
--   monster_research research jobs (0037): read, plus inserts into the ML tables.
--
-- Production decisions go through SECURITY DEFINER functions owned by the owner.
-- The functions that decide (approve, reject, cancel, publish) take the admin's
-- bearer token and check it against gov_private.admins themselves, so holding the
-- monster_app password is not enough to approve anything. The functions the app
-- may call without a token can only propose, expire, suspend, or perform the
-- documented automatic registry steps (first champion, engine-upgrade relabel).
BEGIN;

-- ---------------------------------------------------------------- roles
DO $$ BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'monster_app') THEN
        -- no password here: scripts/ops/rotate_research_password.py --role monster_app sets one
        CREATE ROLE monster_app LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS;
    END IF;
END $$;
GRANT CONNECT ON DATABASE monsteros TO monster_app;
GRANT USAGE ON SCHEMA public TO monster_app;
REVOKE CREATE ON SCHEMA public FROM PUBLIC;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO monster_app;
GRANT USAGE, SELECT, UPDATE ON ALL SEQUENCES IN SCHEMA public TO monster_app;
-- tables added by later migrations are ordinary application tables unless the
-- migration revokes (a governed table must revoke explicitly, as below)
ALTER DEFAULT PRIVILEGES FOR ROLE monster IN SCHEMA public GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO monster_app;
ALTER DEFAULT PRIVILEGES FOR ROLE monster IN SCHEMA public GRANT USAGE, SELECT, UPDATE ON SEQUENCES TO monster_app;

-- ---------------------------------------------------------------- admin credentials
-- A schema neither service role can use: a later "GRANT SELECT ON ALL TABLES IN
-- SCHEMA public" (0037 re-applied, say) cannot expose it. Only SHA-256 digests
-- of tokens are stored; the tokens themselves live in .secrets/ (git-ignored).
CREATE SCHEMA IF NOT EXISTS gov_private;
REVOKE ALL ON SCHEMA gov_private FROM PUBLIC;
CREATE TABLE IF NOT EXISTS gov_private.admins (
    principal    text        PRIMARY KEY CHECK (principal ~ '^[A-Za-z0-9_.@-]{2,40}$'),
    token_sha256 bytea       NOT NULL UNIQUE CHECK (length(token_sha256) = 32),
    role         text        NOT NULL CHECK (role IN ('approver', 'auditor')),  -- auditor: authenticates, decides nothing
    created_at   timestamptz NOT NULL DEFAULT now(),
    expires_at   timestamptz NOT NULL DEFAULT now() + interval '180 days',
    revoked_at   timestamptz
);
REVOKE ALL ON gov_private.admins FROM PUBLIC;

-- ---------------------------------------------------------------- governed tables
REVOKE ALL ON promotion_requests, promotion_request_events, publication_holds FROM PUBLIC, monster_app;
GRANT SELECT ON promotion_requests, promotion_request_events, publication_holds TO monster_app;
REVOKE ALL ON SEQUENCE promotion_requests_request_id_seq, promotion_request_events_id_seq, publication_holds_id_seq FROM monster_app;
-- registries: the tuner adds candidates, shadows, rejections and retirements; champion rows are guarded below
REVOKE DELETE, TRUNCATE ON model_registry, parlay_models FROM monster_app;
-- audit history: append-only for everyone, governed actions written by the functions only
REVOKE UPDATE, DELETE, TRUNCATE ON learning_audit FROM monster_app;
-- research skip records: write-once (a skip can be added, never removed or rewritten)
REVOKE UPDATE, DELETE, TRUNCATE ON research_backtest_skips FROM monster_app;

CREATE OR REPLACE FUNCTION gov_is_owner(rel oid) RETURNS boolean LANGUAGE sql STABLE AS $$
    SELECT pg_has_role(current_user, (SELECT relowner FROM pg_class WHERE oid = rel), 'MEMBER')
$$;

-- A champion row is created, changed or retired only inside the owner's functions
-- (an approved request, the first champion, the engine-upgrade relabel).
-- Others may still update a champion's evidence and notes.
CREATE OR REPLACE FUNCTION gov_champion_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF gov_is_owner(TG_RELID) THEN RETURN COALESCE(NEW, OLD); END IF;
    IF TG_OP = 'INSERT' AND NEW.status = 'champion' THEN
        RAISE EXCEPTION 'governance: a % row becomes champion only through an approved promotion request', TG_TABLE_NAME USING ERRCODE = '42501';
    END IF;
    IF TG_OP = 'UPDATE' AND (OLD.status = 'champion' OR NEW.status = 'champion')
       AND (to_jsonb(NEW) - 'evidence' - 'notes') IS DISTINCT FROM (to_jsonb(OLD) - 'evidence' - 'notes') THEN
        RAISE EXCEPTION 'governance: % champion % changes only through an approved promotion request', TG_TABLE_NAME, OLD.version USING ERRCODE = '42501';
    END IF;
    IF TG_OP = 'DELETE' AND OLD.status = 'champion' THEN
        RAISE EXCEPTION 'governance: champion rows are never deleted' USING ERRCODE = '42501';
    END IF;
    RETURN COALESCE(NEW, OLD);
END $$;
DROP TRIGGER IF EXISTS model_registry_champion_guard ON model_registry;
CREATE TRIGGER model_registry_champion_guard BEFORE INSERT OR UPDATE OR DELETE ON model_registry FOR EACH ROW EXECUTE FUNCTION gov_champion_guard();
DROP TRIGGER IF EXISTS parlay_models_champion_guard ON parlay_models;
CREATE TRIGGER parlay_models_champion_guard BEFORE INSERT OR UPDATE OR DELETE ON parlay_models FOR EACH ROW EXECUTE FUNCTION gov_champion_guard();

-- learning_audit: append-only; promote / rollback / first-champion / engine-upgrade rows only from the functions
CREATE OR REPLACE FUNCTION gov_audit_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP <> 'INSERT' THEN RAISE EXCEPTION 'learning_audit is append-only (id %)', OLD.id; END IF;
    IF NEW.action IN ('promote', 'rollback', 'register', 'engine_upgrade') AND NOT gov_is_owner(TG_RELID) THEN
        RAISE EXCEPTION 'governance: % audit rows are written by the governance functions only', NEW.action USING ERRCODE = '42501';
    END IF;
    RETURN NEW;
END $$;
DROP TRIGGER IF EXISTS learning_audit_guard ON learning_audit;
CREATE TRIGGER learning_audit_guard BEFORE INSERT OR UPDATE OR DELETE ON learning_audit FOR EACH ROW EXECUTE FUNCTION gov_audit_guard();
DROP TRIGGER IF EXISTS learning_audit_no_truncate ON learning_audit;
CREATE TRIGGER learning_audit_no_truncate BEFORE TRUNCATE ON learning_audit FOR EACH STATEMENT EXECUTE FUNCTION ml_no_truncate();

-- a request is born PROPOSED with nothing decided (also binds the owner's own inserts)
CREATE OR REPLACE FUNCTION promotion_request_born() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.status <> 'PROPOSED' OR NEW.approver IS NOT NULL OR NEW.approved_at IS NOT NULL OR NEW.executed_at IS NOT NULL
       OR NEW.decision_reason IS NOT NULL OR NEW.failure IS NOT NULL THEN
        RAISE EXCEPTION 'a promotion request is created PROPOSED, undecided';
    END IF;
    RETURN NEW;
END $$;
DROP TRIGGER IF EXISTS promotion_requests_born ON promotion_requests;
CREATE TRIGGER promotion_requests_born BEFORE INSERT ON promotion_requests FOR EACH ROW EXECUTE FUNCTION promotion_request_born();

-- ---------------------------------------------------------------- authentication
-- principal of an active token; p_need 'approver' also requires the approver role
CREATE OR REPLACE FUNCTION gov_authenticate(p_token text, p_need text DEFAULT 'approver') RETURNS text
LANGUAGE plpgsql STABLE SECURITY DEFINER SET search_path = pg_catalog, public AS $$
DECLARE who text; r text;
BEGIN
    SELECT a.principal, a.role INTO who, r FROM gov_private.admins a
     WHERE a.token_sha256 = sha256(convert_to(coalesce(p_token, ''), 'UTF8')) AND a.revoked_at IS NULL AND a.expires_at > now();
    IF who IS NULL THEN RAISE EXCEPTION 'governance: not an active admin token' USING ERRCODE = '28000'; END IF;
    IF p_need = 'approver' AND r <> 'approver' THEN
        RAISE EXCEPTION 'governance: principal % (role %) may not make production decisions', who, r USING ERRCODE = '42501';
    END IF;
    RETURN who;
END $$;

CREATE OR REPLACE FUNCTION gov_whoami(p_token text) RETURNS jsonb
LANGUAGE plpgsql STABLE SECURITY DEFINER SET search_path = pg_catalog, public AS $$
DECLARE who text := gov_authenticate(p_token, 'any');
BEGIN
    RETURN (SELECT jsonb_build_object('principal', principal, 'role', role, 'expires_at', expires_at) FROM gov_private.admins WHERE principal = who);
END $$;

CREATE OR REPLACE FUNCTION gov_admins_configured() RETURNS boolean
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = pg_catalog, public AS $$
    SELECT EXISTS (SELECT 1 FROM gov_private.admins WHERE role = 'approver' AND revoked_at IS NULL AND expires_at > now())
$$;

-- ---------------------------------------------------------------- the validation artifact a request binds
-- sha256 over the version's evidence AND what defines the model (config / engine, dep weight)
CREATE OR REPLACE FUNCTION gov_artifact(p_kind text, p_engine text, p_version text, OUT evidence jsonb, OUT hash text, OUT status text)
LANGUAGE plpgsql STABLE SECURITY DEFINER SET search_path = pg_catalog, public AS $$
BEGIN
    IF p_kind = 'parlay' THEN
        SELECT coalesce(m.evidence, '{}'::jsonb), encode(sha256(convert_to(jsonb_build_object('evidence', coalesce(m.evidence, '{}'::jsonb),
               'dep_weight', m.dep_weight)::text, 'UTF8')), 'hex'), m.status INTO evidence, hash, status
          FROM parlay_models m WHERE m.version = p_version AND p_engine = 'parlay';
    ELSE
        SELECT coalesce(m.evidence, '{}'::jsonb), encode(sha256(convert_to(jsonb_build_object('evidence', coalesce(m.evidence, '{}'::jsonb),
               'config', m.config, 'engine', m.engine)::text, 'UTF8')), 'hex'), m.status INTO evidence, hash, status
          FROM model_registry m WHERE m.sport = p_engine AND m.version = p_version;
    END IF;
END $$;

CREATE OR REPLACE FUNCTION gov_champion_of(p_kind text, p_engine text) RETURNS text
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = pg_catalog, public AS $$
    SELECT CASE WHEN p_kind = 'parlay' THEN (SELECT version FROM parlay_models WHERE status = 'champion' AND p_engine = 'parlay')
                ELSE (SELECT version FROM model_registry WHERE sport = p_engine AND status = 'champion') END
$$;

CREATE OR REPLACE FUNCTION gov_subject_ok(p_kind text, p_action text, p_status text) RETURNS boolean LANGUAGE sql IMMUTABLE AS $$
    SELECT (p_kind = 'engine' AND p_action = 'promote' AND p_status IN ('shadow', 'candidate'))
        OR (p_kind = 'parlay' AND p_action = 'promote' AND p_status = 'candidate')
        OR (p_action = 'rollback' AND p_status IN ('retired', 'shadow'))
$$;

-- ---------------------------------------------------------------- proposing (no token: proposing changes nothing)
CREATE OR REPLACE FUNCTION gov_propose(p_kind text, p_engine text, p_action text, p_candidate text, p_proposed_by text) RETURNS jsonb
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, public AS $$
DECLARE champ text; a record; key text; id bigint; o record; v_by text := left(coalesce(nullif(trim(p_proposed_by), ''), 'unknown'), 80);
BEGIN
    IF p_kind NOT IN ('engine', 'parlay') OR p_action NOT IN ('promote', 'rollback') OR (p_kind = 'parlay') <> (p_engine = 'parlay') THEN
        RAISE EXCEPTION 'governance: kind engine|parlay (engine parlay), action promote|rollback' USING ERRCODE = '22023';
    END IF;
    champ := gov_champion_of(p_kind, p_engine);
    IF champ IS NULL THEN RAISE EXCEPTION 'governance: no current champion' USING ERRCODE = '55000'; END IF;
    IF p_candidate = champ THEN RAISE EXCEPTION 'governance: the candidate is already the champion' USING ERRCODE = '55000'; END IF;
    SELECT * INTO a FROM gov_artifact(p_kind, p_engine, p_candidate);
    IF a.status IS NULL THEN RAISE EXCEPTION 'governance: unknown candidate version' USING ERRCODE = '55000'; END IF;
    IF NOT gov_subject_ok(p_kind, p_action, a.status) THEN
        RAISE EXCEPTION 'governance: % is %; it cannot be the subject of a %', p_candidate, a.status, p_action USING ERRCODE = '55000';
    END IF;
    key := concat_ws('|', p_kind, p_engine, p_action, p_candidate, champ, a.hash);
    INSERT INTO promotion_requests (idempotency_key, kind, engine, action, candidate_model_id, candidate_version, champion_version, dataset_fingerprint,
                                    validation_report_ref, artifact_hash, calibration, market_comparison, proposed_by, last_actor)
    VALUES (key, p_kind, p_engine, p_action, p_engine || ':' || p_candidate, p_candidate, champ,
            encode(sha256(convert_to(coalesce(a.evidence -> 'live', 'null'::jsonb)::text, 'UTF8')), 'hex'),
            CASE WHEN p_kind = 'parlay' THEN 'parlay_models:' || p_candidate ELSE 'model_registry:' || p_engine || ':' || p_candidate END,
            a.hash, coalesce(a.evidence -> 'gate', a.evidence -> 'live' -> 'markets'),
            coalesce(a.evidence -> 'live', CASE WHEN a.evidence ? 'test_diff' THEN a.evidence END), v_by, v_by)
    ON CONFLICT DO NOTHING RETURNING request_id INTO id;
    IF id IS NULL THEN
        -- only open requests block a proposal: the same proposal, or another open one for this model line
        SELECT request_id, status INTO o FROM promotion_requests
         WHERE (idempotency_key = key OR (kind = p_kind AND engine = p_engine)) AND status IN ('PROPOSED', 'PENDING_APPROVAL', 'APPROVED')
         ORDER BY request_id DESC LIMIT 1;
        RETURN jsonb_build_object('created', false, 'request_id', o.request_id, 'status', o.status, 'note', 'an identical or open request already exists');
    END IF;
    -- the artifact is attached at creation, so the request goes straight to the approval queue
    UPDATE promotion_requests SET status = 'PENDING_APPROVAL', last_actor = v_by WHERE request_id = id;
    RETURN jsonb_build_object('created', true, 'request_id', id, 'status', 'PENDING_APPROVAL', 'candidate', p_candidate, 'champion', champ, 'artifact_hash', a.hash);
END $$;

CREATE OR REPLACE FUNCTION gov_expire() RETURNS integer
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, public AS $$
DECLARE n integer;
BEGIN
    UPDATE promotion_requests SET status = 'EXPIRED', decision_reason = 'not decided within its approval window', last_actor = 'autopilot'
     WHERE status IN ('PROPOSED', 'PENDING_APPROVAL') AND expires_at < now();
    GET DIAGNOSTICS n = ROW_COUNT;
    RETURN n;
END $$;

-- ---------------------------------------------------------------- deciding (admin token required)
-- Approve: the approver restates the binding; it is re-checked against the live
-- registry; the champion swap, the EXECUTED state and the audit row commit together.
-- Returns ok=false with the problems instead of raising, so an invalidation is kept.
CREATE OR REPLACE FUNCTION gov_approve(p_id bigint, p_token text, p_candidate text, p_action text, p_environment text, p_artifact_hash text,
                                       p_reason text, p_dry_run boolean DEFAULT false) RETURNS jsonb
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, public AS $$
DECLARE who text := gov_authenticate(p_token, 'approver'); r promotion_requests; a record; champ text; problems text[] := '{}'; restated boolean; n integer; msg text;
BEGIN
    IF length(trim(coalesce(p_reason, ''))) < 10 THEN RAISE EXCEPTION 'governance: a reason of at least 10 characters is required' USING ERRCODE = '22023'; END IF;
    SELECT * INTO r FROM promotion_requests WHERE request_id = p_id FOR UPDATE;   -- one approver at a time
    IF NOT FOUND THEN RAISE EXCEPTION 'governance: no such request' USING ERRCODE = 'P0002'; END IF;
    IF r.status <> 'PENDING_APPROVAL' THEN RAISE EXCEPTION 'governance: request % is %', p_id, r.status USING ERRCODE = '55000'; END IF;
    IF p_candidate IS DISTINCT FROM r.candidate_version THEN problems := problems || 'candidate_version differs from the request'::text; END IF;
    IF p_action IS DISTINCT FROM r.action THEN problems := problems || 'action differs from the request'::text; END IF;
    IF p_environment IS DISTINCT FROM r.target_environment THEN problems := problems || 'target_environment differs from the request'::text; END IF;
    IF p_artifact_hash IS DISTINCT FROM r.artifact_hash THEN problems := problems || 'artifact_hash differs from the request'::text; END IF;
    restated := cardinality(problems) > 0;
    SELECT * INTO a FROM gov_artifact(r.kind, r.engine, r.candidate_version);
    champ := gov_champion_of(r.kind, r.engine);
    IF a.hash IS DISTINCT FROM r.artifact_hash THEN problems := problems || 'the validation artifact changed since the request'::text; END IF;
    IF champ IS DISTINCT FROM r.champion_version THEN problems := problems || 'the champion changed since the request'::text; END IF;
    IF NOT coalesce(gov_subject_ok(r.kind, r.action, a.status), false) THEN problems := problems || ('the candidate is now ' || coalesce(a.status, 'missing'))::text; END IF;
    IF cardinality(problems) > 0 THEN
        -- a mismatch in what the approver restated is refused; a change underneath the request invalidates it
        IF NOT restated AND NOT p_dry_run THEN
            UPDATE promotion_requests SET status = 'EXPIRED', decision_reason = 'invalidated: ' || array_to_string(problems, '; '), last_actor = who WHERE request_id = p_id;
        END IF;
        RETURN jsonb_build_object('ok', false, 'problems', to_jsonb(problems), 'invalidated', NOT restated AND NOT p_dry_run);
    END IF;
    IF p_dry_run THEN RETURN jsonb_build_object('ok', true, 'request_id', p_id, 'would_execute', true, 'approver', who); END IF;
    UPDATE promotion_requests SET status = 'APPROVED', approver = who, approved_at = now(), decision_reason = trim(p_reason), last_actor = who WHERE request_id = p_id;
    BEGIN
        IF r.kind = 'parlay' THEN
            UPDATE parlay_models SET status = 'retired', retired_at = now() WHERE status = 'champion' AND version = r.champion_version;
            UPDATE parlay_models SET status = 'champion', promoted_at = now(), retired_at = NULL WHERE version = r.candidate_version;
            GET DIAGNOSTICS n = ROW_COUNT;
            IF n <> 1 THEN RAISE EXCEPTION 'candidate not found at execution'; END IF;
        ELSE
            UPDATE model_registry SET status = CASE WHEN version = r.champion_version THEN 'retired' ELSE 'champion' END,
                   retired_at = CASE WHEN version = r.champion_version THEN now() ELSE NULL END,
                   promoted_at = CASE WHEN version = r.candidate_version THEN now() ELSE promoted_at END
             WHERE sport = r.engine AND version IN (r.champion_version, r.candidate_version);
            GET DIAGNOSTICS n = ROW_COUNT;
            IF n <> 2 THEN RAISE EXCEPTION 'champion or candidate not found at execution'; END IF;
        END IF;
        UPDATE promotion_requests SET status = 'EXECUTED', executed_at = now(), last_actor = who WHERE request_id = p_id;
        INSERT INTO learning_audit (sport, action, version, detail)
        VALUES (r.engine, r.action, r.candidate_version, jsonb_build_object('from', r.champion_version, 'request_id', p_id, 'approver', who, 'reason', trim(p_reason)));
    EXCEPTION WHEN OTHERS THEN
        GET STACKED DIAGNOSTICS msg = MESSAGE_TEXT;
        UPDATE promotion_requests SET status = 'FAILED', failure = msg, last_actor = who WHERE request_id = p_id;
        RETURN jsonb_build_object('ok', false, 'request_id', p_id, 'status', 'FAILED', 'error', msg);
    END;
    RETURN jsonb_build_object('ok', true, 'request_id', p_id, 'status', 'EXECUTED', 'champion', r.candidate_version, 'previous', r.champion_version, 'approver', who);
END $$;

CREATE OR REPLACE FUNCTION gov_close(p_id bigint, p_token text, p_to text, p_reason text) RETURNS jsonb
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, public AS $$
DECLARE who text := gov_authenticate(p_token, 'approver'); n integer;
BEGIN
    IF p_to NOT IN ('REJECTED', 'CANCELLED') THEN RAISE EXCEPTION 'governance: close to REJECTED or CANCELLED' USING ERRCODE = '22023'; END IF;
    IF length(trim(coalesce(p_reason, ''))) < 10 THEN RAISE EXCEPTION 'governance: a reason of at least 10 characters is required' USING ERRCODE = '22023'; END IF;
    UPDATE promotion_requests SET status = p_to, decision_reason = trim(p_reason), last_actor = who
     WHERE request_id = p_id AND (status = 'PENDING_APPROVAL' OR (p_to = 'CANCELLED' AND status = 'PROPOSED'));
    GET DIAGNOSTICS n = ROW_COUNT;
    IF n <> 1 THEN RAISE EXCEPTION 'governance: request % is not open', p_id USING ERRCODE = '55000'; END IF;
    RETURN jsonb_build_object('request_id', p_id, 'status', p_to, 'by', who);
END $$;

-- ---------------------------------------------------------------- publication
CREATE OR REPLACE FUNCTION gov_record_decision(p_engine text, p_market text, p_action text, p_reason text, p_by text, p_how text) RETURNS void
LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog, public AS $$
    INSERT INTO publication_holds (engine, market, action, reason, decided_by, auth_method, evidence_snapshot)
    SELECT p_engine, p_market, p_action, p_reason, p_by, p_how,
           coalesce((SELECT jsonb_agg(to_jsonb(e) - 'detail') FROM pick_evidence e WHERE e.engine = p_engine AND e.market = p_market), '[]'::jsonb)
$$;
REVOKE ALL ON FUNCTION gov_record_decision(text, text, text, text, text, text) FROM PUBLIC;

CREATE OR REPLACE FUNCTION gov_publish(p_token text, p_engine text, p_market text, p_status text, p_reason text) RETURNS jsonb
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, public AS $$
DECLARE who text := gov_authenticate(p_token, 'approver'); act text; cur text;
BEGIN
    act := CASE p_status WHEN 'published' THEN 'publish' WHEN 'leans_only' THEN 'leans_only' WHEN 'suspended' THEN 'suspend' END;
    IF act IS NULL THEN RAISE EXCEPTION 'governance: status must be published, leans_only or suspended' USING ERRCODE = '22023'; END IF;
    IF length(trim(coalesce(p_reason, ''))) < 10 THEN RAISE EXCEPTION 'governance: a reason of at least 10 characters is required' USING ERRCODE = '22023'; END IF;
    SELECT status INTO cur FROM publication_state WHERE engine = p_engine AND market = p_market;
    IF cur = p_status THEN RETURN jsonb_build_object('engine', p_engine, 'market', p_market, 'status', p_status, 'changed', false, 'decided_by', who); END IF;
    PERFORM gov_record_decision(p_engine, p_market, act, trim(p_reason), who, 'admin_token');
    RETURN jsonb_build_object('engine', p_engine, 'market', p_market, 'status', p_status, 'previous', coalesce(cur, 'unpublished'), 'decided_by', who, 'changed', true);
END $$;

-- The safety rule may only suspend a market that is published now (fail-safe:
-- it can hide, never show). The tier check itself runs in the service.
CREATE OR REPLACE FUNCTION gov_safety_suspend(p_engine text, p_market text, p_tier text) RETURNS boolean
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, public AS $$
BEGIN
    IF p_tier IN ('proven', 'baseline') THEN RAISE EXCEPTION 'governance: tier % is eligible; the safety rule does not apply', p_tier USING ERRCODE = '22023'; END IF;
    IF NOT EXISTS (SELECT 1 FROM publication_state WHERE engine = p_engine AND market = p_market AND status = 'published') THEN RETURN false; END IF;
    PERFORM gov_record_decision(p_engine, p_market, 'suspend',
        format('safety rule: evidence tier is now %s, below the publication bar; restoring needs an admin decision', coalesce(p_tier, 'missing')),
        'safety:evidence', 'safety_rule');
    RETURN true;
END $$;

-- ---------------------------------------------------------------- automatic registry steps (documented in governance.md)
-- First champion of an empty model line: the engine defaults (config {}), nothing else.
CREATE OR REPLACE FUNCTION gov_register_initial(p_sport text, p_version text, p_engine text) RETURNS boolean
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, public AS $$
DECLARE n integer;
BEGIN
    IF EXISTS (SELECT 1 FROM model_registry WHERE sport = p_sport AND status = 'champion') THEN RETURN false; END IF;
    IF EXISTS (SELECT 1 FROM model_registry WHERE sport = p_sport AND version = p_version) THEN
        RAISE EXCEPTION 'governance: % already exists; the first champion must be a new engine-default row', p_version USING ERRCODE = '55000';
    END IF;
    INSERT INTO model_registry (sport, version, engine, config, status, notes, promoted_at)
    VALUES (p_sport, p_version, p_engine, '{}', 'champion', 'initial champion: engine defaults', now());
    GET DIAGNOSTICS n = ROW_COUNT;
    INSERT INTO learning_audit (sport, action, version, detail)
    VALUES (p_sport, 'register', p_version, jsonb_build_object('status', 'champion', 'reason', 'registry was empty; engine defaults become the first champion'));
    RETURN n = 1;
END $$;

-- New engine code (a deploy): the champion's configuration carries over, unchanged, under
-- a version naming the new engine. Refuses a target row whose configuration differs.
CREATE OR REPLACE FUNCTION gov_engine_upgrade(p_sport text, p_new_version text, p_engine text) RETURNS text
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, public AS $$
DECLARE c model_registry; t model_registry;
BEGIN
    SELECT * INTO c FROM model_registry WHERE sport = p_sport AND status = 'champion' FOR UPDATE;
    IF NOT FOUND THEN RAISE EXCEPTION 'governance: % has no champion' , p_sport USING ERRCODE = '55000'; END IF;
    IF c.engine = p_engine THEN RETURN c.version; END IF;
    SELECT * INTO t FROM model_registry WHERE sport = p_sport AND version = p_new_version FOR UPDATE;
    IF FOUND AND (t.config IS DISTINCT FROM c.config OR t.engine IS DISTINCT FROM p_engine) THEN
        RAISE EXCEPTION 'governance: % exists with another configuration; an engine upgrade only relabels the champion', p_new_version USING ERRCODE = '42501';
    END IF;
    UPDATE model_registry SET status = 'retired', retired_at = now() WHERE sport = p_sport AND version = c.version;
    IF t.version IS NOT NULL THEN
        UPDATE model_registry SET status = 'champion', promoted_at = now(), retired_at = NULL WHERE sport = p_sport AND version = p_new_version;
    ELSE
        INSERT INTO model_registry (sport, version, engine, config, status, parent, notes, promoted_at)
        VALUES (p_sport, p_new_version, p_engine, c.config, 'champion', c.version, 'engine upgrade: champion configuration on the new engine', now());
    END IF;
    INSERT INTO learning_audit (sport, action, version, detail)
    VALUES (p_sport, 'engine_upgrade', p_new_version, jsonb_build_object('from', c.version, 'from_engine', c.engine, 'engine', p_engine));
    RETURN p_new_version;
END $$;

-- ---------------------------------------------------------------- who may call what
DO $$
DECLARE f text;
BEGIN
    FOREACH f IN ARRAY ARRAY['gov_authenticate(text,text)', 'gov_whoami(text)', 'gov_admins_configured()', 'gov_artifact(text,text,text)',
        'gov_champion_of(text,text)', 'gov_propose(text,text,text,text,text)', 'gov_expire()',
        'gov_approve(bigint,text,text,text,text,text,text,boolean)', 'gov_close(bigint,text,text,text)',
        'gov_publish(text,text,text,text,text)', 'gov_safety_suspend(text,text,text)', 'gov_register_initial(text,text,text)',
        'gov_engine_upgrade(text,text,text)'] LOOP
        EXECUTE format('REVOKE ALL ON FUNCTION %s FROM PUBLIC', f);
        EXECUTE format('GRANT EXECUTE ON FUNCTION %s TO monster_app', f);
    END LOOP;
END $$;
-- research jobs propose nothing and decide nothing
REVOKE ALL ON ALL TABLES IN SCHEMA gov_private FROM monster_research;

COMMIT;
