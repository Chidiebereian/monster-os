"""R7 (research, read-only): NFL key-number reshaping of the team-points engine's spread prices.

The team-points engine draws each team's points from a smooth count distribution, so its
margins miss the NFL's spikes at 3 and 7 (scripts/ml/margin_shape.py). This experiment keeps
the engine's projection and changes only the shape:

  1. the engine's projected home margin mu: from its own cover probability p at line s,
     mu = s + sigma * Phi^-1(p), sigma = the engine's margin spread measured from its frozen
     distributions (prediction_ledger);
  2. the reshaped price: among earlier-season games whose closing spread was near mu, the share
     whose margin beat s (pushes left out, as in the backtest's outcomes). Real margins carry
     the key numbers, so the price does too;
  3. walk-forward: each season is priced only from seasons before it (from 2010).

Compared on the same games: the engine's price, the reshaped price, and the coin flip at the
closing line (the market). Nothing is written to the database; report under
%LOCALAPPDATA%/MonsterOS/reports.

  python scripts/ml/r7_keynumbers.py
"""
import bisect
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
MIN_NEIGHBOURS = 150
KEY = (2.5, 3.0, 3.5, 6.5, 7.0, 7.5)


def inv_phi(p):
    """Inverse standard normal CDF (Acklam's rational approximation, |error| < 1.2e-9)."""
    p = min(max(p, 1e-6), 1 - 1e-6)
    a = [-3.969683028665376e+01, 2.209460984245205e+02, -2.759285104469687e+02, 1.383577518672690e+02, -3.066479806614716e+01, 2.506628277459239e+00]
    b = [-5.447609879822406e+01, 1.615858368580409e+02, -1.556989798598866e+02, 6.680131188771972e+01, -1.328068155288572e+01]
    c = [-7.784894002430293e-03, -3.223964580411365e-01, -2.400758277161838e+00, -2.549671010229528e+00, 4.374664141464968e+00, 2.938163982698783e+00]
    d = [7.784695709041462e-03, 3.224671290700398e-01, 2.445134137142996e+00, 3.754408661907416e+00]
    if p < 0.02425:
        q = math.sqrt(-2 * math.log(p))
        return (((((c[0] * q + c[1]) * q + c[2]) * q + c[3]) * q + c[4]) * q + c[5]) / ((((d[0] * q + d[1]) * q + d[2]) * q + d[3]) * q + 1)
    if p > 1 - 0.02425:
        return -inv_phi(1 - p)
    q = p - 0.5
    r = q * q
    return (((((a[0] * r + a[1]) * r + a[2]) * r + a[3]) * r + a[4]) * r + a[5]) * q / (((((b[0] * r + b[1]) * r + b[2]) * r + b[3]) * r + b[4]) * r + 1)


def engine_sigma(cx):
    sds = []
    for (m,) in cx.execute("SELECT payload->'dist'->'margin' FROM prediction_ledger WHERE sport='nfl_team' AND payload ? 'dist'"):
        pmf, off = m["pmf"], m["pmf_offset"]
        s = sum(pmf)
        mean = sum((i + off) * p for i, p in enumerate(pmf)) / s
        sds.append(math.sqrt(sum((i + off - mean) ** 2 * p for i, p in enumerate(pmf)) / s))
    return statistics.median(sds), len(sds)


def reshaped(history, mu, line):
    """Share of earlier games with closing spread near mu whose margin beat `line` (pushes excluded)."""
    spreads = [h[0] for h in history]
    width = 0.5
    while True:
        lo, hi = bisect.bisect_left(spreads, mu - width), bisect.bisect_right(spreads, mu + width)
        if hi - lo >= MIN_NEIGHBOURS or width >= 10:
            break
        width += 0.5
    # shift each neighbour's margin by the gap between mu and its own spread, keeping integer margins
    win = push = 0
    for spread, margin in history[lo:hi]:
        m = margin + round(mu - spread)
        if m > line:
            win += 1
        elif m == line:
            push += 1
    n = hi - lo - push
    return (win / n if n else 0.5), hi - lo, width


