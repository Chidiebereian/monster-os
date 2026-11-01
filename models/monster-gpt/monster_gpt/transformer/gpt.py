"""Decoder-only GPT: token embeddings + positions (learned, sinusoidal or
rotary), pre-norm transformer blocks (attention + MLP with residuals), final
norm and a tied language-model head. Next-token cross-entropy objective."""
from __future__ import annotations

import torch
import torch.nn as nn
import torch.nn.functional as F
from torch.utils.checkpoint import checkpoint

from ..core.config import GPTConfig
from ..nlp.text import sinusoidal_positions
from ..nn.torch_foundations import LayerNorm, RMSNorm
from .attention import CausalSelfAttention, KVCache, rope_tables


def make_norm(cfg: GPTConfig):
    return RMSNorm(cfg.n_embd) if cfg.norm == "rmsnorm" else LayerNorm(cfg.n_embd, bias=cfg.bias)


class MLP(nn.Module):
    def __init__(self, cfg: GPTConfig):
        super().__init__()
        self.fc = nn.Linear(cfg.n_embd, cfg.mlp_ratio * cfg.n_embd, bias=cfg.bias)
        self.proj = nn.Linear(cfg.mlp_ratio * cfg.n_embd, cfg.n_embd, bias=cfg.bias)
        self.drop = nn.Dropout(cfg.dropout)

    def forward(self, x):
        return self.drop(self.proj(F.gelu(self.fc(x))))


class Block(nn.Module):
    """Pre-norm: x + attn(norm(x)), then x + mlp(norm(x))."""

    def __init__(self, cfg: GPTConfig):
        super().__init__()
        self.ln1 = make_norm(cfg)
        self.attn = CausalSelfAttention(cfg.n_embd, cfg.n_head, cfg.n_kv_head, cfg.dropout, cfg.bias, rope=cfg.pos == "rope")
        self.ln2 = make_norm(cfg)
        self.mlp = MLP(cfg)

    def forward(self, x, rope_cs=None, cache=None, layer=0, start=0):
        x = x + self.attn(self.ln1(x), rope_cs, cache, layer, start)
        return x + self.mlp(self.ln2(x))


class GPT(nn.Module):
    def __init__(self, cfg: GPTConfig):
        super().__init__()
        self.cfg = cfg
        self.wte = nn.Embedding(cfg.vocab_size, cfg.n_embd)
        if cfg.pos == "learned":
            self.wpe = nn.Embedding(cfg.block_size, cfg.n_embd)
        elif cfg.pos == "sinusoidal":
            self.register_buffer("pe", sinusoidal_positions(self.max_positions, cfg.n_embd), persistent=False)
        self.drop = nn.Dropout(cfg.dropout)
        self.blocks = nn.ModuleList(Block(cfg) for _ in range(cfg.n_layer))
        self.ln_f = make_norm(cfg)
        self.head = nn.Linear(cfg.n_embd, cfg.vocab_size, bias=False)
        if cfg.tie_weights:
            self.head.weight = self.wte.weight
        self.apply(self._init)
        # GPT-2: scale residual projections by 1/sqrt(2 * n_layer)
        for n, p in self.named_parameters():
            if n.endswith("proj.weight"):
                nn.init.normal_(p, 0.0, 0.02 / (2 * cfg.n_layer) ** 0.5)
        self.grad_checkpointing = False
        self._rope = None

    @property
    def max_positions(self) -> int:
        """Longest supported sequence: learned positions are fixed; sinusoidal/rope
        extend by rope_scale (positional interpolation)."""
        return self.cfg.block_size if self.cfg.pos == "learned" else int(self.cfg.block_size * self.cfg.rope_scale)

    @staticmethod
    def _init(m):
        if isinstance(m, nn.Linear):
            nn.init.normal_(m.weight, 0.0, 0.02)
            if m.bias is not None:
                nn.init.zeros_(m.bias)
        elif isinstance(m, nn.Embedding):
            nn.init.normal_(m.weight, 0.0, 0.02)

    def num_params(self) -> int:
        return sum(p.numel() for p in self.parameters())

    def rope_cs(self, device):
        if self.cfg.pos != "rope":
            return None
        if self._rope is None or self._rope[0].device != device or self._rope[0].shape[0] < self.max_positions:
            self._rope = rope_tables(self.max_positions, self.cfg.head_dim, self.cfg.rope_base, self.cfg.rope_scale, device)
        return self._rope

    def forward(self, idx, targets=None, cache: KVCache | None = None, start: int = 0):
        b, t = idx.shape
        if start + t > self.max_positions:
            raise ValueError(f"sequence of {start + t} tokens exceeds the model's {self.max_positions}-token context")
        x = self.wte(idx)
        if self.cfg.pos == "learned":
            x = x + self.wpe(torch.arange(start, start + t, device=idx.device))
        elif self.cfg.pos == "sinusoidal":
            x = x + self.pe[start : start + t]
        x = self.drop(x)
        cs = self.rope_cs(idx.device)
        for i, blk in enumerate(self.blocks):
            if self.grad_checkpointing and self.training and cache is None:
                x = checkpoint(blk, x, cs, None, i, start, use_reentrant=False)
            else:
                x = blk(x, cs, cache, i, start)
        logits = self.head(self.ln_f(x))
        loss = None
        if targets is not None:
            loss = F.cross_entropy(logits.view(-1, logits.size(-1)), targets.view(-1), ignore_index=-100)
        return logits, loss

    def new_cache(self, batch: int, max_len: int | None = None, device=None) -> KVCache:
        return KVCache(self.cfg.n_layer, batch, self.cfg.n_kv_head, self.cfg.head_dim, max_len or self.max_positions,
                       device=device or self.wte.weight.device, dtype=self.wte.weight.dtype)

    def configure_optimizer(self, lr: float, weight_decay: float, betas=(0.9, 0.95)):
        """AdamW with weight decay on matrices only (not norms, biases or embeddings' duplicates)."""
        params = {n: p for n, p in self.named_parameters() if p.requires_grad}
        decay = [p for n, p in params.items() if p.dim() >= 2]
        no_decay = [p for n, p in params.items() if p.dim() < 2]
        return torch.optim.AdamW([{"params": decay, "weight_decay": weight_decay}, {"params": no_decay, "weight_decay": 0.0}], lr=lr, betas=betas)
