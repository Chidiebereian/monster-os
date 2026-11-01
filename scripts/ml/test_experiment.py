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


def test_game_clustered_interval_is_wider_than_naive():
    rng = np.random.default_rng(3)
    games = np.repeat(np.arange(50), 20)
    v = np.repeat(rng.normal(0, 1, 50), 20) + rng.normal(0, 0.1, 1000)  # rows of one game move together
    c = ex.clustered(v, games)
    naive = 1.96 * v.std() / np.sqrt(len(v))
    assert c["lo95"] <= c["mean"] <= c["hi95"]
    assert (c["hi95"] - c["lo95"]) / 2 > 2 * naive
    assert c["lo99"] <= c["lo95"] and c["hi99"] >= c["hi95"]
