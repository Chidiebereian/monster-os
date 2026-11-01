"""Walk-forward ML experiments against the market (results -> ml_experiments).

Market-aware residual learning: every learned model starts from the market's
log-odds and may only move away from it when features justify it, so the
question each experiment answers is "does Monster know something the market
did not price?", not "can it predict games?".

Models compared on the same out-of-sample rows:
  market         the de-vigged consensus at decision time (live) or the closing
                 line / naive model the backtest compares against
  engine_raw     the engine's own probability
  blend          production AI Picks: logit(market) + w * (logit(engine) - logit(market)),
                 w fitted by Brier on the training folds
  residual_enet  logistic regression with the market as offset, elastic-net penalty
  residual_lgbm  LightGBM with init_score = logit(market)
  residual_xgb   XGBoost with base_margin = logit(market)

Folds are chronological: live data by day (train on every earlier day, test
the next one), backtests by season. Intervals are bootstrap, resampling whole
games. The gate uses 99% intervals because the best of three learned models
is picked.

Targets
  outcome    did the over / home side win?
  movement   (live) did the consensus move toward the over / home side between
             the decision time and the close? (a CLV model, judged separately:
             predicting movement is not predicting the result)

  python scripts/ml/experiment.py --sport nba --markets pa
  python scripts/ml/experiment.py --sport nhl --markets sog --target movement
  python scripts/ml/experiment.py --suite          # the standard research set
"""
import argparse
import hashlib
import json
import os
import subprocess
import sys
import warnings

import numpy as np
import pandas as pd
import psycopg
from scipy.optimize import minimize

warnings.filterwarnings("ignore")
DB = os.environ.get("ML_DATABASE_URL", "postgresql://monster:monster@localhost:5434/monsteros")
CODE = "experiment.py v1.1"
# a training game must have finished this long before the first decision of the test fold
GAME_HOURS = 4
LGBM = {"objective": "binary", "learning_rate": 0.03, "num_leaves": 7, "min_data_in_leaf": 50, "feature_fraction": 0.8, "bagging_fraction": 0.8,
        "bagging_freq": 1, "lambda_l2": 5.0, "verbose": -1, "seed": 7, "deterministic": True, "force_row_wise": True, "num_threads": 1, "rounds": 200}
XGB = {"objective": "binary:logistic", "eta": 0.03, "max_depth": 3, "min_child_weight": 20, "lambda": 5.0, "subsample": 0.8, "colsample_bytree": 0.8,
       "seed": 7, "verbosity": 0, "nthread": 1, "rounds": 200}
ENET = {"l2": 1.0, "l1": 0.5, "standardize": True, "offset": "logit(market)"}
FEATURES = {
    "d_model": "logit(engine probability) - logit(market at decision time)",
    "line": "the line (0 for moneylines)",
    "d_naive": "logit(player's own prior rate over the line) - logit(market); 0 when absent (props, live)",
    "has_naive": "1 when the naive rate exists",
    "d_open": "logit(market at decision) - logit(opening consensus): movement so far (live)",
    "books": "clean books quoting both sides at decision time (live)",
    "hours": "hours from decision to start (live)",
    "hold": "1/best over + 1/best under - 1 across clean books at decision time (live)",
    "m_<market>": "market indicator when several markets are pooled",
}
EPS = 0.005
MIN_TEST = 300
MIN_TEST_GAMES = 30
EV_MIN = 0.02


def logit(p):
    p = np.clip(np.asarray(p, float), EPS, 1 - EPS)
    return np.log(p / (1 - p))


def sigmoid(x):
    return 1 / (1 + np.exp(-x))


# ---------------------------------------------------------------- data

def load(sport, markets, source):
    q = ("SELECT id, sport, engine, engine_version, season, game_id, game_date, kind, market, subject, line, comparison, decision_at, starts_at, "
         "p_model, p_base, features, y FROM ml_examples WHERE sport=%s AND source=%s AND y IS NOT NULL AND p_base IS NOT NULL")
    args = [sport, source]
    if markets:
        q += " AND market = ANY(%s)"
        args.append(markets)
    with psycopg.connect(DB) as cx:
        df = pd.DataFrame(cx.cursor(row_factory=psycopg.rows.dict_row).execute(q, args).fetchall())
    if df.empty:
        return df
    f = pd.json_normalize(df["features"]).set_index(df.index)
    df = pd.concat([df.drop(columns=["features"]), f], axis=1)
    if source == "live":
        # by the game's start date (ET): every row of one game lands in the same fold
        df["fold"] = pd.to_datetime(df["starts_at"], utc=True).dt.tz_convert("America/New_York").dt.date.astype(str)
    else:
        df["fold"] = df["season"].astype(str)
    return df.sort_values(["fold", "game_id"]).reset_index(drop=True)


