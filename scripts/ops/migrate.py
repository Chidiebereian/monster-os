"""Apply pending SQL migrations to the live database and record them.

  python scripts/ops/migrate.py                    # status: applied, pending, changed since applied
  python scripts/ops/migrate.py --apply            # apply pending migrations in order, stop at the first error
  python scripts/ops/migrate.py --baseline 0043    # one-off: record 0001..0043 as already applied (no SQL run)

A fresh volume runs every file through docker-entrypoint-initdb.d; an existing database
needs this script. Each file runs in its own psql session as the owner role inside the
Postgres container (ON_ERROR_STOP), and is recorded in schema_migrations with its
sha256. A recorded file whose content changed afterwards is reported, never re-run.
"""
import argparse
import hashlib
import os
import subprocess
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
import dbenv  # noqa: E402

MIG = os.path.join(dbenv.ROOT, "infrastructure", "postgres", "migrations")
DB = os.environ.get("MIGRATE_DB", "monsteros")
PSQL = ["docker", "exec", "-i", "monster-os-postgres-1", "psql", "-U", "monster", "-d", DB, "-v", "ON_ERROR_STOP=1", "-q", "-t", "-A"]
TABLE = """CREATE TABLE IF NOT EXISTS schema_migrations (
    file text PRIMARY KEY, sha256 text NOT NULL, applied_at timestamptz NOT NULL DEFAULT now(), how text NOT NULL)"""


def psql(sql):
    r = subprocess.run(PSQL, input=sql, text=True, capture_output=True, encoding="utf-8")
    if r.returncode:
        raise RuntimeError(r.stderr.strip()[:600])
    return r.stdout


def files():
    return sorted(f for f in os.listdir(MIG) if f.endswith(".sql"))


def sha(f):
    return hashlib.sha256(open(os.path.join(MIG, f), "rb").read()).hexdigest()


def recorded():
    psql(TABLE + ";")
    out = psql("SELECT file || '|' || sha256 FROM schema_migrations;")
    return dict(line.split("|", 1) for line in out.splitlines() if "|" in line)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--apply", action="store_true")
    ap.add_argument("--baseline", metavar="NNNN")
    a = ap.parse_args()
    done = recorded()
    if a.baseline:
        if done:
            raise SystemExit("schema_migrations already has rows; --baseline is only for the first run")
        base = [f for f in files() if f[:4] <= a.baseline]
        psql("".join(f"INSERT INTO schema_migrations (file, sha256, how) VALUES ('{f}', '{sha(f)}', 'baseline');\n" for f in base))
        print(f"recorded {len(base)} migrations up to {a.baseline} as already applied")
        return 0
    pending = [f for f in files() if f not in done]
    changed = [f for f in files() if f in done and done[f] != sha(f)]
    print(f"{len(done)} applied, {len(pending)} pending{': ' + ', '.join(pending) if pending else ''}")
    if changed:
        print(f"changed after being applied (not re-run): {', '.join(changed)}")
    if not done and files():
        print("nothing recorded yet: on an existing database run --baseline <last applied> first")
        return 1
    if not a.apply:
        return 0
    for f in pending:
        try:
            psql(open(os.path.join(MIG, f), encoding="utf-8").read())
        except RuntimeError as e:
            print(f"FAILED {f}: {e}")
            return 1
        psql(f"INSERT INTO schema_migrations (file, sha256, how) VALUES ('{f}', '{sha(f)}', 'applied');")
        print(f"applied {f}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
