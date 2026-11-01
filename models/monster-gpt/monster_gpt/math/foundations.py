"""Mathematical foundations in numpy: gradient descent, activations, softmax,
cross-entropy and linear regression (forward pass + training)."""
from __future__ import annotations

from typing import Callable

import numpy as np


def gradient_descent(grad: Callable[[np.ndarray], np.ndarray], x0, lr: float = 0.1, steps: int = 100, tol: float = 0.0):
    """Minimise a function given its gradient. Returns (x, path)."""
    x = np.asarray(x0, dtype=float).copy()
    path = [x.copy()]
    for _ in range(steps):
        g = grad(x)
        x = x - lr * g
        path.append(x.copy())
        if tol and np.linalg.norm(g) < tol:
            break
    return x, np.array(path)


def sigmoid(x):
    x = np.asarray(x, dtype=float)
    # numerically stable for large |x|
    out = np.empty_like(x)
    pos = x >= 0
    out[pos] = 1.0 / (1.0 + np.exp(-x[pos]))
    e = np.exp(x[~pos])
    out[~pos] = e / (1.0 + e)
    return out


def sigmoid_grad(x):
    s = sigmoid(x)
    return s * (1.0 - s)


def relu(x):
    return np.maximum(0.0, np.asarray(x, dtype=float))


def relu_grad(x):
    return (np.asarray(x) > 0).astype(float)


def softmax(z, axis: int = -1):
    z = np.asarray(z, dtype=float)
    z = z - z.max(axis=axis, keepdims=True)  # shift for stability
    e = np.exp(z)
    return e / e.sum(axis=axis, keepdims=True)


def cross_entropy(logits, targets) -> float:
    """Mean cross-entropy of integer targets under softmax(logits). logits: (n, c)."""
    logits = np.asarray(logits, dtype=float)
    t = np.asarray(targets, dtype=int)
    z = logits - logits.max(axis=1, keepdims=True)
    log_probs = z - np.log(np.exp(z).sum(axis=1, keepdims=True))
    return float(-log_probs[np.arange(len(t)), t].mean())


def cross_entropy_grad(logits, targets):
    """d(mean CE)/d(logits) = (softmax - onehot) / n."""
    p = softmax(logits, axis=1)
    t = np.asarray(targets, dtype=int)
    p[np.arange(len(t)), t] -= 1.0
    return p / len(t)


def linear_regression_predict(X, w, b):
    return np.asarray(X, dtype=float) @ w + b


def linear_regression_fit(X, y, lr: float = 0.05, epochs: int = 2000):
    """Fit y ~ Xw + b by gradient descent on mean squared error. Returns (w, b, losses)."""
    X = np.asarray(X, dtype=float)
    y = np.asarray(y, dtype=float)
    n, d = X.shape
    w, b = np.zeros(d), 0.0
    losses = []
    for _ in range(epochs):
        err = linear_regression_predict(X, w, b) - y
        losses.append(float((err ** 2).mean()))
        w -= lr * (2.0 / n) * X.T @ err
        b -= lr * (2.0 / n) * err.sum()
    return w, b, losses
