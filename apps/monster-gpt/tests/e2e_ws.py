"""End-to-end test of MonsterGPT's WebSocket protocol v1 against a running
monster-gpt (default ws://localhost:8085). Uses the small model so it runs in
minutes on CPU. Run:  python -m pytest apps/monster-gpt/tests/e2e_ws.py -v

Covers: session issue, auth rejection, hello/ready, streamed answer with a
bet card and sources, ordered deltas, duplicate request replay, cancel,
busy/one-at-a-time, bad frames, oversize text, origin check, rate limit,
and owner isolation of conversations.
"""
import asyncio
import json
import os
import time
import urllib.error
import urllib.parse
import urllib.request

import pytest
import websockets

BASE = os.environ.get("MONSTER_GPT_URL", "http://localhost:8085")
WS = BASE.replace("http", "ws", 1) + "/ws"
SMALL = os.environ.get("MONSTER_GPT_TEST_MODEL", "qwen2.5:0.5b")


def http(method, path, token=None, body=None, headers=None):
    req = urllib.request.Request(BASE + path, method=method, data=json.dumps(body).encode() if body is not None else None)
    req.add_header("content-type", "application/json")
    if token:
        req.add_header("authorization", f"Bearer {token}")
    for k, v in (headers or {}).items():
        req.add_header(k, v)
    try:
        # /chat runs a whole generation; everything else should answer quickly
        with urllib.request.urlopen(req, timeout=300 if path == "/chat" else 30) as r:
            raw = r.read()
            return r.status, (json.loads(raw) if raw else None)
    except urllib.error.HTTPError as e:
        raw = e.read()
        return e.code, (json.loads(raw) if raw else None)


def session(user="e2e"):
    st, body = http("POST", "/session", body={"user": user})
    assert st == 200, body
    return body["token"], body["owner"]


async def connect(token):
    ws = await websockets.connect(WS, max_size=2**22)
    await ws.send(json.dumps({"v": 1, "type": "hello", "token": token}))
    ready = json.loads(await ws.recv())
    assert ready["type"] == "ready" and ready["v"] == 1, ready
    return ws, ready


async def collect(ws, request_id, timeout=600):
    """Frames for one request until done (or an error that ends it)."""
    frames = []
    deadline = time.time() + timeout
    while time.time() < deadline:
        f = json.loads(await asyncio.wait_for(ws.recv(), timeout=deadline - time.time()))
        if f.get("request_id") not in (request_id, None):
            continue
        frames.append(f)
        if f["type"] == "done" or (f["type"] == "error" and f.get("request_id") == request_id and f["code"] in ("busy", "conversation", "rate_limited")):
            return frames
    raise AssertionError(f"no done frame in {timeout}s: {frames[-3:]}")


def run(coro):
    return asyncio.run(coro)


def test_session_is_required():
    async def go():
        async with websockets.connect(WS) as ws:
            await ws.send(json.dumps({"type": "message", "request_id": "x", "text": "hi"}))
            f = json.loads(await ws.recv())
            assert f["type"] == "error" and f["code"] == "unauthorized"
        async with websockets.connect(WS) as ws:
            await ws.send(json.dumps({"type": "hello", "token": "not-a-token"}))
            f = json.loads(await ws.recv())
            assert f["code"] == "unauthorized"
    run(go())
    st, _ = http("GET", "/conversations")
    assert st == 401


def test_bad_origin_is_refused():
    async def go():
        with pytest.raises(Exception):
            async with websockets.connect(WS, origin="http://evil.example"):
                pass
    run(go())
    st, _ = http("POST", "/session", body={}, headers={"origin": "http://evil.example"})
    assert st == 403


def test_bad_frames_and_oversize_text():
    tok, _ = session()
    async def go():
        ws, _ = await connect(tok)
        await ws.send("not json")
        assert json.loads(await ws.recv())["code"] == "bad_frame"
        await ws.send(json.dumps({"type": "message", "request_id": "r", "text": "x" * 8001}))
        f = json.loads(await ws.recv())
        assert f["code"] == "bad_frame" and "longer" in f["message"]
        await ws.send(json.dumps({"v": 2, "type": "ping"}))
        assert "version" in json.loads(await ws.recv())["message"]
        await ws.send(json.dumps({"type": "ping"}))
        assert json.loads(await ws.recv())["type"] == "pong"
        await ws.close()
    run(go())


