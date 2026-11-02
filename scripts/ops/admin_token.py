"""Create, rotate or revoke an admin principal for production governance (migration 0039).

Generates a random token, writes it to .secrets/admin_<name>.token (git-ignored,
readable by the current Windows user only) and stores only its SHA-256 in
gov_private.admins through the owner connection (OPS_DATABASE_URL). The database
checks tokens itself, so a rotation or revocation takes effect on the next call:
the previous token stops working at once. Nothing secret is printed.

Roles: approver (approve / reject / cancel promotion requests, publication
decisions) and auditor (can authenticate, e.g. /governance/whoami; decides nothing).

  python scripts/ops/admin_token.py --name owner                      # approver, 180 days
  python scripts/ops/admin_token.py --name selftest --role auditor
  python scripts/ops/admin_token.py --name owner --days 30            # rotate with a shorter life
  python scripts/ops/admin_token.py --name owner --revoke
  python scripts/ops/admin_token.py --list
"""
import argparse
import getpass
import hashlib
import os
import re
import secrets
import subprocess
import sys

import psycopg

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, ".."))
import dbenv  # noqa: E402

SECRETS = os.path.join(dbenv.ROOT, ".secrets")


def restrict(path, folder=False):
    """Owner-only access: chmod where it means something, an explicit ACL (no inherited entries) on Windows."""
    try:
        os.chmod(path, 0o700 if folder else 0o600)
    except OSError:
        pass
    if os.name == "nt":
        user = os.environ.get("USERNAME") or getpass.getuser()
        inherit = "(OI)(CI)" if folder else ""
        subprocess.run(["icacls", path, "/inheritance:r", "/grant:r", f"{user}:{inherit}F", "/grant:r", f"SYSTEM:{inherit}F"], capture_output=True, check=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--name")
    ap.add_argument("--role", default="approver", choices=["approver", "auditor"])
    ap.add_argument("--days", type=int, default=180)
    ap.add_argument("--revoke", action="store_true")
    ap.add_argument("--list", action="store_true")
    ap.add_argument("--db-url-key", default="OPS_DATABASE_URL", help="variable holding the owner URL of the target database")
    a = ap.parse_args()
    owner = dbenv.url(a.db_url_key)
    with psycopg.connect(owner, autocommit=True) as cx:
        if a.list:
            for p, r, c, e, rv in cx.execute("SELECT principal, role, created_at, expires_at, revoked_at FROM gov_private.admins ORDER BY principal"):
                print(f"{p:12} {r:9} created {c:%Y-%m-%d}  expires {e:%Y-%m-%d}  {'REVOKED ' + format(rv, '%Y-%m-%d') if rv else 'active'}")
            return
        if not a.name or not re.fullmatch(r"[A-Za-z0-9_.@-]{2,40}", a.name):
            sys.exit("--name: 2-40 letters, digits, _ . @ -")
        path = os.path.join(SECRETS, f"admin_{a.name}.token")
        if a.revoke:
            n = cx.execute("UPDATE gov_private.admins SET revoked_at = now() WHERE principal = %s AND revoked_at IS NULL", (a.name,)).rowcount
            if os.path.exists(path):
                os.remove(path)
            print(f"revoked {a.name}" if n else f"{a.name} had no active token")
            return
        token = secrets.token_urlsafe(32)
        digest = hashlib.sha256(token.encode()).digest()
        # rotation replaces the digest: the old token is refused from the next call on
        cx.execute("""INSERT INTO gov_private.admins (principal, token_sha256, role, expires_at) VALUES (%s, %s, %s, now() + make_interval(days => %s))
                      ON CONFLICT (principal) DO UPDATE SET token_sha256 = EXCLUDED.token_sha256, role = EXCLUDED.role, created_at = now(),
                      expires_at = EXCLUDED.expires_at, revoked_at = NULL""", (a.name, digest, a.role, a.days))
    os.makedirs(SECRETS, exist_ok=True)
    restrict(SECRETS, folder=True)
    with open(path, "w", encoding="utf-8") as f:
        f.write(token)
    restrict(path)
    print(f"admin {a.name} ({a.role}, {a.days} days): token written to .secrets/admin_{a.name}.token (not shown); "
          f"only its SHA-256 is stored; active immediately, any previous token for {a.name} no longer works")


if __name__ == "__main__":
    main()
