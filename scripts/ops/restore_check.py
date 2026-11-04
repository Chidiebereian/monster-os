"""Prove a backup restores (ops backlog B4). Never touches the live database.

  python scripts/ops/restore_check.py              # latest backup, irreplaceable tables (about 0.6 GB)
  python scripts/ops/restore_check.py --full       # every table (needs ~1.5x the live database free)
  python scripts/ops/restore_check.py --backup <dir>

Restores into a scratch database (monsteros_restore) inside the Postgres container:
  1. checks the dump's sha256 against its manifest
  2. restores the whole schema (tables, triggers, functions, grants)
  3. restores the data of the irreplaceable tables (--full: all of it)
  4. compares: every table's row count is at least the manifest's (rows written while the
     dump ran may add a few), the forecast ledger's fingerprint over the manifest's rows
     matches, the write-once triggers are present, the applied migrations are identical
  5. drops the scratch database and writes a report with the time each step took

Why not always --full: the container's disk is a file on C:, which grows and does not
shrink; with C: nearly full a 4+ GB restore could fill it. --full refuses unless free
space allows.
"""
import argparse
import datetime as dt
import hashlib
import json
import os
import shutil
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import backup  # noqa: E402  (IRREPLACEABLE, ROOT_DIR, sha256, dbenv)

CONTAINER, SCRATCH = "monster-os-postgres-1", "monsteros_restore"
ADMIN = ["docker", "exec", CONTAINER, "psql", "-U", "monster", "-d", "postgres", "-q", "-c"]
WRITE_ONCE = ["ml_examples_immutable", "feature_snapshots_immutable", "publication_holds_immutable", "gpt_claims_immutable", "gpt_sim_runs_final"]
REPORTS = os.path.join(os.environ.get("LOCALAPPDATA", os.path.expanduser("~")), "MonsterOS", "reports")


def restore(dump, args):
    with open(dump, "rb") as f:
        r = subprocess.run(["docker", "exec", "-i", CONTAINER, "pg_restore", "-U", "monster", "-d", SCRATCH, "--no-owner", "--exit-on-error"] + args,
                           stdin=f, capture_output=True)
    if r.returncode:
        raise RuntimeError(r.stderr.decode(errors="replace")[:600])


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--full", action="store_true")
    ap.add_argument("--backup")
    a = ap.parse_args()
    src = a.backup or open(os.path.join(backup.ROOT_DIR, "LATEST")).read().strip()
    man = json.load(open(os.path.join(src, "manifest.json")))
    dump = os.path.join(src, "monsteros.dump")
    report = {"backup": src, "mode": "full" if a.full else "irreplaceable", "at": dt.datetime.now().isoformat(timespec="seconds"), "steps": {}, "checks": {}}
    t = time.time()
    report["checks"]["dump_sha256"] = backup.sha256(dump) == man["files"]["monsteros.dump"]["sha256"]
    report["steps"]["verify_sha256_s"] = round(time.time() - t, 1)
    if not report["checks"]["dump_sha256"]:
        print("FAILED: the dump does not match its manifest sha256")
        return 1
    if a.full:
        import psycopg
        with psycopg.connect(backup.dbenv.url("OPS_DATABASE_URL")) as cx:
            live = cx.execute("SELECT pg_database_size('monsteros')").fetchone()[0]
        free = shutil.disk_usage(os.path.expandvars("%LOCALAPPDATA%")).free
        if free < 1.5 * live + 5 * backup.GB:
            print(f"REFUSED --full: {free / backup.GB:.1f} GB free on C:, {(1.5 * live + 5 * backup.GB) / backup.GB:.1f} GB needed. Run the default check, or free space first.")
            return 2
    subprocess.run(ADMIN + [f"DROP DATABASE IF EXISTS {SCRATCH} WITH (FORCE)"], capture_output=True)
    subprocess.run(ADMIN + [f"CREATE DATABASE {SCRATCH}"], check=True, capture_output=True)
    ok = False
    try:
        t = time.time()
        restore(dump, ["--schema-only"])
        report["steps"]["schema_s"] = round(time.time() - t, 1)
        t = time.time()
        tables = [] if a.full else [x for t_ in man["rows"] for x in ("-t", t_)]
        restore(dump, ["--data-only", "--disable-triggers"] + tables)
        report["steps"]["data_s"] = round(time.time() - t, 1)
        import psycopg
        with psycopg.connect(backup.dbenv.url("OPS_DATABASE_URL").rsplit("/", 1)[0] + "/" + SCRATCH) as cx:
            rows, short = {}, []
            for tbl, n in man["rows"].items():
                got = cx.execute(f"SELECT count(*) FROM {tbl}").fetchone()[0]
                rows[tbl] = got
                if got < n:
                    short.append(f"{tbl} {got} < {n}")
            report["checks"]["rows"] = not short
            report["checks"]["rows_short"] = short
            n = man["rows"].get("prediction_ledger", 0)
            fp = cx.execute("SELECT md5(coalesce(string_agg(id::text || ':' || payload_sha256, ',' ORDER BY id), '')) FROM "
                            "(SELECT id, payload_sha256 FROM prediction_ledger ORDER BY id LIMIT %s) x", (n,)).fetchone()[0]
            report["checks"]["ledger_fingerprint"] = fp == man["ledger_fingerprint"]
            trig = {r[0] for r in cx.execute("SELECT tgname FROM pg_trigger WHERE NOT tgisinternal")}
            report["checks"]["write_once_triggers"] = [x for x in WRITE_ONCE if x not in trig] == []
            report["checks"]["migrations"] = [r[0] for r in cx.execute("SELECT file FROM schema_migrations ORDER BY file")] == man["migrations"]
            report["restored_rows"] = rows
        ok = all(v for k, v in report["checks"].items() if k != "rows_short")
    except Exception as e:
        report["error"] = str(e)
    finally:
        subprocess.run(ADMIN + [f"DROP DATABASE IF EXISTS {SCRATCH} WITH (FORCE)"], capture_output=True)
    report["ok"] = ok
    report["restore_seconds"] = round(report["steps"].get("schema_s", 0) + report["steps"].get("data_s", 0), 1)
    os.makedirs(REPORTS, exist_ok=True)
    path = os.path.join(REPORTS, f"restore_check_{dt.datetime.now():%Y%m%d_%H%M%S}.json")
    json.dump(report, open(path, "w"), indent=1)
    print(f"{'PASS' if ok else 'FAIL'} restore check ({report['mode']}) of {os.path.basename(src)}: {report['checks']}; "
          f"restore took {report['restore_seconds']} s{'; error: ' + report['error'] if report.get('error') else ''}\nreport: {path}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
