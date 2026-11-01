"""Monster GPT: a from-scratch GPT-style decoder (educational + pipeline
validation) for Monster OS. See models/monster-gpt/README.md.

Layout
  core/         config (validated, param count, memory estimate), model registry
  math/         gradient descent, activations, softmax, cross-entropy, linear regression (numpy)
  nn/           neuron, backprop, MLP, init, gradient checking (numpy); torch foundations
  nlp/          preprocessing, word embeddings, sentiment baseline, positional encodings, masks
  tokenizer/    byte-level BPE
  data/         corpus with provenance, deterministic splits, token datasets
  transformer/  causal multi-head / grouped-query attention with KV cache, blocks, GPT
  training/     training loop, schedule, checkpoints and resume
  inference/    sampling, streaming generation, batching, OpenAI-compatible server
  evaluation/   perplexity, KV-cache equivalence, throughput and sequence-length benchmarks
"""

__version__ = "0.1.0"
