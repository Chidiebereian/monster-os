"""Margin-shape checks (research, read-only): do the engines' score-margin distributions have
the shape real games have?

  NFL: key numbers. Real NFL margins pile up on 3, 7, 10 (field goals and touchdowns); a
       model that draws each team's points from a smooth count distribution spreads that mass
       out. Compares each engine's predicted P(|margin| = k) with real margins, and the
       nfl_team spread backtest's calibration on key-number lines vs other lines.
  NHL: the puck line. Empty-net goals push real margins from 1 to 2 goals. Compares predicted
       P(|margin| = 1), P(= 2), P(>= 2) with real NHL results, and checks the event simulator's
       stored puck-line probabilities against the games already played.

Engine distributions come from the frozen pregame forecasts (prediction_ledger), real results
from nhl_games_api / sports_games. Connects as the research role (ML_DATABASE_URL); writes only
the report under %LOCALAPPDATA%/MonsterOS/reports.

  python scripts/ml/margin_shape.py
"""
import datetime as dt
import json
import os
import sys

import psycopg

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
import dbenv  # noqa: E402

OUT = os.path.join(os.environ.get("LOCALAPPDATA", os.path.expanduser("~")), "MonsterOS", "reports")
NFL_KEYS = [1, 2, 3, 4, 6, 7, 10, 14]


def abs_pmf(pmf, offset):
    """P(|margin| = k) from a signed pmf whose first entry is margin `offset`."""
    out = {}
    for i, p in enumerate(pmf):
        k = abs(i + offset)
        out[k] = out.get(k, 0.0) + p
    return out


def mean_abs_pmf(rows):
    """Average P(|margin| = k) over games, each a (pmf, offset) pair."""
    acc, n = {}, 0
    for pmf, off in rows:
        s = sum(pmf)
        if not s:
            continue
        for k, p in abs_pmf([x / s for x in pmf], off).items():
            acc[k] = acc.get(k, 0.0) + p
        n += 1
    return {k: v / n for k, v in acc.items()}, n


def empirical(margins):
    n = len(margins)
    out = {}
    for m in margins:
        out[abs(m)] = out.get(abs(m), 0) + 1
    return {k: v / n for k, v in out.items()}, n


def ledger(cx, sport, path):
    """(pmf, offset, game_id) of each frozen forecast; path is the JSON path to the margin pmf object."""
    rows = cx.execute(f"SELECT game_id, payload #> %s FROM prediction_ledger WHERE sport=%s AND payload #> %s IS NOT NULL",
                      (path, sport, path)).fetchall()
    return [(r[1]["pmf"], r[1]["pmf_offset"], r[0]) for r in rows if r[1] and "pmf" in r[1]]


def brier(rows):
    return sum((p - y) ** 2 for p, y in rows) / len(rows) if rows else None


