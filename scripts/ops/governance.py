"""Production governance from the command line (projection-service /governance).

The only way to change a production model or what AI Picks publishes. Admin
commands send the token in .secrets/admin_<name>.token (or MONSTER_ADMIN_TOKEN);
the service derives the approver from it. Approving restates the request's
candidate, action, environment and artifact hash, so it can only approve exactly
what was shown.

  python scripts/ops/governance.py requests
  python scripts/ops/governance.py request --kind engine --engine nba --action promote --candidate <version>
  python scripts/ops/governance.py approve 12 --as owner --reason "live gate passed; reviewed calibration" [--dry-run]
  python scripts/ops/governance.py reject 12 --as owner --reason "..."
  python scripts/ops/governance.py cancel 12 --as owner --reason "..."
  python scripts/ops/governance.py publication
  python scripts/ops/governance.py whoami --as owner
  python scripts/ops/governance.py publish nhl_team team_total_home --status published --as owner --reason "..."
"""
import argparse
import json
import os
import sys
import urllib.error
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, ".."))
import dbenv  # noqa: E402

BASE = os.environ.get("PROJECTION_URL", "http://localhost:8121")


def token(name):
    t = os.environ.get("MONSTER_ADMIN_TOKEN")
    if t:
        return t
    p = os.path.join(dbenv.ROOT, ".secrets", f"admin_{name}.token")
    if not os.path.exists(p):
        sys.exit(f"no admin token for {name!r}: run python scripts/ops/admin_token.py --name {name}")
    return open(p, encoding="utf-8").read().strip()


def call(method, path, body=None, admin=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(BASE + path, data=data, method=method, headers={"content-type": "application/json"})
    if admin:
        req.add_header("authorization", f"Bearer {token(admin)}")
    try:
        with urllib.request.urlopen(req, timeout=120) as r:
            return r.status, json.load(r)
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"{}")


def show(st, d):
    print(st, json.dumps(d, indent=1, default=str)[:4000])
    return 0 if st < 400 else 1


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    sub.add_parser("requests")
    r = sub.add_parser("request"); r.add_argument("--kind", required=True); r.add_argument("--engine", required=True)
    r.add_argument("--action", required=True); r.add_argument("--candidate")
    for c in ("approve", "reject", "cancel"):
        p = sub.add_parser(c); p.add_argument("id", type=int); p.add_argument("--as", dest="who", required=True); p.add_argument("--reason", required=True)
        if c == "approve":
            p.add_argument("--dry-run", action="store_true")
    sub.add_parser("publication")
    w = sub.add_parser("whoami"); w.add_argument("--as", dest="who", required=True)
    p = sub.add_parser("publish"); p.add_argument("engine"); p.add_argument("market")
    p.add_argument("--status", required=True, choices=["published", "leans_only", "suspended"]); p.add_argument("--as", dest="who", required=True); p.add_argument("--reason", required=True)
    a = ap.parse_args()

    if a.cmd == "requests":
        st, d = call("GET", "/governance/promotion-requests")
        for q in d.get("requests", []):
            print(f"#{q['request_id']} {q['status']:16} {q['kind']}/{q['engine']} {q['action']} {q['candidate_version']} (champion {q['champion_version']}) "
                  f"proposed by {q['proposed_by']}" + (f", decided by {q['approver'] or q.get('last_actor')}: {q['decision_reason']}" if q.get("decision_reason") else ""))
        return 0
    if a.cmd == "request":
        return show(*call("POST", "/governance/promotion-requests", {"kind": a.kind, "engine": a.engine, "action": a.action, "candidate_version": a.candidate}))
    if a.cmd in ("approve", "reject", "cancel"):
        st, d = call("GET", "/governance/promotion-requests")
        req = next((q for q in d.get("requests", []) if q["request_id"] == a.id), None)
        if not req:
            sys.exit(f"no request {a.id}")
        print(f"request #{a.id}: {req['kind']}/{req['engine']} {req['action']} {req['candidate_version']} replacing {req['champion_version']}, "
              f"artifact {req['artifact_hash'][:12]}..., status {req['status']}")
        body = {"reason": a.reason, "candidate_version": req["candidate_version"], "action": req["action"],
                "target_environment": req["target_environment"], "artifact_hash": req["artifact_hash"], "dry_run": getattr(a, "dry_run", False)}
        return show(*call("POST", f"/governance/promotion-requests/{a.id}/{a.cmd}", body, admin=a.who))
    if a.cmd == "publication":
        return show(*call("GET", "/governance/publication"))
    if a.cmd == "whoami":
        return show(*call("GET", "/governance/whoami", admin=a.who))
    if a.cmd == "publish":
        return show(*call("POST", "/governance/publication", {"engine": a.engine, "market": a.market, "status": a.status, "reason": a.reason}, admin=a.who))


if __name__ == "__main__":
    sys.exit(main())
