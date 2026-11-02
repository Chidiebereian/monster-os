"""Governance tests: admin authentication, database privileges, promotion lifecycle, publication.

Two parts:
  live     against the running projection-service and production database, READ-ONLY:
           every write attempted here is one that must be refused, so nothing is
           added to production (no requests, no decisions, no audit rows).
  scratch  a second projection-service (port 8131, autopilot off, connected as
           monster_app like production) on a fresh database with every migration:
           the full lifecycle, invalidation, privilege boundary, token handling and
           publication rules. The scratch database is dropped afterwards.

  python -m pytest scripts/ops/test_governance.py -q
"""
import hashlib
import json
import os
import re
import secrets
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request

import psycopg
import pytest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, ".."))
import dbenv  # noqa: E402

ROOT = dbenv.ROOT
LIVE = os.environ.get("PROJECTION_URL", "http://localhost:8121")
OWNER = dbenv.url("OPS_DATABASE_URL")
APP = dbenv.url("APP_DATABASE_URL")
RESEARCH = dbenv.url("ML_DATABASE_URL")
HELD = {("nhl_team", "team_total_away"), ("nhl_team", "team_total_home"), ("nba_team", "moneyline_1h"),
        ("nhl_team", "moneyline_1h"), ("nhl_team", "team_total_away_1h")}
REMOTE_HEADERS = [{"x-forwarded-for": "203.0.113.7"}, {"x-forwarded-for": "127.0.0.1, 203.0.113.7"}, {"x-real-ip": "203.0.113.7"},
                  {"forwarded": "for=203.0.113.7"}, {"forwarded": "for=unknown"}]


def file_token(name):
    p = os.path.join(ROOT, ".secrets", f"admin_{name}.token")
    if not os.path.exists(p):
        pytest.skip(f"no .secrets/admin_{name}.token (python scripts/ops/admin_token.py --name {name})")
    return open(p, encoding="utf-8").read().strip()


def call(base, method, path, body=None, tok=None, headers=None):
    req = urllib.request.Request(base + path, data=json.dumps(body).encode() if body is not None else None, method=method,
                                 headers={"content-type": "application/json", **(headers or {})})
    if tok:
        req.add_header("authorization", f"Bearer {tok}")
    try:
        with urllib.request.urlopen(req, timeout=120) as r:
            return r.status, json.load(r)
    except urllib.error.HTTPError as e:
        raw = e.read()
        return e.code, (json.loads(raw) if raw else {})


def on_db(url, db):
    return url.rsplit("/", 1)[0] + "/" + db


def champions(url=OWNER):
    with psycopg.connect(url) as cx:
        return dict(cx.execute("SELECT sport, version FROM model_registry WHERE status='champion'").fetchall()) | \
            {"parlay": (cx.execute("SELECT version FROM parlay_models WHERE status='champion'").fetchone() or [None])[0]}


def refused(url, sql, params=None):
    """The statement fails (and anything it did is rolled back); returns the error text."""
    with psycopg.connect(url) as cx:
        try:
            cx.execute(sql, params)
        except psycopg.Error as e:
            cx.rollback()
            return str(e).splitlines()[0]
        cx.rollback()
    pytest.fail(f"not refused: {sql[:90]}")


def counts(url=OWNER):
    with psycopg.connect(url) as cx:
        return cx.execute("SELECT (SELECT count(*) FROM promotion_requests), (SELECT count(*) FROM promotion_request_events), "
                          "(SELECT count(*) FROM publication_holds), (SELECT count(*) FROM learning_audit WHERE action IN ('promote','rollback'))").fetchone()


def lan_ip():
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        s.connect(("192.0.2.1", 9))  # no packet is sent; picks the outward interface
        ip = s.getsockname()[0]
    except OSError:
        return None
    finally:
        s.close()
    return None if ip.startswith("127.") else ip


# ======================================================================== live (read-only)

@pytest.fixture(scope="module")
def production_unchanged():
    before = champions(), counts()
    yield
    assert champions() == before[0], "a champion changed during the tests"
    assert counts() == before[1], "the live tests wrote governance rows to production"


def test_admin_calls_need_a_token(production_unchanged):
    for path, body in [("/governance/promotion-requests/1/approve", {"reason": "x" * 12}), ("/governance/promotion-requests/1/reject", {"reason": "x" * 12}),
                       ("/governance/promotion-requests/1/cancel", {"reason": "x" * 12}),
                       ("/governance/publication", {"engine": "nba", "market": "pts", "status": "published", "reason": "x" * 12})]:
        assert call(LIVE, "POST", path, body)[0] == 401, path
    assert call(LIVE, "GET", "/governance/whoami")[0] == 401


