"""Golden-question harness for MonsterGPT v2 (docs/architecture/monster-gpt-v2.md, 11).

  python apps/monster-gpt/tests/golden.py --check     # validate golden.json, compute every truth (read-only)
  python apps/monster-gpt/tests/golden.py --run       # ask every question, grade, write a report

--run starts its own monster-gpt (target/debug/monster-gpt.exe) on port 8185 against a
scratch database (monsteros_golden: every migration, plus a copy of the RAG index), so no
conversation or tool-audit row is written to production. Its tools read the live
projection-service, as the real chat does. The report goes to
%LOCALAPPDATA%/MonsterOS/reports/golden_<time>.json.

Grading per question (all must hold):
  tools    every listed tool was called (status frames, stage "tool")
  facts    every {n} template, filled from the truth row, appears in the answer;
           a team code also passes as any of its aliases; numbers match as written or
           to the precision written in the truth
  must_any at least one pattern matches
  forbid   no pattern matches
  abstain  the answer says it could not verify, and holds no number the question lacks
  claims   (questions with a truth query) every expected fact sits in a sentence the
           verifier marked verified or cross_verified; an abstain answer has no verified sentence
A question with "turns" is a conversation: every turn is sent in order in one
conversation, and the checks apply to the last answer (tools: called on any turn).
"""
import argparse
import datetime as dt
import json
import os
import re
import subprocess
import sys
import time
import urllib.error
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
sys.path.insert(0, os.path.join(ROOT, "scripts"))
import dbenv  # noqa: E402

GOLDEN = os.path.join(HERE, "golden.json")
DB = "monsteros_golden"
PORT = 8185
BASE = f"http://localhost:{PORT}"
REPORTS = os.path.join(os.environ.get("LOCALAPPDATA", os.path.expanduser("~")), "MonsterOS", "reports")
ADMIN = ["docker", "exec", "monster-os-postgres-1", "psql", "-U", "monster", "-d", "postgres", "-q", "-c"]
ABSTAIN = re.compile(r"(?i)((could not|couldn't|cannot|can't|unable to|not able to|don't have|do not have)[^.]{0,80}(verify|find|confirm|data|record|information)"
                     r"|no (relevant |stored |verified )?(information|data|record)s? (is |are )?(available|found|stored)|not available in)")
WORKFLOWS = "ABCDEFGHI"  # H: multi-turn (follow-ups, clarifications); I: bets answered by the language model


def load():
    g = json.load(open(GOLDEN, encoding="utf-8"))
    ids = set()
    for q in g["questions"]:
        assert q["id"] not in ids, f"duplicate id {q['id']}"
        ids.add(q["id"])
        assert q["workflow"] in WORKFLOWS and q["text"].strip(), q["id"]
        assert isinstance(q.get("tools", []), list) and isinstance(q.get("facts", []), list), q["id"]
        assert not q.get("facts") or q.get("truth") or all("{" not in f for f in q["facts"]), f"{q['id']}: templates need a truth query"
        assert not (q.get("truth") or "").lstrip().upper().startswith(("INSERT", "UPDATE", "DELETE", "DROP", "ALTER", "CREATE")), q["id"]
        for p in q.get("forbid", []) + q.get("must_any", []):
            re.compile(p)
    for q in g["questions"]:
        assert isinstance(q.get("turns", [q["text"]]), list) and q.get("turns", [q["text"]])[0] == q["text"], f"{q['id']}: turns must start with text"
    missing = [w for w in WORKFLOWS if not any(q["workflow"] == w for q in g["questions"])]
    assert not missing, f"no question for workflow(s) {missing}"
    return g


def truths(g):
    """Every truth row, from production, read-only."""
    import psycopg
    out = {}
    with psycopg.connect(dbenv.url("APP_DATABASE_URL")) as cx:
        cx.read_only = True
        for q in g["questions"]:
            if q.get("truth"):
                row = cx.execute(q["truth"]).fetchone()
                assert row is not None and all(v is not None for v in row), f"{q['id']}: truth query returned no complete row"
                out[q["id"]] = [str(v) for v in row]
    return out


def expected(q, row, aliases):
    """Each fact as a list of acceptable strings."""
    exp = []
    for f in q.get("facts", []):
        s = f.format(*row) if row else f
        alts = [s] + aliases.get(s, [])
        if re.fullmatch(r"-?\d+\.\d+", s):            # 27.40 also passes as 27.4
            alts.append(s.rstrip("0").rstrip("."))
        exp.append(alts)
    return exp


