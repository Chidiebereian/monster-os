"""Integrity checks against the running database and projection-service.

Write-once tables, decision-time features, game separation, determinism,
provenance, research / production isolation and publication holds. Every test
is read-only for production data: mutations are attempted only where they must
be refused, and the slow research-mode run touches nothing but ml_examples.

  python -m pytest scripts/ml/test_integrity.py -q            # needs Postgres :5434 and projection-service :8121
  python -m pytest scripts/ml/test_integrity.py -q -m "not slow"
"""
import json
import os
import subprocess
import sys
import urllib.error
import urllib.request

import numpy as np
import pandas as pd
import psycopg
import pytest

import experiment as ex

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
import dbenv  # noqa: E402

OWNER = dbenv.url("OPS_DATABASE_URL")
RESEARCH = dbenv.url("ML_DATABASE_URL")
BASE = os.environ.get("PROJECTION_URL", "http://localhost:8121")
HERE = os.path.dirname(os.path.abspath(__file__))
HELD = {("nhl_team", "team_total_away"), ("nhl_team", "team_total_home"), ("nba_team", "moneyline_1h"),
        ("nhl_team", "moneyline_1h"), ("nhl_team", "team_total_away_1h")}


def q(sql, args=(), url=OWNER):
    with psycopg.connect(url) as cx:
        return cx.execute(sql, args).fetchall()


def http(path, method="GET"):
    req = urllib.request.Request(BASE + path, method=method)
    try:
        with urllib.request.urlopen(req, timeout=600) as r:
            return r.status, json.load(r)
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"{}")


def refused(sql, url=OWNER):
    with psycopg.connect(url, autocommit=True) as cx:
        try:
            cx.execute(sql)
        except psycopg.Error as e:
            return str(e).splitlines()[0]
    return None


# ---------------------------------------------------------------- write-once

@pytest.mark.parametrize("table", ["ml_examples", "ml_experiments", "feature_snapshots", "publication_holds"])
def test_research_and_hold_tables_refuse_update_delete_truncate(table):
    col = {"ml_examples": "y", "ml_experiments": "verdict", "feature_snapshots": "value", "publication_holds": "reason"}[table]
    val = {"ml_examples": "0", "ml_experiments": "'rejected'", "feature_snapshots": "0", "publication_holds": "'x'"}[table]
    assert refused(f"UPDATE {table} SET {col}={val} WHERE id=(SELECT min(id) FROM {table})"), table
    assert refused(f"DELETE FROM {table} WHERE id=(SELECT min(id) FROM {table})"), table
    assert refused(f"TRUNCATE {table}"), table


def test_guards_are_installed():
    names = {r[0] for r in q("SELECT tgname FROM pg_trigger WHERE NOT tgisinternal AND tgrelid IN "
                             "('ml_examples'::regclass,'ml_experiments'::regclass,'feature_snapshots'::regclass,'publication_holds'::regclass)")}
    for t in ["ml_examples_immutable", "ml_examples_no_truncate", "ml_experiments_immutable", "ml_experiments_no_truncate",
              "feature_snapshots_immutable", "feature_snapshots_no_truncate", "publication_holds_immutable", "publication_holds_no_truncate"]:
        assert t in names, t


# ---------------------------------------------------------------- decision-time features

def test_feature_rows_are_never_available_before_they_are_observed_or_after_the_event():
    bad = q("SELECT feature_set, count(*) FROM feature_snapshots WHERE available_at < observed_at "
            "OR (feature_set IN ('availability','lineup','minutes') AND event_at IS NOT NULL AND available_at > event_at) "
            "OR (quality = 'missing' AND value IS NOT NULL) GROUP BY 1")
    assert bad == [], bad


def test_engine_inputs_keep_the_freeze_timestamp():
    # every engine-input feature is stamped with the frozen_at of a real ledger row of that game
    bad = q("SELECT count(*) FROM feature_snapshots f WHERE f.source LIKE 'engine_inputs:%%' AND NOT EXISTS "
            "(SELECT 1 FROM prediction_ledger l WHERE l.game_id = f.game_id AND l.frozen_at = f.available_at)")
    assert bad[0][0] == 0