def test_bets_answer_streams_with_card_sources_and_replays_duplicates():
    tok, _ = session()
    async def go():
        ws, ready = await connect(tok)
        assert any(m["id"] == SMALL for m in ready["models"]), ready["models"]
        rid = f"e2e-{int(time.time() * 1000)}"
        t0 = time.time()
        await ws.send(json.dumps({"v": 1, "type": "message", "request_id": rid, "text": "give me 3 safe bets, I have $200", "model": SMALL}))
        frames = await collect(ws, rid)
        types = [f["type"] for f in frames]
        assert types[0] == "ack" and not frames[0]["duplicate"]
        conv = frames[0]["conversation_id"]
        assert conv
        route = next(f for f in frames if f["type"] == "status" and f["stage"] == "route")
        assert "$200 bankroll" in route["detail"] and "safe" in route["detail"]
        tools = {f["name"]: f["state"] for f in frames if f["type"] == "status" and f["stage"] == "tool"}
        assert tools.get("picks.today") == "completed", tools
        card = next(f for f in frames if f["type"] == "card")["card"]
        assert len(card["bets"]) <= 3
        for b in card["bets"]:
            assert b["tier"] in ("proven", "baseline") and b["stake_usd"] is not None
        srcs = next(f for f in frames if f["type"] == "sources")["sources"]
        assert srcs and srcs[0]["id"] == "S1" and srcs[0]["fetched_at"]
        # card and sources arrive before the first word of the explanation
        assert types.index("card") < types.index("delta")
        deltas = [f for f in frames if f["type"] == "delta"]
        assert len(deltas) > 3, "answer was not streamed incrementally"
        assert [d["seq"] for d in deltas] == list(range(len(deltas)))
        done = frames[-1]
        assert done["type"] == "done" and done["model"] == SMALL and not done["cancelled"]
        assert done["usage"]["ttft_ms"] is not None and done["usage"]["context"]["used_est"] <= done["usage"]["context"]["budget"]
        # a "reset" (checker rewrite or cleanup) discards the text streamed before it
        last_reset = max((i for i, f in enumerate(frames) if f["type"] == "reset"), default=-1)
        answer = "".join(f["text"] for f in frames[last_reset + 1:] if f["type"] == "delta")
        print(f"\nTTFT {done['usage']['ttft_ms']} ms, total {time.time() - t0:.1f}s, {done['usage'].get('tokens_per_s')} tok/s; unsupported numbers: {done['usage'].get('unsupported_numbers')}")

        # resend the same request id: replayed from storage, not regenerated
        await ws.send(json.dumps({"type": "message", "request_id": rid, "conversation_id": conv, "text": "give me 3 safe bets, I have $200", "model": SMALL}))
        again = await collect(ws, rid, timeout=30)
        assert again[0]["type"] == "ack" and again[0]["duplicate"] is True
        assert "".join(f["text"] for f in again if f["type"] == "delta") == answer
        await ws.close()
        return conv
    conv = run(go())
    st, body = http("GET", f"/conversations/{conv}", token=tok)
    assert st == 200 and [m["role"] for m in body["messages"]] == ["user", "assistant"]


def test_empty_card_is_answered_without_a_model():
    tok, _ = session()
    # a sport with no open picks gets the "nothing open" reply straight away
    st, r = http("POST", "/chat", token=tok, body={"text": "best MLB props on December 25", "model": SMALL})
    assert st == 200
    card = next((e["card"] for e in r["events"] if e["type"] == "card"), None)
    done = next(e for e in r["events"] if e["type"] == "done")
    if card is None or card["bets"]:
        return  # picks exist for that request today; nothing to check
    assert done["model"] is None and done["usage"].get("empty_card") is True
    assert "only suggest bets the models actually priced" in r["answer"]


def test_follow_up_inherits_filters():
    tok, _ = session()
    async def go():
        ws, _ = await connect(tok)
        r1 = f"f1-{time.time()}"
        await ws.send(json.dumps({"type": "message", "request_id": r1, "text": "2 safe NHL props tonight with $100", "model": SMALL}))
        f1 = await collect(ws, r1)
        conv = f1[0]["conversation_id"]
        r2 = f"f2-{time.time()}"
        await ws.send(json.dumps({"type": "message", "request_id": r2, "conversation_id": conv, "text": "what about basketball?", "model": SMALL}))
        f2 = await collect(ws, r2)
        route = next(f for f in f2 if f["type"] == "status" and f["stage"] == "route")
        assert route["detail"].startswith("NBA") and "$100 bankroll" in route["detail"] and "kept from your last request" in route["detail"], route
        await ws.close()
    run(go())


def test_cancel_stops_generation_and_busy_is_enforced():
    tok, _ = session()
    async def go():
        ws, _ = await connect(tok)
        rid = f"c-{time.time()}"
        await ws.send(json.dumps({"type": "message", "request_id": rid, "text": "Explain in detail how Kelly staking works and why fractional Kelly is safer", "model": SMALL}))
        # a second message while the first runs is refused
        await ws.send(json.dumps({"type": "message", "request_id": rid + "-b", "text": "another question"}))
        got_delta = False
        busy = False
        frames = []
        while True:
            f = json.loads(await asyncio.wait_for(ws.recv(), timeout=600))
            frames.append(f)
            if f.get("type") == "error" and f.get("request_id") == rid + "-b":
                busy = f["code"] == "busy"
            if f.get("type") == "delta" and f["request_id"] == rid and not got_delta:
                got_delta = True
                await ws.send(json.dumps({"type": "cancel", "request_id": rid}))
            if f.get("type") == "done" and f["request_id"] == rid:
                break
        assert busy, "second message should be refused while one is running"
        done = frames[-1]
        assert done["cancelled"] is True, done
        n = sum(1 for f in frames if f.get("type") == "delta")
        assert n < 60, f"generation kept going after cancel ({n} deltas)"
        # the slot is free again: metrics show no generation stuck
        await asyncio.sleep(1)
        st, m = http("GET", "/metrics")
        assert m["model_slots"]["free"] == m["model_slots"]["max"], m["model_slots"]
        await ws.close()
    run(go())


