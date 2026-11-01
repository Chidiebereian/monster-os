"""Math, from-scratch networks, torch foundations and NLP basics."""
import numpy as np
import pytest
import torch
import torch.nn as nn

from monster_gpt.math import (cross_entropy, cross_entropy_grad, gradient_descent, linear_regression_fit, relu, relu_grad, sigmoid,
                              sigmoid_grad, softmax)
from monster_gpt.nlp import (BagOfWordsSentiment, SENTIMENT_EXAMPLES, Vocab, causal_mask, pad_batch, padding_mask, sinusoidal_positions,
                             train_skipgram, word_tokenize)
from monster_gpt.nn import MLP, Neuron, gradient_check, init_weights, numerical_grad
from monster_gpt.nn.torch_foundations import (BatchNorm1d, DeadReLUDetector, DigitClassifier, LayerNorm, RMSNorm, digits_loaders, fit)


# ----------------------------------------------------------------- math
def test_gradient_descent_finds_quadratic_minimum():
    x, path = gradient_descent(lambda v: 2 * (v - np.array([3.0, -1.0])), [0.0, 0.0], lr=0.1, steps=200)
    assert np.allclose(x, [3.0, -1.0], atol=1e-6)
    assert np.linalg.norm(path[0] - [3, -1]) > np.linalg.norm(path[10] - [3, -1])


def test_activations_and_their_gradients():
    x = np.array([-1000.0, -2.0, 0.0, 2.0, 1000.0])
    s = sigmoid(x)
    assert np.all(np.isfinite(s)) and s[0] == 0.0 and s[-1] == 1.0 and s[2] == 0.5
    for f, df in ((sigmoid, sigmoid_grad), (relu, relu_grad)):
        pts = np.array([-1.5, -0.3, 0.4, 2.0])
        num = (f(pts + 1e-6) - f(pts - 1e-6)) / 2e-6
        assert np.allclose(df(pts), num, atol=1e-6)


def test_softmax_is_stable_and_sums_to_one():
    p = softmax(np.array([[1000.0, 1000.0, 999.0], [1.0, 2.0, 3.0]]))
    assert np.allclose(p.sum(1), 1.0) and np.all(np.isfinite(p))
    assert np.allclose(softmax(np.array([1.0, 2.0])), softmax(np.array([101.0, 102.0])))


def test_cross_entropy_matches_torch_and_gradient_checks():
    rng = np.random.default_rng(0)
    logits, t = rng.normal(size=(5, 4)), rng.integers(0, 4, 5)
    ref = torch.nn.functional.cross_entropy(torch.tensor(logits), torch.tensor(t))
    assert abs(cross_entropy(logits, t) - float(ref)) < 1e-9
    num = numerical_grad(lambda: cross_entropy(logits, t), logits)
    assert np.allclose(cross_entropy_grad(logits, t), num, atol=1e-7)


def test_linear_regression_recovers_weights():
    rng = np.random.default_rng(1)
    X = rng.normal(size=(200, 3))
    y = X @ np.array([2.0, -1.0, 0.5]) + 4.0
    w, b, losses = linear_regression_fit(X, y, lr=0.1, epochs=500)
    assert np.allclose(w, [2.0, -1.0, 0.5], atol=1e-3) and abs(b - 4.0) < 1e-3
    assert losses[-1] < losses[0] * 1e-4


# ----------------------------------------------------------------- from scratch
def test_single_neuron_learns_and():
    X = np.array([[0, 0], [0, 1], [1, 0], [1, 1]], dtype=float)
    y = np.array([0, 0, 0, 1], dtype=float)
    n = Neuron(2)
    for _ in range(3000):
        n.step(X, y, lr=1.0)
    assert np.all((n.forward(X) > 0.5) == y.astype(bool))


@pytest.mark.parametrize("act", ["relu", "sigmoid"])
def test_mlp_backprop_matches_numerical_gradients(act):
    rng = np.random.default_rng(2)
    m = MLP([4, 6, 5, 3], hidden_activation=act, init="xavier", seed=3)
    X, y = rng.normal(size=(7, 4)), rng.integers(0, 3, 7)
    assert gradient_check(m, X, y) < 1e-5


def test_mlp_learns_xor():
    X = np.array([[0, 0], [0, 1], [1, 0], [1, 1]], dtype=float)
    y = np.array([0, 1, 1, 0])
    m = MLP([2, 16, 2], seed=4)
    for _ in range(2000):
        m.step(X, y, lr=0.5)
    assert m.accuracy(X, y) == 1.0