def grade(q, answer, events, row, aliases):
    called = {e.get("name") for e in events if e.get("type") == "status" and e.get("stage") == "tool"}
    fails = []
    for t in q.get("tools", []):
        if t not in called:
            fails.append(f"tool {t} not called (called: {sorted(x for x in called if x)})")
    for alts in expected(q, row, aliases):
        if not any(re.search(r"(?<![\d.])" + re.escape(a) + r"(?![\d])", answer) if re.match(r"-?\d", a) else a.lower() in answer.lower() for a in alts):
            fails.append(f"fact missing: {' / '.join(alts)}")
    if q.get("must_any") and not any(re.search(p, answer) for p in q["must_any"]):
        fails.append(f"none of {q['must_any']}")
    for p in q.get("forbid", []):
        if re.search(p, answer):
            fails.append(f"forbidden /{p}/")
    claims = [c for e in events if e.get("type") == "claims" for c in (e.get("claims") or [])]
    last_claims = next((e.get("claims") or [] for e in reversed(events) if e.get("type") == "claims"), [])
    if q.get("truth"):
        verified = " ".join(c["text"] for c in last_claims if c["status"] in ("verified", "cross_verified"))
        for alts in expected(q, row, aliases):
            if any(a.lower() in answer.lower() for a in alts) and not any(a.lower() in verified.lower() for a in alts):
                fails.append(f"fact {alts[0]} is in the answer but not in a verified sentence")
    # only when the language model wrote the answer (with no open picks the reply is built in code)
    modeled = any(e.get("type") == "done" and e.get("model") for e in events)
    if q.get("statuses") and modeled and not any(c["status"] in q["statuses"] for c in last_claims):
        fails.append(f"no sentence marked {' or '.join(q['statuses'])} (got {sorted({c['status'] for c in last_claims})})")
    if q.get("abstain") and any(c["status"] in ("verified", "cross_verified") for c in last_claims):
        fails.append("an unverifiable answer has a verified sentence")
    if q.get("abstain"):
        if not ABSTAIN.search(answer):
            fails.append("did not say it could not verify")
        extra = set(re.findall(r"\d[\d,.]*", answer)) - set(re.findall(r"\d[\d,.]*", q["text"]))
        if extra:
            fails.append(f"numbers in an unverifiable answer: {sorted(extra)[:5]}")
    return fails, sorted(x for x in called if x), [c["status"] for c in last_claims],         [{"status": c["status"], "text": c["text"][:200], "issues": c.get("issues", [])} for c in last_claims if c["status"] not in ("verified", "cross_verified")]


# ------------------------------------------------------------------ scratch instance

def scratch_db():
    import psycopg
    subprocess.run(ADMIN + [f"DROP DATABASE IF EXISTS {DB} WITH (FORCE)"], capture_output=True)
    subprocess.run(ADMIN + [f"CREATE DATABASE {DB}"], check=True, capture_output=True)
    mig = os.path.join(ROOT, "infrastructure", "postgres", "migrations")
    for f in sorted(x for x in os.listdir(mig) if x.endswith(".sql")):
        sql = open(os.path.join(mig, f), encoding="utf-8").read().replace("ON DATABASE monsteros TO", f"ON DATABASE {DB} TO")
        r = subprocess.run(["docker", "exec", "-i", "monster-os-postgres-1", "psql", "-U", "monster", "-d", DB, "-v", "ON_ERROR_STOP=1", "-q"],
                           input=sql, text=True, capture_output=True)
        if r.returncode:
            raise SystemExit(f"{f}: {r.stderr[:300]}")
    owner = dbenv.url("OPS_DATABASE_URL")
    with psycopg.connect(owner) as src, psycopg.connect(owner.rsplit("/", 1)[0] + "/" + DB, autocommit=True) as dst:
        for table, cols in (("rag_docs", "doc_id,title,kind,hash,access,chunks,indexed_at"),
                            ("rag_chunks", "id,doc_id,chunk_ix,heading,text,sport,access,suspicious,embedding,embed_model")):
            rows = src.execute(f"SELECT {cols} FROM {table}").fetchall()
            with dst.cursor() as cur:
                cur.executemany(f"INSERT INTO {table} ({cols}) VALUES ({','.join(['%s'] * len(cols.split(',')))})", rows)


