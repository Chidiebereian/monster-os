"""Neural networks from scratch in numpy: a single neuron, dense layers with
hand-written backprop, a multilayer perceptron, weight initialisation and
numerical gradient checking."""
from __future__ import annotations

import numpy as np

from ..math.foundations import cross_entropy, cross_entropy_grad, relu, relu_grad, sigmoid, sigmoid_grad


def init_weights(fan_in: int, fan_out: int, scheme: str = "he", rng: np.random.Generator | None = None) -> np.ndarray:
    """xavier (tanh/sigmoid): var = 2/(in+out); he (ReLU): var = 2/in; normal: 0.01 std."""
    rng = rng or np.random.default_rng(0)
    if scheme == "xavier":
        std = np.sqrt(2.0 / (fan_in + fan_out))
    elif scheme == "he":
        std = np.sqrt(2.0 / fan_in)
    elif scheme == "normal":
        std = 0.01
    else:
        raise ValueError(f"unknown init {scheme!r}")
    return rng.normal(0.0, std, size=(fan_in, fan_out))


class Neuron:
    """One sigmoid neuron trained with binary cross-entropy."""

    def __init__(self, n_in: int, seed: int = 0):
        rng = np.random.default_rng(seed)
        self.w = rng.normal(0, 0.1, n_in)
        self.b = 0.0

    def forward(self, X):
        return sigmoid(np.asarray(X, dtype=float) @ self.w + self.b)

    def loss(self, X, y) -> float:
        p = np.clip(self.forward(X), 1e-12, 1 - 1e-12)
        return float(-(y * np.log(p) + (1 - y) * np.log(1 - p)).mean())

    def step(self, X, y, lr: float = 0.5) -> float:
        X = np.asarray(X, dtype=float)
        p = self.forward(X)
        # d(BCE)/dz = p - y for a sigmoid output
        dz = (p - y) / len(y)
        self.w -= lr * X.T @ dz
        self.b -= lr * dz.sum()
        return self.loss(X, y)


class Dense:
    def __init__(self, n_in: int, n_out: int, activation: str = "relu", init: str = "he", rng=None):
        self.W = init_weights(n_in, n_out, init, rng)
        self.b = np.zeros(n_out)
        self.activation = activation
        self.x = self.z = None
        self.dW = self.db = None

    def forward(self, x):
        self.x = x
        self.z = x @ self.W + self.b
        if self.activation == "relu":
            return relu(self.z)
        if self.activation == "sigmoid":
            return sigmoid(self.z)
        return self.z  # linear

    def backward(self, grad_out):
        if self.activation == "relu":
            grad_out = grad_out * relu_grad(self.z)
        elif self.activation == "sigmoid":
            grad_out = grad_out * sigmoid_grad(self.z)
        self.dW = self.x.T @ grad_out
        self.db = grad_out.sum(axis=0)
        return grad_out @ self.W.T

    def params(self):
        return [(self, "W", "dW"), (self, "b", "db")]


class MLP:
    """Multilayer perceptron with softmax cross-entropy output and manual backprop."""

    def __init__(self, sizes: list[int], hidden_activation: str = "relu", init: str = "he", seed: int = 0):
        rng = np.random.default_rng(seed)
        self.layers = [Dense(sizes[i], sizes[i + 1], hidden_activation if i < len(sizes) - 2 else "linear", init, rng) for i in range(len(sizes) - 1)]

    def forward(self, X):
        h = np.asarray(X, dtype=float)
        for layer in self.layers:
            h = layer.forward(h)
        return h  # logits

    def loss(self, X, y) -> float:
        return cross_entropy(self.forward(X), y)

    def backward(self, X, y) -> float:
        logits = self.forward(X)
        loss = cross_entropy(logits, y)
        g = cross_entropy_grad(logits, y)
        for layer in reversed(self.layers):
            g = layer.backward(g)
        return loss

    def step(self, X, y, lr: float = 0.1) -> float:
        loss = self.backward(X, y)
        for layer in self.layers:
            layer.W -= lr * layer.dW
            layer.b -= lr * layer.db
        return loss

    def accuracy(self, X, y) -> float:
        return float((self.forward(X).argmax(axis=1) == np.asarray(y)).mean())


def numerical_grad(f, x: np.ndarray, eps: float = 1e-5) -> np.ndarray:
    """Central-difference gradient of scalar f at x (x is perturbed in place, then restored)."""
    g = np.zeros_like(x)
    it = np.nditer(x, flags=["multi_index"], op_flags=["readwrite"])
    while not it.finished:
        i = it.multi_index
        old = x[i]
        x[i] = old + eps
        fp = f()
        x[i] = old - eps
        fm = f()
        x[i] = old
        g[i] = (fp - fm) / (2 * eps)
        it.iternext()
    return g


def gradient_check(model: MLP, X, y, eps: float = 1e-5) -> float:
    """Max relative error between backprop and numerical gradients over all parameters."""
    model.backward(X, y)
    worst = 0.0
    for layer in model.layers:
        for name, gname in (("W", "dW"), ("b", "db")):
            p = getattr(layer, name)
            analytic = getattr(layer, gname).copy()
            numeric = numerical_grad(lambda: model.loss(X, y), p, eps)
            denom = np.maximum(1e-8, np.abs(analytic) + np.abs(numeric))
            worst = max(worst, float((np.abs(analytic - numeric) / denom).max()))
    return worst
