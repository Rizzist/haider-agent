"""Owned loopback-only OpenAI-compatible listener for account-selection QA."""

from __future__ import annotations

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import threading
from typing import Any


class _Server(ThreadingHTTPServer):
    daemon_threads = True
    block_on_close = False

    def __init__(self, sentinel: str, model: str):
        super().__init__(("127.0.0.1", 0), _Handler)
        self.sentinel = sentinel
        self.model = model
        self.requests: list[dict[str, Any]] = []
        self.lock = threading.Lock()

    def record(self, method: str, path: str, body: bytes) -> None:
        with self.lock:
            self.requests.append(
                {"method": method, "path": path, "body": body.decode("utf-8", "replace")}
            )


class _Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, _format: str, *_args: object) -> None:
        return

    def _reply(self, body: bytes, content_type: str = "application/json") -> None:
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:  # noqa: N802 - stdlib handler API
        server: _Server = self.server  # type: ignore[assignment]
        server.record("GET", self.path, b"")
        body = json.dumps(
            {
                "object": "list",
                "data": [
                    {"id": server.model, "object": "model", "created": 0, "owned_by": "qa"}
                ],
            },
            separators=(",", ":"),
        ).encode()
        self._reply(body)

    def do_POST(self) -> None:  # noqa: N802 - stdlib handler API
        server: _Server = self.server  # type: ignore[assignment]
        length = int(self.headers.get("Content-Length", "0"))
        body = self.rfile.read(length)
        server.record("POST", self.path, body)
        chunks = [
            {
                "id": "chatcmpl-qa",
                "object": "chat.completion.chunk",
                "created": 0,
                "model": server.model,
                "choices": [
                    {
                        "index": 0,
                        "delta": {"role": "assistant", "content": server.sentinel},
                        "finish_reason": None,
                    }
                ],
            },
            {
                "id": "chatcmpl-qa",
                "object": "chat.completion.chunk",
                "created": 0,
                "model": server.model,
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            },
        ]
        stream = "".join(
            f"data: {json.dumps(chunk, separators=(',', ':'))}\n\n" for chunk in chunks
        )
        self._reply((stream + "data: [DONE]\n\n").encode(), "text/event-stream")


class OpenAIStub:
    def __init__(self, sentinel: str, model: str = "fixture-model"):
        self._server = _Server(sentinel, model)
        self._thread = threading.Thread(
            target=self._server.serve_forever,
            kwargs={"poll_interval": 0.02},
            daemon=True,
        )

    @property
    def base_url(self) -> str:
        host, port = self._server.server_address
        return f"http://{host}:{port}/v1"

    @property
    def chat_requests(self) -> list[dict[str, Any]]:
        with self._server.lock:
            return [
                dict(request)
                for request in self._server.requests
                if request["method"] == "POST" and request["path"].endswith("/chat/completions")
            ]

    @property
    def chat_count(self) -> int:
        return len(self.chat_requests)

    def start(self) -> "OpenAIStub":
        self._thread.start()
        return self

    def close(self) -> None:
        self._server.shutdown()
        self._server.server_close()
        self._thread.join()
