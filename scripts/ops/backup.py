"""Back up the Monster OS Postgres database (ops backlog B3).

  python scripts/ops/backup.py                 # dump, manifest, keep the last 3
  python scripts/ops/backup.py --keep 5
  python scripts/ops/backup.py --offsite gdrive:monster-os-backups   # also copy off this machine (rclone)

Writes %LOCALAPPDATA%/MonsterOS/backups/pg_<time>/:
  monsteros.dump   pg_dump custom format, compressed (-Fc -Z 6), streamed out of the container
  globals.sql      roles and grants (pg_dumpall --globals-only --no-role-passwords: no password
                   hashes; after a restore, scripts/ops/rotate_research_password.py sets them again)
  manifest.json    sha256 and size of each file, row counts and a fingerprint of the tables
                   that cannot be rebuilt from their sources, applied migrations, git commit

Refuses to start when free disk on the target is below 3x the last dump plus 2 GB (C: is
nearly full; a filled disk wedges Docker). Retention deletes only directories this script
made (they carry a manifest), oldest first. Restoring is checked by restore_check.py.
"""
import argparse
import datetime as dt
import hashlib
import json
import os
import shutil
import subprocess
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
import dbenv  # noqa: E402

CONTAINER = "monster-os-postgres-1"
DB = "monsteros"
ROOT_DIR = os.environ.get("MONSTER_BACKUP_DIR") or os.path.join(os.environ.get("LOCALAPPDATA", os.path.expanduser("~")), "MonsterOS", "backups")
# append-only or observed-at-the-time data: lost for good without a backup
IRREPLACEABLE = ["prediction_ledger", "prediction_forecasts", "model_registry", "learning_audit", "promotion_requests", "promotion_request_events",
                 "publication_holds", "market_lines", "feature_snapshots", "ai_picks", "pick_line_evals", "pick_evidence", "parlay_tickets",
                 "copy_bot_events", "gpt_conversations", "gpt_messages", "gpt_claims", "gpt_sim_runs", "ml_experiments", "ml_examples",
                 "data_sources", "schema_migrations"]
GB = 1024 ** 3


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for b in iter(lambda: f.read(1 << 20), b""):
            h.update(b)
    return h.hexdigest()


def ours():
    """Backup directories this script made, oldest first."""
    if not os.path.isdir(ROOT_DIR):
        return []
    return sorted(d for d in os.listdir(ROOT_DIR) if d.startswith("pg_") and os.path.exists(os.path.join(ROOT_DIR, d, "manifest.json")))


def last_dump_size():
    sizes = [os.path.getsize(os.path.join(ROOT_DIR, d, "monsteros.dump")) for d in ours() if os.path.exists(os.path.join(ROOT_DIR, d, "monsteros.dump"))]
    return sizes[-1] if sizes else 600 * 1024 ** 2