def start():
    exe = os.path.join(ROOT, "target", "debug", "monster-gpt.exe")
    if not os.path.exists(exe):
        raise SystemExit("build monster-gpt first: cargo build -p monster-gpt")
    env = dict(os.environ, PORT=str(PORT), DATABASE_URL=dbenv.url("APP_DATABASE_URL").rsplit("/", 1)[0] + "/" + DB)
    for k in ("LLM_BASE_URL", "LLM_MODEL", "LLM_PROVIDER", "LLM_NUM_CTX", "LLM_MAX_TOKENS", "OLLAMA_URL", "RAG_EMBED_MODEL"):
        v = dbenv.read_env_file(k)
        if v and k not in os.environ:
            env[k] = v
    log = open(os.path.join(REPORTS, "golden_instance.log"), "w")
    proc = subprocess.Popen([exe], cwd=ROOT, env=env, stdout=log, stderr=subprocess.STDOUT)
    for _ in range(120):
        try:
            if urllib.request.urlopen(BASE + "/healthz", timeout=2).status == 200:
                return proc
        except OSError:
            time.sleep(1)
    proc.kill()
    raise SystemExit("scratch monster-gpt did not become healthy; see golden_instance.log")


def post(path, body, tok=None, timeout=300):
    req = urllib.request.Request(BASE + path, method="POST", data=json.dumps(body).encode())
    req.add_header("content-type", "application/json")
    if tok:
        req.add_header("authorization", f"Bearer {tok}")
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read())


def run(g, rows):
    os.makedirs(REPORTS, exist_ok=True)
    scratch_db()
    proc = start()
    results, stored = [], {}
    try:
        tok = post("/session", {"user": "golden"})["token"]
        for q in g["questions"]:
            t0 = time.time()
            answer, events, conv = "", [], None
            try:
                for turn in q.get("turns", [q["text"]]):
                    r = post("/chat", {"text": turn, **({"conversation_id": conv} if conv else {})}, tok)
                    answer = r.get("answer", "")
                    events += r.get("events", [])
                    conv = conv or next((e.get("conversation_id") for e in r.get("events", []) if e.get("type") == "ack"), None)
            except (urllib.error.URLError, TimeoutError) as e:
                answer, events = "", events + [{"type": "error", "message": str(e)}]
            fails, called, statuses, flagged = grade(q, answer, events, rows.get(q["id"]), g.get("aliases", {}))
            results.append({"id": q["id"], "workflow": q["workflow"], "pass": not fails, "fails": fails, "tools_called": called, "claims": statuses, "flagged": flagged,
                            "expected": rows.get(q["id"]), "answer": answer, "seconds": round(time.time() - t0, 1)})
            print(f"{'PASS' if not fails else 'FAIL'}  {q['id']}  {round(time.time() - t0, 1):>5}s  {'; '.join(fails)[:160]}", flush=True)
        import psycopg
        with psycopg.connect(dbenv.url("OPS_DATABASE_URL").rsplit("/", 1)[0] + "/" + DB) as cx:
            stored = dict(cx.execute("SELECT status, count(*) FROM gpt_claims GROUP BY 1").fetchall())
    finally:
        proc.kill()
        subprocess.run(ADMIN + [f"DROP DATABASE IF EXISTS {DB} WITH (FORCE)"], capture_output=True)
    by = {w: [r for r in results if r["workflow"] == w] for w in WORKFLOWS}
    summary = {"passed": sum(r["pass"] for r in results), "total": len(results),
               "by_workflow": {w: f"{sum(r['pass'] for r in v)}/{len(v)}" for w, v in by.items()},
               "tool_selection": round(sum(all(t in r["tools_called"] for t in q.get("tools", [])) for r, q in zip(results, g["questions"])) / len(results), 3),
               "abstain_violations": sum(1 for r, q in zip(results, g["questions"]) if q.get("abstain") and not r["pass"]),
               "claim_statuses": {k: sum(r["claims"].count(k) for r in results) for k in sorted({x for r in results for x in r["claims"]})},
               "claims_stored": stored}
    path = os.path.join(REPORTS, f"golden_{dt.datetime.now():%Y%m%d_%H%M%S}.json")
    json.dump({"at": dt.datetime.now(dt.timezone.utc).isoformat(), "summary": summary, "results": results}, open(path, "w", encoding="utf-8"), indent=1)
    print(json.dumps(summary), f"\nreport: {path}")
    return 0 if summary["passed"] == summary["total"] else 1


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--check", action="store_true")
    ap.add_argument("--run", action="store_true")
    a = ap.parse_args()
    g = load()
    rows = truths(g)
    if a.run:
        return run(g, rows)
    for q in g["questions"]:
        print(f"{q['id']}  {q['text'][:60]:60}  expect {expected(q, rows.get(q['id']), g.get('aliases', {})) or '(no facts)'}")
    print(f"\n{len(g['questions'])} questions valid, {len(rows)} truths computed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