def test_owners_are_isolated():
    tok_a, owner_a = session("alice")
    tok_b, owner_b = session("bob")
    assert owner_a != owner_b
    async def go():
        ws, _ = await connect(tok_a)
        rid = f"iso-{time.time()}"
        await ws.send(json.dumps({"type": "message", "request_id": rid, "text": "how are your picks doing?", "model": SMALL}))
        frames = await collect(ws, rid)
        await ws.close()
        return frames[0]["conversation_id"]
    conv = run(go())
    assert http("GET", f"/conversations/{conv}", token=tok_a)[0] == 200
    assert http("GET", f"/conversations/{conv}", token=tok_b)[0] == 404
    assert conv not in [c["id"] for c in http("GET", "/conversations", token=tok_b)[1]["conversations"]]
    assert http("DELETE", f"/conversations/{conv}", token=tok_b)[0] == 404
    async def hijack():
        ws, _ = await connect(tok_b)
        rid = f"hj-{time.time()}"
        await ws.send(json.dumps({"type": "message", "request_id": rid, "conversation_id": conv, "text": "show me", "model": SMALL}))
        f = json.loads(await ws.recv())
        await ws.close()
        return f
    f = run(hijack())
    assert f["type"] == "error" and f["code"] == "conversation", f


def test_rate_limit():
    tok, _ = session()
    async def go():
        ws, _ = await connect(tok)
        codes = []
        # first starts a generation; the rest are refused as busy until the 21st hits the rate limit
        for i in range(21):
            await ws.send(json.dumps({"type": "message", "request_id": f"rl-{i}-{time.time()}", "text": "explain parlays", "model": SMALL}))
        deadline = time.time() + 20
        while time.time() < deadline and "rate_limited" not in codes:
            f = json.loads(await asyncio.wait_for(ws.recv(), timeout=20))
            if f["type"] == "error":
                codes.append(f["code"])
        assert "rate_limited" in codes, codes
        await ws.close()  # disconnect cancels the running generation
    run(go())


GATEWAY = os.environ.get("MONSTER_OS_GATEWAY", "http://localhost:8090")


def test_gateway_passthrough_and_forwarded_address():
    """The api-gateway relays the WebSocket (origin checked upstream) and its
    session endpoint; a spoofed X-Forwarded-For can't get a token."""
    req = urllib.request.Request(GATEWAY + "/api/monster-gpt/session", data=b'{"user":"e2e"}', method="POST", headers={"content-type": "application/json"})
    tok = json.loads(urllib.request.urlopen(req, timeout=30).read())["token"]
    async def go():
        ws_url = GATEWAY.replace("http", "ws", 1) + "/ws/monster-gpt/ws"
        async with websockets.connect(ws_url, origin="http://localhost:5190") as ws:
            await ws.send(json.dumps({"type": "hello", "token": tok}))
            assert json.loads(await ws.recv())["type"] == "ready"
            await ws.send(json.dumps({"type": "ping"}))
            assert json.loads(await ws.recv())["type"] == "pong"
        with pytest.raises(Exception):
            async with websockets.connect(ws_url, origin="http://evil.example"):
                pass
    run(go())
    st, body = http("POST", "/session", body={}, headers={"x-forwarded-for": "203.0.113.9"})
    assert st == 403, body


def test_memory_commands_and_isolation():
    tok_a, _ = session("mem-a")
    tok_b, _ = session("mem-b")
    st, r = http("POST", "/chat", token=tok_a, body={"text": "remember my bankroll is $250", "model": SMALL})
    assert st == 200 and "$250" in r["answer"]
    items = http("GET", "/memory", token=tok_a)[1]["items"]
    assert any(i["key"] == "bankroll" and i["value"] == "250" for i in items)
    assert http("GET", "/memory", token=tok_b)[1]["items"] == []
    st, r = http("POST", "/chat", token=tok_a, body={"text": "best bets today", "model": SMALL})
    route = next(e for e in r["events"] if e["type"] == "status" and e["stage"] == "route")
    assert "$250 bankroll" in route["detail"] and "saved" in route["detail"], route
    item = next(i for i in items if i["key"] == "bankroll")
    assert http("DELETE", f"/memory/{item['id']}", token=tok_b)[0] == 404
    st, r = http("POST", "/chat", token=tok_a, body={"text": "forget everything"})
    assert http("GET", "/memory", token=tok_a)[1]["items"] == []


def test_document_search_finds_and_refuses():
    st, d = http("GET", "/rag/search?q=" + urllib.parse.quote("how are pick grades capped by track record") + "&k=3")
    assert st == 200 and d["hits"][0]["doc_id"] == "docs/architecture/ai-picks.md"
    st, d = http("GET", "/rag/search?q=" + urllib.parse.quote("recipe for chocolate chip cookies"))
    assert st == 200 and d["hits"] == []
