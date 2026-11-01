"""Word embeddings: skip-gram with negative sampling (word2vec) in PyTorch."""
from __future__ import annotations

import torch
import torch.nn as nn
import torch.nn.functional as F


def skipgram_pairs(ids: list[int], window: int = 2) -> torch.Tensor:
    """(center, context) pairs within `window` positions. (n, 2)"""
    pairs = [(c, ids[j]) for i, c in enumerate(ids) for j in range(max(0, i - window), min(len(ids), i + window + 1)) if j != i]
    return torch.tensor(pairs, dtype=torch.long)


class SkipGram(nn.Module):
    def __init__(self, vocab: int, dim: int = 32):
        super().__init__()
        self.inp = nn.Embedding(vocab, dim)
        self.out = nn.Embedding(vocab, dim)
        nn.init.uniform_(self.inp.weight, -0.5 / dim, 0.5 / dim)
        nn.init.zeros_(self.out.weight)

    def loss(self, center, context, negatives):
        """-log sigma(u_o . v_c) - sum log sigma(-u_k . v_c)"""
        v = self.inp(center)                       # (b, d)
        pos = (self.out(context) * v).sum(-1)      # (b,)
        neg = torch.bmm(self.out(negatives), v[:, :, None]).squeeze(-1)  # (b, k)
        return -(F.logsigmoid(pos) + F.logsigmoid(-neg).sum(-1)).mean()

    def similar(self, idx: int, k: int = 5) -> list[int]:
        w = F.normalize(self.inp.weight, dim=-1)
        sims = w @ w[idx]
        sims[idx] = -1
        return sims.topk(k).indices.tolist()


def train_skipgram(ids: list[int], vocab: int, dim: int = 32, window: int = 2, negatives: int = 5, epochs: int = 5,
                   lr: float = 0.01, batch: int = 512, seed: int = 0) -> tuple[SkipGram, list[float]]:
    torch.manual_seed(seed)
    pairs = skipgram_pairs(ids, window)
    model = SkipGram(vocab, dim)
    opt = torch.optim.Adam(model.parameters(), lr=lr)
    # unigram^0.75 negative sampling distribution
    freq = torch.bincount(torch.tensor(ids), minlength=vocab).float().pow(0.75)
    freq[:4] = 0  # never sample special tokens
    losses = []
    for _ in range(epochs):
        perm = torch.randperm(len(pairs))
        total = 0.0
        for s in range(0, len(pairs), batch):
            b = pairs[perm[s : s + batch]]
            neg = torch.multinomial(freq, len(b) * negatives, replacement=True).view(len(b), negatives)
            loss = model.loss(b[:, 0], b[:, 1], neg)
            opt.zero_grad()
            loss.backward()
            opt.step()
            total += loss.item() * len(b)
        losses.append(total / len(pairs))
    return model, losses
