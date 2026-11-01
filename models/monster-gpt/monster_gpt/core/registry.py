"""A tiny file-based model registry: one JSON line per trained checkpoint with
its config, tokenizer, data provenance and evaluation numbers."""
from __future__ import annotations

import json
import time
from pathlib import Path


class ModelRegistry:
    def __init__(self, path: str | Path):
        self.path = Path(path)
        self.path.parent.mkdir(parents=True, exist_ok=True)

    def register(self, name: str, checkpoint: str, config: dict, metrics: dict, data: dict, notes: str = "") -> dict:
        row = {"name": name, "checkpoint": str(checkpoint), "config": config, "metrics": metrics, "data": data,
               "notes": notes, "registered_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}
        with self.path.open("a", encoding="utf-8") as f:
            f.write(json.dumps(row) + "\n")
        return row

    def all(self) -> list[dict]:
        if not self.path.exists():
            return []
        return [json.loads(l) for l in self.path.read_text(encoding="utf-8").splitlines() if l.strip()]

    def latest(self, name: str) -> dict | None:
        rows = [r for r in self.all() if r["name"] == name]
        return rows[-1] if rows else None
