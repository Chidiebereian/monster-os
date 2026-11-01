"""Database URLs for scripts, from the environment or the git-ignored .env file.

No credential lives in tracked files. A script asks for a variable by name:

  ML_DATABASE_URL    research role (monster_research): read everything, insert into
                     the research tables only. Set by scripts/ops/rotate_research_password.py.
  OPS_DATABASE_URL   owner role, for operations such as publication decisions.

The value comes from the process environment first, then from KEY=value lines in
the repository's .env. Values are never printed; a missing variable stops the
script with an actionable message.
"""
import os

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))
ENV_FILE = os.path.join(ROOT, ".env")

HELP = {
    "ML_DATABASE_URL": "run `python scripts/ops/rotate_research_password.py` once (it sets the research role's password and writes ML_DATABASE_URL to .env)",
    "OPS_DATABASE_URL": "add OPS_DATABASE_URL=postgresql://<owner>:<password>@localhost:5434/monsteros to .env or the environment",
}


class MissingCredential(SystemExit):
    pass


def read_env_file(key, path=ENV_FILE):
    """Value of `key` in a KEY=value file, or None. Reads bytes so CRLF / LF both work."""
    try:
        with open(path, "rb") as f:
            for raw in f.read().splitlines():
                line = raw.decode("utf-8", "replace").strip()
                if line.startswith("#") or "=" not in line:
                    continue
                k, v = line.split("=", 1)
                if k.strip() == key:
                    return v.strip().strip('"').strip("'") or None
    except OSError:
        return None
    return None


def url(key, path=ENV_FILE):
    v = os.environ.get(key) or read_env_file(key, path)
    if not v:
        raise MissingCredential(f"{key} is not set. {HELP.get(key, 'set it in the environment or .env')}. (Nothing was written.)")
    return v
