"""Checkpoints: model + optimizer + step + RNG state + history, written
atomically (temp file then rename) so a crash never leaves a half-written file."""
from __future__ import annotations

import os
from pathlib import Path

import torch


def save_checkpoint(path: str | Path, model, optimizer, step: int, model_cfg: dict, train_cfg: dict, history: list, rng: dict | None = None) -> None:
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    state = {
        "model": model.state_dict(),
        "optimizer": optimizer.state_dict() if optimizer is not None else None,
        "step": step,
        "model_cfg": model_cfg,
        "train_cfg": train_cfg,
        "history": history,
        "rng": rng or {"torch": torch.get_rng_state()},
    }
    tmp = path.with_suffix(path.suffix + ".tmp")
    torch.save(state, tmp)
    os.replace(tmp, path)


def load_checkpoint(path: str | Path, model=None, optimizer=None, map_location="cpu") -> dict:
    state = torch.load(path, map_location=map_location, weights_only=False)
    if model is not None:
        model.load_state_dict(state["model"])
    if optimizer is not None and state.get("optimizer"):
        optimizer.load_state_dict(state["optimizer"])
    return state
