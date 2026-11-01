"""Train Monster GPT Micro end to end and write a report.

    cd models/monster-gpt
    python scripts/train_micro.py --steps 1500            # fresh or resumed run in runs/micro
    python scripts/train_micro.py --steps 1500 --report   # evaluation + benchmarks only

Steps: corpus (provenance manifest) -> byte-level BPE -> token splits ->
memory estimate -> training (staged sequence length, checkpoints, resume) ->
test perplexity, KV-cache check, inference and sequence-length benchmarks,
long-context probe, samples -> runs/micro/report.json + registry entry.
"""
from __future__ import annotations

import argparse
import json
import sys
import time
from pathlib import Path

import numpy as np
import torch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from monster_gpt.core import GPTConfig, ModelRegistry  # noqa: E402
from monster_gpt.core.config import PRESETS  # noqa: E402
from monster_gpt.data.corpus import build_corpus  # noqa: E402
from monster_gpt.evaluation import inference_benchmark, kv_cache_max_diff, long_context_probe, perplexity, sequence_length_benchmark  # noqa: E402
from monster_gpt.inference import generate  # noqa: E402
from monster_gpt.tokenizer import ByteBPE  # noqa: E402
from monster_gpt.training import TrainConfig, load_checkpoint, train  # noqa: E402
from monster_gpt.transformer import GPT  # noqa: E402


def main():
    # samples can hold any character; a redirected Windows console is cp1252
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    ap = argparse.ArgumentParser()
    ap.add_argument("--run", default=str(ROOT / "runs" / "micro"))
    ap.add_argument("--steps", type=int, default=1500)
    ap.add_argument("--vocab", type=int, default=4096)
    ap.add_argument("--batch", type=int, default=16)
    ap.add_argument("--threads", type=int, default=8)
    ap.add_argument("--report", action="store_true", help="skip training; evaluate the checkpoint")
    a = ap.parse_args()
    torch.set_num_threads(a.threads)
    run = Path(a.run)
    run.mkdir(parents=True, exist_ok=True)
    data = ROOT / "data" / "corpus"

    # 1. corpus
    manifest = build_corpus(ROOT.parents[1], data) if not (data / "manifest.json").exists() else json.loads((data / "manifest.json").read_text())
    print("corpus:", {k: v["chars"] for k, v in manifest["splits"].items()})

    # 2. tokenizer (trained on the train split only)
    tok_path = run / "tokenizer.json"
    if tok_path.exists():
        tok = ByteBPE.load(tok_path)
    else:
        t0 = time.time()
        tok = ByteBPE.train((data / "train.txt").read_text(encoding="utf-8"), a.vocab)
        tok.save(tok_path)
        print(f"tokenizer: {tok.vocab_size} tokens in {time.time() - t0:.0f}s")

    # 3. tokens
    splits = {}
    for s in ("train", "val", "test"):
        f = run / f"{s}.npy"
        if not f.exists():
            np.save(f, np.array(tok.encode((data / f"{s}.txt").read_text(encoding="utf-8")), dtype=np.int32))
        splits[s] = np.load(f).astype(np.int64)
    print("tokens:", {k: len(v) for k, v in splits.items()}, f"({len(manifest['splits'] and (data / 'train.txt').read_text(encoding='utf-8')) / len(splits['train']):.2f} chars/token)")

    # 4. model + memory estimate
    cfg = GPTConfig(**{**PRESETS["micro"], "vocab_size": tok.vocab_size, "dropout": 0.1, "rope_scale": 1.0})
    cfg.save(run / "config.json")
    est = cfg.memory_estimate(a.batch)
    print(f"model: {cfg.param_count():,} params; training memory estimate {est['total_mb']} MB at batch {a.batch} x {cfg.block_size}")
    model = GPT(cfg)
    tc = TrainConfig(out_dir=str(run), max_steps=a.steps, batch_size=a.batch, lr=1e-3, min_lr=1e-4, warmup=100, weight_decay=0.1,
                     eval_interval=250, eval_iters=20, log_interval=50, ckpt_interval=250, seq_stages=[(300, 128)], threads=a.threads)

    # 5. train (resumes from runs/micro/ckpt.pt if present)
    t0 = time.time()
    if not a.report:
        train(model, splits["train"], splits["val"], tc, on_log=lambda r: print(json.dumps(r)))
    state = load_checkpoint(run / "ckpt.pt", model)
    train_secs = time.time() - t0
    model.eval()

    # 6. evaluation
    report = {"model": "monster-gpt-micro", "params": model.num_params(), "config": cfg.to_dict(), "steps": state["step"],
              "train_seconds_this_run": round(train_secs), "tokenizer": {"type": "byte-bpe", "vocab_size": tok.vocab_size},
              "data": {"splits": manifest["splits"], "preprocessing": manifest["preprocessing"],
                       "licenses": sorted({d["license"] for d in manifest["documents"]})},
              "history_tail": state["history"][-3:]}
    report["test"] = perplexity(model, splits["test"])
    report["val"] = perplexity(model, splits["val"])
    report["train_sample"] = perplexity(model, splits["train"][:200_000])
    report["kv_cache_max_abs_logit_diff"] = kv_cache_max_diff(model, splits["val"][:64].tolist(), steps=16)
    report["inference"] = inference_benchmark(model, prompt_len=64, new_tokens=64)

    def factory(L):
        c = GPTConfig(**{**cfg.to_dict(), "rope_scale": max(1.0, L / cfg.block_size), "dropout": 0.0})
        m = GPT(c)
        m.load_state_dict(model.state_dict())
        return m
    report["sequence_lengths"] = sequence_length_benchmark(factory, lengths=(128, 256, 512, 1024, 2048), batch=2, steps=2)
    # long context: same weights, positional interpolation to 2x / 4x
    filler = (data / "val.txt").read_text(encoding="utf-8")[:4000]
    report["long_context"] = {}
    for scale in (1, 2, 4):
        m = factory(cfg.block_size * scale)
        m.eval()
        L = cfg.block_size * scale
        vt = torch.as_tensor(splits["val"])
        ppl = perplexity(m, vt, block=L, max_windows=20) if len(vt) > L + 1 else None
        report["long_context"][f"{L}"] = {"rope_scale": scale, "val_ppl": ppl and ppl["perplexity"], "needle": long_context_probe(m, tok, filler, length=L)}

    prompts = ["The model", "def ", "AI Picks", "Kelly"]
    report["samples"] = {p: tok.decode(list(generate(model, tok.encode(p), 60, temperature=0.7, top_k=40, top_p=0.95, seed=0))) for p in prompts}
    (run / "report.json").write_text(json.dumps(report, indent=1), encoding="utf-8")
    ModelRegistry(ROOT / "runs" / "registry.jsonl").register("monster-gpt-micro", str(run / "ckpt.pt"), cfg.to_dict(),
        {"test_ppl": report["test"]["perplexity"], "val_ppl": report["val"]["perplexity"], "steps": state["step"]},
        {"manifest": str(data / "manifest.json"), "licenses": report["data"]["licenses"]}, notes="scratch-trained pipeline validation model")
    print(json.dumps({k: report[k] for k in ("params", "steps", "test", "val", "train_sample", "kv_cache_max_abs_logit_diff", "inference")}, indent=1))
    print(json.dumps(report["long_context"], indent=1))
    print(json.dumps(report["sequence_lengths"], indent=1))
    for p, s in report["samples"].items():
        print(f"--- {p!r}\n{s}")


if __name__ == "__main__":
    main()
