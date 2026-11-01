"""Record a publication decision for an engine x market (publication_holds, migration 0037).

Releasing a hold makes the market's picks visible in public AI Picks again,
under its normal evidence tier. It needs a named decider, a reason and
--confirm; the decision is appended (the table refuses edits and deletes), so
the full history stays. There is deliberately no HTTP endpoint for this.

  python scripts/ops/release_hold.py --list
  python scripts/ops/release_hold.py --engine nhl_team --market team_total_home --release \
      --by "owner name" --reason "reviewed evidence after the scheduled refresh" --confirm
  python scripts/ops/release_hold.py --engine nhl_team --market total --suppress --by "owner name" --reason "..." --confirm
"""
import argparse
import os
import sys

import psycopg

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
import dbenv  # noqa: E402

# production decisions use the owner role; research scripts use monster_research, which cannot write here
DB = dbenv.url("OPS_DATABASE_URL")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--list", action="store_true")
    ap.add_argument("--engine")
    ap.add_argument("--market")
    g = ap.add_mutually_exclusive_group()
    g.add_argument("--release", action="store_true")
    g.add_argument("--suppress", action="store_true")
    ap.add_argument("--by")
    ap.add_argument("--reason")
    ap.add_argument("--confirm", action="store_true")
    a = ap.parse_args()
    with psycopg.connect(DB) as cx:
        if a.list or not (a.release or a.suppress):
            for r in cx.execute("SELECT engine, market, action, decided_by, decided_at, reason FROM publication_holds ORDER BY engine, market, decided_at, id"):
                print(" | ".join(str(x) for x in r))
            return 0
        if not (a.engine and a.market and a.by and a.reason and len(a.reason) >= 10):
            ap.error("--engine, --market, --by and a --reason of at least 10 characters are required")
        if not a.confirm:
            print("dry run: add --confirm to record this decision")
            return 1
        action = "release" if a.release else "suppress"
        snap = cx.execute("SELECT COALESCE(jsonb_agg(to_jsonb(e) - 'detail'), '[]'::jsonb) FROM pick_evidence e WHERE engine=%s AND market=%s",
                          (a.engine, a.market)).fetchone()[0]
        cx.execute("INSERT INTO publication_holds (engine, market, action, reason, decided_by, evidence_snapshot) VALUES (%s,%s,%s,%s,%s,%s)",
                   (a.engine, a.market, action, a.reason, a.by, psycopg.types.json.Jsonb(snap)))
        cx.commit()
        held = cx.execute("SELECT 1 FROM publication_holds_active WHERE engine=%s AND market=%s", (a.engine, a.market)).fetchone() is not None
        print(f"recorded {action} for {a.engine} {a.market} by {a.by}; now {'held' if held else 'public'}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