def test_invalid_spoofed_and_auditor_tokens_are_refused(production_unchanged):
    tok = file_token("selftest")  # an auditor in production: authenticates, decides nothing
    assert call(LIVE, "GET", "/governance/whoami", tok="not-a-token")[0] == 403
    st, who = call(LIVE, "GET", "/governance/whoami", tok=tok)
    assert st == 200 and who["principal"] == "selftest" and who["role"] == "auditor"
    for h in REMOTE_HEADERS:
        assert call(LIVE, "GET", "/governance/whoami", tok=tok, headers=h)[0] == 403, h
    # role authorization: an auditor cannot approve or publish (refused before anything is read or written)
    st, d = call(LIVE, "POST", "/governance/promotion-requests/999999999/approve", {"reason": "governance self-test (auditor)"}, tok=tok)
    assert st == 403 and "may not make production decisions" in d["error"]
    st, d = call(LIVE, "POST", "/governance/publication", {"engine": "nba", "market": "pts", "status": "published", "reason": "governance self-test (auditor)",
                                                           "decided_by": "mallory"}, tok=tok)
    assert st == 403
    # no response carries a token or a digest
    for st, d in [call(LIVE, "GET", "/governance/promotion-requests"), call(LIVE, "GET", "/governance/publication")]:
        text = json.dumps(d)
        assert tok not in text and hashlib.sha256(tok.encode()).hexdigest() not in text


def test_direct_promotion_endpoints_are_retired(production_unchanged):
    for path in ("/learning/nba/promote?version=x&by=a&reason=bbbbbb", "/learning/nba/rollback?by=a&reason=bbbbbb",
                 "/parlays/model/promote?version=x&by=a&reason=bbbbbb", "/parlays/model/rollback?by=a&reason=bbbbbb"):
        assert call(LIVE, "POST", path)[0] == 410, path


def test_service_listens_on_loopback_only(production_unchanged):
    ip = lan_ip()
    if not ip:
        pytest.skip("no non-loopback interface")
    with pytest.raises(OSError):
        socket.create_connection((ip, 8121), timeout=3).close()


def test_production_roles_cannot_bypass_governance(production_unchanged):
    with psycopg.connect(APP) as cx:
        su, user = cx.execute("SELECT rolsuper, current_user FROM pg_roles WHERE rolname = current_user").fetchone()
        assert user == "monster_app" and not su
        assert cx.execute("SELECT count(*) FROM pg_tables WHERE tableowner = current_user").fetchone()[0] == 0
    with psycopg.connect(OWNER) as cx:
        champ = cx.execute("SELECT version FROM model_registry WHERE sport='nba' AND status='champion'").fetchone()[0]
        other = cx.execute("SELECT version FROM model_registry WHERE sport='nba' AND status <> 'champion' LIMIT 1").fetchone()
    for sql in [f"UPDATE model_registry SET config = config || '{{\"x\":1}}' WHERE sport='nba' AND version='{champ}'",
                f"UPDATE model_registry SET status='retired' WHERE sport='nba' AND version='{champ}'",
                "INSERT INTO model_registry (sport, version, engine, status) VALUES ('nba','forged','e','champion')",
                f"DELETE FROM model_registry WHERE sport='nba' AND version='{champ}'",
                "UPDATE parlay_models SET dep_weight = 0.5 WHERE status='champion'",
                "UPDATE promotion_requests SET status='APPROVED', approver='owner', approved_at=now()",
                "INSERT INTO promotion_requests (idempotency_key, kind, engine, action, candidate_model_id, candidate_version, champion_version, "
                "validation_report_ref, artifact_hash, proposed_by) VALUES ('k','engine','nba','promote','a','b','c','r','h','x')",
                "INSERT INTO promotion_request_events (request_id, to_status, actor) VALUES (1, 'EXECUTED', 'owner')",
                "INSERT INTO publication_holds (engine, market, action, reason, decided_by) VALUES ('nba','pts','publish','forged decision','owner')",
                "INSERT INTO learning_audit (sport, action, version) VALUES ('nba','promote','forged')",
                "DELETE FROM learning_audit WHERE id = (SELECT min(id) FROM learning_audit)",
                "SELECT * FROM gov_private.admins",
                "SELECT gov_approve(1, 'not-a-token', 'a', 'promote', 'production', 'h', 'reason long enough')",
                "SELECT gov_publish('not-a-token', 'nba', 'pts', 'published', 'reason long enough')",
                "DELETE FROM research_backtest_skips"] + ([f"UPDATE model_registry SET status='champion' WHERE sport='nba' AND version='{other[0]}'"] if other else []):
        refused(APP, sql)
    for sql in ["SELECT gov_propose('engine','nba','rollback','x','research')", "INSERT INTO model_registry (sport, version, engine, status) VALUES ('nba','r','e','candidate')",
                "UPDATE pick_evidence SET n = n", "INSERT INTO publication_holds (engine, market, action, reason, decided_by) VALUES ('nba','pts','publish','research','r')",
                "SELECT * FROM gov_private.admins"]:
        refused(RESEARCH, sql)


def test_production_self_test_records_are_inert(production_unchanged):
    with psycopg.connect(OWNER) as cx:
        rows = cx.execute("SELECT request_id, status, approver, executed_at, decision_reason, last_actor FROM promotion_requests "
                          "WHERE decision_reason ILIKE 'governance self-test%' ORDER BY request_id").fetchall()
        executed = cx.execute("SELECT count(*) FROM promotion_requests WHERE status IN ('APPROVED','EXECUTED')").fetchone()[0]
        open_ = cx.execute("SELECT count(*) FROM promotion_requests WHERE status IN ('PROPOSED','PENDING_APPROVAL','APPROVED')").fetchone()[0]
        audit = cx.execute("SELECT count(*) FROM learning_audit WHERE action IN ('promote','rollback') AND at > '2026-11-01'").fetchone()[0]
    assert len(rows) == 4 and all(r[1] == "REJECTED" and r[2] is None and r[3] is None and r[5] == "selftest" for r in rows)
    assert executed == 0 and open_ == 0 and audit == 0


