"""Evaluation and benchmarks: perplexity, KV-cache equivalence, cached vs
uncached generation speed, training cost by sequence length, and a
long-context retrieval probe."""
from __future__ import annotations

import gc
import math
import time
import tracemalloc

import torch

from ..inference.generate import generate


@torch.no_grad()
def perplexity(model, tokens, block: int | None = None, max_windows: int = 200) -> dict:
    """Non-overlapping windows over the token stream; mean NLL and perplexity."""
    model.eval()
    t = torch.as_tensor(tokens, dtype=torch.long)
    L = block or model.cfg.block_size
    n = min(max_windows, (len(t) - 1) // L)
    if n == 0:
        raise ValueError("not enough tokens for one window")
    nll = 0.0
    for i in range(n):
        x = t[i * L : i * L + L][None]
        y = t[i * L + 1 : i * L + L + 1][None]
        nll += float(model(x, y)[1])
    mean = nll / n
    return {"windows": n, "tokens": n * L, "loss": round(mean, 4), "perplexity": round(math.exp(mean), 2)}


@torch.no_grad()
def kv_cache_max_diff(model, ids: list[int], steps: int = 8) -> float:
    """Max |logit difference| between incremental cached decoding and a full
    uncached forward over the same tokens (should be ~1e-5 in float32)."""
    model.eval()
    full_ids = list(ids)
    cache = model.new_cache(1, len(ids) + steps)
    logits, _ = model(torch.tensor([ids]), cache=cache, start=0)
    cached = [logits[0, -1]]
    pos = len(ids)
    for s in range(steps):
        nxt = int(cached[-1].argmax())
        full_ids.append(nxt)
        logits, _ = model(torch.tensor([[nxt]]), cache=cache, start=pos)
        pos += 1
        cached.append(logits[0, -1])
    ref, _ = model(torch.tensor([full_ids]))
    ref_rows = [ref[0, len(ids) - 1 + s] for s in range(steps + 1)]
    return float(max((a - b).abs().max() for a, b in zip(cached, ref_rows)))


@torch.no_grad()
def inference_benchmark(model, prompt_len: int = 64, new_tokens: int = 64, seed: int = 0) -> dict:
    ids = torch.randint(0, model.cfg.vocab_size, (prompt_len,), generator=torch.Generator().manual_seed(seed)).tolist()
    out = {}
    for name, cache in (("cached", True), ("uncached", False)):
        t0 = time.perf_counter()
        first = None
        n = 0
        for _ in generate(model, ids, new_tokens, temperature=0.0, top_k=None, top_p=None, use_cache=cache):
            n += 1
            if first is None:
                first = time.perf_counter() - t0
        dt = time.perf_counter() - t0
        out[name] = {"tokens": n, "seconds": round(dt, 3), "tokens_per_s": round(n / dt, 1), "ttft_ms": round(1000 * (first or 0), 1)}
    out["speedup"] = round(out["cached"]["tokens_per_s"] / max(out["uncached"]["tokens_per_s"], 1e-9), 2)
    return out


def sequence_length_benchmark(model_factory, lengths=(128, 256, 512, 1024), batch: int = 4, steps: int = 3) -> list[dict]:
    """Forward+backward time and peak Python-tracked memory per sequence length.
    `model_factory(seq_len)` returns a model whose context allows seq_len."""
    rows = []
    for L in lengths:
        model = model_factory(L)
        model.train()
        x = torch.randint(0, model.cfg.vocab_size, (batch, L))
        gc.collect()
        tracemalloc.start()
        t0 = time.perf_counter()
        try:
            for _ in range(steps):
                _, loss = model(x, x)
                loss.backward()
                model.zero_grad(set_to_none=True)
            dt = (time.perf_counter() - t0) / steps
            _, peak = tracemalloc.get_traced_memory()
            rows.append({"seq_len": L, "ms_per_step": round(1000 * dt, 1), "tokens_per_s": round(batch * L / dt), "peak_traced_mb": round(peak / 2**20, 1), "ok": True})
        except (RuntimeError, ValueError) as e:
            rows.append({"seq_len": L, "ok": False, "error": str(e)[:200]})
        finally:
            tracemalloc.stop()
        del model
    return rows


@torch.no_grad()
def long_context_probe(model, tok, filler: str, key: str = "The passcode is 7421.", query: str = " The passcode is", depths=(0.1, 0.5, 0.9), length: int | None = None) -> list[dict]:
    """Needle-in-a-haystack: put `key` at several depths of a filler context and
    measure the log-probability the model gives the needle's continuation."""
    model.eval()
    L = length or model.max_positions
    target = tok.encode(" 7421")
    fill = tok.encode(filler)
    needle = tok.encode(" " + key)
    q = tok.encode(query)
    rows = []
    for d in depths:
        budget = L - len(needle) - len(q) - len(target)
        if budget <= 0:
            break
        body = (fill * (budget // max(len(fill), 1) + 1))[:budget]
        at = int(d * len(body))
        ids = body[:at] + needle + body[at:] + q + target
        x = torch.tensor([ids[:-1]])
        logits, _ = model(x)
        lp = torch.log_softmax(logits[0], -1)
        start = len(ids) - len(target) - 1
        score = sum(float(lp[start + i, t]) for i, t in enumerate(target))
        rows.append({"depth": d, "context_tokens": len(ids), "needle_logprob": round(score, 3)})
    return rows