def table_facts():
    """Row counts of the irreplaceable tables and a fingerprint of the forecast ledger."""
    import psycopg
    out = {}
    with psycopg.connect(dbenv.url("OPS_DATABASE_URL")) as cx:
        cx.read_only = True
        have = {r[0] for r in cx.execute("SELECT tablename FROM pg_tables WHERE schemaname='public'")}
        for t in IRREPLACEABLE:
            if t in have:
                out[t] = cx.execute(f"SELECT count(*) FROM {t}").fetchone()[0]
        out["_ledger_fingerprint"] = cx.execute("SELECT md5(coalesce(string_agg(id::text || ':' || payload_sha256, ',' ORDER BY id), '')) FROM prediction_ledger").fetchone()[0]
        out["_migrations"] = [r[0] for r in cx.execute("SELECT file FROM schema_migrations ORDER BY file")]
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--keep", type=int, default=3)
    ap.add_argument("--offsite", help="rclone destination, e.g. gdrive:monster-os-backups")
    a = ap.parse_args()
    os.makedirs(ROOT_DIR, exist_ok=True)
    free = shutil.disk_usage(ROOT_DIR).free
    need = 3 * last_dump_size() + 2 * GB
    if free < need:
        print(f"REFUSED: {free / GB:.1f} GB free on the backup disk, {need / GB:.1f} GB needed (3x the last dump + 2 GB). Free space first.")
        return 2
    stamp = dt.datetime.now().strftime("%Y%m%d_%H%M%S")
    out = os.path.join(ROOT_DIR, f"pg_{stamp}")
    os.makedirs(out)
    t0 = dt.datetime.now()
    facts = table_facts()  # counted just before the dump; rows written in between show as a small surplus on restore
    try:
        with open(os.path.join(out, "monsteros.dump"), "wb") as f:
            r = subprocess.run(["docker", "exec", CONTAINER, "pg_dump", "-U", "monster", "-Fc", "-Z", "6", DB], stdout=f, stderr=subprocess.PIPE)
        if r.returncode:
            raise RuntimeError(r.stderr.decode(errors="replace")[:500])
        with open(os.path.join(out, "globals.sql"), "wb") as f:
            r = subprocess.run(["docker", "exec", CONTAINER, "pg_dumpall", "-U", "monster", "--globals-only", "--no-role-passwords"], stdout=f, stderr=subprocess.PIPE)
        if r.returncode:
            raise RuntimeError(r.stderr.decode(errors="replace")[:500])
    except Exception as e:
        shutil.rmtree(out, ignore_errors=True)
        print(f"FAILED: {e}")
        return 1
    files = {n: {"bytes": os.path.getsize(os.path.join(out, n)), "sha256": sha256(os.path.join(out, n))} for n in ("monsteros.dump", "globals.sql")}
    commit = subprocess.run(["git", "rev-parse", "--short", "HEAD"], cwd=dbenv.ROOT, capture_output=True, text=True).stdout.strip()
    manifest = {"created": t0.isoformat(timespec="seconds"), "seconds": round((dt.datetime.now() - t0).total_seconds(), 1), "database": DB,
                "files": files, "rows": {k: v for k, v in facts.items() if not k.startswith("_")}, "ledger_fingerprint": facts["_ledger_fingerprint"],
                "migrations": facts["_migrations"], "git_commit": commit, "offsite": None}
    if a.offsite:
        rc = shutil.which("rclone") or os.path.expandvars(r"%LOCALAPPDATA%\Microsoft\WinGet\Packages\Rclone.Rclone_Microsoft.Winget.Source_8wekyb3d8bbwe\rclone-v1.75.1-windows-amd64\rclone.exe")
        r = subprocess.run([rc, "copy", out, f"{a.offsite.rstrip('/')}/pg_{stamp}"], capture_output=True, text=True)
        manifest["offsite"] = {"destination": a.offsite, "ok": r.returncode == 0, "error": None if r.returncode == 0 else r.stderr.strip()[-400:]}
    json.dump(manifest, open(os.path.join(out, "manifest.json"), "w"), indent=1)
    with open(os.path.join(ROOT_DIR, "LATEST"), "w") as f:
        f.write(out)
    # retention: only our own directories, oldest first
    removed = []
    for d in ours()[:-a.keep] if a.keep > 0 else []:
        shutil.rmtree(os.path.join(ROOT_DIR, d), ignore_errors=True)
        removed.append(d)
    print(f"backup {out}: dump {files['monsteros.dump']['bytes'] / 1024 ** 2:.0f} MB in {manifest['seconds']} s; "
          f"{len(manifest['rows'])} irreplaceable tables counted; ledger fingerprint {manifest['ledger_fingerprint'][:12]}"
          + (f"; removed {', '.join(removed)}" if removed else "")
          + (f"; offsite {'ok' if manifest['offsite']['ok'] else 'FAILED: ' + manifest['offsite']['error']}" if manifest["offsite"] else "; offsite: not configured"))
    return 0 if not manifest["offsite"] or manifest["offsite"]["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
