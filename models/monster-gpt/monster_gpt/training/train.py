"""Training loop for next-token prediction.

AdamW (decay on matrices only), linear warmup + cosine decay, gradient
clipping, optional gradient accumulation / checkpointing / bfloat16 autocast,
staged sequence lengths (short first, then the full window), periodic
validation loss + perplexity, JSONL metrics (loss, lr, grad norm, tokens/s,
memory), and atomic checkpoints that resume exactly (step, optimizer, RNG).
"""
from __future__ import annotations

import json
import math
import time
from dataclasses import asdict, dataclass, field
from pathlib import Path

import psutil
import torch

from .checkpoint import load_checkpoint, save_checkpoint


@dataclass
class TrainConfig:
    out_dir: str = "runs/micro"
    max_steps: int = 2000
    batch_size: int = 16
    grad_accum: int = 1
    lr: float = 1e-3
    min_lr: float = 1e-4
    warmup: int = 100
    weight_decay: float = 0.1
    grad_clip: float = 1.0
    eval_interval: int = 200
    eval_iters: int = 20
    log_interval: int = 20
    ckpt_interval: int = 200
    seed: int = 1337
    device: str = "cpu"
    dtype: str = "float32"            # float32 | bfloat16 (autocast)
    grad_checkpointing: bool = False
    # (until_step, seq_len): e.g. [(500, 128)] trains on 128 tokens until step 500, then the full block
    seq_stages: list = field(default_factory=list)
    threads: int = 0


def lr_at(step: int, c: TrainConfig) -> float:
    if step < c.warmup:
        return c.lr * (step + 1) / c.warmup
    progress = min(1.0, (step - c.warmup) / max(1, c.max_steps - c.warmup))
    return c.min_lr + 0.5 * (c.lr - c.min_lr) * (1 + math.cos(math.pi * progress))


def seq_len_at(step: int, c: TrainConfig, block: int) -> int:
    for until, L in sorted(c.seq_stages):
        if step < until:
            return min(L, block)
    return block


def get_batch(tokens: torch.Tensor, L: int, b: int, g: torch.Generator, device: str):
    ix = torch.randint(0, len(tokens) - L - 1, (b,), generator=g)
    x = torch.stack([tokens[i : i + L] for i in ix])
    y = torch.stack([tokens[i + 1 : i + L + 1] for i in ix])
    return x.to(device), y.to(device)


@torch.no_grad()
def estimate_loss(model, tokens: torch.Tensor, L: int, b: int, iters: int, device: str, seed: int = 0) -> float:
    """Mean loss over `iters` fixed random batches (same batches every eval)."""
    was = model.training
    model.eval()
    g = torch.Generator().manual_seed(seed)
    losses = []
    for _ in range(iters):
        x, y = get_batch(tokens, L, b, g, device)
        losses.append(float(model(x, y)[1]))
    model.train(was)
    return sum(losses) / len(losses)


def train(model, train_tokens, val_tokens, c: TrainConfig, resume: bool = True, on_log=None, stop_after: int | None = None) -> list[dict]:
    """Train to c.max_steps (or stop early at `stop_after`, checkpointing there, without changing the schedule)."""
    if c.threads:
        torch.set_num_threads(c.threads)
    out = Path(c.out_dir)
    out.mkdir(parents=True, exist_ok=True)
    ckpt = out / "ckpt.pt"
    model.to(c.device)
    model.grad_checkpointing = c.grad_checkpointing
    opt = model.configure_optimizer(c.lr, c.weight_decay)
    train_t = torch.as_tensor(train_tokens, dtype=torch.long)
    val_t = torch.as_tensor(val_tokens, dtype=torch.long)
    g = torch.Generator().manual_seed(c.seed)
    torch.manual_seed(c.seed)
    step, history = 0, []
    if resume and ckpt.exists():
        st = load_checkpoint(ckpt, model, opt)
        step, history = st["step"], st["history"]
        g.set_state(st["rng"]["batch"])
        torch.set_rng_state(st["rng"]["torch"])
        print(f"resumed from step {step}")
    block = model.cfg.block_size
    proc = psutil.Process()
    log_path = out / "metrics.jsonl"
    amp = torch.autocast(device_type="cpu" if c.device == "cpu" else "cuda", dtype=torch.bfloat16) if c.dtype == "bfloat16" else None
    model.train()
    t_last, toks_since = time.time(), 0
    end = min(c.max_steps, stop_after or c.max_steps)
    while step < end:
        lr = lr_at(step, c)
        for grp in opt.param_groups:
            grp["lr"] = lr
        L = seq_len_at(step, c, block)
        loss_acc = 0.0
        for _ in range(c.grad_accum):
            x, y = get_batch(train_t, L, c.batch_size, g, c.device)
            if amp:
                with amp:
                    _, loss = model(x, y)
            else:
                _, loss = model(x, y)
            (loss / c.grad_accum).backward()
            loss_acc += loss.item() / c.grad_accum
            toks_since += x.numel()
        gn = float(torch.nn.utils.clip_grad_norm_(model.parameters(), c.grad_clip if c.grad_clip else float("inf")))
        opt.step()
        opt.zero_grad(set_to_none=True)
        step += 1
        row = None
        if step % c.log_interval == 0 or step == 1:
            dt = time.time() - t_last
            row = {"step": step, "loss": round(loss_acc, 4), "lr": lr, "grad_norm": round(gn, 4), "seq_len": L,
                   "tokens_per_s": round(toks_since / max(dt, 1e-9)), "rss_mb": round(proc.memory_info().rss / 2**20)}
            t_last, toks_since = time.time(), 0
        if step % c.eval_interval == 0 or step == c.max_steps:
            vl = estimate_loss(model, val_t, min(block, len(val_t) - 2), c.batch_size, c.eval_iters, c.device)
            tl = estimate_loss(model, train_t, block, c.batch_size, c.eval_iters, c.device)
            row = (row or {"step": step}) | {"train_loss_eval": round(tl, 4), "val_loss": round(vl, 4), "val_ppl": round(math.exp(vl), 2)}
        if row:
            history.append(row)
            with log_path.open("a", encoding="utf-8") as f:
                f.write(json.dumps(row) + "\n")
            if on_log:
                on_log(row)
        if step % c.ckpt_interval == 0 or step == end:
            save_checkpoint(ckpt, model, opt, step, model.cfg.to_dict(), asdict(c), history, {"torch": torch.get_rng_state(), "batch": g.get_state()})
    return history
