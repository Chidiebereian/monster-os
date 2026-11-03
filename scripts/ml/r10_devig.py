"""R10 (research, read-only): which way of removing the bookmaker's margin predicts results best?

A two-way quote implies probabilities q1 + q2 = S > 1; the excess S - 1 is the margin. Methods:

  multiplicative  p = q / S (what Monster OS uses today: blend::novig)
  additive        p = q - (S - 1) / 2
  power           p = q ** k, k such that the two sum to 1 (often called the logarithmic method)
  shin            Shin (1993): margin explained by a share z of insider money; z solved so the two sum to 1
  odds_ratio      p = q / (c + q - c q), c such that the two sum to 1 (Cheung 2015)

Test set: every settled selection Monster OS priced (pick_line_evals, y oriented to home / over) with
each book's last quote of both sides before the start (market_lines, same line; quarantined feeds left
out). Scored two ways, both with standard errors over selections:
  consensus   de-vig each book, average over books (how picks are priced)
  per book    every book's pair scored on its own, averaged per selection
Plus the favourite-longshot check (calibration of the less likely side by probability bucket), and the
closing-line EV of settled AI picks recomputed under each method (closing_ev_report.py uses
multiplicative).

  python scripts/ml/r10_devig.py
"""
import datetime as dt
import json
import math
import os
import statistics
import sys

import psycopg

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
import dbenv  # noqa: E402

OUT = os.path.join(os.environ.get("LOCALAPPDATA", os.path.expanduser("~")), "MonsterOS", "reports")
METHODS = ["multiplicative", "additive", "power", "shin", "odds_ratio"]
EPS = 1e-4


def implied(price):
    return 100 / (price + 100) if price > 0 else -price / (-price + 100)


def solve(f, lo, hi, n=80):
    """Root of an increasing f on [lo, hi] by bisection."""
    for _ in range(n):
        mid = (lo + hi) / 2
        if f(mid) > 0:
            hi = mid
        else:
            lo = mid
    return (lo + hi) / 2


def devig(q1, q2, method):
    """Fair probability of side 1 from the implied probabilities of a two-way quote."""
    s = q1 + q2
    if s <= 1 or method == "multiplicative":
        return q1 / s
    if method == "additive":
        return min(max(q1 - (s - 1) / 2, EPS), 1 - EPS)
    if method == "power":
        k = solve(lambda k: 1 - (q1 ** k + q2 ** k), 1.0, 50.0)
        return q1 ** k
    if method == "shin":
        def p(q, z):
            return (math.sqrt(z * z + 4 * (1 - z) * q * q / s) - z) / (2 * (1 - z))
        z = solve(lambda z: 1 - (p(q1, z) + p(q2, z)), 0.0, 0.5)
        return p(q1, z)
    if method == "odds_ratio":
        def p(q, c):
            return q / (c + q - c * q)
        c = solve(lambda c: 1 - (p(q1, c) + p(q2, c)), 1.0, 100.0)
        return p(q1, c)
    raise ValueError(method)


def logloss(p, y):
    p = min(max(p, EPS), 1 - EPS)
    return -(y * math.log(p) + (1 - y) * math.log(1 - p))


def mean_se(v):
    return {"mean": round(statistics.mean(v), 5), "se": round(statistics.stdev(v) / math.sqrt(len(v)), 5) if len(v) > 1 else None, "n": len(v)}


SELECTIONS = """
WITH s AS (SELECT DISTINCT sport, game_id, market, subject, line, y, starts_at FROM pick_line_evals
           WHERE y IS NOT NULL AND starts_at IS NOT NULL)
SELECT s.sport, s.game_id, s.market, s.subject, s.line, s.y, m.book, m.side, m.price
FROM s JOIN LATERAL (
    SELECT DISTINCT ON (book, side) book, side, price FROM market_lines m
    WHERE m.sport = s.sport AND m.game_id = s.game_id AND m.market = s.market AND m.subject = s.subject
      AND m.observed_at < s.starts_at
      AND (s.market LIKE 'moneyline%%'
           OR (m.side IN ('home', 'over', 'under') AND m.line = s.line)
           OR (m.side = 'away' AND m.line = -s.line))
    ORDER BY book, side, observed_at DESC) m ON true
WHERE NOT EXISTS (SELECT 1 FROM feed_quarantine q WHERE q.sport = s.sport AND q.market = s.market AND q.book = m.book)"""

PICKS = """
SELECT a.id, a.side, a.price, a.result, o.price
FROM ai_picks a
JOIN LATERAL (SELECT price FROM market_lines m
              WHERE m.sport = a.sport AND m.game_id = a.game_id AND m.market = a.market AND m.subject = a.subject AND m.book = a.book
                AND m.side = CASE a.side WHEN 'home' THEN 'away' WHEN 'away' THEN 'home' WHEN 'over' THEN 'under' ELSE 'over' END
                AND (a.market LIKE 'moneyline%%' OR m.line = CASE WHEN a.side IN ('home', 'away') THEN -a.close_line ELSE a.close_line END)
                AND m.observed_at <= COALESCE(a.starts_at, now())
              ORDER BY observed_at DESC LIMIT 1) o ON true
WHERE a.status = 'settled' AND a.result IN ('win', 'loss') AND a.close_price IS NOT NULL AND a.close_line IS NOT DISTINCT FROM a.line
  AND NOT EXISTS (SELECT 1 FROM feed_quarantine q WHERE q.sport = a.sport AND q.market = a.market AND q.book = a.book)"""