def test_publication_state_in_production(production_unchanged):
    with psycopg.connect(OWNER) as cx:
        held = {(e, m) for e, m in cx.execute("SELECT engine, market FROM publication_state WHERE status='suspended'")}
        suspended_since = cx.execute("SELECT count(*) FROM publication_holds WHERE auth_method='safety_rule'").fetchone()[0]
        # old rule (hold list only) vs new rule (publication state), same rows, same filters
        lost, gained = cx.execute("""
            WITH old AS (SELECT id FROM ai_picks WHERE status='open' AND starts_at > now() AND tier IN ('proven','baseline') AND NOT EXISTS
                         (SELECT 1 FROM publication_holds h WHERE h.auth_method='migration' AND h.engine=ai_picks.engine AND h.market=ai_picks.market)),
                 new AS (SELECT id FROM ai_picks WHERE status='open' AND starts_at > now() AND tier IN ('proven','baseline') AND EXISTS
                         (SELECT 1 FROM publication_state s WHERE s.engine=ai_picks.engine AND s.market=ai_picks.market AND s.status='published'))
            SELECT (SELECT count(*) FROM (SELECT * FROM old EXCEPT SELECT * FROM new) a), (SELECT count(*) FROM (SELECT * FROM new EXCEPT SELECT * FROM old) b)""").fetchone()
        any_lost, any_gained = cx.execute("""
            WITH old AS (SELECT id FROM ai_picks WHERE NOT EXISTS (SELECT 1 FROM publication_holds h WHERE h.auth_method='migration' AND h.engine=ai_picks.engine AND h.market=ai_picks.market)),
                 new AS (SELECT id FROM ai_picks WHERE EXISTS (SELECT 1 FROM publication_state s WHERE s.engine=ai_picks.engine AND s.market=ai_picks.market
                         AND s.status IN ('published','leans_only')))
            SELECT (SELECT count(*) FROM (SELECT * FROM old EXCEPT SELECT * FROM new) a), (SELECT count(*) FROM (SELECT * FROM new EXCEPT SELECT * FROM old) b)""").fetchone()
        # markets with no decision whose first pick came after the cut-over: unpublished by design, never public
        late = cx.execute("""SELECT count(*), count(*) FILTER (WHERE a.published_at <= (SELECT max(decided_at) FROM publication_holds WHERE auth_method='seed'))
                             FROM ai_picks a WHERE NOT EXISTS (SELECT 1 FROM publication_state s WHERE s.engine=a.engine AND s.market=a.market)""").fetchone()
    assert HELD <= held
    assert late[1] == 0, "a market that had picks at the cut-over has no publication decision"
    # every pick the old rule showed is still shown, except picks of markets that appeared after the cut-over
    # (unpublished until an admin decides) and markets the safety rule suspended since (which only removes)
    assert (lost == 0 or suspended_since > 0) and gained == 0
    assert (any_lost <= late[0] or suspended_since > 0) and any_gained == 0
    st, d = call(LIVE, "GET", "/picks?status=all&limit=1000&include_hidden=true")
    assert st == 200 and not any((p["engine"], p["market"]) in HELD for p in d["picks"])


def test_every_public_reader_uses_the_shared_filter():
    src = {f: open(os.path.join(ROOT, "apps", "projection-service", "src", f), encoding="utf-8").read() for f in ("picks.rs", "game_hub.rs", "governance.rs")}
    # the picks list, pick detail, menus and the Games hub read ai_picks through governance::{public_filter, VISIBLE}
    assert "governance::public_filter(q.include_hidden)" in src["picks.rs"]
    assert src["picks.rs"].count("crate::governance::VISIBLE") >= 2 and src["game_hub.rs"].count("crate::governance::VISIBLE") >= 2
    # MonsterGPT only reads /picks (never the internal include_held view, never the database's ai_picks)
    gpt = "".join(open(os.path.join(ROOT, "apps", "monster-gpt", "src", f), encoding="utf-8").read()
                  for f in os.listdir(os.path.join(ROOT, "apps", "monster-gpt", "src")) if f.endswith(".rs"))
    assert "include_held" not in gpt and not re.search(r"(?i)(from|join|update|into)\s+ai_picks\b", gpt)
    # automation proposes only: no code path outside governance.rs calls a deciding function
    for f in os.listdir(os.path.join(ROOT, "apps", "projection-service", "src")):
        text = open(os.path.join(ROOT, "apps", "projection-service", "src", f), encoding="utf-8").read()
        if f != "governance.rs":
            assert not re.search(r"gov_(approve|close|publish)\b", text), f


# ======================================================================== scratch instance

SCRATCH_DB = "monsteros_gov"
SCRATCH_PORT = 8131
SCRATCH = f"http://localhost:{SCRATCH_PORT}"
S_OWNER = on_db(OWNER, SCRATCH_DB)
S_APP = on_db(APP, SCRATCH_DB)
TOK = {"approver": secrets.token_urlsafe(32), "auditor": secrets.token_urlsafe(32), "expired": secrets.token_urlsafe(32), "revoked": secrets.token_urlsafe(32)}


