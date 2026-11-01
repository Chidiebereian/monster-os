"""Synthetic checks of the residual learners and the statistics behind the gate.

  python -m pytest scripts/ml/test_experiment.py -q
"""
import numpy as np
import pandas as pd

import experiment as ex


def synth(n=6000, signal=0.0, seed=1):
    """A market that is right on average, an engine that is noise, and one feature with `signal` beyond the market."""
    rng = np.random.default_rng(seed)
    lm = rng.normal(0, 0.6, n)
    x = rng.normal(0, 1, n)
    y = (rng.random(n) < ex.sigmoid(lm + signal * x)).astype(float)
    X = pd.DataFrame({"d_model": rng.normal(0, 0.5, n), "x": x})
    return X, lm, y


def brier_gain(fit, signal):
    X, lm, y = synth(signal=signal)
    tr, te = slice(0, 4000), slice(4000, None)
    f, _ = fit(X.iloc[tr] if fit is not ex.fit_enet else X.iloc[tr].to_numpy(), lm[tr], y[tr])
    p = f(X.iloc[te] if fit is not ex.fit_enet else X.iloc[te].to_numpy(), lm[te])
    return np.mean((p - y[te]) ** 2) - np.mean((ex.sigmoid(lm[te]) - y[te]) ** 2)


def test_no_signal_means_no_gain_over_the_market():
    for fit in (ex.fit_enet, ex.fit_lgbm, ex.fit_xgb):
        assert brier_gain(fit, 0.0) > -0.002, fit.__name__


def test_a_real_signal_is_found():
    for fit in (ex.fit_enet, ex.fit_lgbm, ex.fit_xgb):
        assert brier_gain(fit, 0.5) < -0.005, fit.__name__


def test_calibration_error():
    p = np.full(1000, 0.6)
    assert ex.ece(p, (np.arange(1000) % 10 < 6).astype(float)) < 1e-9
    assert abs(ex.ece(np.full(1000, 0.8), (np.arange(1000) % 10 < 6).astype(float)) - 0.2) < 1e-9


def _snap(rows):
    return pd.DataFrame(rows, columns=["game_id", "subject", "feature_set", "name", "value", "value_text", "available_at"]).assign(
        available_at=lambda d: pd.to_datetime(d["available_at"], utc=True))


def test_asof_ignores_snapshots_after_the_decision():
    S = _snap([("g1", "p1", "availability", "p_active", 1.0, None, "2026-10-30T18:00Z"),
               ("g1", "p1", "availability", "p_active", 0.0, None, "2026-10-30T23:00Z")])  # injury news after the decision
    keys = pd.DataFrame({"row": [0], "game_id": ["g1"], "subject": ["p1"], "decision_at": pd.to_datetime(["2026-10-30T22:00Z"], utc=True)})
    assert ex.asof(keys, S, ["p_active"]).loc[0, "p_active"] == 1.0


def test_asof_takes_the_latest_snapshot_known_in_time_and_exact_ties_count():
    S = _snap([("g1", "p1", "minutes", "exp_minutes", 20.0, None, "2026-10-30T12:00Z"),
               ("g1", "p1", "minutes", "exp_minutes", 31.0, None, "2026-10-30T22:00Z")])
    keys = pd.DataFrame({"row": [0, 1], "game_id": ["g1", "g1"], "subject": ["p1", "p1"],
                         "decision_at": pd.to_datetime(["2026-10-30T22:00Z", "2026-10-30T21:59Z"], utc=True)})
    r = ex.asof(keys, S, ["exp_minutes"])
    assert r.loc[0, "exp_minutes"] == 31.0 and r.loc[1, "exp_minutes"] == 20.0


def test_asof_never_borrows_another_game_or_player():
    S = _snap([("g2", "p1", "availability", "p_active", 0.0, None, "2026-10-01T00:00Z"),
               ("g1", "p2", "availability", "p_active", 0.0, None, "2026-10-01T00:00Z")])
    keys = pd.DataFrame({"row": [0], "game_id": ["g1"], "subject": ["p1"], "decision_at": pd.to_datetime(["2026-10-30T22:00Z"], utc=True)})
    r = ex.asof(keys, S, ["p_active"])
    assert r.empty or "p_active" not in r or pd.isna(r.reindex([0]).loc[0, "p_active"])


def test_only_observed_and_derived_features_are_usable():
    assert set(ex.USABLE) == {"observed", "derived"}


def test_game_clustered_interval_is_wider_than_naive():
    rng = np.random.default_rng(3)
    games = np.repeat(np.arange(50), 20)
    v = np.repeat(rng.normal(0, 1, 50), 20) + rng.normal(0, 0.1, 1000)  # rows of one game move together
    c = ex.clustered(v, games)
    naive = 1.96 * v.std() / np.sqrt(len(v))
    assert c["lo95"] <= c["mean"] <= c["hi95"]
    assert (c["hi95"] - c["lo95"]) / 2 > 2 * naive
    assert c["lo99"] <= c["lo95"] and c["hi99"] >= c["hi95"]
