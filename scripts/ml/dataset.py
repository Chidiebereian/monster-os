"""Snapshot settled live lines into the immutable ML dataset (ml_examples, source 'live').

For every line AI Picks priced and saw settled (pick_line_evals), the features
are rebuilt from the full quote history (market_lines) as of the decision time
(the last pricing before the game), leaving out quarantined odds feeds:

  p_dec    de-vigged consensus at decision time (the market you could bet into)
  p_open   de-vigged consensus at the first quote of that line
  p_close  de-vigged consensus at the last quote before the start
  best_over / best_under   best decimal price at decision time, any clean book
  books, hours_to_start, p_naive (player's own prior rate, props)

Rows are only ever inserted (the table refuses updates), one per engine
version x game x market x subject x line, so re-running is safe and the
dataset only grows.

  python scripts/ml/dataset.py            # snapshot everything settled
  python scripts/ml/dataset.py --dry-run  # count only
"""
import argparse
import json
import os
import sys

import psycopg

DB = os.environ.get("ML_DATABASE_URL", "postgresql://monster:monster@localhost:5434/monsteros")

# One row per settled eval with features from the quote history, clean books only.
SQL = r"""
WITH ev AS (
    SELECT e.engine, e.engine_version, e.sport, e.game_id, e.market, e.subject, e.line, e.starts_at, e.observed_at AS decision_at,
           e.p_model, e.p_naive, e.p_market AS p_eval, e.books, e.y
    FROM pick_line_evals e
    WHERE e.y IS NOT NULL AND e.observed_at < e.starts_at
),
q AS (
    -- every quote of the same selection and line, per book, before the start
    SELECT ev.engine, ev.game_id, ev.market, ev.subject, ev.line, m.book,
           CASE WHEN m.side IN ('over', 'home') THEN 'a' ELSE 'b' END AS s, m.price, m.observed_at, ev.decision_at
    FROM ev JOIN market_lines m
      ON m.sport = ev.sport AND m.game_id = ev.game_id AND m.market = ev.market AND m.subject = ev.subject
     AND m.observed_at < ev.starts_at
     AND (m.line IS NULL OR m.line = CASE WHEN m.side IN ('away') AND ev.market LIKE 'spread%%' THEN -ev.line ELSE ev.line END)
    WHERE NOT EXISTS (SELECT 1 FROM feed_quarantine fq WHERE fq.sport = ev.sport AND fq.market = ev.market AND fq.book = m.book)
),
pb AS (
    -- per book: opening, decision-time and closing price of each side
    SELECT engine, game_id, market, subject, line, book,
           (array_agg(price ORDER BY observed_at) FILTER (WHERE s = 'a'))[1] AS a_open,
           (array_agg(price ORDER BY observed_at) FILTER (WHERE s = 'b'))[1] AS b_open,
           (array_agg(price ORDER BY observed_at DESC) FILTER (WHERE s = 'a' AND observed_at <= decision_at))[1] AS a_dec,
           (array_agg(price ORDER BY observed_at DESC) FILTER (WHERE s = 'b' AND observed_at <= decision_at))[1] AS b_dec,
           (array_agg(price ORDER BY observed_at DESC) FILTER (WHERE s = 'a'))[1] AS a_close,
           (array_agg(price ORDER BY observed_at DESC) FILTER (WHERE s = 'b'))[1] AS b_close,
           min(observed_at) AS first_seen
    FROM q GROUP BY 1,2,3,4,5,6
)
SELECT ev.*, json_agg(json_build_object('book', pb.book, 'a_open', pb.a_open, 'b_open', pb.b_open, 'a_dec', pb.a_dec, 'b_dec', pb.b_dec,
                                        'a_close', pb.a_close, 'b_close', pb.b_close, 'first', pb.first_seen)) FILTER (WHERE pb.book IS NOT NULL) AS books_json
FROM ev LEFT JOIN pb USING (engine, game_id, market, subject, line)
GROUP BY ev.engine, ev.engine_version, ev.sport, ev.game_id, ev.market, ev.subject, ev.line, ev.starts_at, ev.decision_at, ev.p_model, ev.p_naive, ev.p_eval, ev.books, ev.y
"""


def dec(a):
    return 1 + a / 100 if a > 0 else 1 + 100 / -a


def novig(a, b):
    x, y = 1 / dec(a), 1 / dec(b)
    return x / (x + y)


def consensus(books, ka, kb):
    ps = [novig(b[ka], b[kb]) for b in books if b.get(ka) is not None and b.get(kb) is not None]
    return (sum(ps) / len(ps), len(ps)) if ps else (None, 0)


def features(r):
    books = r["books_json"] or []
    p_dec, n_dec = consensus(books, "a_dec", "b_dec")
    p_open, _ = consensus(books, "a_open", "b_open")
    p_close, _ = consensus(books, "a_close", "b_close")
    best_a = max((dec(b["a_dec"]) for b in books if b.get("a_dec") is not None), default=None)
    best_b = max((dec(b["b_dec"]) for b in books if b.get("b_dec") is not None), default=None)
    hours = (r["starts_at"] - r["decision_at"]).total_seconds() / 3600
    return p_dec, {
        "p_open": p_open, "p_close": p_close, "p_eval": r["p_eval"], "p_naive": r["p_naive"],
        "best_over": best_a, "best_under": best_b, "books": n_dec, "hours_to_start": round(hours, 3),
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dry-run", action="store_true")
    a = ap.parse_args()
    with psycopg.connect(DB) as cx:
        cur = cx.cursor(row_factory=psycopg.rows.dict_row)
        cur.execute(SQL)
        rows = cur.fetchall()
        out, skipped = [], 0
        for r in rows:
            p_dec, f = features(r)
            # a line with no clean two-sided quote at decision time has no market to learn against
            if p_dec is None or f["best_over"] is None or f["best_under"] is None:
                skipped += 1
                continue
            sport = r["sport"]
            kind = "game" if r["subject"] == "game" else "prop"
            out.append((sport, r["engine"], r["engine_version"], r["game_id"], r["starts_at"].date(), kind, r["market"], r["subject"], r["line"],
                        r["decision_at"], r["starts_at"], r["p_model"], p_dec, json.dumps(f), r["y"]))
        print(f"settled evals {len(rows)}, usable {len(out)}, skipped (no clean two-sided quote) {skipped}")
        if a.dry_run:
            return
        before = cx.execute("SELECT count(*) FROM ml_examples WHERE source='live'").fetchone()[0]
        with cx.cursor() as c:
            c.executemany(
                "INSERT INTO ml_examples (source, sport, engine, engine_version, game_id, game_date, kind, market, subject, line, comparison, "
                "decision_at, starts_at, p_model, p_base, features, y) VALUES ('live', %s,%s,%s,%s,%s,%s,%s,%s,%s,'market',%s,%s,%s,%s,%s::jsonb,%s) "
                "ON CONFLICT DO NOTHING", out)
        cx.commit()
        after = cx.execute("SELECT count(*) FROM ml_examples WHERE source='live'").fetchone()[0]
        print(f"inserted {after - before} new live examples (total {after})")


if __name__ == "__main__":
    sys.exit(main())
