"""Sentiment baseline: bag-of-words logistic regression (PyTorch).

The examples are short betting-flavoured sentences written for this project
(no third-party data), labelled 1 = positive / confident, 0 = negative / worried.
It's a baseline to exercise the NLP pipeline, not a production classifier.
"""
from __future__ import annotations

import torch
import torch.nn as nn
import torch.nn.functional as F

from .text import Vocab, word_tokenize

SENTIMENT_EXAMPLES: list[tuple[str, int]] = [
    ("great value on this line, love the edge", 1), ("the model loves this matchup", 1), ("strong play, clear advantage", 1),
    ("excellent price at this book", 1), ("happy with that win tonight", 1), ("nice cash, the over hit easily", 1),
    ("this pick looks fantastic", 1), ("good spot for the underdog", 1), ("solid edge and a great number", 1),
    ("we won again, what a night", 1), ("brilliant read on that total", 1), ("best bet of the day, love it", 1),
    ("the price is good and the edge is real", 1), ("winning streak continues, great run", 1), ("i like this one a lot", 1),
    ("awful line, no value at all", 0), ("terrible beat in overtime", 0), ("this pick looks bad", 0), ("we lost again, brutal night", 0),
    ("poor price, the edge is gone", 0), ("i hate this matchup for the over", 0), ("bad spot, the injury kills it", 0),
    ("horrible result, the under never had a chance", 0), ("the line moved against us, no value", 0), ("ugly loss and a bad read", 0),
    ("worst bet of the week", 0), ("i don't like this one at all", 0), ("losing streak, rough stretch", 0), ("the number is bad now", 0),
    ("disappointing finish, missed by one", 0),
]


class BagOfWordsSentiment(nn.Module):
    def __init__(self, vocab: Vocab):
        super().__init__()
        self.vocab = vocab
        self.linear = nn.Linear(len(vocab), 1)

    def featurize(self, texts: list[str]) -> torch.Tensor:
        x = torch.zeros(len(texts), len(self.vocab))
        for i, t in enumerate(texts):
            for idx in self.vocab.encode(word_tokenize(t)):
                x[i, idx] = 1.0
        return x

    def forward(self, x):
        return self.linear(x).squeeze(-1)

    def fit(self, texts: list[str], labels: list[int], epochs: int = 300, lr: float = 0.1, l2: float = 1e-3) -> list[float]:
        x, y = self.featurize(texts), torch.tensor(labels, dtype=torch.float32)
        opt = torch.optim.Adam(self.parameters(), lr=lr, weight_decay=l2)
        losses = []
        for _ in range(epochs):
            loss = F.binary_cross_entropy_with_logits(self(x), y)
            opt.zero_grad()
            loss.backward()
            opt.step()
            losses.append(float(loss))
        return losses

    @torch.no_grad()
    def predict_proba(self, texts: list[str]) -> torch.Tensor:
        return torch.sigmoid(self(self.featurize(texts)))
