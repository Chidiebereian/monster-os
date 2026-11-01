"""PyTorch foundations: hand-written normalisation layers (checked against
torch's own), a reusable training loop with diagnostics, a dead-ReLU detector,
and a handwritten-digit classifier (scikit-learn's 8x8 digits, BSD-licensed)."""
from __future__ import annotations

import math
import time
from dataclasses import dataclass, field

import torch
import torch.nn as nn
import torch.nn.functional as F


class LayerNorm(nn.Module):
    """Normalise each example over its features: (x - mean) / sqrt(var + eps) * g + b."""

    def __init__(self, dim: int, eps: float = 1e-5, bias: bool = True):
        super().__init__()
        self.eps = eps
        self.weight = nn.Parameter(torch.ones(dim))
        self.bias = nn.Parameter(torch.zeros(dim)) if bias else None

    def forward(self, x):
        mu = x.mean(-1, keepdim=True)
        var = x.var(-1, keepdim=True, unbiased=False)
        y = (x - mu) / torch.sqrt(var + self.eps) * self.weight
        return y + self.bias if self.bias is not None else y


class RMSNorm(nn.Module):
    """Scale by the root mean square only (no centring): x / sqrt(mean(x^2) + eps) * g."""

    def __init__(self, dim: int, eps: float = 1e-6):
        super().__init__()
        self.eps = eps
        self.weight = nn.Parameter(torch.ones(dim))

    def forward(self, x):
        return x * torch.rsqrt(x.pow(2).mean(-1, keepdim=True) + self.eps) * self.weight


class BatchNorm1d(nn.Module):
    """Normalise each feature over the batch; running statistics for inference."""

    def __init__(self, dim: int, eps: float = 1e-5, momentum: float = 0.1):
        super().__init__()
        self.eps, self.momentum = eps, momentum
        self.weight = nn.Parameter(torch.ones(dim))
        self.bias = nn.Parameter(torch.zeros(dim))
        self.register_buffer("running_mean", torch.zeros(dim))
        self.register_buffer("running_var", torch.ones(dim))

    def forward(self, x):
        if self.training:
            mu = x.mean(0)
            var = x.var(0, unbiased=False)
            with torch.no_grad():
                n = x.shape[0]
                self.running_mean.mul_(1 - self.momentum).add_(self.momentum * mu)
                self.running_var.mul_(1 - self.momentum).add_(self.momentum * var * n / max(n - 1, 1))
        else:
            mu, var = self.running_mean, self.running_var
        return (x - mu) / torch.sqrt(var + self.eps) * self.weight + self.bias


class DeadReLUDetector:
    """Forward hooks on every ReLU: share of units that never fired over the observed batches."""

    def __init__(self, model: nn.Module):
        self.alive: dict[str, torch.Tensor] = {}
        self.handles = []
        for name, m in model.named_modules():
            if isinstance(m, nn.ReLU):
                self.handles.append(m.register_forward_hook(self._hook(name)))

    def _hook(self, name):
        def fn(_m, _inp, out):
            fired = (out.detach().reshape(-1, out.shape[-1]) > 0).any(0)
            self.alive[name] = fired if name not in self.alive else (self.alive[name] | fired)
        return fn

    def report(self) -> dict[str, float]:
        return {k: float(1.0 - v.float().mean()) for k, v in self.alive.items()}

    def remove(self):
        for h in self.handles:
            h.remove()


@dataclass
class History:
    train_loss: list = field(default_factory=list)
    val_loss: list = field(default_factory=list)
    val_acc: list = field(default_factory=list)
    grad_norm: list = field(default_factory=list)
    lr: list = field(default_factory=list)
    seconds: float = 0.0


def grad_norm(model: nn.Module) -> float:
    return math.sqrt(sum(float(p.grad.pow(2).sum()) for p in model.parameters() if p.grad is not None))


def fit(model: nn.Module, train, val, epochs: int = 10, lr: float = 1e-3, weight_decay: float = 0.0, clip: float | None = 1.0) -> History:
    """Reusable classification loop: AdamW, cosine LR, gradient clipping, per-epoch diagnostics."""
    opt = torch.optim.AdamW(model.parameters(), lr=lr, weight_decay=weight_decay)
    total = epochs * len(train)
    sched = torch.optim.lr_scheduler.LambdaLR(opt, lambda s: 0.5 * (1 + math.cos(math.pi * min(s, total) / total)))
    h = History()
    t0 = time.time()
    for _ in range(epochs):
        model.train()
        losses, norms = [], []
        for xb, yb in train:
            loss = F.cross_entropy(model(xb), yb)
            opt.zero_grad(set_to_none=True)
            loss.backward()
            norms.append(grad_norm(model))
            if clip:
                torch.nn.utils.clip_grad_norm_(model.parameters(), clip)
            opt.step()
            sched.step()
            losses.append(loss.item())
        h.train_loss.append(sum(losses) / len(losses))
        h.grad_norm.append(sum(norms) / len(norms))
        h.lr.append(sched.get_last_lr()[0])
        vl, va = evaluate(model, val)
        h.val_loss.append(vl)
        h.val_acc.append(va)
    h.seconds = time.time() - t0
    return h


@torch.no_grad()
def evaluate(model: nn.Module, loader) -> tuple[float, float]:
    model.eval()
    n, loss, correct = 0, 0.0, 0
    for xb, yb in loader:
        out = model(xb)
        loss += float(F.cross_entropy(out, yb, reduction="sum"))
        correct += int((out.argmax(1) == yb).sum())
        n += len(yb)
    return loss / n, correct / n


def digits_loaders(batch_size: int = 64, seed: int = 0):
    """scikit-learn's bundled 8x8 handwritten digits (1,797 images), 80/20 split."""
    from sklearn.datasets import load_digits

    d = load_digits()
    X = torch.tensor(d.data, dtype=torch.float32) / 16.0
    y = torch.tensor(d.target, dtype=torch.long)
    g = torch.Generator().manual_seed(seed)
    idx = torch.randperm(len(X), generator=g)
    cut = int(0.8 * len(X))
    tr, va = idx[:cut], idx[cut:]
    ds = torch.utils.data.TensorDataset
    return (torch.utils.data.DataLoader(ds(X[tr], y[tr]), batch_size=batch_size, shuffle=True, generator=g),
            torch.utils.data.DataLoader(ds(X[va], y[va]), batch_size=256))


class DigitClassifier(nn.Module):
    def __init__(self, hidden: int = 128, norm: str = "batch"):
        super().__init__()
        n = {"batch": BatchNorm1d, "layer": LayerNorm, "rms": RMSNorm, "none": lambda d: nn.Identity()}[norm]
        self.net = nn.Sequential(nn.Linear(64, hidden), n(hidden), nn.ReLU(), nn.Linear(hidden, hidden), n(hidden), nn.ReLU(), nn.Linear(hidden, 10))

    def forward(self, x):
        return self.net(x)