@pytest.mark.parametrize("sport", ["nba", "nhl"])
def test_enrichment_only_uses_snapshots_available_at_decision_time(sport):
    """Recompute every attached player feature with an independent SQL as-of query."""
    df = ex.load(sport, None, "live")
    df = df[df["kind"] == "prop"].sample(min(300, (df["kind"] == "prop").sum()), random_state=1)
    out, _ = ex.enrich(df, sport)
    for i, r in out.iterrows():
        for name in ("p_active", "exp_minutes", "starter"):
            sql = q("SELECT value FROM feature_snapshots WHERE sport=%s AND game_id=%s AND subject=%s AND name=%s AND quality IN ('observed','derived') "
                    "AND available_at <= %s ORDER BY available_at DESC, id DESC LIMIT 1", (sport, str(r["game_id"]), str(r["subject"]), name, r["decision_at"]))
            want = sql[0][0] if sql else None
            got = r[name] if name in r and not pd.isna(r[name]) else None
            assert (want is None and got is None) or (want is not None and got is not None and abs(want - got) < 1e-9), (r["id"], name, want, got)


def test_every_feature_group_uses_the_same_asof_filter():
    # one function, one filter: every group goes through asof() (no group reads feature_snapshots on its own)
    src = open(os.path.join(HERE, "experiment.py"), encoding="utf-8").read()
    assert src.count("FROM feature_snapshots") == 1 and "available_at\"] <= m[\"decision_at\"]" in src


# ---------------------------------------------------------------- game separation, determinism, idempotency

@pytest.mark.parametrize("sport,markets,source", [("nba", None, "live"), ("nhl", None, "live"), ("nfl", ["spread"], "backtest")])
def test_no_game_in_train_and_test_and_runs_are_deterministic(sport, markets, source):
    # run() asserts game separation itself; two runs must agree exactly
    a = ex.run(sport, markets, source, save=False)[1]
    b = ex.run(sport, markets, source, save=False)[1]
    assert np.array_equal(a["ids"], b["ids"])
    for k in a["preds"]:
        assert np.allclose(a["preds"][k], b["preds"][k], atol=0, rtol=0), k


def test_builders_are_idempotent():
    # the first run may add newly settled games; an immediate second run must add nothing
    for script in ("dataset.py", "features.py"):
        subprocess.run([sys.executable, os.path.join(HERE, script)], capture_output=True, text=True, timeout=900)
        out = subprocess.run([sys.executable, os.path.join(HERE, script)], capture_output=True, text=True, timeout=900).stdout
        new = [int(x) for x in __import__("re").findall(r"(\d+) new", out)]
        assert new and all(n == 0 for n in new), (script, out[-400:])


def test_unchanged_experiment_is_not_recorded_twice():
    before = q("SELECT count(*) FROM ml_experiments")[0][0]
    ex.run("nba", ["moneyline"], "backtest", save=True)
    mid = q("SELECT count(*) FROM ml_experiments")[0][0]
    ex.run("nba", ["moneyline"], "backtest", save=True)
    assert q("SELECT count(*) FROM ml_experiments")[0][0] == mid and mid - before <= 1


def test_stored_experiments_carry_provenance():
    rows = q("SELECT name, results->'info', gate, dataset_hash, code_version, n, n_test FROM ml_experiments "
             "WHERE code_version >= 'experiment.py v1.1' AND results->'info' IS NOT NULL ORDER BY id DESC LIMIT 20")
    assert rows
    for name, info, gate, h, code, n, n_test in rows:
        for k in ("config", "feature_definitions", "splits", "games", "test_rows", "folds"):
            assert k in info, (name, k)
        assert gate.get("caveats") and gate.get("why") and h and code and n >= n_test


# ---------------------------------------------------------------- research / production isolation

def test_research_role_cannot_write_production_tables():
    for sql in ["INSERT INTO pick_evidence (engine, market, source, comparison, n, detail) VALUES ('x','x','x','x',0,'{}')",
                "UPDATE ai_picks SET grade='A+' WHERE id=-1", "DELETE FROM ai_picks WHERE id=-1",
                "INSERT INTO publication_holds (engine, market, action, reason, decided_by) VALUES ('nhl_team','team_total_home','release','x','x')",
                "INSERT INTO prediction_ledger (sport) VALUES ('x')", "UPDATE model_registry SET status='champion' WHERE false"]:
        assert refused(sql, RESEARCH), sql


