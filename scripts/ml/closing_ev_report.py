"""Closing-line report (read-only): does each market's price beat the close by enough to clear the vig?

For every settled AI pick whose line was still quoted at the close (quarantined feeds left out):
  CLV          de-vigged closing probability - de-vigged probability when the pick was made
  closing EV   expected value of the price actually taken at the closing fair probability
               (p_close x (decimal - 1) - (1 - p_close)); > 0 means the bet was +EV after the vig
Verdict per engine x market: QUALIFIES when n >= --min-n and closing EV is above 0 by 2 standard
errors. That is the default-feed standard proposed in research-backlog R9.

  python scripts/ml/closing_ev_report.py
  python scripts/ml/closing_ev_report.py --min-n 300 --json report.json
"""
import argparse
import json
import math
import os
import sys

import psycopg

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
import dbenv  # noqa: E402

SQL = """
WITH p AS (SELECT a.engine, a.market, a.tier, a.clv, a.profit,
                  a.close_p_market * ((CASE WHEN a.price > 0 THEN 1 + a.price / 100.0 ELSE 1 + 100.0 / (-a.price) END) - 1)
                    - (1 - a.close_p_market) AS ev
           FROM ai_picks a LEFT JOIN feed_quarantine q ON q.sport = a.sport AND q.market = a.market AND q.book = a.book
           WHERE a.status = 'settled' AND a.result IN ('win', 'loss', 'push') AND q.sport IS NULL
             AND a.close_p_market IS NOT NULL AND a.close_line = a.line)
SELECT engine, market, count(*), avg(ev), stddev(ev), avg(clv), avg((clv > 0)::int), sum(profit)
FROM p GROUP BY 1, 2 ORDER BY count(*) DESC"""


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--min-n", type=int, default=300)
    ap.add_argument("--json")
    a = ap.parse_args()
    with psycopg.connect(dbenv.url("ML_DATABASE_URL")) as cx:
        cx.read_only = True
        rows = cx.execute(SQL).fetchall()
    out = []
    for engine, market, n, ev, sd, clv, beat, units in rows:
        se = (sd or 0) / math.sqrt(n) if n > 1 else None
        verdict = ("QUALIFIES" if n >= a.min_n and se is not None and ev - 2 * se > 0 else
                   "too few" if n < a.min_n else "positive, not significant" if ev > 0 else "negative")
        out.append({"engine": engine, "market": market, "n": n, "closing_ev": round(ev, 4), "se": round(se, 4) if se else None,
                    "avg_clv": round(clv, 4) if clv is not None else None, "beat_close": round(beat, 3), "units": round(units or 0, 1), "verdict": verdict})
    print(f"{'engine':9} {'market':20} {'n':>5} {'closing EV':>10} {'se':>7} {'CLV':>7} {'beat':>6} {'units':>7}  verdict")
    for r in out:
        print(f"{r['engine']:9} {r['market']:20} {r['n']:>5} {r['closing_ev']:>10} {r['se'] or '':>7} {r['avg_clv'] or '':>7} {r['beat_close']:>6} {r['units']:>7}  {r['verdict']}")
    q = [r for r in out if r["verdict"] == "QUALIFIES"]
    print(f"\n{len(out)} markets with settled picks; {len(q)} qualify (closing EV > 0 by 2 SE, n >= {a.min_n})")
    if a.json:
        json.dump(out, open(a.json, "w"), indent=1)


if __name__ == "__main__":
    main()