def psql(db, sql):
    r = subprocess.run(["docker", "exec", "-i", "monster-os-postgres-1", "psql", "-U", "monster", "-d", db, "-v", "ON_ERROR_STOP=1", "-q"], input=sql,
                       text=True, capture_output=True)
    assert r.returncode == 0, r.stderr[:500]


def owner_sql(sql, params=None):
    with psycopg.connect(S_OWNER, autocommit=True) as cx:
        cur = cx.execute(sql, params)
        return cur.fetchall() if cur.description else None


@pytest.fixture(scope="module")
def scratch():
    admin = ["docker", "exec", "monster-os-postgres-1", "psql", "-U", "monster", "-d", "postgres", "-q", "-c"]
    subprocess.run(admin + [f"DROP DATABASE IF EXISTS {SCRATCH_DB}"], capture_output=True)
    subprocess.run(admin + [f"CREATE DATABASE {SCRATCH_DB}"], check=True, capture_output=True)
    mig = os.path.join(ROOT, "infrastructure", "postgres", "migrations")
    for f in sorted(x for x in os.listdir(mig) if x.endswith(".sql")):
        psql(SCRATCH_DB, open(os.path.join(mig, f), encoding="utf-8").read().replace("ON DATABASE monsteros TO", f"ON DATABASE {SCRATCH_DB} TO"))
    # champions, shadows that passed their gate, a parlay champion and candidate; tokens known only to this process
    psql(SCRATCH_DB, """
        INSERT INTO model_registry (sport, version, engine, config, status, promoted_at, evidence) VALUES
          ('nba','nba-poss-sim-v4+default','nba-poss-sim-v4','{}','champion', now(), '{}'),
          ('nba','nba-poss-sim-v4+cfg.aaaa0001','nba-poss-sim-v4','{"x":1}','shadow', NULL, '{"gate":{"pass":true},"live":{"games":40,"markets":{}}}'),
          ('nba','nba-poss-sim-v4+cfg.aaaa0002','nba-poss-sim-v4','{"x":2}','shadow', NULL, '{"gate":{"pass":true},"live":{"games":40,"markets":{}}}'),
          ('nba','nba-poss-sim-v4+cfg.aaaa0003','nba-poss-sim-v4','{"x":3}','shadow', NULL, '{"gate":{"pass":true},"live":{"games":40,"markets":{}}}'),
          ('nhl','nhl-event-sim-v4+default','nhl-event-sim-v4','{}','champion', now(), '{}'),
          ('nhl','nhl-event-sim-v4+cfg.bbbb0001','nhl-event-sim-v4','{"y":1}','shadow', NULL, '{"gate":{"pass":true}}'),
          ('mlb','mlb-pa-sim-v1+default','mlb-pa-sim-v1','{}','champion', now(), '{}'),
          ('mlb','mlb-pa-sim-v1+cfg.cccc0001','mlb-pa-sim-v1','{"z":1}','shadow', NULL, '{"gate":{"pass":true}}');
        INSERT INTO parlay_models (version, dep_weight, status, evidence, promoted_at) VALUES
          ('parlay-joint-v1+dep1.00', 1.0, 'champion', '{}', now()) ON CONFLICT DO NOTHING;
        INSERT INTO parlay_models (version, dep_weight, status, evidence) VALUES
          ('parlay-joint-v1+dep0.60', 0.6, 'candidate', '{"pass":true,"test_diff":-0.01}') ON CONFLICT DO NOTHING;
    """)
    for name, role, life, revoked in [("selftest", "approver", "1 day", False), ("reader", "auditor", "1 day", False),
                                      ("old", "approver", "-1 day", False), ("gone", "approver", "1 day", True)]:
        key = {"selftest": "approver", "reader": "auditor", "old": "expired", "gone": "revoked"}[name]
        owner_sql("INSERT INTO gov_private.admins (principal, token_sha256, role, expires_at, revoked_at) VALUES (%s, %s, %s, now() + %s::interval, %s)",
                  (name, hashlib.sha256(TOK[key].encode()).digest(), role, life, None))
        if revoked:
            owner_sql("UPDATE gov_private.admins SET revoked_at = now() WHERE principal = %s", (name,))
    env = dict(os.environ, PORT=str(SCRATCH_PORT), LEARNING_AUTOPILOT="off", DATABASE_URL=S_APP, BIND_ADDR="0.0.0.0")
    exe = os.path.join(ROOT, "target", "debug", "projection-service.exe")
    proc = subprocess.Popen([exe], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(60):
            try:
                if urllib.request.urlopen(SCRATCH + "/healthz", timeout=2).status == 200:
                    break
            except Exception:
                time.sleep(1)
        else:
            pytest.fail("scratch projection-service did not start")
        yield SCRATCH_DB
    finally:
        proc.terminate()
        proc.wait(timeout=30)
        subprocess.run(admin + [f"DROP DATABASE IF EXISTS {SCRATCH_DB}"], capture_output=True)


def bound(rid):
    q = next(q for q in call(SCRATCH, "GET", "/governance/promotion-requests")[1]["requests"] if q["request_id"] == rid)
    return {"candidate_version": q["candidate_version"], "action": q["action"], "target_environment": "production", "artifact_hash": q["artifact_hash"]}


def status(rid):
    return owner_sql("SELECT status FROM promotion_requests WHERE request_id=%s", (rid,))[0][0]


def propose(engine, cand, action="promote", kind="engine"):
    st, r = call(SCRATCH, "POST", "/governance/promotion-requests", {"kind": kind, "engine": engine, "action": action, "candidate_version": cand})
    assert st == 200, r
    return r


def test_scratch_service_runs_without_owner_rights(scratch):
    with psycopg.connect(S_OWNER) as cx:
        users = {r[0] for r in cx.execute("SELECT usename FROM pg_stat_activity WHERE datname=%s AND usename IS NOT NULL", (SCRATCH_DB,))}
    assert "monster_app" in users


def test_tokens_rotation_expiry_revocation_and_roles(scratch):
    assert call(SCRATCH, "GET", "/governance/whoami", tok=TOK["approver"])[1]["principal"] == "selftest"
    assert call(SCRATCH, "GET", "/governance/whoami", tok=TOK["auditor"])[1]["role"] == "auditor"
    for k in ("expired", "revoked"):
        assert call(SCRATCH, "GET", "/governance/whoami", tok=TOK[k])[0] == 403, k
    assert call(SCRATCH, "GET", "/governance/whoami", tok=TOK["approver"] + "x")[0] == 403
    # rotation: the next call with the old token is refused, no restart needed
    old, new = TOK["approver"], secrets.token_urlsafe(32)
    owner_sql("UPDATE gov_private.admins SET token_sha256=%s WHERE principal='reader'", (hashlib.sha256(new.encode()).digest(),))
    assert call(SCRATCH, "GET", "/governance/whoami", tok=TOK["auditor"])[0] == 403
    assert call(SCRATCH, "GET", "/governance/whoami", tok=new)[1]["principal"] == "reader"
    TOK["auditor"] = new
    assert call(SCRATCH, "GET", "/governance/whoami", tok=old)[0] == 200


def test_remote_peers_and_spoofed_headers_are_refused(scratch):
    for h in REMOTE_HEADERS:
        assert call(SCRATCH, "GET", "/governance/whoami", tok=TOK["approver"], headers=h)[0] == 403, h
        assert call(SCRATCH, "POST", "/governance/promotion-requests", {"kind": "engine", "engine": "mlb", "action": "promote",
                                                                        "candidate_version": "mlb-pa-sim-v1+cfg.cccc0001"}, headers=h)[0] == 403, h
    ip = lan_ip()
    if ip:  # the scratch instance listens on every interface: a real non-loopback socket peer
        st, d = call(f"http://{ip}:{SCRATCH_PORT}", "GET", "/governance/whoami", tok=TOK["approver"], headers={"x-forwarded-for": "127.0.0.1"})
        assert st == 403 and "this machine only" in d["error"]


def test_engine_promotion_executes_once_and_rollback_restores(scratch):
    r = propose("nba", "nba-poss-sim-v4+cfg.aaaa0001")
    assert r["status"] == "PENDING_APPROVAL"
    rid, b = r["request_id"], bound(r["request_id"])
    assert champions(S_OWNER)["nba"] == "nba-poss-sim-v4+default"  # nothing changes before approval
    assert call(SCRATCH, "POST", f"/governance/promotion-requests/{rid}/approve", {**b, "reason": "scratch: auditor tries"}, tok=TOK["auditor"])[0] == 403
    st, d = call(SCRATCH, "POST", f"/governance/promotion-requests/{rid}/approve", {**b, "reason": "scratch: dry run", "dry_run": True, "approver": "mallory"}, tok=TOK["approver"])
    assert st == 200 and d["would_execute"] and d["approver"] == "selftest" and status(rid) == "PENDING_APPROVAL"
    st, d = call(SCRATCH, "POST", f"/governance/promotion-requests/{rid}/approve", {**b, "reason": "scratch: approve and execute"}, tok=TOK["approver"])
    assert st == 200 and d["status"] == "EXECUTED" and d["approver"] == "selftest", d
    assert champions(S_OWNER)["nba"] == "nba-poss-sim-v4+cfg.aaaa0001"
    assert call(SCRATCH, "POST", f"/governance/promotion-requests/{rid}/approve", {**b, "reason": "scratch: replay"}, tok=TOK["approver"])[0] == 409
    ev = owner_sql("SELECT to_status, actor FROM promotion_request_events WHERE request_id=%s ORDER BY id", (rid,))
    audit = owner_sql("SELECT detail->>'approver' FROM learning_audit WHERE action='promote' ORDER BY id DESC LIMIT 1")[0][0]
    assert [e[0] for e in ev] == ["PROPOSED", "PENDING_APPROVAL", "APPROVED", "EXECUTED"] and ev[-1][1] == "selftest" and audit == "selftest"
    # rollback: the previous champion comes back, again only through an approved request
    st, r = call(SCRATCH, "POST", "/governance/promotion-requests", {"kind": "engine", "engine": "nba", "action": "rollback"})
    assert st == 200 and r["candidate"] == "nba-poss-sim-v4+default", r
    st, d = call(SCRATCH, "POST", f"/governance/promotion-requests/{r['request_id']}/approve", {**bound(r["request_id"]), "reason": "scratch: roll back"}, tok=TOK["approver"])
    assert st == 200 and champions(S_OWNER)["nba"] == "nba-poss-sim-v4+default"


def test_an_approval_cannot_be_transferred(scratch):
    rid = propose("nba", "nba-poss-sim-v4+cfg.aaaa0002")["request_id"]
    b = bound(rid)
    for change in [{"candidate_version": "nba-poss-sim-v4+cfg.aaaa0003"}, {"action": "rollback"}, {"target_environment": "staging"},
                   {"artifact_hash": "0" * 64}, {"candidate_version": None}]:
        st, d = call(SCRATCH, "POST", f"/governance/promotion-requests/{rid}/approve", {**b, **change, "reason": "scratch: transfer attempt"}, tok=TOK["approver"])
        assert st == 409 and any("differs" in p for p in d["problems"]) and not d["invalidated"], change
        assert status(rid) == "PENDING_APPROVAL"
    st, d = call(SCRATCH, "POST", f"/governance/promotion-requests/{rid}/cancel", {"reason": "scratch: cancelled by admin"}, tok=TOK["approver"])
    assert st == 200 and d["status"] == "CANCELLED" and d["by"] == "selftest"
    assert call(SCRATCH, "POST", f"/governance/promotion-requests/{rid}/approve", {**b, "reason": "scratch: after cancel"}, tok=TOK["approver"])[0] == 409
    assert champions(S_OWNER)["nba"] == "nba-poss-sim-v4+default"


@pytest.mark.parametrize("change,problem", [
    ("UPDATE model_registry SET evidence = evidence || '{\"note\":\"re-evaluated\"}' WHERE version='nba-poss-sim-v4+cfg.aaaa0002'", "artifact changed"),
    ("UPDATE model_registry SET config = '{\"x\":22}' WHERE version='nba-poss-sim-v4+cfg.aaaa0002'", "artifact changed"),
    ("UPDATE model_registry SET status='rejected' WHERE version='nba-poss-sim-v4+cfg.aaaa0002'", "candidate is now rejected"),
])
def test_a_change_underneath_invalidates_the_request(scratch, change, problem):
    owner_sql("UPDATE model_registry SET status='shadow', config='{\"x\":2}', evidence='{\"gate\":{\"pass\":true},\"live\":{\"games\":40,\"markets\":{}}}' "
              "WHERE version='nba-poss-sim-v4+cfg.aaaa0002'")
    rid = propose("nba", "nba-poss-sim-v4+cfg.aaaa0002")["request_id"]
    b = bound(rid)
    # the change is made with the service's own role: allowed on a non-champion row, but it voids the request
    with psycopg.connect(S_APP, autocommit=True) as cx:
        cx.execute(change)
    st, d = call(SCRATCH, "POST", f"/governance/promotion-requests/{rid}/approve", {**b, "reason": "scratch: stale request"}, tok=TOK["approver"])
    assert st == 409 and d["invalidated"] and any(problem in p for p in d["problems"]), d
    assert status(rid) == "EXPIRED" and champions(S_OWNER)["nba"] == "nba-poss-sim-v4+default"


def test_a_champion_change_invalidates_other_requests_and_duplicates_are_refused(scratch):
    r1 = propose("nhl", "nhl-event-sim-v4+cfg.bbbb0001")
    dup = propose("nhl", "nhl-event-sim-v4+cfg.bbbb0001")
    assert dup["created"] is False and dup["request_id"] == r1["request_id"]
    # rejected: the same proposal may be filed again
    assert call(SCRATCH, "POST", f"/governance/promotion-requests/{r1['request_id']}/reject", {"reason": "scratch: rejected by admin"}, tok=TOK["approver"])[1]["status"] == "REJECTED"
    r2 = propose("nhl", "nhl-event-sim-v4+cfg.bbbb0001")
    assert r2["created"] is True and r2["request_id"] != r1["request_id"]
    b = bound(r2["request_id"])
    # the champion changes underneath (a break-glass owner edit): the open request no longer applies
    owner_sql("INSERT INTO model_registry (sport, version, engine, config, status, evidence) VALUES ('nhl','nhl-event-sim-v4+cfg.bbbb0009','nhl-event-sim-v4','{\"y\":9}','retired','{}')")
    owner_sql("UPDATE model_registry SET status = CASE WHEN version='nhl-event-sim-v4+default' THEN 'retired' ELSE 'champion' END "
              "WHERE sport='nhl' AND version IN ('nhl-event-sim-v4+default','nhl-event-sim-v4+cfg.bbbb0009')")
    st, d = call(SCRATCH, "POST", f"/governance/promotion-requests/{r2['request_id']}/approve", {**b, "reason": "scratch: champion moved"}, tok=TOK["approver"])
    assert st == 409 and d["invalidated"] and any("champion changed" in p for p in d["problems"]), d


def test_undecided_requests_expire(scratch):
    owner_sql("""INSERT INTO promotion_requests (idempotency_key, kind, engine, action, candidate_model_id, candidate_version, champion_version,
                 validation_report_ref, artifact_hash, proposed_by, expires_at) VALUES ('expiry-test','engine','mlb','promote','mlb:x','mlb-pa-sim-v1+cfg.cccc0001',
                 'mlb-pa-sim-v1+default','r','h','scratch', now() - interval '1 minute')""")
    with psycopg.connect(S_APP, autocommit=True) as cx:
        assert cx.execute("SELECT gov_expire()").fetchone()[0] == 1
    assert owner_sql("SELECT status, last_actor FROM promotion_requests WHERE idempotency_key='expiry-test'")[0] == ("EXPIRED", "autopilot")
    # the owner's own direct insert must also start undecided
    with pytest.raises(psycopg.Error):
        owner_sql("""INSERT INTO promotion_requests (idempotency_key, kind, engine, action, candidate_model_id, candidate_version, champion_version,
                     validation_report_ref, artifact_hash, proposed_by, status, approver, approved_at) VALUES ('forged','engine','mlb','promote','a','b','c','r','h','x',
                     'APPROVED','owner', now())""")


def test_parlay_promotion_needs_approval(scratch):
    r = propose("parlay", "parlay-joint-v1+dep0.60", kind="parlay")
    assert champions(S_OWNER)["parlay"] == "parlay-joint-v1+dep1.00"
    assert call(SCRATCH, "POST", f"/governance/promotion-requests/{r['request_id']}/approve", {**bound(r["request_id"]), "reason": "scratch: parlay"})[0] == 401
    st, d = call(SCRATCH, "POST", f"/governance/promotion-requests/{r['request_id']}/approve", {**bound(r["request_id"]), "reason": "scratch: parlay"}, tok=TOK["approver"])
    assert st == 200 and champions(S_OWNER)["parlay"] == "parlay-joint-v1+dep0.60"


def test_requests_cannot_be_rewritten_or_deleted_even_by_the_owner(scratch):
    rid = owner_sql("SELECT max(request_id) FROM promotion_requests")[0][0]
    for sql in [f"UPDATE promotion_requests SET candidate_version='x' WHERE request_id={rid}",
                f"UPDATE promotion_requests SET status='PENDING_APPROVAL' WHERE request_id={rid}",
                f"DELETE FROM promotion_requests WHERE request_id={rid}", "TRUNCATE promotion_requests CASCADE",
                "UPDATE promotion_request_events SET actor='x'", "DELETE FROM promotion_request_events", "DELETE FROM learning_audit"]:
        refused(S_OWNER, sql)


def test_the_service_role_cannot_bypass_the_workflow(scratch):
    champ = champions(S_OWNER)["nba"]
    for sql in ["UPDATE model_registry SET status='champion' WHERE version='nba-poss-sim-v4+cfg.aaaa0003'",
                f"UPDATE model_registry SET config='{{\"x\":9}}' WHERE version='{champ}'",
                "INSERT INTO model_registry (sport, version, engine, status) VALUES ('nfl','forged','e','champion')",
                "UPDATE parlay_models SET status='retired' WHERE status='champion'",
                "UPDATE promotion_requests SET status='APPROVED', approver='selftest', approved_at=now()",
                "INSERT INTO promotion_request_events (request_id, to_status, actor) VALUES (1,'EXECUTED','selftest')",
                "INSERT INTO publication_holds (engine, market, action, reason, decided_by) VALUES ('nba','pts','publish','forged','selftest')",
                "INSERT INTO learning_audit (sport, action, version) VALUES ('nba','rollback','forged')",
                "SELECT * FROM gov_private.admins",
                "SELECT gov_approve(1, 'not-a-token', 'a', 'promote', 'production', 'h', 'reason long enough')",
                "SELECT gov_close(1, NULL, 'REJECTED', 'reason long enough')",
                "SELECT gov_safety_suspend('nba', 'pts', 'baseline')",
                # the automatic registry steps cannot be bent into a promotion
                "SELECT gov_engine_upgrade('nba', 'nba-poss-sim-v4+cfg.aaaa0003', 'some-new-engine')"]:
        refused(S_APP, sql)
    with psycopg.connect(S_APP, autocommit=True) as cx:
        assert cx.execute("SELECT gov_register_initial('nba', 'nba-poss-sim-v9+default', 'nba-poss-sim-v9')").fetchone()[0] is False  # a champion exists
        # what stays allowed: evidence on a champion, a new candidate
        cx.execute(f"UPDATE model_registry SET evidence = coalesce(evidence,'{{}}') || '{{\"seen\":1}}' WHERE version='{champ}'")
        cx.execute("INSERT INTO model_registry (sport, version, engine, config, status) VALUES ('nba','nba-poss-sim-v4+cfg.dddd0001','nba-poss-sim-v4','{\"x\":4}','candidate')")
    assert champions(S_OWNER)["nba"] == champ
    # the engine-upgrade relabel keeps the configuration and is audited
    with psycopg.connect(S_APP, autocommit=True) as cx:
        cfg = cx.execute("SELECT config FROM model_registry WHERE sport='mlb' AND status='champion'").fetchone()[0]
        assert cx.execute("SELECT gov_engine_upgrade('mlb', 'mlb-pa-sim-v2+default', 'mlb-pa-sim-v2')").fetchone()[0] == "mlb-pa-sim-v2+default"
        assert cx.execute("SELECT config, engine FROM model_registry WHERE sport='mlb' AND status='champion'").fetchone() == (cfg, "mlb-pa-sim-v2")
    assert owner_sql("SELECT count(*) FROM learning_audit WHERE action='engine_upgrade' AND sport='mlb'")[0][0] == 1


def seed_pick(market, tier="baseline", game="g1"):
    psql(SCRATCH_DB, f"""
        WITH l AS (INSERT INTO prediction_ledger (sport, game_id, game_date, model_version, engine, lineup_mode, seed, n_sims, data_cutoff, payload_sha256, payload)
                   VALUES ('nba','{game}', current_date, 'nba-poss-sim-v4+default','nba-poss-sim-v4','projected',1,1, current_date,'x','{{}}') RETURNING id)
        INSERT INTO ai_picks (sport, engine, model_version, ledger_id, game_id, game_label, kind, market, subject, subject_name, side, book, price,
                              p_model, p_market, edge, ev, grade, tier, facts, writeup, writeup_source, starts_at)
        SELECT 'nba','nba','nba-poss-sim-v4+default', l.id, '{game}','A @ B','prop','{market}','p1','Player One','over','draftkings',-110,
               0.6,0.5,0.1,0.1,'B+','{tier}','{{}}','w','template', now() + interval '1 day' FROM l;
    """)
    return owner_sql("SELECT max(id) FROM ai_picks WHERE market=%s", (market,))[0][0]


def visible(q=""):
    return [p["market"] for p in call(SCRATCH, "GET", f"/picks?status=open&limit=100{q}")[1]["picks"]]


def menus():
    return [m["id"] for m in call(SCRATCH, "GET", "/picks/meta?sport=nba")[1].get("markets", [])]


def test_new_markets_stay_private_until_an_admin_publishes(scratch):
    pid = seed_pick("brand_new_market")
    # an evidence refresh that makes the market eligible (tier proven) still does not publish it
    with psycopg.connect(S_APP, autocommit=True) as cx:
        cx.execute("UPDATE ai_picks SET tier='proven' WHERE id=%s", (pid,))
    for check in (visible(), visible("&include_hidden=true"), menus()):
        assert "brand_new_market" not in check
    assert call(SCRATCH, "GET", f"/picks/{pid}")[0] == 404
    assert "brand_new_market" in visible("&include_held=true")  # internal view still has it
    assert owner_sql("SELECT count(*) FROM publication_holds WHERE market='brand_new_market'")[0][0] == 0
    assert call(SCRATCH, "POST", "/governance/publication", {"engine": "nba", "market": "brand_new_market", "status": "published", "reason": "scratch: publish"})[0] == 401
    assert call(SCRATCH, "POST", "/governance/publication", {"engine": "nba", "market": "brand_new_market", "status": "published", "reason": "scratch: publish"},
                tok=TOK["auditor"])[0] == 403
    st, d = call(SCRATCH, "POST", "/governance/publication", {"engine": "nba", "market": "brand_new_market", "status": "published", "reason": "scratch: publish after review",
                                                              "decided_by": "mallory"}, tok=TOK["approver"])
    assert st == 200 and d["decided_by"] == "selftest" and d["previous"] == "unpublished"
    assert "brand_new_market" in visible() and "brand_new_market" in menus() and call(SCRATCH, "GET", f"/picks/{pid}")[0] == 200
    # a routine tier change moves eligibility, never the publication state
    n = owner_sql("SELECT count(*) FROM publication_holds")[0][0]
    with psycopg.connect(S_APP, autocommit=True) as cx:
        cx.execute("UPDATE ai_picks SET tier='unproven' WHERE id=%s", (pid,))
        assert "brand_new_market" not in visible() and "brand_new_market" in visible("&include_hidden=true")
        cx.execute("UPDATE ai_picks SET tier='baseline' WHERE id=%s", (pid,))
    assert "brand_new_market" in visible() and owner_sql("SELECT count(*) FROM publication_holds")[0][0] == n


def test_the_safety_rule_suspends_but_never_publishes_or_restores(scratch):
    seed_pick("safety_market", game="g2")
    call(SCRATCH, "POST", "/governance/publication", {"engine": "nba", "market": "safety_market", "status": "published", "reason": "scratch: publish"}, tok=TOK["approver"])
    with psycopg.connect(S_APP, autocommit=True) as cx:
        # no suspension while the tier is eligible
        with pytest.raises(psycopg.Error):
            cx.execute("SELECT gov_safety_suspend('nba', 'safety_market', 'proven')")
        assert "safety_market" in visible()
        # the tier falls below the bar: suspended
        assert cx.execute("SELECT gov_safety_suspend('nba', 'safety_market', 'losing')").fetchone()[0] is True
        assert "safety_market" not in visible("&include_hidden=true")
        # it cannot act on a market that is not published: nothing is written, nothing is restored
        assert cx.execute("SELECT gov_safety_suspend('nba', 'safety_market', 'losing')").fetchone()[0] is False
        assert cx.execute("SELECT gov_safety_suspend('nba', 'never_decided', 'losing')").fetchone()[0] is False
    log = owner_sql("SELECT action, decided_by, auth_method FROM publication_holds WHERE market IN ('safety_market','never_decided') ORDER BY id")
    assert log == [("publish", "selftest", "admin_token"), ("suspend", "safety:evidence", "safety_rule")]
    # restoring needs an admin decision
    assert call(SCRATCH, "POST", "/governance/publication", {"engine": "nba", "market": "safety_market", "status": "published", "reason": "scratch: restore"})[0] == 401
    st, d = call(SCRATCH, "POST", "/governance/publication", {"engine": "nba", "market": "safety_market", "status": "published", "reason": "scratch: restore after review"},
                 tok=TOK["approver"])
    assert st == 200 and d["previous"] == "suspended" and "safety_market" in visible()