def test_init_scales():
    he = init_weights(1000, 1000, "he", np.random.default_rng(0))
    xa = init_weights(1000, 1000, "xavier", np.random.default_rng(0))
    assert abs(he.std() - np.sqrt(2 / 1000)) < 2e-3
    assert abs(xa.std() - np.sqrt(2 / 2000)) < 2e-3
    with pytest.raises(ValueError):
        init_weights(2, 2, "nope")


# ----------------------------------------------------------------- torch foundations
def test_norm_layers_match_reference_implementations():
    x = torch.randn(8, 16)
    ln, ref = LayerNorm(16), nn.LayerNorm(16)
    assert torch.allclose(ln(x), ref(x), atol=1e-5)
    rms = RMSNorm(16, eps=1e-6)
    assert torch.allclose(rms(x), x / torch.sqrt((x ** 2).mean(-1, keepdim=True) + 1e-6), atol=1e-6)
    bn, bref = BatchNorm1d(16), nn.BatchNorm1d(16)
    assert torch.allclose(bn(x), bref(x), atol=1e-5)
    assert torch.allclose(bn.running_var, bref.running_var, atol=1e-5)
    bn.eval(); bref.eval()
    assert torch.allclose(bn(x), bref(x), atol=1e-5)


def test_dead_relu_detector_finds_dead_units():
    net = nn.Sequential(nn.Linear(4, 8), nn.ReLU())
    with torch.no_grad():
        net[0].bias[:4] = -100.0  # half the units can never fire
    det = DeadReLUDetector(net)
    net(torch.randn(64, 4))
    frac = list(det.report().values())[0]
    det.remove()
    assert 0.5 <= frac < 1.0


def test_digit_classifier_trains_with_diagnostics():
    torch.manual_seed(0)
    tr, va = digits_loaders()
    h = fit(DigitClassifier(norm="batch"), tr, va, epochs=8, lr=3e-3)
    assert h.val_acc[-1] > 0.93, h.val_acc
    assert h.train_loss[-1] < h.train_loss[0]
    assert all(g > 0 for g in h.grad_norm)


# ----------------------------------------------------------------- NLP
def test_text_vocab_padding_and_masks():
    toks = word_tokenize("The O/U is 223.5 — don't chase!")
    assert "don't" in toks and "223" in toks
    v = Vocab(toks + toks[:2])
    ids = v.encode(toks, bos=True, eos=True)
    assert ids[0] == 2 and ids[-1] == 3 and v.decode(ids) == toks
    assert v.encode(["zzz-unseen"]) == [1]
    batch, lens = pad_batch([[5, 6, 7], [8]])
    assert batch.tolist() == [[5, 6, 7], [8, 0, 0]]
    assert padding_mask(lens, 3).tolist() == [[True, True, True], [True, False, False]]
    assert causal_mask(3).tolist() == [[True, False, False], [True, True, False], [True, True, True]]


def test_sinusoidal_positions_properties():
    pe = sinusoidal_positions(50, 16)
    assert pe.shape == (50, 16)
    assert torch.allclose(pe[0, 0::2], torch.zeros(8)) and torch.allclose(pe[0, 1::2], torch.ones(8))
    assert pe.abs().max() <= 1.0 + 1e-6
    assert len({tuple(r.round(decimals=4).tolist()) for r in pe}) == 50  # every position distinct


def test_skipgram_learns_cooccurrence():
    text = ("the puck hits the net . the goalie saves the puck . " * 40 + "the ball hits the rim . the guard shoots the ball . " * 40).split()
    v = Vocab(text)
    model, losses = train_skipgram(v.encode(text), len(v), dim=16, window=2, epochs=8, lr=0.02)
    assert losses[-1] < losses[0]
    near_puck = [v.itos[i] for i in model.similar(v.stoi["puck"], k=4)]
    assert set(near_puck) & {"goalie", "net", "saves"}, near_puck


def test_sentiment_baseline_fits_and_generalises_a_little():
    torch.manual_seed(0)
    texts, labels = zip(*SENTIMENT_EXAMPLES)
    v = Vocab([w for t in texts for w in word_tokenize(t)])
    m = BagOfWordsSentiment(v)
    losses = m.fit(list(texts), list(labels))
    assert losses[-1] < 0.1
    p = m.predict_proba(["great edge, love this price", "awful beat, terrible line"])
    assert p[0] > 0.5 > p[1]
