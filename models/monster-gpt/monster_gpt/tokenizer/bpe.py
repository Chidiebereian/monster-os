"""Byte-level BPE tokenizer.

Every string is UTF-8 bytes first, so any text (emoji, accents, CJK, control
characters) round-trips exactly and there is no unknown token. Training
pre-splits text GPT-2 style (words, numbers, punctuation, spaces kept with the
following word), counts adjacent pairs, and merges the most frequent pair
repeatedly. Pair counts are updated incrementally, touching only the words that
contain the merged pair.
"""
from __future__ import annotations

import heapq
import json
import re
from collections import Counter, defaultdict
from pathlib import Path

# words with a leading space, numbers in runs of up to 3 digits, punctuation runs, whitespace
_SPLIT = re.compile(r"""'(?:[sdmt]|ll|ve|re)| ?[^\W\d_]+| ?\d{1,3}| ?[^\s\w]+|\s+(?!\S)|\s+""", re.UNICODE)


class ByteBPE:
    def __init__(self, merges: list[tuple[int, int]] | None = None, special: list[str] | None = None):
        self.merges: list[tuple[int, int]] = [tuple(m) for m in (merges or [])]
        self.ranks = {p: i for i, p in enumerate(self.merges)}
        self.vocab: dict[int, bytes] = {i: bytes([i]) for i in range(256)}
        for i, (a, b) in enumerate(self.merges):
            self.vocab[256 + i] = self.vocab[a] + self.vocab[b]
        self.special = list(special or ["<|endoftext|>"])
        self.special_ids = {s: 256 + len(self.merges) + i for i, s in enumerate(self.special)}
        self._cache: dict[str, list[int]] = {}

    @property
    def vocab_size(self) -> int:
        return 256 + len(self.merges) + len(self.special)

    @property
    def eot(self) -> int:
        return self.special_ids[self.special[0]]

    # ---------------------------------------------------------------- training
    @classmethod
    def train(cls, text: str, vocab_size: int, special: list[str] | None = None, min_freq: int = 2, verbose: bool = False) -> "ByteBPE":
        special = list(special or ["<|endoftext|>"])
        n_merges = vocab_size - 256 - len(special)
        if n_merges < 0:
            raise ValueError("vocab_size must be at least 256 + number of special tokens")
        words = Counter(_SPLIT.findall(text))
        seqs: list[list[int]] = [list(w.encode("utf-8")) for w in words]
        freqs: list[int] = list(words.values())
        pairs: Counter = Counter()
        where: dict[tuple[int, int], set[int]] = defaultdict(set)
        for wi, s in enumerate(seqs):
            for p in zip(s, s[1:]):
                pairs[p] += freqs[wi]
                where[p].add(wi)
        merges: list[tuple[int, int]] = []
        # max-heap of (-count, pair) with lazy deletion: stale entries are skipped on pop
        heap = [(-c, p) for p, c in pairs.items()]
        heapq.heapify(heap)
        for k in range(n_merges):
            pair = None
            while heap:
                c, p = heapq.heappop(heap)
                if pairs.get(p, 0) == -c and c < 0:
                    pair, count = p, -c
                    break
            # most frequent pair; ties broken by the smaller pair (heap order) for determinism
            if pair is None or count < min_freq:
                break
            new_id = 256 + len(merges)
            merges.append(pair)
            touched: set = set()
            for wi in list(where[pair]):
                s, f = seqs[wi], freqs[wi]
                # remove this word's old pair counts
                for p in zip(s, s[1:]):
                    pairs[p] -= f
                    if pairs[p] <= 0:
                        del pairs[p]
                    else:
                        touched.add(p)
                    where[p].discard(wi)
                out, i = [], 0
                while i < len(s):
                    if i + 1 < len(s) and s[i] == pair[0] and s[i + 1] == pair[1]:
                        out.append(new_id)
                        i += 2
                    else:
                        out.append(s[i])
                        i += 1
                seqs[wi] = out
                for p in zip(out, out[1:]):
                    pairs[p] += f
                    where[p].add(wi)
                    touched.add(p)
            where.pop(pair, None)
            for p in touched:
                if p in pairs:
                    heapq.heappush(heap, (-pairs[p], p))
            if verbose and k % 500 == 0:
                print(f"merge {k}: {pair} x{count}")
        return cls(merges, special)

    # ---------------------------------------------------------------- encode/decode
    def _bpe(self, chunk: bytes) -> list[int]:
        ids = list(chunk)
        while len(ids) >= 2:
            best = min(((self.ranks.get(p, 1 << 30), i) for i, p in enumerate(zip(ids, ids[1:]))), default=(1 << 30, -1))
            rank, i = best
            if rank == 1 << 30:
                break
            ids[i : i + 2] = [256 + rank]
        return ids

    def encode(self, text: str, allow_special: bool = True) -> list[int]:
        out: list[int] = []
        parts = [text]
        if allow_special and self.special:
            pat = "(" + "|".join(re.escape(s) for s in self.special) + ")"
            parts = re.split(pat, text)
        for part in parts:
            if part in self.special_ids and allow_special:
                out.append(self.special_ids[part])
                continue
            for w in _SPLIT.findall(part):
                ids = self._cache.get(w)
                if ids is None:
                    ids = self._bpe(w.encode("utf-8"))
                    if len(self._cache) < 200_000:
                        self._cache[w] = ids
                out.extend(ids)
        return out

    def decode_bytes(self, ids: list[int]) -> bytes:
        inv = {v: k for k, v in self.special_ids.items()}
        return b"".join(inv[i].encode() if i in inv else self.vocab[i] for i in ids)

    def decode(self, ids: list[int]) -> str:
        return self.decode_bytes(ids).decode("utf-8", errors="replace")

    # ---------------------------------------------------------------- persistence
    def save(self, path: str | Path) -> None:
        Path(path).write_text(json.dumps({"type": "byte-bpe", "merges": self.merges, "special": self.special}), encoding="utf-8")

    @classmethod
    def load(cls, path: str | Path) -> "ByteBPE":
        d = json.loads(Path(path).read_text(encoding="utf-8"))
        return cls([tuple(m) for m in d["merges"]], d["special"])
