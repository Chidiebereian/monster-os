"""Local gate for research and publication safety (no cloud service needed).

Runs, in order, and fails closed (exit 1) on the first broken guarantee:
  1. migrations      every migration applies to an empty scratch database; the research
                     migrations (0035 on) apply a second time without change (older ones,
                     e.g. 0016, are not written to be re-run and are applied once)
  2. ml-unit         synthetic learner and leakage tests (scripts/ml/test_experiment.py)
  3. integrity       write-once tables, decision-time features, game separation,
                     determinism, provenance, research isolation, publication holds
                     (scripts/ml/test_integrity.py; needs Postgres and projection-service)
  4. rust            cargo test for projection-service and monster-gpt
  5. dashboard       TypeScript check
  6. e2e (--e2e)     MonsterGPT end-to-end tests (needs monster-gpt running)
  7. secrets         tracked, staged and to-be-added files plus git history: database URLs
                     with passwords, the live .env credentials, private keys, API-key shapes.
                     Files that already carried the local owner login before this gate
                     existed are listed as pre-existing; anything else fails. No value is printed.

Each step's output goes to %LOCALAPPDATA%/MonsterOS/logs/research/checks_<time>.log.

  python scripts/ci/research_checks.py            # before asking to commit research work
  python scripts/ci/research_checks.py --fast     # skip the slow research-mode batch and cargo
"""
import argparse
import datetime as dt
import os
import subprocess
import sys

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
LOGDIR = os.path.join(os.environ.get("LOCALAPPDATA", os.path.expanduser("~")), "MonsterOS", "logs", "research")
REAPPLY_FROM = "0035"
PG = ["docker", "exec", "-i", "monster-os-postgres-1", "psql", "-U", "monster", "-v", "ON_ERROR_STOP=1", "-q"]


def migrations():
    """Apply every migration to a scratch database twice (idempotent, no errors)."""
    admin = ["docker", "exec", "monster-os-postgres-1", "psql", "-U", "monster", "-d", "postgres", "-q", "-c"]
    subprocess.run(admin + ["DROP DATABASE IF EXISTS monsteros_ci"], check=True, capture_output=True)
    subprocess.run(admin + ["CREATE DATABASE monsteros_ci"], check=True, capture_output=True)
    try:
        files = sorted(f for f in os.listdir(os.path.join(ROOT, "infrastructure", "postgres", "migrations")) if f.endswith(".sql"))
        for rnd in (1, 2):
            for f in (files if rnd == 1 else [x for x in files if x >= REAPPLY_FROM]):
                sql = open(os.path.join(ROOT, "infrastructure", "postgres", "migrations", f), encoding="utf-8").read()
                # the research role is granted on monsteros; the scratch copy grants on itself
                sql = sql.replace("GRANT CONNECT ON DATABASE monsteros TO", "GRANT CONNECT ON DATABASE monsteros_ci TO")
                r = subprocess.run(PG + ["-d", "monsteros_ci"], input=sql, text=True, capture_output=True)
                if r.returncode != 0:
                    return 1, f"round {rnd}: {f}: {r.stderr.strip()[:400]}"
        holds = subprocess.run(PG + ["-d", "monsteros_ci", "-t", "-A", "-c", "SELECT count(*) FROM publication_holds"], capture_output=True, text=True).stdout.strip()
        return 0, f"{len(files)} migrations applied to an empty database, {REAPPLY_FROM}+ re-applied cleanly; publication_holds rows: {holds}"
    finally:
        subprocess.run(admin + ["DROP DATABASE IF EXISTS monsteros_ci"], capture_output=True)


# tracked before this gate existed; they carry the local docker owner login (reported, not hidden)
PREEXISTING = {".env.example", "docs/architecture/top-traders.md", "scripts/windows/run-projection-service.ps1", "scripts/windows/start-stack.ps1"}
SECRET_PATTERNS = [
    # a real password, not a template placeholder such as <password> or {password}
    (r"postgres(?:ql)?://[^:/\s'\"@]+:[^@\s'\"<{$][^@\s'\"]{2,}@", "database URL with a password"),
    # a key body, not just the header (detection tests mention the header)
    (r"-----BEGIN [A-Z ]*PRIVATE KEY-----\s*[A-Za-z0-9+/=]{40,}", "private key"),
    (r"sk-[A-Za-z0-9]{20,}", "API key (sk-)"),
    (r"AKIA[0-9A-Z]{16}", "AWS access key"),
    (r"gh[pousr]_[A-Za-z0-9]{30,}", "GitHub token"),
    (r"(?i)(api[_-]?key|secret|token|password)\s*[:=]\s*['\"][A-Za-z0-9/+_\-]{16,}['\"]", "hard-coded key/secret"),
]


