"""R7 step 2 (research, read-only): game-by-game NFL team ratings vs the closing spread.

R7 step 1 showed the team-points engine's spread prices lose to the closing line because its
projected margin is off, not because of the margin's shape. This tests two projections that
update after every game:

  least squares  before each game, fit  home_margin = hfa + r_home - r_away  on all earlier
                 games (exponential recency weights, ridge toward 0, previous seasons kept with
                 lower weight), Massey-style
  margin Elo     ratings move after every game by k * f(margin) * (result - expected); between
                 seasons each rating keeps 2/3 of its distance from the mean

A projected home margin mu becomes a cover probability at the closing line s through the
model's own residuals (margin - mu) from earlier seasons: P(mu + r > s), pushes left out, so
key-number clustering in real margins carries over. A blended price p_b = 0.5 + w (p - 0.5)
(toward the coin flip at the line, w in [0, 1]) is fitted on the three earlier seasons only.

Scored walk-forward per season (2015 on, ratings from 2010) on every game with a closing
spread, and on the games of the team-points engine's backtest for a like-for-like comparison.
Read-only (research role); report under %LOCALAPPDATA%/MonsterOS/reports.

  python scripts/ml/r7_ratings.py
"""
import bisect
import datetime as dt
import json
import math
import os
import statistics
import sys

import numpy as np
import psycopg

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
import dbenv  # noqa: E402

OUT = os.path.join(os.environ.get("LOCALAPPDATA", os.path.expanduser("~")), "MonsterOS", "reports")
FIRST_TEST = 2015
HALF_LIFE_GAMES = 120.0      # least squares: weight halves every 120 league games (~half a season)
RIDGE = 2.0                  # least squares: shrinkage of ratings toward 0
ELO_K, ELO_SCALE, ELO_HFA, ELO_KEEP = 20.0, 25.0, 48.0, 2 / 3   # Elo points per margin point = 25


def least_squares(games):
    """Projected home margin for each game from earlier games only (refit once per game date)."""
    teams = sorted({g["home"] for g in games} | {g["away"] for g in games})
    ix = {t: i for i, t in enumerate(teams)}
    n_t = len(teams)
    mu = [None] * len(games)
    dates = sorted({g["date"] for g in games})
    by_date = {}
    for i, g in enumerate(games):
        by_date.setdefault(g["date"], []).append(i)
    seen = []
    for d in dates:
        if len(seen) >= 200:
            m = len(seen)
            X = np.zeros((m, n_t + 1)); y = np.zeros(m); w = np.zeros(m)
            for j, k in enumerate(seen):
                g = games[k]
                X[j, ix[g["home"]]], X[j, ix[g["away"]]], X[j, n_t] = 1, -1, 1
                y[j] = g["margin"]
                w[j] = 0.5 ** ((m - 1 - j) / HALF_LIFE_GAMES)
            W = np.sqrt(w)
            A = X * W[:, None]
            reg = np.eye(n_t + 1) * RIDGE
            reg[n_t, n_t] = 1e-6  # home edge not shrunk
            beta = np.linalg.solve(A.T @ A + reg, A.T @ (y * W))
            for k in by_date[d]:
                g = games[k]
                mu[k] = beta[n_t] + beta[ix[g["home"]]] - beta[ix[g["away"]]]
        seen.extend(by_date[d])
    return mu


def elo(games):
    r, season, mu = {}, None, []
    for g in games:
        if g["season"] != season:
            season = g["season"]
            mean = statistics.mean(r.values()) if r else 1500.0
            r = {t: mean + ELO_KEEP * (v - mean) for t, v in r.items()}
        h, a = r.get(g["home"], 1500.0), r.get(g["away"], 1500.0)
        diff = h + ELO_HFA - a
        mu.append(diff / ELO_SCALE)
        expected = 1 / (1 + 10 ** (-diff / 400))
        result = 1.0 if g["margin"] > 0 else 0.0 if g["margin"] < 0 else 0.5
        mov = math.log(abs(g["margin"]) + 1) * (2.2 / ((diff if result == 1 else -diff) * 0.001 + 2.2))
        delta = ELO_K * mov * (result - expected)
        r[g["home"]], r[g["away"]] = h + delta, a - delta
    return mu


def cover_prob(residuals, mu, line):
    """P(mu + r > line) over earlier residuals; residuals sorted."""
    n = len(residuals)
    above = n - bisect.bisect_right(residuals, line - mu)
    return min(max(above / n, 0.001), 0.999)


def brier(pairs):
    return sum((p - y) ** 2 for p, y in pairs) / len(pairs)


