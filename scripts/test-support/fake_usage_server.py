#!/usr/bin/env python3
"""Fake usage / reset-credit / Haider Code plan server for end-to-end tests.

Pairs with the debug-only daemon hook `debug_fake_usage`
(`HAIDER_TEST_FAKE_USAGE_ORIGIN`, honoured only with `HAIDER_TEST_FAKE_PROVIDER`);
see docs/testing/fake-usage.md. It binds 127.0.0.1 only and never contacts a
real provider.

Served paths (the live URL's path on the fake origin):
  GET  /backend-api/wham/usage                              OpenAI WHAM meter
  GET  /backend-api/wham/rate-limit-reset-credits           reset-credit inventory
  POST /backend-api/wham/rate-limit-reset-credits/consume   reset consume (holdable)
  GET  /v1/account                                          Haider Code plan
Control:
  POST /__control  {"hold_consume": bool, "release": true, "consume_code": str,
                    "refresh_after_s": int}
  GET  /__state    counters and recorded consume redeem ids
  GET  /__log      method, path, status, bearer class per request (never a token)

`--write-codex-auth PATH` writes a Codex `auth.json` whose JWTs carry
`iss` = this fake origin (so the daemon's real-credential guard admits them).
The ready file carries `origin` and a fake Haider Code key with the required
`hk-haider-test-fake-` prefix.
"""

import argparse
import base64
import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

FAKE_KEY_PREFIX = "hk-haider-test-fake-"
ACCOUNT_ID = "fake-chatgpt-account"


def b64url(raw):
    return base64.urlsafe_b64encode(raw).rstrip(b"=").decode()


def mint_jwt(claims):
    header = b64url(json.dumps({"alg": "none", "typ": "JWT"}).encode())
    payload = b64url(json.dumps(claims).encode())
    return f"{header}.{payload}.fake-signature"


def jwt_iss(token):
    parts = token.split(".")
    if len(parts) != 3:
        return None
    try:
        padded = parts[1] + "=" * (-len(parts[1]) % 4)
        return json.loads(base64.urlsafe_b64decode(padded)).get("iss")
    except Exception:  # noqa: BLE001 - classification only
        return None


class State:
    def __init__(self):
        self.lock = threading.Lock()
        self.hold_consume = False
        self.release = threading.Event()
        self.release.set()
        self.consume_code = "reset"
        self.refresh_after_s = 15
        self.counts = {}
        self.consumes = []
        self.log = []
        self.origin = ""


