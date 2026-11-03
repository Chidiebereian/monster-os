"""Closing-line report (read-only): does each market's price beat the close by enough to clear the vig?

For every settled AI pick whose line was still quoted at the close (quarantined feeds left out):
  CLV          de-vigged closing probability - de-vigged probability when the pick was made (as stored)
  closing EV   expected value of the price actually taken at the closing fair probability, the
               closing pair de-vigged with the power method (research R10; --method to compare)
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
import statistics
import sys

import psycopg

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
import dbenv  # noqa: E402
from r10_devig import METHODS, devig, implied  # noqa: E402

# each settled pick with its closing price and the other side's closing price at the same book and line
SQL = """
SELECT a.engine, a.market, a.price, a.close_price, o.price, a.clv, a.profit
FROM ai_picks a
JOIN LATERAL (SELECT price FROM market_lines m
              WHERE m.sport = a.sport AND m.game_id = a.game_id AND m.market = a.market AND m.subject = a.subject AND m.book = a.book
                AND m.side = CASE a.side WHEN 'home' THEN 'away' WHEN 'away' THEN 'home' WHEN 'over' THEN 'under' ELSE 'over' END
                AND (a.market LIKE 'moneyline%' OR m.line = CASE WHEN a.side IN ('home', 'away') THEN -a.close_line ELSE a.close_line END)
                AND m.observed_at <= COALESCE(a.starts_at, now())
              ORDER BY observed_at DESC LIMIT 1) o ON true
WHERE a.status = 'settled' AND a.result IN ('win', 'loss', 'push') AND a.close_price IS NOT NULL AND a.close_line = a.line
  AND NOT EXISTS (SELECT 1 FROM feed_quarantine q WHERE q.sport = a.sport AND q.market = a.market AND q.book = a.book)"""


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--min-n", type=int, default=300)
    ap.add_argument("--json")
    ap.add_argument("--method", default="power", choices=METHODS)
    a = ap.parse_args()
    with psycopg.connect(dbenv.url("ML_DATABASE_URL")) as cx:
        cx.read_only = True
        picks = cx.execute(SQL).fetchall()
    by = {}
    for engine, market, price, close, other, clv, profit in picks:
        p = devig(implied(close), implied(other), a.method)
        dec = 1 + price / 100 if price > 0 else 1 + 100 / -price
        by.setdefault((engine, market), []).append((p * (dec - 1) - (1 - p), clv, profit))
    rows = []
    for (engine, market), v in by.items():
        evs, clvs = [x[0] for x in v], [x[1] for x in v if x[1] is not None]
        rows.append((engine, market, len(v), statistics.mean(evs), statistics.stdev(evs) if len(v) > 1 else None,
                     statistics.mean(clvs) if clvs else None, statistics.mean(c > 0 for c in clvs) if clvs else 0.0, sum(x[2] or 0 for x in v)))
    rows.sort(key=lambda r: -r[2])
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
    allev = [x[0] for v in by.values() for x in v]
    if allev:
        print(f"\nall: n={len(allev)}, closing EV {statistics.mean(allev):+.4f} ({a.method} de-vig), +EV at the close {statistics.mean(e > 0 for e in allev):.1%}")
    print(f"{len(out)} markets with settled picks; {len(q)} qualify (closing EV > 0 by 2 SE, n >= {a.min_n})")
    if a.json:
        json.dump(out, open(a.json, "w"), indent=1)


if __name__ == "__main__":
    main()
