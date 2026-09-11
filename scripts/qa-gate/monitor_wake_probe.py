#!/usr/bin/env python3
"""Exercise candidate CLI/daemon monitor wakes over real Anthropic HTTP/SSE.

Usage: monitor_wake_probe.py --bin target/debug/haider --evidence DIR
All profiles, watched files and no-auth provider data are synthetic and isolated.
"""

import argparse
import hashlib
import http.server
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading


DETAIL = "messages.2: tool_use ids were found without tool_result blocks immediately after: fixture-call"


def valid_messages(body):
    pending = set()
    seen = set()
    effective = []
    for message in body["messages"]:
        blocks = message["content"]
        if isinstance(blocks, str):
            blocks = [{"type": "text", "text": blocks}]
        assert blocks, "empty content"
        if effective and effective[-1][0] == message["role"]:
            effective[-1][1].extend(blocks)
        else:
            effective.append((message["role"], list(blocks)))
    for role, blocks in effective:
        for block in blocks:
            kind = block["type"]
            if kind == "tool_use":
                assert role == "assistant"
                assert block["id"] not in seen, "duplicate tool_use"
                seen.add(block["id"])
                pending.add(block["id"])
            elif kind == "tool_result":
                assert role == "user"
                assert block["tool_use_id"] in pending, "unmatched tool_result"
                pending.remove(block["tool_use_id"])
            elif kind == "text":
                assert block["text"].strip(), "empty text"
                if role == "user":
                    assert not pending, f"user text before results: {pending}"
        if role == "user":
            assert not pending, f"unpaired calls: {pending}"
    assert not pending, f"unfinished tool calls: {pending}"


