"""Training corpus with provenance and deterministic splits.

Sources (both local, both licensed for this use):
  - repo_docs: Monster OS's own Markdown (docs/, research/, READMEs): project-owned.
  - python_docstrings: docstrings of the Python standard library that ships
    with this interpreter: Python Software Foundation License 2.0.

Each document gets a stable id and a SHA-256; the split (train/val/test) is a
function of that hash alone, so it never changes when documents are added.
"""
from __future__ import annotations

import ast
import hashlib
import json
import sysconfig
import unicodedata
from dataclasses import asdict, dataclass
from pathlib import Path


@dataclass
class Document:
    id: str
    source: str
    license: str
    text: str
    sha256: str = ""

    def __post_init__(self):
        if not self.sha256:
            self.sha256 = hashlib.sha256(self.text.encode("utf-8")).hexdigest()


def clean(text: str) -> str:
    """NFC, unify newlines, drop control characters except newline/tab, trim trailing spaces."""
    text = unicodedata.normalize("NFC", text).replace("\r\n", "\n").replace("\r", "\n")
    text = "".join(c for c in text if c in "\n\t" or unicodedata.category(c)[0] != "C")
    return "\n".join(l.rstrip() for l in text.split("\n")).strip()


def repo_docs(root: str | Path, dirs=("docs", "research", "README.md", "models")) -> list[Document]:
    root = Path(root)
    out = []
    for d in dirs:
        p = root / d
        files = [p] if p.is_file() else sorted(p.rglob("*.md")) if p.exists() else []
        for f in files:
            if any(part in ("node_modules", "target", "checkpoints", "runs") for part in f.parts):
                continue
            t = clean(f.read_text(encoding="utf-8", errors="replace"))
            if len(t) > 200:
                out.append(Document(id=f"repo:{f.relative_to(root).as_posix()}", source="monster-os repository", license="project-owned", text=t))
    return out


def python_docstrings(max_docs: int | None = None, min_chars: int = 200) -> list[Document]:
    """Module/class/function docstrings from the interpreter's standard library."""
    stdlib = Path(sysconfig.get_paths()["stdlib"])
    out = []
    for f in sorted(stdlib.rglob("*.py")):
        rel = f.relative_to(stdlib).as_posix()
        if any(x in rel for x in ("test", "idlelib", "site-packages", "lib2to3", "turtledemo", "__phello__")):
            continue
        try:
            tree = ast.parse(f.read_text(encoding="utf-8", errors="replace"))
        except (SyntaxError, ValueError):
            continue
        parts = []
        for node in [tree] + [n for n in ast.walk(tree) if isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef))]:
            doc = ast.get_docstring(node)
            if doc and len(doc) >= 60:
                name = getattr(node, "name", rel)
                parts.append(f"{name}\n{doc}")
        text = clean("\n\n".join(parts))
        if len(text) >= min_chars:
            out.append(Document(id=f"pystdlib:{rel}", source="Python standard library docstrings", license="PSF-2.0", text=text))
        if max_docs and len(out) >= max_docs:
            break
    return out


def split_of(doc: Document, val: float = 0.05, test: float = 0.05) -> str:
    """train / val / test from the document hash (deterministic, stable under additions)."""
    u = int(doc.sha256[:8], 16) / 0xFFFFFFFF
    return "test" if u < test else "val" if u < test + val else "train"


def split_documents(docs: list[Document], val: float = 0.05, test: float = 0.05) -> dict[str, list[Document]]:
    out = {"train": [], "val": [], "test": []}
    for d in docs:
        out[split_of(d, val, test)].append(d)
    return out


def build_corpus(root: str | Path, out_dir: str | Path, val: float = 0.05, test: float = 0.05, eot: str = "<|endoftext|>") -> dict:
    """Write train/val/test text files and a provenance manifest. Returns the manifest."""
    out = Path(out_dir)
    out.mkdir(parents=True, exist_ok=True)
    docs = sorted(repo_docs(root) + python_docstrings(), key=lambda d: d.id)
    # exact-duplicate removal
    seen, uniq = set(), []
    for d in docs:
        if d.sha256 not in seen:
            seen.add(d.sha256)
            uniq.append(d)
    splits = split_documents(uniq, val, test)
    manifest = {"preprocessing": {"normalization": "NFC", "control_chars": "removed except \\n \\t", "dedupe": "exact sha256",
                                  "separator": eot, "split": f"sha256-bucket val={val} test={test}"},
                "splits": {}}
    for name, ds in splits.items():
        (out / f"{name}.txt").write_text(f"\n{eot}\n".join(d.text for d in ds), encoding="utf-8")
        manifest["splits"][name] = {"documents": len(ds), "chars": sum(len(d.text) for d in ds),
                                    "by_source": {s: sum(1 for d in ds if d.source == s) for s in sorted({d.source for d in ds})}}
    manifest["documents"] = [{"id": d.id, "source": d.source, "license": d.license, "sha256": d.sha256, "split": split_of(d, val, test), "chars": len(d.text)} for d in uniq]
    (out / "manifest.json").write_text(json.dumps(manifest, indent=1), encoding="utf-8")
    return manifest
