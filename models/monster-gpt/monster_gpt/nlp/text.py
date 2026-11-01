"""Text preprocessing, vocabularies, sequence padding, masks and sinusoidal positions."""
from __future__ import annotations

import re
import unicodedata
from collections import Counter

import torch

_WORD = re.compile(r"[a-z0-9]+(?:'[a-z]+)?|[^\sa-z0-9]", re.IGNORECASE)


def normalize(text: str) -> str:
    """NFKC, lower-case, collapse whitespace."""
    return re.sub(r"\s+", " ", unicodedata.normalize("NFKC", text).lower()).strip()


def word_tokenize(text: str) -> list[str]:
    return _WORD.findall(normalize(text))


class Vocab:
    PAD, UNK, BOS, EOS = "<pad>", "<unk>", "<bos>", "<eos>"

    def __init__(self, tokens: list[str], min_freq: int = 1, max_size: int | None = None):
        specials = [self.PAD, self.UNK, self.BOS, self.EOS]
        counts = Counter(tokens)
        words = [w for w, c in sorted(counts.items(), key=lambda x: (-x[1], x[0])) if c >= min_freq and w not in specials]
        if max_size:
            words = words[: max_size - len(specials)]
        self.itos = specials + words
        self.stoi = {w: i for i, w in enumerate(self.itos)}

    def __len__(self):
        return len(self.itos)

    def encode(self, tokens: list[str], bos: bool = False, eos: bool = False) -> list[int]:
        ids = [self.stoi.get(t, 1) for t in tokens]
        return ([2] if bos else []) + ids + ([3] if eos else [])

    def decode(self, ids: list[int]) -> list[str]:
        return [self.itos[i] for i in ids if i > 3]


def pad_batch(seqs: list[list[int]], pad_id: int = 0, max_len: int | None = None) -> tuple[torch.Tensor, torch.Tensor]:
    """Right-pad to the longest (or max_len, truncating). Returns (ids, lengths)."""
    L = max_len or max(len(s) for s in seqs)
    out = torch.full((len(seqs), L), pad_id, dtype=torch.long)
    lens = torch.tensor([min(len(s), L) for s in seqs])
    for i, s in enumerate(seqs):
        s = s[:L]
        out[i, : len(s)] = torch.tensor(s, dtype=torch.long)
    return out, lens


def padding_mask(lengths: torch.Tensor, max_len: int) -> torch.Tensor:
    """True where a position holds a real token. (b, t)"""
    return torch.arange(max_len)[None, :] < lengths[:, None]


def causal_mask(t: int, device=None) -> torch.Tensor:
    """True where query i may attend to key j (j <= i). (t, t)"""
    return torch.ones(t, t, dtype=torch.bool, device=device).tril()


def sinusoidal_positions(n_pos: int, dim: int, base: float = 10000.0) -> torch.Tensor:
    """PE[p, 2i] = sin(p / base^(2i/d)), PE[p, 2i+1] = cos(same). (n_pos, dim)"""
    pos = torch.arange(n_pos, dtype=torch.float32)[:, None]
    i = torch.arange(0, dim, 2, dtype=torch.float32)
    angle = pos / base ** (i / dim)
    pe = torch.zeros(n_pos, dim)
    pe[:, 0::2] = torch.sin(angle)
    pe[:, 1::2] = torch.cos(angle[:, : dim // 2])
    return pe
