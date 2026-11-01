"""Answer-quality and speed evaluation across chat models.

    python apps/monster-gpt/tests/eval_answers.py --models qwen2.5:3b qwen2.5:7b qwen2.5:0.5b

Runs a fixed set of plain-English questions through POST /chat (each in a
fresh conversation, owner "eval"), and reports per model:
  - speed: time to first token, total time, tokens/s
  - judgement: share of drafts the checker flagged (no checker = this rate),
    share of final answers still flagged, rewrite rate, and the issue codes.
Writes apps/monster-gpt/tests/eval_answers.last.json.
"""
from __future__ import annotations

import argparse
import json
import statistics
import time
import urllib.request
from collections import Counter
from pathlib import Path

BASE = "http://localhost:8085"
QUESTIONS = [
    "What are the best bets today?",
    "give me 3 safe bets for tonight, I have $200",
    "Any plus-money props today? I have $100",
    "Make me a 3-leg parlay with $50",
    "best NHL props tonight",
    "aggressive bets today, bankroll $1000",
    "how are your picks doing?",
    "Explain how AI pick grades are capped by track record",
    "What is fractional Kelly and why use it?",
    "give me 5 nba bets",
]


def post(path, body, token=None, timeout=600):
    req = urllib.request.Request(BASE + path, data=json.dumps(body).encode(), method="POST", headers={"content-type": "application/json"})
    if token:
        req.add_header("authorization", f"Bearer {token}")
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read())


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--models", nargs="+", default=["qwen2.5:3b"])
    ap.add_argument("--n", type=int, default=len(QUESTIONS))
    a = ap.parse_args()
    token = post("/session", {"user": "eval"})["token"]
    results = {}
    for model in a.models:
        rows = []
        for q in QUESTIONS[: a.n]:
            t0 = time.time()
            r = post("/chat", {"text": q, "model": model}, token)
            wall = time.time() - t0
            done = next(e for e in r["events"] if e["type"] == "done")
            u = done.get("usage") or {}
            j = u.get("judge") or {}
            rows.append({"q": q, "wall_s": round(wall, 1), "ttft_ms": u.get("ttft_ms"), "tokens_per_s": u.get("tokens_per_s"),
                         "draft_flagged": any(i["serious"] for i in j.get("draft_issues", [])), "final_flagged": bool(j.get("issues")),
                         "revised": j.get("revised", False), "draft_codes": [i["code"] for i in j.get("draft_issues", [])],
                         "final_codes": [i["code"] for i in j.get("issues", [])], "model_used": done.get("model"), "answer": r["answer"][:600]})
            print(f"{model:14} {wall:6.1f}s ttft={u.get('ttft_ms')}ms draft={rows[-1]['draft_codes']} final={rows[-1]['final_codes']}  {q[:40]}")
        n = len(rows)
        ttfts = [r["ttft_ms"] for r in rows if r["ttft_ms"] is not None]
        tps = [r["tokens_per_s"] for r in rows if r["tokens_per_s"]]
        results[model] = {
            "questions": n,
            "median_ttft_s": round(statistics.median(ttfts) / 1000, 2) if ttfts else None,
            "median_total_s": round(statistics.median(r["wall_s"] for r in rows), 1),
            "mean_tokens_per_s": round(statistics.mean(tps), 1) if tps else None,
            "drafts_flagged": f"{sum(r['draft_flagged'] for r in rows)}/{n}",
            "finals_flagged": f"{sum(r['final_flagged'] for r in rows)}/{n}",
            "rewrites": f"{sum(r['revised'] for r in rows)}/{n}",
            "draft_issue_codes": dict(Counter(c for r in rows for c in r["draft_codes"])),
            "final_issue_codes": dict(Counter(c for r in rows for c in r["final_codes"])),
            "rows": rows,
        }
    out = Path(__file__).with_name("eval_answers.last.json")
    out.write_text(json.dumps(results, indent=1), encoding="utf-8")
    print(json.dumps({m: {k: v for k, v in r.items() if k != "rows"} for m, r in results.items()}, indent=1))


if __name__ == "__main__":
    main()