def design(df, source, markets_seen):
    """Features known at decision time, relative to the market."""
    lm = logit(df["p_base"])
    X = pd.DataFrame({"d_model": logit(df["p_model"]) - lm, "lm": lm, "line": df["line"].fillna(0.0)})
    if source == "live":
        pn = df["p_naive"].astype(float)
        X["d_naive"] = np.where(pn.notna(), logit(pn.fillna(0.5)) - lm, 0.0)
        X["has_naive"] = pn.notna().astype(float)
        X["d_open"] = lm - logit(df["p_open"].fillna(df["p_base"]))
        X["books"] = df["books"].astype(float)
        X["hours"] = df["hours_to_start"].astype(float)
        X["hold"] = 1 / df["best_over"] + 1 / df["best_under"] - 1
    for m in markets_seen[1:]:
        X[f"m_{m}"] = (df["market"] == m).astype(float)
    return X.astype(float), lm


# ---------------------------------------------------------------- models

def fit_blend(lm, dm, y):
    ws = np.linspace(0, 1, 51)
    losses = [np.mean((sigmoid(lm + w * dm) - y) ** 2) for w in ws]
    return float(ws[int(np.argmin(losses))])


def fit_enet(X, off, y, l2=ENET["l2"], l1=ENET["l1"]):
    """Logistic regression with an offset and a smooth elastic-net penalty (per-sample scaled)."""
    mu, sd = X.mean(0), X.std(0) + 1e-9
    Z = (X - mu) / sd
    n = len(y)

    def obj(b):
        z = off + b[0] + Z @ b[1:]
        p = sigmoid(z)
        ll = -np.mean(y * np.log(np.clip(p, 1e-9, 1)) + (1 - y) * np.log(np.clip(1 - p, 1e-9, 1)))
        w = b[1:]
        pen = (l2 * np.sum(w ** 2) + l1 * np.sum(np.sqrt(w ** 2 + 1e-8))) / n
        g = np.concatenate([[np.mean(p - y)], Z.T @ (p - y) / n]) + np.concatenate([[0], (2 * l2 * w + l1 * w / np.sqrt(w ** 2 + 1e-8)) / n])
        return ll + pen, g

    b = minimize(obj, np.zeros(Z.shape[1] + 1), jac=True, method="L-BFGS-B").x
    coef = (b[1:] / sd).round(4).tolist()
    return (lambda Xt, ot: sigmoid(ot + b[0] + ((Xt - mu) / sd) @ b[1:])), coef


def fit_lgbm(X, off, y):
    import lightgbm as lgb
    params = {k: v for k, v in LGBM.items() if k != "rounds"}
    m = lgb.train(params, lgb.Dataset(X, y, init_score=off), num_boost_round=LGBM["rounds"])
    imp = dict(zip(X.columns, m.feature_importance("gain").round(1)))
    return (lambda Xt, ot: sigmoid(ot + m.predict(Xt, raw_score=True))), imp


def fit_xgb(X, off, y):
    import xgboost as xgb
    d = xgb.DMatrix(X, label=y, base_margin=off)
    m = xgb.train({k: v for k, v in XGB.items() if k != "rounds"}, d, num_boost_round=XGB["rounds"])
    imp = m.get_score(importance_type="gain")
    return (lambda Xt, ot: m.predict(xgb.DMatrix(Xt, base_margin=ot))), {k: round(v, 2) for k, v in imp.items()}


# ---------------------------------------------------------------- metrics

def ece(p, y, bins=10):
    b = np.minimum((p * bins).astype(int), bins - 1)
    return float(sum(abs(p[b == k].mean() - y[b == k].mean()) * (b == k).mean() for k in range(bins) if (b == k).any()))


def clustered(values, groups, seed=7, B=1000):
    """Mean and 95% / 99% bootstrap intervals, resampling whole games."""
    s = pd.DataFrame({"v": values, "g": groups}).groupby("g")["v"].agg(["sum", "count"]).to_numpy()
    rng = np.random.default_rng(seed)
    k = len(s)
    idx = rng.integers(0, k, size=(B, k))
    means = s[idx, 0].sum(1) / s[idx, 1].sum(1)
    q = np.quantile(means, [0.005, 0.025, 0.975, 0.995])
    return {"mean": float(np.mean(values)), "lo95": float(q[1]), "hi95": float(q[2]), "lo99": float(q[0]), "hi99": float(q[3])}


