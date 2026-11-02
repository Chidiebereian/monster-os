"""Rotate a service role's database password and store its URL in .env.

Generates a random password, sets it on the role with the owner connection
(OPS_DATABASE_URL), and writes the URL to the git-ignored .env, editing only that
key (binary-safe, other lines untouched). Nothing secret is printed.

  monster_research -> ML_DATABASE_URL   (research jobs, migration 0037)
  monster_app      -> APP_DATABASE_URL  (every running service, migration 0039)
  monster (owner)  -> OPS_DATABASE_URL and POSTGRES_PASSWORD (break-glass; restart nothing,
                      no service uses it; docker exec psql inside the container is unaffected)

  python scripts/ops/rotate_research_password.py
  python scripts/ops/rotate_research_password.py --role monster_app
  python scripts/ops/rotate_research_password.py --host 127.0.0.1 --port 5434 --db monsteros
"""
import argparse
import os
import secrets
import sys

import psycopg
from psycopg import sql

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
import dbenv  # noqa: E402

ROLES = {"monster_research": ("ML_DATABASE_URL", "0037"), "monster_app": ("APP_DATABASE_URL", "0039"), "monster": ("OPS_DATABASE_URL", "")}


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
    ap.add_argument("--host", default="127.0.0.1", help="Postgres is published on IPv4 loopback only; localhost would try ::1 first")
    ap.add_argument("--port", default="5434")
    ap.add_argument("--db", default="monsteros")
    ap.add_argument("--role", default="monster_research", choices=sorted(ROLES))
    a = ap.parse_args()
    role, (key, migration) = a.role, ROLES[a.role]
    owner = dbenv.url("OPS_DATABASE_URL")
    password = secrets.token_urlsafe(32)
    with psycopg.connect(owner, autocommit=True) as cx:
        if not cx.execute("SELECT 1 FROM pg_roles WHERE rolname=%s", (role,)).fetchone():
            sys.exit(f"role {role} does not exist: apply migration {migration} first")
        cx.execute(sql.SQL("ALTER ROLE {} WITH PASSWORD {}").format(sql.Identifier(role), sql.Literal(password)))
    set_env_key(dbenv.ENV_FILE, key, f"postgresql://{role}:{password}@{a.host}:{a.port}/{a.db}")
    if role == "monster":
        set_env_key(dbenv.ENV_FILE, "POSTGRES_PASSWORD", password)  # compose (initialisation only)
    os.environ.pop(key, None)  # read the new value back from .env, not a stale environment copy
    # prove the new credential works (and that a service role is not a superuser), still without printing it
    with psycopg.connect(dbenv.url(key)) as cx:
        if role != "monster" and cx.execute("SELECT rolsuper FROM pg_roles WHERE rolname = current_user").fetchone()[0]:
            sys.exit(f"{role} is a superuser; refusing")
    print(f"rotated the {role} password; {key} updated in .env (value not shown); restart the services that use it")


if __name__ == "__main__":
    main()
