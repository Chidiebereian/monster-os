"""R8 (research, read-only): NHL puck line (home -1.5) from the team-goals engine's backtests.

The engines put too little weight on 2+ goal margins (real 59.3%, engines ~55%; margin_shape.py),
the empty-net effect. Puck-line prices are not in the backtests, so they are rebuilt per game:

  engine-style  each side's expected goals from the engine's own team-total price at 2.5
                (Poisson: P(X >= 3) = p), home -1.5 = P(home - away >= 2) for independent
                Poisson goals (a regulation tie goes to overtime and ends 1-goal).
  win x share   the engine's home-win probability x the share of home wins by 2+ goals among
                earlier-season games with a similar engine win probability (walk-forward).
  base rate     the earlier seasons' home -1.5 rate.

Real results from nhl_games_api (shootouts included). Nothing is written to the database;
report under %LOCALAPPDATA%/MonsterOS/reports.

  python scripts/ml/r8_puckline.py
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


def pois(k, lam):
    return math.exp(-lam) * lam ** k / math.factorial(k)


def lam_from_over(p_over, line=2.5):
    """Poisson mean with P(X > line) = p_over (bisection)."""
    k = int(math.floor(line))
    lo, hi = 0.05, 12.0
    for _ in range(60):
        mid = (lo + hi) / 2
        over = 1 - sum(pois(i, mid) for i in range(k + 1))
        lo, hi = (mid, hi) if over < p_over else (lo, mid)
    return (lo + hi) / 2


def p_home_by_2(lh, la, top=20):
    ph = [pois(i, lh) for i in range(top)]
    pa = [pois(i, la) for i in range(top)]
    return sum(ph[i] * pa[j] for i in range(top) for j in range(top) if i - j >= 2)


def scores(pairs):
    b = sum((p - y) ** 2 for p, y in pairs) / len(pairs)
    ll = -sum(y * math.log(max(p, 1e-6)) + (1 - y) * math.log(max(1 - p, 1e-6)) for p, y in pairs) / len(pairs)
    return round(b, 4), round(ll, 4)


def main():
    cx = psycopg.connect(dbenv.url("ML_DATABASE_URL"))
    cx.read_only = True
    rows = cx.execute("""
        SELECT e.game_id, e.season,
               max(e.p_model) FILTER (WHERE e.market='moneyline')                         AS p_win,
               max(e.p_model) FILTER (WHERE e.market='team_total_home' AND e.line = 2.5)  AS p_home_over,
               max(e.p_model) FILTER (WHERE e.market='team_total_away' AND e.line = 2.5)  AS p_away_over
        FROM ml_examples e WHERE e.source='backtest' AND e.engine='nhl_team'
        GROUP BY e.game_id, e.season""").fetchall()
    result = {str(g): (h - a) for g, h, a in cx.execute(
        "SELECT game_id, home_score, away_score FROM nhl_games_api WHERE completed AND game_type IN (2,3)")}
    games = [(s, pw, ph, pa, result[g]) for g, s, pw, ph, pa in rows if g in result and None not in (pw, ph, pa)]
    out = {"generated": dt.datetime.now().isoformat(timespec="seconds"), "games_matched": len(games), "games_in_backtest": len(rows), "seasons": {}}
    pooled = {"engine_style": [], "win_x_share": [], "base_rate": []}
    buckets = lambda p: min(int(p * 20), 19)  # 5-point win-probability buckets
    for season in sorted({g[0] for g in games}):
        past = [g for g in games if g[0] < season]
        if len(past) < 500:
            continue
        base = sum(1 for g in past if g[4] >= 2) / len(past)
        wins, by2 = {}, {}
        for _, pw, _, _, m in past:
            if m > 0:
                b = buckets(pw)
                wins[b] = wins.get(b, 0) + 1
                by2[b] = by2.get(b, 0) + (m >= 2)
        all_share = sum(by2.values()) / max(1, sum(wins.values()))
        cur = {"engine_style": [], "win_x_share": [], "base_rate": []}
        for _, pw, ph, pa, m in (g for g in games if g[0] == season):
            y = 1.0 if m >= 2 else 0.0
            b = buckets(pw)
            share = (by2[b] + 20 * all_share) / (wins[b] + 20) if b in wins else all_share  # shrunk toward the overall share
            cur["engine_style"].append((p_home_by_2(lam_from_over(ph), lam_from_over(pa)), y))
            cur["win_x_share"].append((pw * share, y))
            cur["base_rate"].append((base, y))
        out["seasons"][season] = {"n": len(cur["base_rate"]), "actual_rate": round(statistics.mean(y for _, y in cur["base_rate"]), 4)} | \
            {k: {"mean_p": round(statistics.mean(p for p, _ in v), 4), "brier_logloss": scores(v)} for k, v in cur.items()}
        for k in pooled:
            pooled[k] += cur[k]
    n = len(pooled["base_rate"])
    out["all"] = {"n": n, "actual_rate": round(statistics.mean(y for _, y in pooled["base_rate"]), 4)} | \
        {k: {"mean_p": round(statistics.mean(p for p, _ in v), 4), "brier_logloss": scores(v)} for k, v in pooled.items()}
    for a, b in (("win_x_share", "engine_style"), ("engine_style", "base_rate"), ("win_x_share", "base_rate")):
        d = [(pooled[a][i][0] - pooled[a][i][1]) ** 2 - (pooled[b][i][0] - pooled[b][i][1]) ** 2 for i in range(n)]
        out["all"][f"brier_diff_{a}_minus_{b}"] = {"mean": round(statistics.mean(d), 5), "se": round(statistics.stdev(d) / math.sqrt(n), 5)}
    os.makedirs(OUT, exist_ok=True)
    path = os.path.join(OUT, f"r8_puckline_{dt.date.today()}.json")
    json.dump(out, open(path, "w"), indent=1)
    print(json.dumps(out["all"], indent=1))
    for s, v in out["seasons"].items():
        print(s, v["n"], "actual", v["actual_rate"], {k: (v[k]["mean_p"], v[k]["brier_logloss"][0]) for k in pooled})
    print(path)


if __name__ == "__main__":
    main()