def secrets_scan():
    import re
    sys.path.insert(0, os.path.join(ROOT, "scripts"))
    import dbenv
    live = []
    for k in ("ML_DATABASE_URL", "OPS_DATABASE_URL"):
        v = dbenv.read_env_file(k)
        m = re.match(r"postgres(?:ql)?://[^:]+:([^@]+)@", v or "")
        if m and len(m.group(1)) >= 12:  # the short local owner password is covered by the URL pattern
            live.append((m.group(1), f"live {k} password"))
    git = lambda *a: subprocess.run(["git", *a], cwd=ROOT, capture_output=True, text=True, encoding="utf-8", errors="replace").stdout
    files = set(git("ls-files").splitlines()) | set(git("diff", "--cached", "--name-only").splitlines()) \
        | set(git("ls-files", "--others", "--exclude-standard").splitlines())
    files = {f for f in files if f and os.path.isfile(os.path.join(ROOT, f)) and not f.endswith((".png", ".jpg", ".ico", ".woff2", ".lock", ".pt", ".bin"))}
    new, old = [], []
    for f in sorted(files):
        try:
            text = open(os.path.join(ROOT, f), encoding="utf-8", errors="replace").read()
        except OSError:
            continue
        hits = [label for pat, label in SECRET_PATTERNS if re.search(pat, text)] + [label for val, label in live if val in text]
        if hits:
            (old if f in PREEXISTING else new).append(f"{f}: {', '.join(sorted(set(hits)))}")
    history = git("log", "--all", "-p", "--no-color")
    leaked = [label for val, label in live if val in history]
    report = f"{len(files)} files scanned; pre-existing: {len(old)} ({'; '.join(old)}); new: {len(new)}; live credentials in history: {len(leaked)}"
    if new or leaked:
        return 1, report + "\n" + "\n".join(new + leaked)
    return 0, report


def run(cmd, cwd=ROOT, timeout=3600):
    r = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, timeout=timeout, shell=isinstance(cmd, str))
    return r.returncode, (r.stdout + r.stderr)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--fast", action="store_true")
    ap.add_argument("--e2e", action="store_true")
    a = ap.parse_args()
    py = sys.executable
    steps = [("migrations", migrations),
             ("ml-unit", lambda: run([py, "-m", "pytest", "-q", "-p", "no:cacheprovider", "scripts/ml/test_experiment.py"])),
             ("integrity", lambda: run([py, "-m", "pytest", "-q", "-p", "no:cacheprovider", "scripts/ml/test_integrity.py"] + (["-m", "not slow"] if a.fast else [])))]
    if not a.fast:
        steps.append(("rust", lambda: run(["cargo", "test", "-p", "projection-service", "-p", "monster-gpt", "-j", "2", "--bins"], timeout=7200)))
    steps.append(("dashboard", lambda: run("npx tsc --noEmit -p .", cwd=os.path.join(ROOT, "apps", "dashboard"))))
    steps.append(("secrets", secrets_scan))
    if a.e2e:
        steps.append(("e2e", lambda: run([py, "-m", "pytest", "-q", "-p", "no:cacheprovider", "apps/monster-gpt/tests/e2e_ws.py"], timeout=3600)))
    os.makedirs(LOGDIR, exist_ok=True)
    path = os.path.join(LOGDIR, f"checks_{dt.datetime.now():%Y%m%d_%H%M%S}.log")
    with open(path, "w", encoding="utf-8") as log:
        for name, fn in steps:
            code, out = fn()
            last = [l for l in out.strip().splitlines() if l.strip()][-1:] or [""]
            print(f"{'PASS' if code == 0 else 'FAIL'}  {name:11} {last[0][:150]}", flush=True)
            log.write(f"===== {name} (exit {code})\n{out}\n")
            if code != 0:
                print(f"\nstopped at {name}; full output in {path}")
                return 1
    print(f"\nall checks passed; log: {path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