STATE = State()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        pass

    def _bearer_class(self):
        header = self.headers.get("Authorization", "")
        if not header.startswith("Bearer "):
            return "none"
        token = header[len("Bearer "):]
        if token.startswith(FAKE_KEY_PREFIX):
            return "fake-key"
        if (jwt_iss(token) or "").rstrip("/") == STATE.origin:
            return "fake-jwt"
        return "FOREIGN"  # a real credential reached the fake: must never happen

    def _send(self, status, body, note=""):
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)
        with STATE.lock:
            STATE.log.append({
                "t": round(time.time(), 3),
                "method": self.command,
                "path": self.path,
                "status": status,
                "bearer": self._bearer_class(),
                "account_header": bool(self.headers.get("chatgpt-account-id")),
                "note": note,
            })

    def _count(self, key):
        with STATE.lock:
            STATE.counts[key] = STATE.counts.get(key, 0) + 1

    def _body(self):
        length = int(self.headers.get("content-length") or 0)
        return json.loads(self.rfile.read(length) or b"{}") if length else {}

    def do_GET(self):  # noqa: N802
        path = self.path
        if path == "/__state":
            with STATE.lock:
                body = {
                    "counts": dict(STATE.counts),
                    "consumes": list(STATE.consumes),
                    "hold_consume": STATE.hold_consume,
                    "held": not STATE.release.is_set(),
                }
            return self._send(200, body)
        if path == "/__log":
            with STATE.lock:
                body = list(STATE.log)
            return self._send(200, body)
        if self._bearer_class() not in ("fake-jwt", "fake-key"):
            return self._send(401, {"error": "unauthorized"}, "refused non-fake bearer")
        if path == "/backend-api/wham/usage":
            self._count("wham_usage")
            now = int(time.time())
            return self._send(200, {
                "account_id": ACCOUNT_ID,
                "plan_type": "plus",
                "rate_limit": {
                    "allowed": True,
                    "limit_reached": False,
                    "primary_window": {
                        "limit_window_seconds": 18000,
                        "reset_after_seconds": 3600,
                        "reset_at": now + 3600,
                        "used_percent": 72,
                    },
                    "secondary_window": {
                        "limit_window_seconds": 604800,
                        "reset_after_seconds": 200000,
                        "reset_at": now + 200000,
                        "used_percent": 41,
                    },
                },
                "rate_limit_reset_credits": {"available_count": 1, "applicable_available_count": 1},
            })
        if path == "/backend-api/wham/rate-limit-reset-credits":
            self._count("reset_credits")
            return self._send(200, {
                "available_count": 1,
                "applicable_available_count": 1,
                "credits": [],
            })
        if path == "/v1/account":
            self._count("haider_code_account")
            with STATE.lock:
                refresh = STATE.refresh_after_s
            return self._send(200, {
                "plan": "pro",
                "plan_label": "Pro (fake)",
                "weekly_allowance": {
                    "percent_remaining": 80.0,
                    "state": "ok",
                    "resets_at_ms": int(time.time() * 1000) + 3 * 86_400_000,
                },
                "refresh_after_s": refresh,
            })
        return self._send(404, {"error": "not_found"})

    def do_POST(self):  # noqa: N802
        path = self.path
        if path == "/__control":
            body = self._body()
            with STATE.lock:
                if "hold_consume" in body:
                    STATE.hold_consume = bool(body["hold_consume"])
                    if STATE.hold_consume:
                        STATE.release.clear()
                if body.get("release"):
                    STATE.hold_consume = False
                    STATE.release.set()
                if "consume_code" in body:
                    STATE.consume_code = str(body["consume_code"])
                if "refresh_after_s" in body:
                    STATE.refresh_after_s = int(body["refresh_after_s"])
            return self._send(200, {"ok": True})
        if self._bearer_class() not in ("fake-jwt", "fake-key"):
            return self._send(401, {"error": "unauthorized"}, "refused non-fake bearer")
        if path == "/backend-api/wham/rate-limit-reset-credits/consume":
            body = self._body()
            self._count("consume")
            with STATE.lock:
                STATE.consumes.append({
                    "t": round(time.time(), 3),
                    "redeem_request_id": body.get("redeem_request_id"),
                    "held": STATE.hold_consume,
                })
            STATE.release.wait(timeout=600)
            with STATE.lock:
                code = STATE.consume_code
            return self._send(200, {"code": code, "windows_reset": 1}, "consume")
        return self._send(404, {"error": "not_found"})


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--port", type=int, default=0)
    parser.add_argument("--ready-file", required=True)
    parser.add_argument("--write-codex-auth")
    args = parser.parse_args()
    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    server.daemon_threads = True
    STATE.origin = f"http://127.0.0.1:{server.server_address[1]}"
    far_future = int(time.time()) + 10 * 365 * 86_400
    if args.write_codex_auth:
        access = mint_jwt({"iss": STATE.origin, "exp": far_future, "sub": "fake-user"})
        id_token = mint_jwt({
            "iss": STATE.origin,
            "exp": far_future,
            "sub": "fake-user",
            "email": "fake-user@example.test",
            "https://api.openai.com/auth": {
                "chatgpt_account_id": ACCOUNT_ID,
                "chatgpt_plan_type": "plus",
            },
        })
        with open(args.write_codex_auth, "w", encoding="utf-8") as handle:
            json.dump({
                "OPENAI_API_KEY": None,
                "tokens": {
                    "id_token": id_token,
                    "access_token": access,
                    "refresh_token": "fake-refresh-token-never-used",
                    "account_id": ACCOUNT_ID,
                },
                "last_refresh": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            }, handle)
    with open(args.ready_file, "w", encoding="utf-8") as handle:
        json.dump({
            "origin": STATE.origin,
            "haider_code_key": FAKE_KEY_PREFIX + "plan-key",
        }, handle)
    server.serve_forever()


if __name__ == "__main__":
    main()
