"""Causal self-attention with real grouped-query attention (GQA) and a KV cache.

GQA: queries have n_head heads, keys/values only n_kv_head heads; each group of
n_head / n_kv_head query heads shares one K/V head. The K/V projections and the
cache are genuinely smaller (n_kv_head * head_dim wide), and K/V heads are
expanded to the query heads only inside the attention product.
n_kv_head == n_head is ordinary multi-head attention; n_kv_head == 1 is
multi-query attention.
"""
from __future__ import annotations

import math

import torch
import torch.nn as nn
import torch.nn.functional as F


def rope_tables(n_pos: int, head_dim: int, base: float = 10000.0, scale: float = 1.0, device=None):
    """cos/sin tables for rotary embeddings. scale > 1 = positional interpolation
    (positions are divided by `scale`, so a model trained on N tokens sees
    N*scale positions inside its trained angle range)."""
    inv = 1.0 / (base ** (torch.arange(0, head_dim, 2, device=device, dtype=torch.float32) / head_dim))
    pos = torch.arange(n_pos, device=device, dtype=torch.float32) / scale
    ang = torch.outer(pos, inv)  # (n_pos, hd/2)
    return ang.cos(), ang.sin()


def apply_rope(x: torch.Tensor, cos: torch.Tensor, sin: torch.Tensor) -> torch.Tensor:
    """x: (b, h, t, hd); cos/sin: (t, hd/2). Rotates each (even, odd) pair."""
    x1, x2 = x[..., 0::2], x[..., 1::2]
    c, s = cos[None, None], sin[None, None]
    out = torch.empty_like(x)
    out[..., 0::2] = x1 * c - x2 * s
    out[..., 1::2] = x1 * s + x2 * c
    return out


class KVCache:
    """Per-layer key/value tensors (b, n_kv_head, t, head_dim), preallocated to max_len."""

    def __init__(self, n_layer: int, batch: int, n_kv_head: int, head_dim: int, max_len: int, device=None, dtype=torch.float32):
        shape = (batch, n_kv_head, max_len, head_dim)
        self.k = [torch.zeros(shape, device=device, dtype=dtype) for _ in range(n_layer)]
        self.v = [torch.zeros(shape, device=device, dtype=dtype) for _ in range(n_layer)]
        self.len = 0
        self.max_len = max_len

    def update(self, layer: int, k: torch.Tensor, v: torch.Tensor, start: int):
        t = k.shape[2]
        if start + t > self.max_len:
            raise ValueError(f"KV cache full: {start + t} > {self.max_len} tokens")
        self.k[layer][:, :, start : start + t] = k
        self.v[layer][:, :, start : start + t] = v
        return self.k[layer][:, :, : start + t], self.v[layer][:, :, : start + t]

    def nbytes(self) -> int:
        return sum(t.numel() * t.element_size() for t in self.k + self.v)


def reference_attention(q, k, v, causal_offset: int = 0) -> torch.Tensor:
    """Plain softmax attention, explicit mask. q: (b,h,tq,d), k/v: (b,h,tk,d).
    Query i sits at absolute position causal_offset + i and may see keys <= that."""
    tq, tk = q.shape[2], k.shape[2]
    att = (q @ k.transpose(-2, -1)) / math.sqrt(q.shape[-1])
    qpos = torch.arange(tq, device=q.device)[:, None] + causal_offset
    kpos = torch.arange(tk, device=q.device)[None, :]
    att = att.masked_fill(kpos > qpos, float("-inf"))
    return F.softmax(att, dim=-1) @ v


class CausalSelfAttention(nn.Module):
    def __init__(self, n_embd: int, n_head: int, n_kv_head: int, dropout: float = 0.0, bias: bool = False, rope: bool = False, efficient: bool = True):
        super().__init__()
        assert n_embd % n_head == 0 and n_head % n_kv_head == 0
        self.n_head, self.n_kv_head = n_head, n_kv_head
        self.hd = n_embd // n_head
        self.group = n_head // n_kv_head
        self.q = nn.Linear(n_embd, n_embd, bias=bias)
        self.kv = nn.Linear(n_embd, 2 * n_kv_head * self.hd, bias=bias)   # the smaller GQA projection
        self.proj = nn.Linear(n_embd, n_embd, bias=bias)
        self.drop = dropout
        self.resid_drop = nn.Dropout(dropout)
        self.rope = rope
        self.efficient = efficient

    def forward(self, x, rope_cs=None, cache: KVCache | None = None, layer: int = 0, start: int = 0):
        b, t, d = x.shape
        q = self.q(x).view(b, t, self.n_head, self.hd).transpose(1, 2)                # (b, h, t, hd)
        k, v = self.kv(x).view(b, t, 2, self.n_kv_head, self.hd).permute(2, 0, 3, 1, 4)  # each (b, kvh, t, hd)
        if self.rope:
            cos, sin = rope_cs
            q = apply_rope(q, cos[start : start + t], sin[start : start + t])
            k = apply_rope(k, cos[start : start + t], sin[start : start + t])
        if cache is not None:
            k, v = cache.update(layer, k, v, start)
        # expand each K/V head to its group of query heads (only here, not in the cache)
        if self.group > 1:
            k = k.repeat_interleave(self.group, dim=1)
            v = v.repeat_interleave(self.group, dim=1)
        drop = self.drop if self.training else 0.0
        if self.efficient and start == 0 and k.shape[2] == t:
            y = F.scaled_dot_product_attention(q, k, v, is_causal=True, dropout_p=drop)
        elif self.efficient and t == 1:
            # one new token attends to everything cached so far
            y = F.scaled_dot_product_attention(q, k, v, is_causal=False, dropout_p=drop)
        else:
            y = reference_attention(q, k, v, causal_offset=start)
        y = y.transpose(1, 2).contiguous().view(b, t, d)
        return self.resid_drop(self.proj(y))