def main():
    cx = psycopg.connect(dbenv.url("ML_DATABASE_URL"))
    cx.read_only = True
    rows = cx.execute("SELECT game_id, season, game_date, home_team, away_team, home_score - away_score, spread_line FROM sports_games "
                      "WHERE sport='nfl' AND game_type IN ('REG','POST') AND home_score IS NOT NULL AND season >= 2010 "
                      "ORDER BY game_date, game_id").fetchall()
    games = [{"id": r[0], "season": r[1], "date": r[2], "home": r[3], "away": r[4], "margin": r[5], "line": r[6]} for r in rows]
    engine_games = {r[0]: (r[1], r[2]) for r in cx.execute(
        "SELECT game_id, p_model, y FROM ml_examples WHERE source='backtest' AND engine='nfl_team' AND market='spread' AND y IS NOT NULL")}
    models = {"least_squares": least_squares(games), "elo": elo(games)}
    out = {"generated": dt.datetime.now().isoformat(timespec="seconds"), "games": len(games),
           "settings": {"half_life_games": HALF_LIFE_GAMES, "ridge": RIDGE, "elo": {"k": ELO_K, "points_per_margin": ELO_SCALE, "hfa": ELO_HFA, "keep": ELO_KEEP}}}
    for name, mu in models.items():
        seasons, pooled, pooled_eng = {}, {"model": [], "blend": [], "coin": []}, {"model": [], "engine": [], "coin": []}
        mae = []
        for season in range(FIRST_TEST, max(g["season"] for g in games) + 1):
            hist = [(g, m) for g, m in zip(games, mu) if g["season"] < season and m is not None]
            res = sorted(g["margin"] - m for g, m in hist)
            # blend weight fitted on the previous seasons' own probabilities (no look-ahead)
            prev = [(cover_prob(res, m, g["line"]), 1.0 if g["margin"] > g["line"] else 0.0)
                    for g, m in hist if g["season"] >= season - 3 and g["line"] is not None and g["margin"] != g["line"]]
            w = min(range(0, 101), key=lambda k: brier([(0.5 + k / 100 * (p - 0.5), y) for p, y in prev])) / 100 if prev else 0.0
            cur = []
            for g, m in zip(games, mu):
                if g["season"] != season or m is None or g["line"] is None or g["margin"] == g["line"]:
                    continue
                p = cover_prob(res, m, g["line"])
                y = 1.0 if g["margin"] > g["line"] else 0.0
                cur.append((p, 0.5 + w * (p - 0.5), y))
                mae.append((abs(g["margin"] - m), abs(g["margin"] - g["line"])))
                if g["id"] in engine_games:
                    pooled_eng["model"].append((p, y)); pooled_eng["engine"].append((engine_games[g["id"]][0], y)); pooled_eng["coin"].append((0.5, y))
            if cur:
                seasons[season] = {"n": len(cur), "w": w, "brier_model": round(brier([(p, y) for p, _, y in cur]), 4),
                                   "brier_blend": round(brier([(b, y) for _, b, y in cur]), 4)}
                pooled["model"] += [(p, y) for p, _, y in cur]; pooled["blend"] += [(b, y) for _, b, y in cur]; pooled["coin"] += [(0.5, y) for *_, y in cur]
        def diff(a, b):
            d = [(x[0] - x[1]) ** 2 - (z[0] - z[1]) ** 2 for x, z in zip(a, b)]
            return {"mean": round(statistics.mean(d), 4), "se": round(statistics.stdev(d) / math.sqrt(len(d)), 4)}
        out[name] = {"test_games": len(pooled["coin"]), "mae_margin_model": round(statistics.mean(m for m, _ in mae), 2),
                     "mae_margin_closing_spread": round(statistics.mean(s for _, s in mae), 2),
                     "brier": {k: round(brier(v), 4) for k, v in pooled.items()},
                     "model_minus_coin": diff(pooled["model"], pooled["coin"]), "blend_minus_coin": diff(pooled["blend"], pooled["coin"]),
                     "on_engine_backtest_games": {"n": len(pooled_eng["coin"])} | {k: round(brier(v), 4) for k, v in pooled_eng.items()} |
                                                 ({"model_minus_engine": diff(pooled_eng["model"], pooled_eng["engine"])} if pooled_eng["coin"] else {}),
                     "seasons": seasons}
    os.makedirs(OUT, exist_ok=True)
    path = os.path.join(OUT, f"r7_ratings_{dt.date.today()}.json")
    json.dump(out, open(path, "w"), indent=1, default=str)
    for name in models:
        o = out[name]
        print(name, {k: o[k] for k in ("test_games", "mae_margin_model", "mae_margin_closing_spread", "brier", "model_minus_coin", "blend_minus_coin")})
        print("   on engine backtest games:", o["on_engine_backtest_games"])
        print("   blend weights by season:", {s: v["w"] for s, v in o["seasons"].items()})
    print(path)


if __name__ == "__main__":
    main()
