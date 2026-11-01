"""Resumable, bounded research backtests (projection-service research mode).

Calls POST /picks/evidence/run?mode=research&max_games=N for one engine, batch
after batch, until every backtest game is in ml_examples. Each game is
committed as soon as it is evaluated, and the server skips games already
stored, so stopping at any point loses nothing: the next run resumes.

Research mode has no pick_evidence code path, so AI Picks tiers cannot move.

Before every batch the free memory is checked; below --min-free-gb the runner
stops cleanly (exit 3) instead of pushing the machine into swap. Progress goes
to a checkpoint file and a log under %LOCALAPPDATA%/MonsterOS/logs/research
(outside the repository).

  python scripts/ml/backtest_runner.py --engine nba
  python scripts/ml/backtest_runner.py --engine nfl --batch 10 --min-free-gb 2

Exit codes: 0 complete, 2 service error, 3 paused for low memory, 4 batch limit reached.
"""
import argparse
import datetime as dt
import json
import os
import sys
import time
import urllib.error
import urllib.request

import psutil

BASE = os.environ.get("PROJECTION_URL", "http://localhost:8121")
LOGDIR = os.path.join(os.environ.get("LOCALAPPDATA", os.path.expanduser("~")), "MonsterOS", "logs", "research")


def log(engine, msg):
    os.makedirs(LOGDIR, exist_ok=True)
    line = f"{dt.datetime.now().isoformat(timespec='seconds')} [{engine}] {msg}"
    print(line, flush=True)
    with open(os.path.join(LOGDIR, f"backtest_{engine}.log"), "a", encoding="utf-8") as f:
        f.write(line + "\n")


def checkpoint(engine, state):
    os.makedirs(LOGDIR, exist_ok=True)
    with open(os.path.join(LOGDIR, f"backtest_{engine}.json"), "w", encoding="utf-8") as f:
        json.dump(state, f, indent=1, default=str)


def batch(engine, n, timeout):
    req = urllib.request.Request(f"{BASE}/picks/evidence/run?engine={engine}&mode=research&max_games={n}", method="POST")
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.load(r)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--engine", required=True)
    ap.add_argument("--batch", type=int, default=25, help="games per request (1-500)")
    ap.add_argument("--min-free-gb", type=float, default=1.5)
    ap.add_argument("--max-batches", type=int, default=1000)
    ap.add_argument("--timeout", type=int, default=3600, help="seconds per request")
    a = ap.parse_args()
    state = {"engine": a.engine, "started": dt.datetime.now(), "batches": 0, "games_done": 0}
    for i in range(a.max_batches):
        free = psutil.virtual_memory().available / 1e9
        if free < a.min_free_gb:
            log(a.engine, f"paused: {free:.1f} GB free < {a.min_free_gb} GB; nothing lost, rerun to resume")
            state["status"] = "paused_low_memory"
            checkpoint(a.engine, state)
            return 3
        t0 = time.time()
        try:
            r = batch(a.engine, a.batch, a.timeout)
        except urllib.error.HTTPError as e:
            log(a.engine, f"service refused the batch: {e.code} {e.read().decode(errors='replace')[:300]}")
            state["status"] = f"error_{e.code}"
            checkpoint(a.engine, state)
            return 2
        except Exception as e:  # service down, timeout: stored games are safe
            log(a.engine, f"batch failed: {e}; stored games are kept, rerun to resume")
            state["status"] = "error"
            checkpoint(a.engine, state)
            return 2
        if r.get("mode") != "research" or r.get("evidence_rows_written", 1) != 0:
            log(a.engine, f"refusing to continue: response is not a research batch: {json.dumps(r)[:300]}")
            return 2
        state["batches"] += 1
        state["games_done"] += r["games_done"]
        state.update({"last": r, "status": "running", "updated": dt.datetime.now()})
        checkpoint(a.engine, state)
        log(a.engine, f"batch {state['batches']}: +{r['games_done']} games in {time.time() - t0:.0f}s, "
                      f"{r['games_skipped_existing']} already stored, {r['games_remaining']} remaining, free {free:.1f} GB")
        if r["complete"]:
            state["status"] = "complete"
            checkpoint(a.engine, state)
            log(a.engine, "complete")
            return 0
        if r["games_done"] == 0 and r["games_without_forecast"] == 0:
            log(a.engine, "no progress in this batch; stopping")
            return 2
    state["status"] = "batch_limit"
    checkpoint(a.engine, state)
    return 4


if __name__ == "__main__":
    sys.exit(main())