class Proxy(http.server.ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, evidence, workspace):
        super().__init__(("127.0.0.1", 0), Handler)
        self.evidence = evidence
        self.workspace = workspace
        self.registered = threading.Event()
        self.woken = threading.Event()
        self.requests = []
        self.failures = []
        self.force_error = False


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        self.send_body(200, {"data": [{"id": "claude-fixture", "type": "model"}]})

    def send_body(self, status, body):
        if status == 400:
            with (self.server.evidence / "http-errors.jsonl").open("a") as log:
                log.write(json.dumps({"status": status, "request_id": "req-fixture-400", "body": body}) + "\n")
        body = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.send_header("request-id", "req-fixture-400")
        self.end_headers()
        self.wfile.write(body)

    def sse(self, event, body):
        self.wfile.write(f"event: {event}\ndata: {json.dumps(body)}\n\n".encode())
        self.wfile.flush()

    def call(self, index, call_id, args, name="monitor"):
        self.sse("content_block_start", {"type": "content_block_start", "index": index,
            "content_block": {"type": "tool_use", "id": call_id, "name": name, "input": {}}})
        self.sse("content_block_delta", {"type": "content_block_delta", "index": index,
            "delta": {"type": "input_json_delta", "partial_json": json.dumps(args)}})
        self.sse("content_block_stop", {"type": "content_block_stop", "index": index})

    def do_POST(self):
        try:
            assert self.path.endswith("/messages"), self.path
            assert "authorization" not in self.headers and "x-api-key" not in self.headers
            body = json.loads(self.rfile.read(int(self.headers["content-length"])))
            ordinal = len(self.server.requests) + 1
            self.server.requests.append(body)
            (self.server.evidence / f"request-{ordinal}.json").write_text(json.dumps(body, indent=2) + "\n")
            try:
                valid_messages(body)
            except AssertionError as error:
                self.server.failures.append(str(error))
                self.send_body(400, {"type": "error", "error": {"type": "invalid_request_error", "message": str(error)}})
                return
            if self.server.force_error:
                self.send_body(400, {"type": "error", "error": {"type": "invalid_request_error", "message": DETAIL}})
                return
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("connection", "close")
            self.end_headers()
            self.sse("message_start", {"type": "message_start", "message": {
                "id": f"msg-fixture-{ordinal}", "role": "assistant", "type": "message", "model": "claude-fixture",
                "content": [], "usage": {"input_tokens": 10, "output_tokens": 0}}})
            if ordinal == 1:
                self.call(0, "discovery", {"filter": "monitor"}, name="list_tools")
                self.sse("message_delta", {"type": "message_delta", "delta": {"stop_reason": "tool_use"},
                    "usage": {"output_tokens": 5}})
                self.sse("message_stop", {"type": "message_stop"})
                return
            if ordinal == 2:
                assert any(tool["name"] == "monitor" for tool in body["tools"]), "monitor discovered"
                self.call(0, "registration", {"operation": "register", "source": {"kind": "file", "path": "watched.txt"},
                    "occurrence": "once", "action": {"report": True}, "lifetime": {"kind": "session"}})
                assert self.server.registered.wait(90), "CLI did not record successful monitor registration"
                # Registration schedules the watcher; its initial snapshot may race the first write.
                # Repeat fixture mutations until the durable wake proves the observer saw one.
                for change in range(360):
                    (self.server.workspace / "watched.txt").write_text(f"after registration: {change}\n")
                    if self.server.woken.wait(0.25):
                        break
                assert self.server.woken.is_set(), "CLI did not record the real file-monitor wake"
                # A second resolved call in the same response is the exact lost-result branch.
                self.call(1, "held-after-wake", {"operation": "list"})
                return
            assert self.server.woken.is_set(), "provider continuation preceded monitor wake"
            results = [block for message in body["messages"] for block in message["content"]
                       if block.get("type") == "tool_result"]
            registration = [block for block in results if block["tool_use_id"] == "registration"]
            held = [block for block in results if block["tool_use_id"] == "held-after-wake"]
            assert len(registration) == 1 and '"status":"registered"' in registration[0]["content"]
            assert len(held) == 1 and "held before execution for a user subturn" in held[0]["content"]
            wake_blocks = [block for message in body["messages"] for block in message["content"]
                           if block.get("type") == "text" and "<monitor-event" in block.get("text", "")]
            assert len(wake_blocks) == 1, "monitor delivered exactly once"
            assert "```json\n" in wake_blocks[0]["text"], "monitor JSON is fenced"
            self.sse("content_block_start", {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})
            self.sse("content_block_delta", {"type": "content_block_delta", "index": 0,
                "delta": {"type": "text_delta", "text": "Monitor wake accepted."}})
            self.sse("content_block_stop", {"type": "content_block_stop", "index": 0})
            self.sse("message_delta", {"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 5}})
            self.sse("message_stop", {"type": "message_stop"})
        except (BrokenPipeError, ConnectionResetError):
            # The actor drops the interrupted first stream when it holds the call.
            if len(self.server.requests) != 2:
                self.server.failures.append("unexpected dropped provider connection")
        except Exception as error:
            self.server.failures.append(repr(error))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--bin", required=True, type=Path)
    parser.add_argument("--evidence", required=True, type=Path)
    args = parser.parse_args()
    binary = args.bin.resolve()
    evidence = args.evidence.resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="h971-", dir="/tmp") as temporary:
        root = Path(temporary)
        profile, workspace, home, runtime = [root / name for name in ("profile", "workspace", "home", "runtime")]
        for directory in (profile, workspace, home, runtime):
            directory.mkdir(mode=0o700)
        (workspace / "watched.txt").write_text("before registration\n")
        proxy = Proxy(evidence, workspace)
        thread = threading.Thread(target=proxy.serve_forever, daemon=True)
        thread.start()
        (profile / "providers.json").write_text(json.dumps({"providers": [{
            "provider_id": "monitor-proxy", "display_name": "Monitor fixture", "api_family": "anthropic_messages",
            "base_url": f"http://127.0.0.1:{proxy.server_port}", "enabled": True, "auth_requirement": "none",
            "configured_models": ["claude-fixture"], "default_model": "claude-fixture", "promotion_model": None,
            "provenance": "custom"}], "fallback_chain": []}))
        env = {key: os.environ[key] for key in ("PATH", "LANG", "LC_ALL", "TERM") if key in os.environ}
        env.update(HOME=str(home), USERPROFILE=str(home), HAIDER_PROFILE_DIR=str(profile),
            HAIDER_RUNTIME_DIR=str(runtime), XDG_RUNTIME_DIR=str(runtime), HAIDER_DISCOVERY_DISABLED="1", HAIDER_AUTO_HERMETIC="0", HAIDER_NO_UPDATE_CHECK="1",
            NO_PROXY="127.0.0.1")
        base = [str(binary), "run", "--provider", "monitor-proxy", "--model", "claude-fixture", "--auto-allow", "--timeout", "240s"]
        result = {"verdict": "FAIL", "artifacts": {name: {"path": str(binary.with_name(name)),
            "sha256": hashlib.sha256(binary.with_name(name).read_bytes()).hexdigest()}
            for name in ("haider", "haiderd", "haider-tui")}}
        try:
            with (evidence / "wake.stderr").open("w") as stderr:
                process = subprocess.Popen(base + ["--jsonl", "Watch the local fixture file."], cwd=workspace, env=env,
                    stdout=subprocess.PIPE, stderr=stderr, text=True)
                lines = []
                def observe():
                    for line in process.stdout:
                        lines.append(line)
                        if '"tool_result"' in line and "registration" in line and "monitor_id" in line:
                            proxy.registered.set()
                        if "<monitor-event" in line:
                            proxy.woken.set()
                reader = threading.Thread(target=observe, daemon=True)
                reader.start()
                try:
                    result["wake_exit"] = process.wait(timeout=260)
                finally:
                    if process.poll() is None:
                        process.terminate()
                        process.wait(timeout=15)
                    reader.join(timeout=5)
                    (evidence / "wake.jsonl").write_text("".join(lines))
            result["proxy_failures"] = proxy.failures
            assert result["wake_exit"] == 0, result
            assert not proxy.failures, proxy.failures
            assert proxy.registered.is_set() and proxy.woken.is_set()
            assert len(proxy.requests) == 3, "discovery, registration, then one continuation after the wake"
            result["wake_provider_requests"] = len(proxy.requests)
            proxy.force_error = True
            failure = subprocess.run(base + ["--jsonl", "Return the fixture HTTP error."], cwd=workspace, env=env,
                capture_output=True, text=True, timeout=260)
            (evidence / "error.jsonl").write_text(failure.stdout)
            (evidence / "error.stderr").write_text(failure.stderr)
            result["http_400_exit"] = failure.returncode
            assert failure.returncode != 0
            assert DETAIL in failure.stdout, "provider detail absent from durable live error presentation"
            assert "req-fixture-400" in failure.stdout
            assert '"provider_http_status":400' in failure.stdout.replace(" ", "")
            events = [json.loads(line) for line in failure.stdout.splitlines() if line.startswith("{")]
            run_id = next(event["run_id"] for event in events if event.get("run_id"))
            replay = subprocess.run([str(binary), "run", "--replay", run_id, "--json"], cwd=workspace, env=env,
                capture_output=True, text=True, timeout=60)
            (evidence / "error-replay.json").write_text(replay.stdout)
            result["journal_replay_exit"] = replay.returncode
            assert replay.returncode == 0, replay.stderr
            assert DETAIL in replay.stdout, "provider detail absent from journal replay"
            printed = subprocess.run(base + ["Return the printed fixture error."], cwd=workspace, env=env,
                capture_output=True, text=True, timeout=260)
            (evidence / "error-print.txt").write_text(printed.stdout + printed.stderr)
            result["printed_http_400_exit"] = printed.returncode
            assert printed.returncode != 0
            assert DETAIL in printed.stdout + printed.stderr, "provider detail absent from CLI presentation"
            result["verdict"] = "PASS"
        finally:
            # Profile-scoped stop only; no process-name kills or host daemon changes.
            stopped = subprocess.run([str(binary), "daemon", "stop"], cwd=workspace, env=env,
                capture_output=True, text=True, timeout=30)
            (evidence / "daemon-stop.txt").write_text(stopped.stdout + stopped.stderr)
            result["daemon_stop_exit"] = stopped.returncode
            if stopped.returncode not in (0, 69):
                result["verdict"] = "FAIL: fixture daemon did not stop"
            proxy.shutdown()
            proxy.server_close()
            (evidence / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    assert result["verdict"] == "PASS", result
    print(json.dumps(result))


if __name__ == "__main__":
    main()
