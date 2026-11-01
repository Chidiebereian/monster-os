"""OpenAI-compatible server for a trained Monster GPT checkpoint, so the
monster-gpt service can use it through its `openai` provider adapter:

    python -m monster_gpt.inference.server --run runs/micro --port 8130
    # then: LLM_PROVIDER=openai LLM_BASE_URL=http://localhost:8130/v1 LLM_MODEL=monster-gpt-micro

Endpoints: GET /health, GET /v1/models, POST /v1/chat/completions (stream or not).
Chat messages are flattened to "System: / User: / Assistant:" text.
"""
from __future__ import annotations

import argparse
import json
import threading
import time
import uuid
from pathlib import Path

import torch
from fastapi import FastAPI, HTTPException
from fastapi.responses import JSONResponse, StreamingResponse
from pydantic import BaseModel

from ..core.config import GPTConfig
from ..tokenizer.bpe import ByteBPE
from ..training.checkpoint import load_checkpoint
from ..transformer.gpt import GPT
from .generate import generate


class Msg(BaseModel):
    role: str
    content: str


class ChatReq(BaseModel):
    model: str | None = None
    messages: list[Msg]
    stream: bool = False
    temperature: float = 0.8
    top_p: float | None = 0.95
    max_tokens: int | None = 200


def chat_prompt(msgs: list[Msg]) -> str:
    names = {"system": "System", "user": "User", "assistant": "Assistant"}
    return "\n".join(f"{names.get(m.role, m.role.title())}: {m.content}" for m in msgs) + "\nAssistant:"


def build_app(run_dir: str | Path, name: str = "monster-gpt-micro") -> FastAPI:
    run = Path(run_dir)
    cfg = GPTConfig.load(run / "config.json")
    model = GPT(cfg)
    load_checkpoint(run / "ckpt.pt", model)
    model.eval()
    tok = ByteBPE.load(run / "tokenizer.json")
    lock = threading.Lock()  # one generation at a time on CPU
    app = FastAPI(title="Monster GPT scratch model")

    @app.get("/health")
    def health():
        return {"status": "ok", "model": name, "params": model.num_params(), "context": model.max_positions}

    @app.get("/v1/models")
    def models():
        return {"object": "list", "data": [{"id": name, "object": "model", "owned_by": "monster-os", "context_length": model.max_positions}]}

    @app.post("/v1/chat/completions")
    def chat(req: ChatReq):
        if not req.messages:
            raise HTTPException(400, "messages must not be empty")
        ids = tok.encode(chat_prompt(req.messages))
        max_new = max(1, min(req.max_tokens or 200, model.max_positions // 2))
        cid = f"chatcmpl-{uuid.uuid4().hex[:12]}"
        stop = {tok.eot}

        def tokens():
            with lock:
                buf: list[int] = []
                text_so_far = ""
                for t in generate(model, ids, max_new, req.temperature, 50, req.top_p, True, stop):
                    if t == tok.eot:
                        break
                    buf.append(t)
                    full = tok.decode(buf)
                    # emit only complete characters, and stop at the next speaker turn
                    if "\nUser:" in full:
                        yield full[len(text_so_far): full.index("\nUser:")]
                        return
                    if not full.endswith("�"):
                        yield full[len(text_so_far):]
                        text_so_far = full

        if req.stream:
            def sse():
                for piece in tokens():
                    if piece:
                        yield "data: " + json.dumps({"id": cid, "object": "chat.completion.chunk", "model": name,
                                                      "choices": [{"index": 0, "delta": {"content": piece}, "finish_reason": None}]}) + "\n\n"
                yield "data: " + json.dumps({"id": cid, "object": "chat.completion.chunk", "model": name, "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}) + "\n\n"
                yield "data: [DONE]\n\n"
            return StreamingResponse(sse(), media_type="text/event-stream")
        text = "".join(tokens())
        return JSONResponse({"id": cid, "object": "chat.completion", "created": int(time.time()), "model": name,
                             "choices": [{"index": 0, "message": {"role": "assistant", "content": text}, "finish_reason": "stop"}],
                             "usage": {"prompt_tokens": len(ids), "completion_tokens": len(tok.encode(text))}})

    return app


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--run", default="runs/micro")
    ap.add_argument("--port", type=int, default=8130)
    ap.add_argument("--name", default="monster-gpt-micro")
    a = ap.parse_args()
    import uvicorn
    torch.set_num_threads(max(1, torch.get_num_threads() // 2))
    uvicorn.run(build_app(a.run, a.name), host="127.0.0.1", port=a.port)


if __name__ == "__main__":
    main()