def test_research_scripts_use_the_research_role_without_embedded_credentials():
    import re
    for script in ("dataset.py", "features.py", "experiment.py"):
        src = open(os.path.join(HERE, script), encoding="utf-8").read()
        assert 'dbenv.url("ML_DATABASE_URL")' in src, script
        assert not re.search(r"postgres(ql)?://[^:/\s\"']+:[^@\s\"']+@", src), script
    with psycopg.connect(RESEARCH) as cx:
        assert cx.execute("SELECT current_user").fetchone()[0] == "monster_research"


def test_missing_credentials_stop_with_an_actionable_message(tmp_path, monkeypatch):
    monkeypatch.delenv("ML_DATABASE_URL", raising=False)
    with pytest.raises(dbenv.MissingCredential) as e:
        dbenv.url("ML_DATABASE_URL", path=str(tmp_path / "missing.env"))
    assert "ML_DATABASE_URL is not set" in str(e.value) and "rotate_research_password.py" in str(e.value)


def test_research_role_has_no_password_in_the_migration():
    src = open(os.path.join(HERE, "..", "..", "infrastructure", "postgres", "migrations", "0037_publication_holds.sql"), encoding="utf-8").read()
    assert "PASSWORD" not in src.upper().replace("ROTATE_RESEARCH_PASSWORD", "")


def test_manual_backtests_need_an_explicit_mode():
    assert http("/picks/evidence/run?engine=nfl_team", "POST")[0] == 400
    assert http("/picks/evidence/run?engine=nfl_team&mode=prod", "POST")[0] == 400
    assert http("/picks/evidence/run?engine=nfl_team&mode=production&research_only=true", "POST")[0] == 400


@pytest.mark.slow
def test_research_mode_leaves_production_evidence_and_tiers_alone():
    sig = "SELECT md5(string_agg(engine||market||comparison||n||coalesce(metric::text,'')||updated_at::text, ',' ORDER BY engine,market,comparison)) FROM pick_evidence WHERE source='backtest'"
    before, tiers_before = q(sig)[0][0], http("/picks/record?days=1")[1]["tiers"]
    st, r = http("/picks/evidence/run?engine=nfl_team&mode=research&max_games=5", "POST")
    assert st == 200 and r["mode"] == "research" and r["evidence_rows_written"] == 0
    assert q(sig)[0][0] == before
    assert http("/picks/record?days=1")[1]["tiers"] == tiers_before


# ---------------------------------------------------------------- publication holds

def test_exactly_the_five_markets_are_held():
    assert {(e, m) for e, m in q("SELECT engine, market FROM publication_holds_active")} == HELD


def test_held_markets_never_reach_public_picks():
    for path in ("/picks?status=open&limit=1000&include_hidden=true", "/picks?status=all&limit=1000&include_hidden=true"):
        st, d = http(path)
        assert st == 200 and not any((p["engine"], p["market"]) in HELD for p in d["picks"]), path
    st, d = http("/picks/meta")
    assert {(h["engine"], h["market"]) for h in d["publication_holds"]} == HELD


def test_held_picks_stay_available_internally():
    n_internal = q("SELECT count(*) FROM ai_picks WHERE (engine, market) IN (SELECT engine, market FROM publication_holds_active)")[0][0]
    # one request per held market: a single 1,000-row page of the whole table (4,500+ picks, sorted by
    # grade) can leave every held pick on a later page
    shown = 0
    for market in sorted({m for _, m in HELD}):
        st, d = http(f"/picks?status=all&limit=1000&include_hidden=true&include_held=true&market={market}")
        assert st == 200 and len(d["picks"]) < 1000, market  # the page holds the whole market
        shown += sum((p["engine"], p["market"]) in HELD for p in d["picks"])
    # every held pick is still served on the internal view
    assert shown == n_internal


def test_holds_do_not_change_evidence_or_unrelated_markets():
    # holds sit beside the evidence: tiers and caps of every market come from pick_evidence alone
    tiers = {(t["engine"], t["market"]): (t["tier"], t["cap"]) for t in http("/picks/record?days=1")[1]["tiers"]}
    assert tiers[("nhl_team", "team_total_home")][0] == "baseline"  # still baseline underneath, only hidden
    pub = http("/picks?status=open&limit=1000")[1]["picks"]
    internal = http("/picks?status=open&limit=1000&include_held=true")[1]["picks"]
    assert [p["id"] for p in internal if (p["engine"], p["market"]) not in HELD] == [p["id"] for p in pub]
