"""Decision-time feature snapshots for ML research (feature_snapshots, migration 0036).

Every row carries event_at (game start), observed_at, available_at, source and
quality. Experiments may only use a row whose available_at is at or before the
example's decision time (see asof() in experiment.py). Nothing here rebuilds
the past from present-day data: a value with no record of when it was known is
stored as quality 'missing'.

Extractors
  engine_inputs   what the engine knew when it froze a forecast (prediction_ledger
                  is write-once; available_at = frozen_at): per player availability
                  (p_active, injury-report listing), lineup (starter, confirmed
                  lineup), expected minutes / ice time, recent games; per team the
                  players out and the expected minutes they take with them; NHL
                  starting goalie confirmed or projected.          NBA, NHL
  market_horizon  de-vigged consensus of each live example's line 24 / 12 / 6 / 2
                  hours before the start, from the quote history (quarantined
                  feeds left out); 'missing' when no clean two-sided quote existed.
  schedule        rest days and back-to-backs from the schedule (derived; the
                  schedule is public long before the game, available_at = the
                  team's previous game start).                       NBA, NHL

  python scripts/ml/features.py            # all extractors (re-runs add nothing new)
  python scripts/ml/features.py --only market_horizon
"""
import argparse
import json
import os
import sys
from datetime import timedelta

import psycopg

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
import dbenv  # noqa: E402

# research role (migration 0037): reads everything, can only add rows to the research tables
DB = dbenv.url("ML_DATABASE_URL")
EXTRACTOR = "features.py v1"
HORIZONS = (24, 12, 6, 2)

INSERT = ("INSERT INTO feature_snapshots (sport, game_id, subject, feature_set, name, value, value_text, event_at, observed_at, available_at, source, quality, extractor) "
          "VALUES (%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s) ON CONFLICT DO NOTHING")


def dec(a):
    return 1 + a / 100 if a > 0 else 1 + 100 / -a


def novig(a, b):
    x, y = 1 / dec(a), 1 / dec(b)
    return x / (x + y)


def row(sport, game, subject, fset, name, value, event_at, observed_at, available_at, source, quality, text=None):
    if quality == "missing":
        value = None
    return (sport, str(game), str(subject), fset, name, None if value is None else float(value), text, event_at, observed_at, available_at, source, quality, EXTRACTOR)


# ---------------------------------------------------------------- engine inputs

def engine_inputs(cx):
    out = []
    q = ("SELECT sport, game_id, COALESCE(game_start, (SELECT starts_at FROM game_kickoffs k WHERE k.sport=l.sport AND k.game_id=l.game_id)), frozen_at, lineup_mode, "
         "payload->'inputs', payload->>'home', payload->>'away' FROM prediction_ledger l WHERE sport IN ('nba','nhl') AND payload ? 'inputs'")
    for sport, game, start, frozen, mode, inputs, home, away in cx.execute(q):
        src = f"engine_inputs:{mode}"
        teams = inputs.get("teams") or []
        team_out = {}
        for t in teams:
            abbr = t.get("abbr")
            side = "home" if abbr == home else "away" if abbr == away else None
            out_n, out_min, listed = 0, 0.0, 0
            for p in t.get("players") or []:
                pid = p.get("id")
                if pid is None:
                    continue
                status = p.get("status") or ""
                injured = status.startswith("injury report")
                exp_min = p.get("min_mean", p.get("toi_mean"))
                pa = p.get("p_active")
                add = lambda fset, name, v, text=None: out.append(row(sport, game, pid, fset, name, v, start, frozen, frozen, src, "observed" if v is not None or text else "missing", text))
                add("availability", "p_active", pa)
                add("availability", "injury_listed", 1.0 if injured else 0.0)
                add("availability", "status", None, status[:200] if status else None)
                if "starter" in p:
                    add("lineup", "starter", 1.0 if p.get("starter") else 0.0)
                add("lineup", "lineup_confirmed", 1.0 if mode == "confirmed" else 0.0)
                add("lineup", "team", None, f"{abbr}|{side}")
                add("minutes", "exp_minutes", exp_min)
                add("minutes", "games_recent", p.get("games"))
                # a regular (has games, plays minutes) who is not expected to play
                if injured or (pa is not None and pa < 0.5 and (p.get("games") or 0) >= 5 and (exp_min or 0) >= 10):
                    if pa is not None and pa < 0.5:
                        out_n += 1
                        out_min += exp_min or 0
                if injured:
                    listed += 1
            team_out[abbr] = (out_n, out_min, listed, side)
            g = t.get("goalie")
            if g:
                out.append(row(sport, game, f"team:{abbr}", "lineup", "goalie_confirmed", 1.0 if g.get("actual") else 0.0, start, frozen, frozen, src, "observed",
                               (g.get("source") or "")[:200]))
        for abbr, (n, m, listed, side) in team_out.items():
            for name, v in (("out_count", n), ("out_minutes", m), ("injury_listed_count", listed)):
                out.append(row(sport, game, f"team:{abbr}", "availability", name, v, start, frozen, frozen, src, "observed", side))
    return out


