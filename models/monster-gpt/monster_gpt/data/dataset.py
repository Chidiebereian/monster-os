"""Token datasets for next-token prediction: a flat token stream cut into
(block_size + 1)-token windows; x = window[:-1], y = window[1:]."""
from __future__ import annotations

import numpy as np
import torch


class TokenDataset(torch.utils.data.Dataset):
    def __init__(self, tokens, block_size: int, stride: int | None = None):
        self.t = torch.as_tensor(np.asarray(tokens, dtype=np.int64))
        if len(self.t) <= block_size:
            raise ValueError(f"need more than {block_size} tokens, got {len(self.t)}")
        self.block = block_size
        self.stride = stride or block_size
        self.n = (len(self.t) - block_size - 1) // self.stride + 1

    def __len__(self):
        return self.n

    def __getitem__(self, i):
        s = i * self.stride
        w = self.t[s : s + self.block + 1]
        return w[:-1], w[1:]


def make_loader(tokens, block_size: int, batch_size: int, shuffle: bool, seed: int = 0, stride: int | None = None):
    ds = TokenDataset(tokens, block_size, stride)
    g = torch.Generator().manual_seed(seed)
    return torch.utils.data.DataLoader(ds, batch_size=batch_size, shuffle=shuffle, generator=g, drop_last=shuffle)
