"""Text generation: configurable sampling (temperature, top-k, top-p,
greedy), streaming token by token, a KV cache (prefill once, then one token
per step), context-length enforcement, and batched generation."""
from __future__ import annotations

from typing import Iterator

import torch


def sample_next(logits: torch.Tensor, temperature: float = 1.0, top_k: int | None = None, top_p: float | None = None,
                generator: torch.Generator | None = None) -> torch.Tensor:
    """logits: (b, vocab) -> next ids (b,). temperature 0 = greedy."""
    if temperature <= 0:
        return logits.argmax(-1)
    logits = logits / temperature
    if top_k:
        kth = torch.topk(logits, min(top_k, logits.size(-1))).values[:, -1, None]
        logits = logits.masked_fill(logits < kth, float("-inf"))
    if top_p is not None and top_p < 1.0:
        sorted_logits, idx = torch.sort(logits, descending=True)
        probs = torch.softmax(sorted_logits, -1)
        # drop tokens once the cumulative mass before them already exceeds top_p
        drop = probs.cumsum(-1) - probs > top_p
        sorted_logits = sorted_logits.masked_fill(drop, float("-inf"))
        logits = torch.full_like(logits, float("-inf")).scatter(-1, idx, sorted_logits)
    return torch.multinomial(torch.softmax(logits, -1), 1, generator=generator).squeeze(-1)


@torch.no_grad()
def generate(model, prompt_ids: list[int], max_new_tokens: int = 100, temperature: float = 0.8, top_k: int | None = 50,
             top_p: float | None = 0.95, use_cache: bool = True, stop_ids: set[int] | None = None, seed: int | None = None) -> Iterator[int]:
    """Yield new token ids one at a time (streaming). The prompt is cut from the
    left so prompt + new tokens fit the model's context; never exceeds it."""
    model.eval()
    device = next(model.parameters()).device
    limit = model.max_positions
    max_new_tokens = max(0, min(max_new_tokens, limit - 1))
    keep = max(1, limit - max_new_tokens)
    ids = list(prompt_ids)[-keep:] or [0]
    g = torch.Generator(device="cpu").manual_seed(seed) if seed is not None else None
    stop_ids = stop_ids or set()
    if use_cache:
        cache = model.new_cache(1, limit, device)
        logits, _ = model(torch.tensor([ids], device=device), cache=cache, start=0)
        pos = len(ids)
        for _ in range(max_new_tokens):
            nxt = int(sample_next(logits[:, -1, :].float().cpu(), temperature, top_k, top_p, g)[0])
            yield nxt
            if nxt in stop_ids or pos >= limit:
                return
            logits, _ = model(torch.tensor([[nxt]], device=device), cache=cache, start=pos)
            pos += 1
    else:
        for _ in range(max_new_tokens):
            window = ids[-limit:]
            logits, _ = model(torch.tensor([window], device=device))
            nxt = int(sample_next(logits[:, -1, :].float().cpu(), temperature, top_k, top_p, g)[0])
            ids.append(nxt)
            yield nxt
            if nxt in stop_ids:
                return


@torch.no_grad()
def generate_batch(model, prompts: list[list[int]], max_new_tokens: int = 50, temperature: float = 0.0, top_k: int | None = None,
                   top_p: float | None = None, seed: int | None = None) -> list[list[int]]:
    """Batched generation with a shared KV cache. Prompts are bucketed by length
    (each bucket runs as one batch), so no padding enters the attention."""
    model.eval()
    device = next(model.parameters()).device
    limit = model.max_positions
    g = torch.Generator().manual_seed(seed) if seed is not None else None
    out: list[list[int] | None] = [None] * len(prompts)
    buckets: dict[int, list[int]] = {}
    for i, p in enumerate(prompts):
        buckets.setdefault(len(p), []).append(i)
    for L, idxs in buckets.items():
        n_new = max(0, min(max_new_tokens, limit - L))
        x = torch.tensor([prompts[i] for i in idxs], device=device)
        cache = model.new_cache(len(idxs), L + n_new, device)
        logits, _ = model(x, cache=cache, start=0)
        gen = []
        for s in range(n_new):
            nxt = sample_next(logits[:, -1, :].float().cpu(), temperature, top_k, top_p, g)
            gen.append(nxt)
            if s + 1 < n_new:
                logits, _ = model(nxt[:, None].to(device), cache=cache, start=L + s)
        toks = torch.stack(gen, 1).tolist() if gen else [[] for _ in idxs]
        for j, i in enumerate(idxs):
            out[i] = toks[j]
    return out  # type: ignore[return-value]