def main():
    with psycopg.connect(dbenv.url("ML_DATABASE_URL")) as cx:
        cx.read_only = True
        rows = cx.execute(SELECTIONS).fetchall()
        picks = cx.execute(PICKS).fetchall()
        close_prices = {r[0]: r[1] for r in cx.execute("SELECT id, close_price FROM ai_picks WHERE close_price IS NOT NULL")}

    # selection -> book -> {side: price}
    sel = {}
    for sport, game, market, subject, line, y, book, side, price in rows:
        key = (sport, game, market, subject, line)
        sel.setdefault(key, {"y": y, "books": {}})["books"].setdefault(book, {})[side] = price
    pairs = {}
    for key, v in sel.items():
        ps = []
        for book, q in v["books"].items():
            a, b = ("home", "away") if "home" in q or "away" in q else ("over", "under")
            if a in q and b in q:
                ps.append((implied(q[a]), implied(q[b])))
        if ps:
            pairs[key] = (v["y"], ps)

    groups = {"all": lambda k: True, "game": lambda k: k[3] == "game", "prop": lambda k: k[3] != "game"}
    for sp in sorted({k[0] for k in pairs}):
        groups[sp] = (lambda s: lambda k: k[0] == s)(sp)
    out = {"generated": dt.datetime.now().isoformat(timespec="seconds"), "selections": len(pairs),
           "book_pairs": sum(len(p) for _, p in pairs.values()),
           "mean_margin": round(statistics.mean(a + b - 1 for _, ps in pairs.values() for a, b in ps), 4), "groups": {}}

    # per selection: consensus probability and per-book mean loss, per method
    scored = {}
    for key, (y, ps) in pairs.items():
        r = {}
        for m in METHODS:
            probs = [devig(a, b, m) for a, b in ps]
            pc = statistics.mean(probs)
            r[m] = {"p": pc, "ll_c": logloss(pc, y), "br_c": (pc - y) ** 2,
                    "ll_b": statistics.mean(logloss(p, y) for p in probs), "br_b": statistics.mean((p - y) ** 2 for p in probs)}
        scored[key] = (y, r)

    for g, f in groups.items():
        keys = [k for k in scored if f(k)]
        if len(keys) < 30:
            continue
        res = {"n": len(keys)}
        for m in METHODS:
            res[m] = {"logloss_consensus": round(statistics.mean(scored[k][1][m]["ll_c"] for k in keys), 5),
                      "brier_consensus": round(statistics.mean(scored[k][1][m]["br_c"] for k in keys), 5),
                      "logloss_per_book": round(statistics.mean(scored[k][1][m]["ll_b"] for k in keys), 5)}
            if m != "multiplicative":
                res[m]["minus_multiplicative"] = {
                    "logloss_consensus": mean_se([scored[k][1][m]["ll_c"] - scored[k][1]["multiplicative"]["ll_c"] for k in keys]),
                    "logloss_per_book": mean_se([scored[k][1][m]["ll_b"] - scored[k][1]["multiplicative"]["ll_b"] for k in keys])}
        out["groups"][g] = res

    # favourite-longshot: the less likely side, bucketed by its multiplicative probability
    buckets = [(0.0, 0.2), (0.2, 0.3), (0.3, 0.4), (0.4, 0.5)]
    fl = {}
    for lo, hi in buckets:
        items = []
        for k, (y, r) in scored.items():
            pm = r["multiplicative"]["p"]
            dog_p, dog_y = (pm, y) if pm < 0.5 else (1 - pm, 1 - y)
            if lo <= dog_p < hi:
                items.append((dog_y, {m: (r[m]["p"] if pm < 0.5 else 1 - r[m]["p"]) for m in METHODS}))
        if items:
            fl[f"{lo:.1f}-{hi:.1f}"] = {"n": len(items), "won": round(statistics.mean(y for y, _ in items), 4),
                                        **{m: round(statistics.mean(p[m] for _, p in items), 4) for m in METHODS}}
    out["favourite_longshot"] = fl

    # closing-line EV of settled AI picks under each method
    ev = {m: [] for m in METHODS}
    for pid, side, price, result, other in picks:
        cp = close_prices.get(pid)
        if cp is None:
            continue
        dec = 1 + price / 100 if price > 0 else 1 + 100 / -price
        for m in METHODS:
            p = devig(implied(cp), implied(other), m)
            ev[m].append(p * (dec - 1) - (1 - p))
    out["ai_picks_closing_ev"] = {m: {"n": len(v), "mean": round(statistics.mean(v), 4) if v else None,
                                      "share_positive": round(statistics.mean(x > 0 for x in v), 4) if v else None} for m, v in ev.items()}

    os.makedirs(OUT, exist_ok=True)
    path = os.path.join(OUT, f"r10_devig_{dt.date.today()}.json")
    json.dump(out, open(path, "w"), indent=1)
    print(f"{out['selections']} selections, {out['book_pairs']} book pairs, mean margin {out['mean_margin']}")
    for g, res in out["groups"].items():
        line = f"{g:6} n={res['n']:>6}  " + "  ".join(f"{m[:5]} {res[m]['logloss_consensus']:.5f}" for m in METHODS)
        print(line)
        for m in METHODS[1:]:
            d = res[m]["minus_multiplicative"]["logloss_consensus"]
            print(f"         {m:14} - mult: {d['mean']:+.5f} (se {d['se']})   per book {res[m]['minus_multiplicative']['logloss_per_book']['mean']:+.5f} (se {res[m]['minus_multiplicative']['logloss_per_book']['se']})")
    print("favourite-longshot (less likely side):", json.dumps(fl, indent=1))
    print("AI picks closing EV:", json.dumps(out["ai_picks_closing_ev"], indent=1))
    print(path)


if __name__ == "__main__":
    main()
