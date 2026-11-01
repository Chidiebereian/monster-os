"""Tokenizer, data pipeline, attention/GQA, KV cache, GPT, training, checkpoints, generation."""
import math

import pytest
import torch

from monster_gpt.core import GPTConfig
from monster_gpt.core.config import PRESETS
from monster_gpt.data import Document, TokenDataset, split_documents
from monster_gpt.evaluation import kv_cache_max_diff, perplexity
from monster_gpt.inference import generate, generate_batch, sample_next
from monster_gpt.tokenizer import ByteBPE
from monster_gpt.training import TrainConfig, load_checkpoint, train
from monster_gpt.transformer import GPT, CausalSelfAttention, reference_attention

TEXT = ("MonsterGPT explains bets. The model gives Cale Makar under 1.5 points an 81% chance; the market says 65%. "
        "Edge = model - market. Fractional Kelly sizes the stake. ") * 30


# ----------------------------------------------------------------- tokenizer
@pytest.fixture(scope="module")
def tok():
    return ByteBPE.train(TEXT, vocab_size=400)


@pytest.mark.parametrize("s", ["", "hello", "  leading and trailing  ", "Ünïcödé café naïve", "日本語のテキスト", "emoji 🏒🏀⚾ and 👨‍👩‍👧",
                               "tabs\tand\nnewlines\r\n", "\x00\x01 control", "a" * 1000, "<|endoftext|> inside text", "mixed 123,456.78 +150 -230"])
def test_bpe_round_trips_any_text(tok, s):
    assert tok.decode(tok.encode(s)) == s


def test_bpe_compresses_and_is_deterministic(tok):
    ids = tok.encode(TEXT[:500])
    assert len(ids) < len(TEXT[:500].encode()) / 2
    assert ByteBPE.train(TEXT, vocab_size=400).merges == tok.merges
    # merges stop early once no pair occurs twice (min_freq), so the vocab can end below the target
    assert 256 < tok.vocab_size <= 400


def test_bpe_special_token_and_save_load(tok, tmp_path):
    ids = tok.encode("a<|endoftext|>b")
    assert tok.eot in ids
    assert tok.eot not in tok.encode("a<|endoftext|>b", allow_special=False)
    tok.save(tmp_path / "t.json")
    t2 = ByteBPE.load(tmp_path / "t.json")
    assert t2.encode(TEXT[:300]) == tok.encode(TEXT[:300]) and t2.vocab_size == tok.vocab_size


def test_bpe_invalid_utf8_is_replaced_not_crashing(tok):
    assert tok.decode([0xFF, 0xFE]) == "��"


# ----------------------------------------------------------------- data
def test_splits_are_deterministic_and_stable_under_additions():
    docs = [Document(id=str(i), source="s", license="l", text=f"document number {i}") for i in range(400)]
    a = split_documents(docs)
    b = split_documents(docs + [Document(id="new", source="s", license="l", text="brand new")])
    for k in ("train", "val", "test"):
        assert {d.id for d in a[k]} <= {d.id for d in b[k]}
    assert 0 < len(a["val"]) < 60 and 0 < len(a["test"]) < 60


def test_token_dataset_shifts_targets():
    ds = TokenDataset(list(range(100)), block_size=10)
    x, y = ds[2]
    assert x.tolist() == list(range(20, 30)) and y.tolist() == list(range(21, 31))
    with pytest.raises(ValueError):
        TokenDataset([1, 2, 3], block_size=10)


# ----------------------------------------------------------------- config
def test_config_validation_and_param_count():
    with pytest.raises(ValueError):
        GPTConfig(n_embd=100, n_head=3)
    with pytest.raises(ValueError):
        GPTConfig(n_head=4, n_kv_head=3)
    for name, p in PRESETS.items():
        for pos in ("learned", "rope", "sinusoidal"):
            cfg = GPTConfig(**{**p, "pos": pos})
            assert cfg.param_count() == GPT(cfg).num_params(), (name, pos)
    micro = GPTConfig(**PRESETS["micro"])
    assert 1_000_000 <= micro.param_count() <= 10_000_000
    est = micro.memory_estimate(batch_size=16)
    assert est["total"] > est["weights"] > 0