def roi(p, df):
    """Flat 1-unit bets on the side the model rates at EV >= EV_MIN at the best price at decision time."""
    po, pu = df["best_over"].to_numpy(), df["best_under"].to_numpy()
    ev_o, ev_u = p * po - 1, (1 - p) * pu - 1
    over = (ev_o >= ev_u) & (ev_o >= EV_MIN)
    under = (ev_u > ev_o) & (ev_u >= EV_MIN)
    y = df["y"].to_numpy()
    prof = np.where(over, np.where(y == 1, po - 1, -1.0), np.where(under, np.where(y == 0, pu - 1, -1.0), np.nan))
    bet = ~np.isnan(prof)
    if bet.sum() == 0:
        return {"bets": 0}
    out = {"bets": int(bet.sum()), "units": float(np.nansum(prof)), "roi": clustered(prof[bet], df["game_id"].to_numpy()[bet])}
    # closing value: did the consensus move toward the side bet?
    if "p_close" in df:
        pc, pd_ = df["p_close"].to_numpy(dtype=float), df["p_base"].to_numpy(dtype=float)
        clv = np.where(over, pc - pd_, pd_ - pc)[bet]
        clv = clv[~np.isnan(clv)]
        if len(clv):
            out["clv_mean"] = float(clv.mean())
            out["clv_positive"] = float((clv > 0).mean())
    return out


def score(name, p, y, df, ref, live):
    r = {"model": name, "n": int(len(y)), "brier": float(np.mean((p - y) ** 2)),
         "logloss": float(-np.mean(y * np.log(np.clip(p, 1e-9, 1)) + (1 - y) * np.log(np.clip(1 - p, 1e-9, 1)))), "ece": ece(p, y)}
    if ref is not None:
        r["brier_vs_market"] = clustered((p - y) ** 2 - (ref - y) ** 2, df["game_id"].to_numpy())
        r["folds_better"] = int(sum(np.mean((p[m] - y[m]) ** 2) < np.mean((ref[m] - y[m]) ** 2) for m in [df["fold"].to_numpy() == f for f in df["fold"].unique()]))
    if live:
        r["betting"] = roi(p, df)
    return r


# ---------------------------------------------------------------- experiment

