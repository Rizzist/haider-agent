#!/usr/bin/env python3
"""Local fake OAuth + profile server for Haider end-to-end tests.

TEST SUPPORT ONLY. Pair it with a DEBUG `haiderd` started with
``HAIDER_TEST_FAKE_OAUTH_ORIGIN=<origin>`` (the hook is compiled out of
release builds). See docs/testing/fake-oauth.md.

It stands in for Claude's (`anthropic-oauth`) authorization-code + PKCE
login and its `/api/oauth/profile` identity document:

  GET  /oauth/authorize      -> 302 to the daemon's loopback redirect_uri
  POST /oauth/token          -> authorization_code (PKCE S256 checked) and
                                refresh_token grants, JSON or form bodies
  GET  /api/oauth/profile    -> identity bound to the bearer token, after
                                any scripted failure statuses
  GET  /__state, POST /__control  -> inspect / change the script
  GET  /__log                -> request log (paths + statuses, no tokens)

Script (``--state`` JSON file and/or ``POST /__control`` with a partial
object that is merged in):

  {"identity": {"account_uuid": "acct-a", "organization_uuid": "org-1",
                "email": "a@example.test",          # optional
                "display_name": "A", "organization_type": "claude_max"},
   "profile_statuses": [503, 429],   # consumed one per profile request;
                                     # when empty the profile answers 200
   "retry_after": null,              # Retry-After seconds on scripted 429/5xx
   "access_ttl": 3600}

The identity in effect at /oauth/authorize is bound to the code, and from
there to every token minted from it, so each token's profile reports its
OWN account (what the daemon's legacy-row probes rely on).

Only numeric loopback binds are allowed and only loopback redirect URIs are
followed. Tokens are random synthetic strings prefixed ``fake-``.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import secrets
import sys
import threading
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

DEFAULT_IDENTITY = {
    "account_uuid": "fake-account-a",
    "organization_uuid": "fake-org-1",
    "email": "owner-a@example.test",
    "display_name": "Fake Owner A",
    "organization_type": "claude_max",
}
LOOPBACK_REDIRECT_HOSTS = {"localhost", "127.0.0.1"}


class FakeOAuthState:
    def __init__(self, script: dict | None = None):
        # Re-entrant, and never held across a socket write (see _send).
        self.lock = threading.RLock()
        self.script = {
            "identity": dict(DEFAULT_IDENTITY),
            "profile_statuses": [],
            "retry_after": None,
            "access_ttl": 3600,
        }
        if script:
            self.merge(script)
        self.codes: dict[str, dict] = {}
        self.access: dict[str, dict] = {}
        self.refresh: dict[str, dict] = {}
        self.log: list[dict] = []

    def merge(self, update: dict) -> None:
        for key, value in update.items():
            if key == "identity" and isinstance(value, dict):
                self.script["identity"] = dict(value)
            else:
                self.script[key] = value

    def record(self, method: str, path: str, status: int, note: str = "") -> None:
        self.log.append({"method": method, "path": path, "status": status, "note": note})

    def mint(self, identity: dict, scope: str) -> dict:
        access = "fake-at-" + secrets.token_urlsafe(18)
        refresh = "fake-rt-" + secrets.token_urlsafe(18)
        grant = {"identity": dict(identity), "scope": scope}
        self.access[access] = grant
        self.refresh[refresh] = grant
        return {
            "access_token": access,
            "refresh_token": refresh,
            "token_type": "Bearer",
            "expires_in": int(self.script.get("access_ttl") or 3600),
            "scope": scope,
        }


def s256(verifier: str) -> str:
    digest = hashlib.sha256(verifier.encode()).digest()
    return base64.urlsafe_b64encode(digest).decode().rstrip("=")


def profile_document(identity: dict) -> dict:
    account = {"uuid": identity["account_uuid"]}
    for key in ("email", "display_name", "full_name"):
        if identity.get(key) is not None:
            account[key] = identity[key]
    organization = {"uuid": identity["organization_uuid"]}
    if identity.get("organization_type"):
        organization["organization_type"] = identity["organization_type"]
    return {"account": account, "organization": organization}


def make_handler(state: FakeOAuthState):
    class Handler(BaseHTTPRequestHandler):
        server_version = "haider-fake-oauth/1"
        # A real provider speaks HTTP/1.1; an HTTP/1.0 302 makes clients
        # follow the redirect with HTTP/1.0, which the daemon's strict
        # browser-callback parser (rightly) rejects. Every reply sets
        # Content-Length and Connection: close.
        protocol_version = "HTTP/1.1"

        def log_message(self, *_args):  # quiet; /__log is the record
            pass

        def _send(self, status: int, body=b"", headers=None, note: str = ""):
            if isinstance(body, (dict, list)):
                body = json.dumps(body).encode()
                headers = {"Content-Type": "application/json", **(headers or {})}
            self.send_response(status)
            for name, value in (headers or {}).items():
                self.send_header(name, value)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(body)
            path = urllib.parse.urlsplit(self.path).path
            with state.lock:
                state.record(self.command, path, status, note)

        def _body(self) -> dict:
            length = int(self.headers.get("Content-Length") or 0)
            raw = self.rfile.read(length).decode() if length else ""
            if "json" in (self.headers.get("Content-Type") or ""):
                return json.loads(raw or "{}")
            return {k: v[0] for k, v in urllib.parse.parse_qs(raw).items()}

        def do_GET(self):
            url = urllib.parse.urlsplit(self.path)
            query = {k: v[0] for k, v in urllib.parse.parse_qs(url.query).items()}
            if url.path == "/oauth/authorize":
                return self._authorize(query)
            if url.path == "/api/oauth/profile":
                return self._profile()
            if url.path == "/__state":
                with state.lock:
                    snapshot = dict(state.script)
                return self._send(200, snapshot)
            if url.path == "/__log":
                with state.lock:
                    snapshot = list(state.log)
                return self._send(200, snapshot)
            return self._send(404, {"error": "not_found"})

        def do_POST(self):
            path = urllib.parse.urlsplit(self.path).path
            try:
                body = self._body()
            except (ValueError, json.JSONDecodeError):
                return self._send(400, {"error": "invalid_request"})
            if path == "/oauth/token":
                return self._token(body)
            if path == "/__control":
                with state.lock:
                    state.merge(body)
                    snapshot = dict(state.script)
                return self._send(200, snapshot)
            return self._send(404, {"error": "not_found"})

        def _authorize(self, query: dict):
            redirect = query.get("redirect_uri", "")
            target = urllib.parse.urlsplit(redirect)
            if (
                query.get("response_type") != "code"
                or not query.get("client_id")
                or query.get("code_challenge_method") != "S256"
                or not query.get("code_challenge")
                or target.scheme != "http"
                or target.hostname not in LOOPBACK_REDIRECT_HOSTS
            ):
                return self._send(400, {"error": "invalid_request"}, note="authorize rejected")
            code = "fake-code-" + secrets.token_urlsafe(12)
            with state.lock:
                state.codes[code] = {
                    "challenge": query["code_challenge"],
                    "redirect_uri": redirect,
                    "client_id": query["client_id"],
                    "scope": query.get("scope", ""),
                    "identity": dict(state.script["identity"]),
                }
                account = state.script["identity"].get("account_uuid")
            location = redirect + ("&" if target.query else "?") + urllib.parse.urlencode(
                {"code": code, "state": query.get("state", "")}
            )
            return self._send(302, b"", {"Location": location}, note=f"authorize account={account}")

        def _token(self, body: dict):
            grant = body.get("grant_type")
            with state.lock:
                if grant == "authorization_code":
                    entry = state.codes.pop(body.get("code", ""), None)
                    if (
                        entry is None
                        or body.get("redirect_uri") != entry["redirect_uri"]
                        or body.get("client_id") != entry["client_id"]
                        or s256(body.get("code_verifier", "")) != entry["challenge"]
                    ):
                        response, status, note = {"error": "invalid_grant"}, 400, "code rejected"
                    else:
                        response = state.mint(entry["identity"], entry["scope"])
                        status = 200
                        note = f"code grant account={entry['identity'].get('account_uuid')}"
                elif grant == "refresh_token":
                    old = state.refresh.pop(body.get("refresh_token", ""), None)
                    if old is None:
                        response, status, note = {"error": "invalid_grant"}, 400, "refresh rejected"
                    else:
                        response = state.mint(old["identity"], old["scope"])
                        status = 200
                        note = f"refresh grant account={old['identity'].get('account_uuid')}"
                else:
                    response, status, note = {"error": "unsupported_grant_type"}, 400, "bad grant"
            return self._send(status, response, note=note)

        def _profile(self):
            auth = self.headers.get("Authorization") or ""
            token = auth[7:] if auth.startswith("Bearer ") else ""
            with state.lock:
                grant = state.access.get(token)
                statuses = state.script.get("profile_statuses") or []
                scripted = statuses.pop(0) if statuses and grant is not None else None
                retry_after = state.script.get("retry_after")
            if grant is None:
                return self._send(401, {"error": "invalid_token"}, note="unknown bearer")
            if scripted is not None:
                headers = {} if retry_after is None else {"Retry-After": str(retry_after)}
                return self._send(int(scripted), {"error": "scripted"}, headers, note="scripted")
            account = grant["identity"].get("account_uuid")
            return self._send(200, profile_document(grant["identity"]), note=f"account={account}")

    return Handler


def serve(host: str, port: int, script: dict | None = None):
    if not host.startswith("127."):
        raise SystemExit("fake OAuth server binds numeric IPv4 loopback only")
    state = FakeOAuthState(script)
    server = ThreadingHTTPServer((host, port), make_handler(state))
    server.daemon_threads = True
    return server, state


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=0)
    parser.add_argument("--state", help="initial script JSON file")
    parser.add_argument("--ready-file", help="write {\"origin\": ...} here once listening")
    args = parser.parse_args(argv)
    script = json.load(open(args.state)) if args.state else None
    server, _state = serve(args.host, args.port, script)
    origin = f"http://{server.server_address[0]}:{server.server_address[1]}"
    ready = json.dumps({"origin": origin, "pid": __import__("os").getpid()})
    if args.ready_file:
        with open(args.ready_file, "w") as handle:
            handle.write(ready + "\n")
    print(ready, flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
