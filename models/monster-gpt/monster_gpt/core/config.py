"""Model configuration: validated dimensions, parameter count and memory estimates."""
from __future__ import annotations

import json
from dataclasses import asdict, dataclass, field
from pathlib import Path


@dataclass
class GPTConfig:
    vocab_size: int = 2048
    block_size: int = 256          # context length (tokens)
    n_layer: int = 4
    n_head: int = 4
    n_kv_head: int = 4             # < n_head = grouped-query attention; 1 = multi-query
    n_embd: int = 256
    mlp_ratio: int = 4
    dropout: float = 0.1
    bias: bool = False
    norm: str = "layernorm"        # layernorm | rmsnorm
    pos: str = "learned"           # learned | sinusoidal | rope
    rope_base: float = 10000.0
    rope_scale: float = 1.0        # >1 = positional interpolation for longer contexts
    tie_weights: bool = True
    extra: dict = field(default_factory=dict)

    def __post_init__(self):
        self.validate()

    def validate(self) -> None:
        errs = []
        if self.n_embd % self.n_head:
            errs.append(f"n_embd ({self.n_embd}) must be divisible by n_head ({self.n_head})")
        if self.n_head % self.n_kv_head:
            errs.append(f"n_head ({self.n_head}) must be divisible by n_kv_head ({self.n_kv_head})")
        if self.pos == "rope" and (self.n_embd // max(self.n_head, 1)) % 2:
            errs.append("rope needs an even head dimension")
        if self.norm not in ("layernorm", "rmsnorm"):
            errs.append(f"unknown norm {self.norm!r}")
        if self.pos not in ("learned", "sinusoidal", "rope"):
            errs.append(f"unknown positional encoding {self.pos!r}")
        for k in ("vocab_size", "block_size", "n_layer", "n_head", "n_kv_head", "n_embd"):
            if getattr(self, k) <= 0:
                errs.append(f"{k} must be positive")
        if not 0.0 <= self.dropout < 1.0:
            errs.append("dropout must be in [0, 1)")
        if errs:
            raise ValueError("; ".join(errs))

    @property
    def head_dim(self) -> int:
        return self.n_embd // self.n_head

    def param_count(self) -> int:
        """Exact parameter count of transformer.gpt.GPT for this config."""
        d, kv = self.n_embd, self.n_kv_head * self.head_dim
        b = 1 if self.bias else 0
        norm = d * (2 if (self.norm == "layernorm" and self.bias) else 1)
        attn = d * (d + 2 * kv) + b * (d + 2 * kv) + d * d + b * d
        mlp = d * self.mlp_ratio * d * 2 + b * (self.mlp_ratio * d + d)
        block = 2 * norm + attn + mlp
        emb = self.vocab_size * d + (self.block_size * d if self.pos == "learned" else 0)
        head = 0 if self.tie_weights else self.vocab_size * d
        return emb + self.n_layer * block + norm + head

    def memory_estimate(self, batch_size: int, seq_len: int | None = None, bytes_per: int = 4, training: bool = True) -> dict:
        """Rough memory (bytes) before launching: weights, AdamW state, gradients,
        activations (no checkpointing) and the KV cache for inference."""
        t = seq_len or self.block_size
        p = self.param_count()
        weights = p * bytes_per
        grads = p * bytes_per if training else 0
        adam = 2 * p * 4 if training else 0
        # per layer: ~ (attention scores b*h*t*t) + ~16 * b*t*d for qkv/mlp/norm buffers
        act = self.n_layer * (batch_size * self.n_head * t * t + 16 * batch_size * t * self.n_embd) * bytes_per if training else 0
        logits = batch_size * t * self.vocab_size * bytes_per * (2 if training else 1)
        kv_cache = 2 * self.n_layer * batch_size * t * self.n_kv_head * self.head_dim * bytes_per
        total = weights + grads + adam + act + logits + (0 if training else kv_cache)
        return {"params": p, "weights": weights, "grads": grads, "optimizer": adam, "activations": act, "logits": logits,
                "kv_cache": kv_cache, "total": total, "total_mb": round(total / 2**20, 1)}

    def to_dict(self) -> dict:
        return asdict(self)

    def save(self, path: str | Path) -> None:
        Path(path).write_text(json.dumps(self.to_dict(), indent=2))

    @classmethod
    def load(cls, path: str | Path) -> "GPTConfig":
        return cls(**json.loads(Path(path).read_text()))


# Named starting points. "micro" is the pipeline-validation model.
PRESETS = {
    "nano": dict(vocab_size=512, block_size=64, n_layer=2, n_head=2, n_kv_head=2, n_embd=64),
    "micro": dict(vocab_size=4096, block_size=256, n_layer=4, n_head=4, n_kv_head=2, n_embd=256, pos="rope", norm="rmsnorm"),
    "small": dict(vocab_size=8192, block_size=512, n_layer=6, n_head=8, n_kv_head=2, n_embd=384, pos="rope", norm="rmsnorm"),
}