def run(sport, markets, source="live", target="outcome", name=None, save=True):
    df = load(sport, markets, source)
    label = name or f"{sport} {'+'.join(markets) if markets else 'all'} {source} {target}"
    if df.empty:
        print(f"{label}: no data")
        return None
    live = source == "live"
    if target == "movement":
        if not live:
            raise SystemExit("movement needs live data")
        df = df[df["p_close"].notna() & (df["p_close"] != df["p_base"])].reset_index(drop=True)
        df["y"] = (df["p_close"] > df["p_base"]).astype(float)
    markets_seen = sorted(df["market"].unique())
    X, lm = design(df, source, markets_seen)
    y = df["y"].to_numpy(float)
    folds = list(dict.fromkeys(df["fold"]))
    min_train = 4 if live else 1  # backtests: train on every earlier season (NBA closing lines cover two)
    preds = {k: np.full(len(df), np.nan) for k in ["market", "engine_raw", "blend", "residual_enet", "residual_lgbm", "residual_xgb"]}
    info = {"blend_w": [], "enet_coef": None, "lgbm_gain": None, "xgb_gain": None}
    test_mask = np.zeros(len(df), bool)
    train_ranges = []
    for i, f in enumerate(folds):
        if i < min_train:
            continue
        te = (df["fold"] == f).to_numpy()
        tr = df["fold"].isin(folds[:i]).to_numpy()
        if live:
            first_decision = pd.to_datetime(df.loc[te, "decision_at"], utc=True).min()
            done = pd.to_datetime(df["starts_at"], utc=True) + pd.Timedelta(hours=GAME_HOURS) <= first_decision
            tr &= done.to_numpy()
            assert not set(df.loc[tr, "game_id"]) & set(df.loc[te, "game_id"]), "a game is in both train and test"
        train_ranges.append({"test": f, "train_from": str(df.loc[tr, "fold"].min()), "train_to": str(df.loc[tr, "fold"].max()), "train_rows": int(tr.sum()),
                             "train_games": int(df.loc[tr, "game_id"].nunique()), "test_rows": int(te.sum()), "test_games": int(df.loc[te, "game_id"].nunique())})
        if tr.sum() < 200 or te.sum() == 0:
            continue
        test_mask |= te
        if target == "movement":
            base = np.full(te.sum(), y[tr].mean())
            preds["market"][te] = base  # "no information": the training base rate
            off_tr, off_te = np.full(tr.sum(), logit(y[tr].mean())), np.full(te.sum(), logit(y[tr].mean()))
        else:
            preds["market"][te] = df.loc[te, "p_base"]
            preds["engine_raw"][te] = df.loc[te, "p_model"]
            dm = X["d_model"].to_numpy()
            w = fit_blend(lm[tr], dm[tr], y[tr])
            info["blend_w"].append(w)
            preds["blend"][te] = sigmoid(lm[te] + w * dm[te])
            off_tr, off_te = lm[tr], lm[te]
        Xtr, Xte = X[tr].drop(columns=["lm"]), X[te].drop(columns=["lm"])
        fe, coef = fit_enet(Xtr.to_numpy(), off_tr, y[tr])
        info["enet_coef"] = dict(zip(Xtr.columns, coef))
        preds["residual_enet"][te] = fe(Xte.to_numpy(), off_te)
        fl, info["lgbm_gain"] = fit_lgbm(Xtr, off_tr, y[tr])
        preds["residual_lgbm"][te] = fl(Xte, off_te)
        fx, info["xgb_gain"] = fit_xgb(Xtr, off_tr, y[tr])
        preds["residual_xgb"][te] = fx(Xte, off_te)

    for k, p in preds.items():
        pt = p[test_mask]
        assert np.isnan(pt).all() or not np.isnan(pt).any(), f"{k} is missing predictions on some test rows"
    T = df[test_mask].reset_index(drop=True)
    yt = y[test_mask]
    ref = preds["market"][test_mask]
    results = []
    for k, p in preds.items():
        pt = p[test_mask]
        if np.isnan(pt).all():
            continue
        results.append(score(k, pt, yt, T, None if k == "market" else ref, live and target == "outcome"))
    n_test, games = int(test_mask.sum()), int(T["game_id"].nunique()) if len(T) else 0
    learned = [r for r in results if r["model"].startswith("residual_")]
    best = min(learned, key=lambda r: r["brier"]) if learned else None
    mk = next((r for r in results if r["model"] == "market"), None)
    if best is None or n_test < MIN_TEST or games < MIN_TEST_GAMES:
        verdict, why = "insufficient_data", f"{n_test} out-of-sample rows over {games} games (need {MIN_TEST} and {MIN_TEST_GAMES})"
    else:
        d = best["brier_vs_market"]
        folds_used = len(T["fold"].unique())
        calibrated = best["ece"] <= mk["ece"] + 0.01
        if d["hi99"] < 0 and calibrated and best["folds_better"] >= 0.6 * folds_used:
            verdict, why = "shadow_candidate", "beats the market out of sample (99% interval below 0), calibrated, better in most folds"
        elif d["lo99"] > 0:
            verdict, why = "rejected", "worse than the market out of sample (99% interval above 0)"
        else:
            verdict, why = "inconclusive", f"Brier vs market {d['mean']:+.4f} (99% interval {d['lo99']:+.4f} to {d['hi99']:+.4f}) includes 0"
    gate = {"rule": "best residual model: 99% game-bootstrap interval of the Brier difference vs market below 0, calibration error within 1 point of the market's, "
                    f"better in at least 60% of folds, at least {MIN_TEST} test rows and {MIN_TEST_GAMES} games; shadow only, never auto-promoted",
            "why": why, "best_model": best["model"] if best else None,
            "caveats": ["selection: the best of 3 learned models is judged, hence 99% intervals; a suite runs about 15 experiments and is not "
                        "family-wise corrected, so one pass is never proof",
                        "backtest game lines decide at the close (the closing line is the market baseline and the offset); no ROI is claimed for them"
                        if not live else "live: market = de-vigged consensus at decision time (about 2 h before start); the close is used only for CLV and the movement target",
                        "pushes and voids have no label and are left out, including from ROI"]}
    payload = {"results": results, "info": {"blend_w_by_fold": info["blend_w"], "enet_coef_last_fold": info["enet_coef"],
               "lgbm_gain_last_fold": info["lgbm_gain"], "xgb_gain_last_fold": info["xgb_gain"], "features": list(X.columns.drop("lm")),
               "folds": [f for f in folds if (df["fold"] == f).any()], "test_folds": sorted(T["fold"].unique().tolist()), "test_games": games,
               "ev_min": EV_MIN, "splits": train_ranges, "games": int(df["game_id"].nunique()), "test_rows": n_test,
               "feature_definitions": {k: v for k, v in FEATURES.items() if k in X.columns or k == "m_<market>"},
               "config": {"lgbm": LGBM, "xgb": XGB, "enet": ENET, "blend": "w in [0,1] step 0.02 by Brier on training folds", "min_train_folds": min_train,
                          "game_hours": GAME_HOURS if live else None, "bootstrap": "1000 resamples of games, seed 7", "novig": "multiplicative (proportional)"}}}
    h = hashlib.sha256(json.dumps(sorted(zip(df["id"].tolist(), y.tolist()))).encode()).hexdigest()[:16]
    try:
        rev = subprocess.run(["git", "rev-parse", "--short", "HEAD"], capture_output=True, text=True).stdout.strip()
    except Exception:
        rev = "?"
    print_summary(label, results, verdict, why, n_test, games)
    if save:
        with psycopg.connect(DB) as cx:
            same = cx.execute("SELECT id FROM ml_experiments WHERE name=%s AND dataset_hash=%s AND code_version=%s", (label, h, f"{CODE} @ {rev}")).fetchone()
            if same:
                print(f"   unchanged: already recorded as experiment {same[0]} (same data and code)")
                return verdict
            cx.execute("INSERT INTO ml_experiments (name, sport, markets, source, target, dataset_hash, n, n_test, folds, best_model, verdict, results, gate, code_version) "
                       "VALUES (%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s::jsonb,%s::jsonb,%s)",
                       (label, sport, markets_seen, source, target, h, int(len(df)), n_test, len(T["fold"].unique()) if len(T) else 0,
                        gate["best_model"], verdict, json.dumps(payload, default=float), json.dumps(gate), f"{CODE} @ {rev}"))
            cx.commit()
    return verdict


