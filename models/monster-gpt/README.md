# Monster GPT (scratch-built)

A small GPT and its full training pipeline, written from first principles as
milestones M2 and M3 of `docs/architecture/monster-gpt.md`.

It is a learning and validation project. The model is a few million
parameters trained on a couple of MB of text, so it will not answer betting
questions. The MonsterGPT chat service uses an open-weight model
(`qwen2.5:3b` through Ollama) for that. This model can be served through the
same provider interface to prove the pipeline end to end.

## Layout

| Path | What it is |
| --- | --- |
| `monster_gpt/math/foundations.py` | gradient descent, sigmoid/ReLU, softmax, cross-entropy, linear regression |
| `monster_gpt/nn/scratch.py` | neuron, dense layer, MLP and backprop in plain Python; initializers; numerical gradient check |
| `monster_gpt/nn/torch_foundations.py` | LayerNorm, RMSNorm, BatchNorm1d, dead-ReLU detector, training loop, digit classifier |
| `monster_gpt/nlp/` | vocabulary and masks, sinusoidal positions, skip-gram embeddings, bag-of-words sentiment baseline |
| `monster_gpt/tokenizer/bpe.py` | byte-level BPE (incremental pair counts, lossless round trip) |
| `monster_gpt/data/` | corpus builder with a provenance manifest; deterministic sha256 splits; dataset |
| `monster_gpt/transformer/attention.py` | grouped-query attention with a smaller KV projection and cache, RoPE (with scaling), KV cache |
| `monster_gpt/transformer/gpt.py` | decoder-only GPT: learned, sinusoidal or RoPE positions, tied head, gradient checkpointing |
| `monster_gpt/training/` | warmup + cosine LR, staged sequence length, bf16 option, JSONL metrics, atomic checkpoints, exact resume |
| `monster_gpt/inference/` | sampling (temperature, top-k, top-p), streaming generation with KV cache, length-bucketed batching, OpenAI-compatible server |
| `monster_gpt/evaluation/bench.py` | perplexity, cached-vs-uncached check, throughput and sequence-length benchmarks, long-context probe |
| `monster_gpt/core/` | `GPTConfig` with validation, exact parameter count and memory estimate; presets `nano`, `micro`, `small`; model registry |

## Data

`monster_gpt/data/corpus.py` builds the corpus from the repo's own Markdown docs and
Python standard-library docstrings (PSF-2.0). Every document is listed in
`data/corpus/manifest.json` with its source and license. The split (train/val/test)
is by sha256 of the document id, so it is stable across runs. Data, runs and
checkpoints are git-ignored.

## Use

```bash
cd models/monster-gpt
python -m pytest tests -q                                   # 46 tests
python scripts/train_micro.py --steps 1500 --threads 6      # fresh or resumed run in runs/micro
python scripts/train_micro.py --steps 1500 --report         # evaluate the checkpoint, write runs/micro/report.json
python -m monster_gpt.inference.server --run runs/micro --port 8130
```

To chat with it through MonsterGPT, add it as an OpenAI-compatible model:
`LLM_PROVIDER=openai LLM_BASE_URL=http://localhost:8130/v1 LLM_MODEL=monster-gpt-micro`.

Training resumes exactly from `runs/micro/ckpt.pt` (model, optimizer, RNG and
data position). A test checks that a resumed run matches an uninterrupted one.

## Micro preset

vocab 4096, context 256, 4 layers, 4 query heads / 2 KV heads, width 256,
RoPE, RMSNorm: 3.9M parameters, about 508 MB to train at batch 16 × 256.
The tokenizer averages 3.46 characters per token on this corpus (419K training tokens).

## What the tests prove

- Hand-written backprop matches numerical gradients.
- The KV-cached generation produces the same logits as recomputing from scratch.
- Grouped-query attention matches a reference multi-head attention when the KV
  heads are expanded.
- The BPE tokenizer round-trips accents, CJK, emoji, control characters and special-token text losslessly.
- Resuming from a checkpoint gives the same losses as never stopping.
- The parameter count from `GPTConfig` equals PyTorch's count.
