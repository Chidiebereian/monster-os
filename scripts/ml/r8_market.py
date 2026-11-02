"""R8 step 2 (research, read-only): the NHL event simulator's puck-line prices vs the market.

For every settled NHL game with puck-line quotes: the market probability of each -1.5 side is
each book's last pre-start pair (side -1.5 vs other side +1.5), de-vigged multiplicatively,
averaged over books; the simulator's is its last frozen forecast before the start
(prediction_ledger, nhl). Scored on the real result (nhl_games_api). Small sample while
quotes accumulate (stored since 2026-10-21): indicative only.

  python scripts/ml/r8_market.py
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


def implied(price):
    return 100 / (price + 100) if price > 0 else -price / (-price + 100)


def main():
    cx = psycopg.connect(dbenv.url("ML_DATABASE_URL"))
    cx.read_only = True
    quotes = cx.execute("""
        SELECT DISTINCT ON (game_id, book, side) game_id, book, side, line, price
        FROM market_lines WHERE sport='nhl' AND market='spread' AND abs(line)=1.5 AND game_id IS NOT NULL
          AND (starts_at IS NULL OR observed_at < starts_at)
        ORDER BY game_id, book, side, observed_at DESC""").fetchall()
    books = {}
    for g, b, side, line, price in quotes:
        books.setdefault((g, b), {})[side] = (line, price)
    market = {}  # (game, side at -1.5) -> list of de-vigged probabilities
    for (g, _), q in books.items():
        if "home" in q and "away" in q and q["home"][0] == -q["away"][0]:
            fav = "home" if q["home"][0] == -1.5 else "away"
            p_fav, p_dog = implied(q[fav][1]), implied(q["away" if fav == "home" else "home"][1])
            market.setdefault((g, fav), []).append(p_fav / (p_fav + p_dog))
    sim = {g: (ph, pa) for g, ph, pa in cx.execute("""
        SELECT DISTINCT ON (l.game_id) l.game_id, (l.payload->'puckline'->>'home_minus_1_5')::float8, (l.payload->'puckline'->>'away_minus_1_5')::float8
        FROM prediction_ledger l JOIN nhl_games_api g ON g.game_id::text = l.game_id
        WHERE l.sport='nhl' AND l.payload ? 'puckline' AND (g.start_time IS NULL OR l.frozen_at < g.start_time)
        ORDER BY l.game_id, l.frozen_at DESC""")}
    result = {str(g): h - a for g, h, a in cx.execute("SELECT game_id, home_score, away_score FROM nhl_games_api WHERE completed")}
    rows = []
    for (g, fav), ps in market.items():
        if g in sim and g in result and sim[g][0] is not None:
            p_sim = sim[g][0] if fav == "home" else sim[g][1]
            y = 1.0 if (result[g] >= 2 if fav == "home" else result[g] <= -2) else 0.0
            rows.append((fav, statistics.mean(ps), p_sim, y))
    out = {"generated": dt.datetime.now().isoformat(timespec="seconds"), "games": len(rows)}
    if rows:
        d = [(s - y) ** 2 - (m - y) ** 2 for _, m, s, y in rows]
        out |= {"actual_rate": round(statistics.mean(y for *_, y in rows), 4),
                "market": {"mean_p": round(statistics.mean(m for _, m, _, _ in rows), 4), "brier": round(statistics.mean((m - y) ** 2 for _, m, _, y in rows), 4)},
                "simulator": {"mean_p": round(statistics.mean(s for _, _, s, _ in rows), 4), "brier": round(statistics.mean((s - y) ** 2 for _, _, s, y in rows), 4)},
                "brier_diff_simulator_minus_market": {"mean": round(statistics.mean(d), 4), "se": round(statistics.stdev(d) / math.sqrt(len(d)), 4) if len(d) > 1 else None},
                "by_favourite_side": {side: len([r for r in rows if r[0] == side]) for side in ("home", "away")},
                "note": "indicative: few weeks of stored quotes; no claim of an edge"}
    os.makedirs(OUT, exist_ok=True)
    path = os.path.join(OUT, f"r8_market_{dt.date.today()}.json")
    json.dump(out, open(path, "w"), indent=1)
    print(json.dumps(out, indent=1))
    print(path)


if __name__ == "__main__":
    main()