def score(rows):
    """Brier and log loss of (p, y) pairs."""
    eps = 1e-6
    b = sum((p - y) ** 2 for p, y in rows) / len(rows)
    ll = -sum(y * math.log(max(p, eps)) + (1 - y) * math.log(max(1 - p, eps)) for p, y in rows) / len(rows)
    return round(b, 4), round(ll, 4)


def main():
    cx = psycopg.connect(dbenv.url("ML_DATABASE_URL"))
    cx.read_only = True
    sigma, n_sigma = engine_sigma(cx)
    games = cx.execute("SELECT season, spread_line, home_score - away_score FROM sports_games WHERE sport='nfl' AND game_type IN ('REG','POST') "
                       "AND home_score IS NOT NULL AND spread_line IS NOT NULL AND season >= 2010").fetchall()
    tests = cx.execute("SELECT e.season, e.line, e.p_model, e.y FROM ml_examples e WHERE e.source='backtest' AND e.engine='nfl_team' "
                       "AND e.market='spread' AND e.y IS NOT NULL ORDER BY e.season").fetchall()
    out = {"generated": dt.datetime.now().isoformat(timespec="seconds"), "engine_margin_sd": round(sigma, 2), "sd_from_frozen_forecasts": n_sigma,
           "method": "reshaped = share of earlier-season games with closing spread near the engine's mu whose margin beat the line",
           "seasons": {}, "all": {}, "key_lines": {}, "other_lines": {}}
    pooled = {"engine": [], "reshaped": [], "coin_flip": [], "line": []}
    for season in sorted({t[0] for t in tests}):
        history = sorted((s, m) for se, s, m in games if se < season)
        rows = []
        for se, line, p, y in tests:
            if se != season:
                continue
            mu = line + sigma * inv_phi(p)
            q, n_nb, _ = reshaped(history, mu, line)
            rows.append((line, p, q, y))
        eng, res, coin = [(p, y) for _, p, _, y in rows], [(q, y) for _, _, q, y in rows], [(0.5, y) for *_, y in rows]
        out["seasons"][season] = {"n": len(rows), "history_games": len(history), "brier_logloss": {"engine": score(eng), "reshaped": score(res), "coin_flip": score(coin)}}
        for (line, p, q, y) in rows:
            pooled["engine"].append((p, y)); pooled["reshaped"].append((q, y)); pooled["coin_flip"].append((0.5, y)); pooled["line"].append(line)
    for name, sel in (("all", lambda l: True), ("key_lines", lambda l: abs(l) in KEY), ("other_lines", lambda l: abs(l) not in KEY)):
        idx = [i for i, l in enumerate(pooled["line"]) if sel(l)]
        out[name] = {"n": len(idx)} | {k: score([pooled[k][i] for i in idx]) for k in ("engine", "reshaped", "coin_flip")}
        # paired difference reshaped - engine, with a standard error (games are independent)
        d = [(pooled["reshaped"][i][0] - pooled["reshaped"][i][1]) ** 2 - (pooled["engine"][i][0] - pooled["engine"][i][1]) ** 2 for i in idx]
        out[name]["brier_diff_reshaped_minus_engine"] = round(statistics.mean(d), 4)
        out[name]["se"] = round(statistics.stdev(d) / math.sqrt(len(d)), 4)
        d2 = [(pooled["reshaped"][i][0] - pooled["reshaped"][i][1]) ** 2 - 0.25 for i in idx]
        out[name]["brier_diff_reshaped_minus_coin_flip"] = round(statistics.mean(d2), 4)
        out[name]["se_vs_coin_flip"] = round(statistics.stdev(d2) / math.sqrt(len(d2)), 4)
    os.makedirs(OUT, exist_ok=True)
    path = os.path.join(OUT, f"r7_keynumbers_{dt.date.today()}.json")
    json.dump(out, open(path, "w"), indent=1)
    print(json.dumps(out, indent=1))
    print(path)


if __name__ == "__main__":
    main()
