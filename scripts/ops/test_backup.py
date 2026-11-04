"""The restore check catches a bad backup (scripts/ops/restore_check.py).

Uses the latest backup's dump through a hard link (no 300 MB copy) next to a tampered
manifest, so the live backup is never changed. Slow (two restores, ~2 min).

  python -m pytest -q scripts/ops/test_backup.py
"""
import json
import os
import shutil
import subprocess
import sys
import tempfile

import pytest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import backup  # noqa: E402

LATEST = os.path.join(backup.ROOT_DIR, "LATEST")


def tampered(change):
    src = open(LATEST).read().strip()
    d = tempfile.mkdtemp(prefix="pg_tamper_", dir=backup.ROOT_DIR)
    os.link(os.path.join(src, "monsteros.dump"), os.path.join(d, "monsteros.dump"))
    m = json.load(open(os.path.join(src, "manifest.json")))
    change(m)
    json.dump(m, open(os.path.join(d, "manifest.json"), "w"))
    return d


def check(d):
    r = subprocess.run([sys.executable, os.path.join(HERE, "restore_check.py"), "--backup", d], capture_output=True, text=True, timeout=900)
    return r.returncode, r.stdout


@pytest.mark.skipif(not os.path.exists(LATEST), reason="no backup yet: run scripts/ops/backup.py")
@pytest.mark.parametrize("what,change,expect", [
    ("a changed ledger", lambda m: m.update(ledger_fingerprint="0" * 32), "'ledger_fingerprint': False"),
    ("missing rows", lambda m: m["rows"].update(prediction_ledger=m["rows"]["prediction_ledger"] + 1), "'rows': False"),
    ("a different dump", lambda m: m["files"]["monsteros.dump"].update(sha256="0" * 64), "does not match its manifest"),
])
def test_a_bad_backup_fails_the_check(what, change, expect):
    d = tampered(change)
    try:
        code, out = check(d)
        assert code != 0 and expect in out, f"{what}: {out}"
    finally:
        shutil.rmtree(d, ignore_errors=True)