def main():
    cx = psycopg.connect(dbenv.url("ML_DATABASE_URL"))
    cx.read_only = True
    rep = {"generated": dt.datetime.now().isoformat(timespec="seconds")}

    # ------------------------------------------------------------------ NFL key numbers
    real = [h - a for h, a in cx.execute(
        "SELECT home_score, away_score FROM sports_games WHERE sport='nfl' AND game_type IN ('REG','POST') "
        "AND home_score IS NOT NULL AND season >= 2015")]
    emp, n_emp = empirical(real)
    nfl = {"real": {"games": n_emp, "p_abs_margin": {k: round(emp.get(k, 0), 4) for k in NFL_KEYS}}}
    for label, sport, path in (("nfl_team (team-points engine, champion for game markets)", "nfl_team", ["dist", "margin"]),
                               ("nfl (play-by-play simulator)", "nfl", ["margin_home"])):
        m, n = mean_abs_pmf([(p, o) for p, o, _ in ledger(cx, sport, path)])
        if n:
            nfl[label] = {"games": n, "p_abs_margin": {k: round(m.get(k, 0), 4) for k in NFL_KEYS}}
    # spread calibration on key-number lines vs the rest (walk-forward backtest, home covers = y)
    bt = cx.execute("SELECT line, p_model, y FROM ml_examples WHERE source='backtest' AND engine='nfl_team' AND market='spread' AND y IS NOT NULL").fetchall()
    key = lambda line: abs(line) in (2.5, 3, 3.5, 6.5, 7, 7.5)
    cal = {}
    for name, sel in (("key-number lines (2.5-3.5, 6.5-7.5)", lambda r: key(r[0])), ("other lines", lambda r: not key(r[0]))):
        rows = [(p, y) for line, p, y in bt if sel((line, p, y))]
        if rows:
            cal[name] = {"n": len(rows), "mean_p_model": round(sum(p for p, _ in rows) / len(rows), 4),
                         "actual_cover_rate": round(sum(y for _, y in rows) / len(rows), 4),
                         "brier_model": round(brier(rows), 4), "brier_coin_flip": round(brier([(0.5, y) for _, y in rows]), 4)}
    nfl["nfl_team spread backtest calibration"] = cal
    rep["nfl"] = nfl

    # ------------------------------------------------------------------ NHL puck line
    real = cx.execute("SELECT game_id::text, home_score - away_score, last_period FROM nhl_games_api WHERE completed AND game_type IN (2,3) "
                      "AND game_date >= '2019-09-15'").fetchall()
    emp, n_emp = empirical([m for _, m, _ in real])
    by_id = {g: m for g, m, _ in real}
    reg = [m for _, m, lp in real if lp == "REG"]
    nhl = {"real": {"games": n_emp, "p_abs_1": round(emp.get(1, 0), 4), "p_abs_2": round(emp.get(2, 0), 4),
                    "p_abs_ge_2": round(sum(v for k, v in emp.items() if k >= 2), 4),
                    "share_decided_in_ot_or_so": round(1 - len(reg) / n_emp, 4)}}
    for label, sport, path in (("nhl (event simulator)", "nhl", ["margin_home"]), ("nhl_team (team-goals engine)", "nhl_team", ["dist", "margin"])):
        rows = ledger(cx, sport, path)
        m, n = mean_abs_pmf([(p, o) for p, o, _ in rows])
        if not n:
            continue
        played = [abs(by_id[g]) for _, _, g in rows if g in by_id]
        e_played, n_played = empirical(played) if played else ({}, 0)
        nhl[label] = {"games": n, "p_abs_0": round(m.get(0, 0), 4), "p_abs_1": round(m.get(1, 0), 4), "p_abs_2": round(m.get(2, 0), 4),
                      "p_abs_ge_2": round(sum(v for k, v in m.items() if k >= 2), 4),
                      "same_games_played": n_played,
                      "same_games_real_p_abs_1": round(e_played.get(1, 0), 4) if n_played else None,
                      "same_games_real_p_abs_ge_2": round(sum(v for k, v in e_played.items() if k >= 2), 4) if n_played else None}
    # event simulator puck line vs results of the games already played
    pl = cx.execute("SELECT game_id, (payload->'puckline'->>'home_minus_1_5')::float8, (payload->'puckline'->>'away_minus_1_5')::float8 "
                    "FROM prediction_ledger WHERE sport='nhl' AND payload ? 'puckline'").fetchall()
    rows = [(ph, pa, by_id[g]) for g, ph, pa in pl if g in by_id and ph is not None]
    if rows:
        home = [(ph, 1.0 if m >= 2 else 0.0) for ph, _, m in rows]
        away = [(pa, 1.0 if m <= -2 else 0.0) for _, pa, m in rows]
        nhl["nhl event simulator puck line (settled frozen forecasts)"] = {
            "n": len(rows),
            "home_-1.5": {"mean_p": round(sum(p for p, _ in home) / len(home), 4), "actual": round(sum(y for _, y in home) / len(home), 4), "brier": round(brier(home), 4)},
            "away_-1.5": {"mean_p": round(sum(p for p, _ in away) / len(away), 4), "actual": round(sum(y for _, y in away) / len(away), 4), "brier": round(brier(away), 4)},
            "note": "frozen forecasts count each game once per freeze; small sample, indicative only"}
    rep["nhl"] = nhl

    os.makedirs(OUT, exist_ok=True)
    path = os.path.join(OUT, f"margin_shape_{dt.date.today()}.json")
    json.dump(rep, open(path, "w"), indent=1)
    print(json.dumps(rep, indent=1))
    print(path)


if __name__ == "__main__":
    main()
