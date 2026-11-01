"""Rotate the monster_research database password and store ML_DATABASE_URL in .env.

Generates a random password, sets it on the role with the owner connection
(OPS_DATABASE_URL), and writes ML_DATABASE_URL to the git-ignored .env, editing
only that key (binary-safe, other lines untouched). Nothing secret is printed.

  python scripts/ops/rotate_research_password.py
  python scripts/ops/rotate_research_password.py --host localhost --port 5434 --db monsteros
"""
import argparse
import os
import secrets
import sys

import psycopg
from psycopg import sql

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
import dbenv  # noqa: E402

ROLE = "monster_research"


def set_env_key(path, key, value):
    """Replace or append KEY=value in a .env file without touching any other line or the line endings."""
    data = open(path, "rb").read() if os.path.exists(path) else b""
    nl = b"\r\n" if b"\r\n" in data else b"\n"
    lines = data.split(nl) if data else []
    if lines and lines[-1] == b"":
        lines = lines[:-1]
    new = f"{key}={value}".encode()
    out, done = [], False
    for ln in lines:
        if ln.split(b"=", 1)[0].strip() == key.encode():
            if not done:
                out.append(new)
                done = True
            continue
        out.append(ln)
    if not done:
        out.append(new)
    tmp = path + ".tmp"
    with open(tmp, "wb") as f:
        f.write(nl.join(out) + nl)
    os.replace(tmp, path)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="localhost")
    ap.add_argument("--port", default="5434")
    ap.add_argument("--db", default="monsteros")
    a = ap.parse_args()
    owner = dbenv.url("OPS_DATABASE_URL")
    password = secrets.token_urlsafe(32)
    with psycopg.connect(owner, autocommit=True) as cx:
        if not cx.execute("SELECT 1 FROM pg_roles WHERE rolname=%s", (ROLE,)).fetchone():
            sys.exit(f"role {ROLE} does not exist: apply migration 0037 first")
        cx.execute(sql.SQL("ALTER ROLE {} WITH PASSWORD {}").format(sql.Identifier(ROLE), sql.Literal(password)))
    set_env_key(dbenv.ENV_FILE, "ML_DATABASE_URL", f"postgresql://{ROLE}:{password}@{a.host}:{a.port}/{a.db}")
    # prove the new credential works, still without printing it
    with psycopg.connect(dbenv.url("ML_DATABASE_URL")) as cx:
        cx.execute("SELECT 1").fetchone()
    print(f"rotated the {ROLE} password; ML_DATABASE_URL updated in .env (value not shown)")


if __name__ == "__main__":
    main()
