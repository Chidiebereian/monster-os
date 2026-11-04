"""Simulations are reproducible: the same game, seed and simulation count give the same
numbers (docs/architecture/monster-gpt-v2.md 3, gpt_sim_runs). Read-only: calls the
live projection-service simulators, writes nothing.

  python -m pytest -q apps/monster-gpt/tests/test_sim_repro.py
"""
import json
import os
import sys
import urllib.request

import pytest

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "..", "scripts"))
import dbenv  # noqa: E402

BASE = os.environ.get("PROJECTION_URL", "http://localhost:8121")


def next_games():
    import psycopg
    with psycopg.connect(dbenv.url("APP_DATABASE_URL")) as cx:
        cx.read_only = True
        out = []
        r = cx.execute("SELECT game_id::text FROM nhl_games_api WHERE NOT completed AND start_time > now() ORDER BY start_time LIMIT 1").fetchone()
        if r: out.append(("nhl", r[0]))
        r = cx.execute("SELECT game_id FROM sports_games WHERE sport='nfl' AND home_score IS NULL AND game_date >= current_date ORDER BY game_date LIMIT 1").fetchone()
        if r: out.append(("nfl", r[0]))
        return out


def sim(sport, gid, seed, extra=""):
    with urllib.request.urlopen(f"{BASE}/{sport}/simulate/{gid}?n=100000&seed={seed}{extra}", timeout=300) as r:
        v = json.loads(r.read())
    v.pop("sim_ms", None)
    return v


@pytest.mark.parametrize("sport,gid", next_games())
def test_same_seed_same_numbers_and_a_new_seed_differs(sport, gid):
    a, b = sim(sport, gid, 4242), sim(sport, gid, 4242)
    assert a == b, "same game, seed and count must reproduce exactly"
    c = sim(sport, gid, 4243)
    assert c["win"] != a["win"] and c["seed"] == 4243, "another seed is another sample"
    # two seeds agree within the Monte Carlo error MonsterGPT states (sim.rs mc_error_pts:
    # binomial plus the measured between-draw variance 1.0e-5); a difference of two runs
    # has sqrt(2) times that sd, and 4 sd is the bound
    p, n = a["win"]["home"], a["n"]
    se = (p * (1 - p) / n + 1.0e-5) ** 0.5
    assert abs(c["win"]["home"] - p) < 4 * 2 ** 0.5 * se