# ----------------------------------------------------------------- attention
def test_causal_mask_blocks_the_future():
    torch.manual_seed(0)
    cfg = GPTConfig(**PRESETS["nano"], dropout=0.0)
    m = GPT(cfg).eval()
    x = torch.randint(0, cfg.vocab_size, (1, 20))
    a, _ = m(x)
    x2 = x.clone()
    x2[0, 10:] = (x2[0, 10:] + 1) % cfg.vocab_size  # change only the future
    b, _ = m(x2)
    assert torch.allclose(a[0, :10], b[0, :10], atol=1e-6)
    assert not torch.allclose(a[0, 10:], b[0, 10:])


def test_gqa_is_real_and_matches_mha_with_shared_heads():
    torch.manual_seed(1)
    d, h, kvh = 32, 8, 2
    gqa = CausalSelfAttention(d, h, kvh, efficient=False).eval()
    mha = CausalSelfAttention(d, h, h, efficient=False).eval()
    # GQA really has smaller K/V projections
    assert gqa.kv.weight.shape == (2 * kvh * d // h, d) and mha.kv.weight.shape == (2 * d, d)
    # copy weights: each MHA K/V head = the GQA head its group shares
    hd, group = d // h, h // kvh
    with torch.no_grad():
        mha.q.weight.copy_(gqa.q.weight)
        mha.proj.weight.copy_(gqa.proj.weight)
        k, v = gqa.kv.weight.view(2, kvh, hd, d)
        mha.kv.weight.copy_(torch.cat([k.repeat_interleave(group, 0).reshape(h * hd, d), v.repeat_interleave(group, 0).reshape(h * hd, d)]))
    x = torch.randn(2, 9, d)
    assert torch.allclose(gqa(x), mha(x), atol=1e-6)


def test_efficient_attention_equals_reference():
    torch.manual_seed(2)
    q, k, v = torch.randn(2, 4, 7, 8), torch.randn(2, 4, 7, 8), torch.randn(2, 4, 7, 8)
    ref = reference_attention(q, k, v)
    fast = torch.nn.functional.scaled_dot_product_attention(q, k, v, is_causal=True)
    assert torch.allclose(ref, fast, atol=1e-5)


@pytest.mark.parametrize("pos,kvh", [("learned", 4), ("rope", 2), ("sinusoidal", 1)])
def test_kv_cache_equals_uncached(pos, kvh):
    torch.manual_seed(3)
    cfg = GPTConfig(vocab_size=200, block_size=48, n_layer=2, n_head=4, n_kv_head=kvh, n_embd=32, dropout=0.0, pos=pos)
    m = GPT(cfg)
    diff = kv_cache_max_diff(m, list(range(5, 25)), steps=10)
    assert diff < 1e-4, diff


def test_gqa_cache_is_smaller():
    full = GPT(GPTConfig(vocab_size=100, block_size=64, n_layer=2, n_head=8, n_kv_head=8, n_embd=64)).new_cache(1)
    gqa = GPT(GPTConfig(vocab_size=100, block_size=64, n_layer=2, n_head=8, n_kv_head=2, n_embd=64)).new_cache(1)
    assert gqa.nbytes() * 4 == full.nbytes()


def test_context_length_is_enforced_and_rope_interpolation_extends_it():
    cfg = GPTConfig(vocab_size=50, block_size=16, n_layer=1, n_head=2, n_kv_head=1, n_embd=16, pos="learned")
    m = GPT(cfg)
    with pytest.raises(ValueError):
        m(torch.zeros(1, 17, dtype=torch.long))
    long = GPT(GPTConfig(vocab_size=50, block_size=16, n_layer=1, n_head=2, n_kv_head=1, n_embd=16, pos="rope", rope_scale=2.0))
    assert long.max_positions == 32
    long(torch.zeros(1, 32, dtype=torch.long))
    # generation never exceeds the context, even when asked for more
    out = list(generate(m, list(range(10)), max_new_tokens=100, temperature=0.0))
    assert len(out) <= 15


# ----------------------------------------------------------------- loss, training, checkpoints
def test_initial_loss_is_about_log_vocab_and_gradients_flow():
    torch.manual_seed(4)
    cfg = GPTConfig(**PRESETS["nano"], dropout=0.0)
    m = GPT(cfg)
    x = torch.randint(0, cfg.vocab_size, (4, 32))
    _, loss = m(x, torch.randint(0, cfg.vocab_size, (4, 32)))
    assert abs(float(loss) - math.log(cfg.vocab_size)) < 0.5
    loss.backward()
    assert all(p.grad is not None and torch.isfinite(p.grad).all() for p in m.parameters())
    assert m.blocks[0].attn.q.weight.grad.abs().sum() > 0


def test_training_converges_on_tiny_corpus_and_resumes_exactly(tok, tmp_path):
    ids = tok.encode(TEXT)
    cut = int(0.9 * len(ids))
    cfg = GPTConfig(vocab_size=tok.vocab_size, block_size=32, n_layer=2, n_head=2, n_kv_head=1, n_embd=48, dropout=0.0, pos="rope", norm="rmsnorm")
    tc = TrainConfig(out_dir=str(tmp_path / "run"), max_steps=60, batch_size=8, lr=3e-3, warmup=5, eval_interval=30, eval_iters=4,
                     log_interval=10, ckpt_interval=30, seq_stages=[(20, 16)])
    torch.manual_seed(0)
    m = GPT(cfg)
    hist = train(m, ids[:cut], ids[cut:], tc)
    first, last = hist[0]["loss"], hist[-1]["val_loss"]
    assert last < first * 0.5, (first, last)
    assert any(r.get("seq_len") == 16 for r in hist) and hist[-1].get("val_ppl")
    # checkpoint round trip: same logits
    m2 = GPT(cfg)
    st = load_checkpoint(tmp_path / "run" / "ckpt.pt", m2)
    x = torch.tensor([ids[:20]])
    m.eval(); m2.eval()
    assert torch.equal(m(x)[0], m2(x)[0]) and st["step"] == 60
    # resume: a run stopped at 30 then continued matches an uninterrupted run
    tc_a = TrainConfig(**{**tc.__dict__, "out_dir": str(tmp_path / "a")})
    torch.manual_seed(0); ma = GPT(cfg); train(ma, ids[:cut], ids[cut:], tc_a, stop_after=30)
    torch.manual_seed(123); ma2 = GPT(cfg); train(ma2, ids[:cut], ids[cut:], tc_a, resume=True)
    ma2.eval()
    assert torch.allclose(ma2(x)[0], m(x)[0], atol=1e-5)


def test_perplexity_and_generation_after_training(tok, tmp_path):
    ids = tok.encode(TEXT)
    cfg = GPTConfig(vocab_size=tok.vocab_size, block_size=32, n_layer=2, n_head=2, n_kv_head=2, n_embd=48, dropout=0.0)
    torch.manual_seed(0)
    m = GPT(cfg)
    train(m, ids, ids, TrainConfig(out_dir=str(tmp_path / "g"), max_steps=80, batch_size=8, lr=3e-3, warmup=5, eval_interval=80, eval_iters=2, log_interval=40, ckpt_interval=80))
    ppl = perplexity(m, ids)
    assert ppl["perplexity"] < 10
    prompt = tok.encode("Fractional Kelly")
    text = tok.decode(list(generate(m, prompt, 12, temperature=0.0)))
    assert "stake" in text or "sizes" in text, text
    # batched generation matches one-by-one greedy decoding
    b = generate_batch(m, [prompt, prompt, tok.encode("The model")], max_new_tokens=6)
    assert b[0] == b[1] == list(generate(m, prompt, 6, temperature=0.0))


def test_sampling_controls():
    logits = torch.tensor([[0.0, 1.0, 5.0, 2.0]])
    assert int(sample_next(logits, temperature=0)[0]) == 2
    g = torch.Generator().manual_seed(0)
    picks = {int(sample_next(logits, temperature=1.0, top_k=2, generator=g)[0]) for _ in range(200)}
    assert picks <= {2, 3}
    picks = {int(sample_next(logits, temperature=1.0, top_p=0.5, generator=g)[0]) for _ in range(200)}
    assert picks == {2}