def print_summary(label, results, verdict, why, n_test, games):
    print(f"\n== {label}: {verdict} ({n_test} test rows, {games} games) - {why}")
    for r in results:
        d = r.get("brier_vs_market")
        b = r.get("betting") or {}
        roi_s = f" bets {b['bets']} ROI {b['roi']['mean']:+.3f} [{b['roi']['lo95']:+.3f},{b['roi']['hi95']:+.3f}] CLV {b.get('clv_mean', float('nan')):+.4f}" if b.get("bets") else ""
        print(f"  {r['model']:14} brier {r['brier']:.4f} ece {r['ece']:.3f}" + (f"  vs mkt {d['mean']:+.4f} [{d['lo95']:+.4f},{d['hi95']:+.4f}]" if d else "") + roi_s)


SUITE = [
    # the props the scorecard flagged as promising, then each sport pooled
    ("nba", ["pa"], "live", "outcome"), ("nba", ["pra"], "live", "outcome"), ("nhl", ["sog"], "live", "outcome"),
    ("nba", ["spread"], "live", "outcome"), ("nba", ["total"], "live", "outcome"),
    ("nba", None, "live", "outcome"), ("nhl", None, "live", "outcome"),
    ("nba", None, "live", "movement"), ("nhl", None, "live", "movement"),
    # whole seasons against closing lines, once the backtests have stored their rows
    ("nfl", ["spread"], "backtest", "outcome"), ("nfl", ["total"], "backtest", "outcome"), ("nfl", ["moneyline"], "backtest", "outcome"),
    ("nba", ["total"], "backtest", "outcome"), ("nba", ["spread"], "backtest", "outcome"), ("nba", ["moneyline"], "backtest", "outcome"),
]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--sport")
    ap.add_argument("--markets", help="comma list; omit for every market of the sport")
    ap.add_argument("--source", default="live", choices=["live", "backtest"])
    ap.add_argument("--target", default="outcome", choices=["outcome", "movement"])
    ap.add_argument("--suite", action="store_true")
    ap.add_argument("--no-save", action="store_true")
    a = ap.parse_args()
    if a.suite:
        for sport, mk, src, tgt in SUITE:
            run(sport, mk, src, tgt, save=not a.no_save)
        return
    if not a.sport:
        ap.error("--sport or --suite")
    run(a.sport, a.markets.split(",") if a.markets else None, a.source, a.target, save=not a.no_save)


if __name__ == "__main__":
    sys.exit(main())