# ---------------------------------------------------------------- market horizons

def market_horizons(cx):
    """Consensus of each live example's line at fixed horizons before the start."""
    ex = cx.execute("SELECT DISTINCT sport, game_id, market, subject, line, starts_at FROM ml_examples WHERE source='live'").fetchall()
    games = {(s, g) for s, g, *_ in ex}
    quotes = {}
    for sport, game, market, subject, side, line, book, price, obs in cx.execute(
            "SELECT m.sport, m.game_id, m.market, m.subject, m.side, m.line, m.book, m.price, m.observed_at FROM market_lines m "
            "WHERE (m.sport, m.game_id) IN (SELECT DISTINCT sport, game_id FROM ml_examples WHERE source='live') "
            "AND NOT EXISTS (SELECT 1 FROM feed_quarantine q WHERE q.sport=m.sport AND q.market=m.market AND q.book=m.book) ORDER BY m.observed_at"):
        quotes.setdefault((sport, game, market, subject), []).append((side, line, book, price, obs))
    out = []
    for sport, game, market, subject, line, start in ex:
        qs = quotes.get((sport, game, market, subject), [])
        spread = market.startswith("spread")
        key = f"line:{market}|{subject}|{line}"
        for h in HORIZONS:
            t = start - timedelta(hours=h)
            last = {}
            for side, ql, book, price, obs in qs:
                if obs > t:
                    break
                a = side in ("over", "home")
                want = line if a or not spread else -line
                if ql is not None and ql != want:
                    continue
                last[(book, a)] = price
            ps = [novig(last[(b, True)], last[(b, False)]) for b, a in last if a and (b, False) in last]
            p = sum(ps) / len(ps) if ps else None
            out.append(row(sport, game, key, "market_horizon", f"p_h{h}", p, start, t, t, "market_lines", "observed" if p is not None else "missing"))
            out.append(row(sport, game, key, "market_horizon", f"books_h{h}", len(ps) if ps else None, start, t, t, "market_lines", "observed" if ps else "missing"))
    return out


# ---------------------------------------------------------------- schedule

def schedule(cx):
    out = []
    for sport, table, gid in (("nba", "nba_games_espn", "espn_id"), ("nhl", "nhl_games_api", "game_id")):
        rows = cx.execute(f"SELECT {gid}::text, start_time, home, away FROM {table} WHERE start_time IS NOT NULL AND start_time > now() - interval '400 days' ORDER BY start_time").fetchall()
        prev = {}
        for game, start, home, away in rows:
            for team in (home, away):
                p = prev.get(team)
                if p is not None:
                    rest = (start.date() - p.date()).days
                    out.append(row(sport, game, f"team:{team}", "schedule", "rest_days", rest, start, p, p, f"{table}", "derived"))
                    out.append(row(sport, game, f"team:{team}", "schedule", "back_to_back", 1.0 if rest <= 1 else 0.0, start, p, p, f"{table}", "derived"))
                prev[team] = start
    return out


EXTRACTORS = {"engine_inputs": engine_inputs, "market_horizon": market_horizons, "schedule": schedule}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--only", choices=list(EXTRACTORS))
    ap.add_argument("--dry-run", action="store_true")
    a = ap.parse_args()
    with psycopg.connect(DB) as cx:
        for name, fn in EXTRACTORS.items():
            if a.only and name != a.only:
                continue
            rows = fn(cx)
            missing = sum(1 for r in rows if r[11] == "missing")
            if a.dry_run:
                print(f"{name}: {len(rows)} rows ({missing} missing)")
                continue
            before = cx.execute("SELECT count(*) FROM feature_snapshots").fetchone()[0]
            with cx.cursor() as c:
                c.executemany(INSERT, rows)
            cx.commit()
            after = cx.execute("SELECT count(*) FROM feature_snapshots").fetchone()[0]
            print(f"{name}: {len(rows)} rows built ({missing} missing), {after - before} new")


if __name__ == "__main__":
    sys.exit(main())
