#!/usr/bin/env python3
"""Paired E8/L20 direct-versus-Instruct-Pipe measurement.

The provider is a local recording OpenAI chat-completions endpoint.  Every
sample uses the real CLI/daemon and ordinary broker.  Timing is meaningful
only when the caller owns the repository quiet-window grant; this program
records, but does not manufacture, that external grant.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
from datetime import datetime, timezone
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import re
import shlex
import statistics
import sys
import tempfile
import threading
import time
from typing import Any, Mapping, Sequence

from economydiet_measure import ReferenceTokenizer
from turnperf_support import (
    MODEL_ID,
    PROVIDER_ID,
    ProofError,
    ThrowawayProfile,
    chat_chunk,
    parse_json_lines,
    select_exec_tool,
    sha256_file,
)


E8_SEEDS = {
    "a": "alpha architecture notes and stable constraints\n" * 16,
    "b": "bravo interface notes and deterministic inputs\n" * 16,
    "c": "charlie verification notes and expected outputs\n" * 16,
    "d": "delta edge cases and bounded failure behavior\n" * 16,
    "e": "echo integration notes and terminal conditions\n" * 16,
}
E8_OUTPUT = "AHRB economy fixture edit v1\n"
L20_PROMPT = (
    "Read files/01.txt through files/20.txt in numeric order and return a JSON "
    "array containing each path and its full content. Do not modify files."
)
E8_PROMPT = "Complete the standardized AHRB harness-economy task exactly as scripted."
INTER_SAMPLE_COOLDOWN_SECONDS = 4.0


def canonical(value: Any) -> bytes:
    return json.dumps(
        value, ensure_ascii=False, sort_keys=True, separators=(",", ":")
    ).encode("utf-8")


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def percentile95(values: Sequence[float]) -> float:
    ordered = sorted(values)
    return ordered[max(0, (95 * len(ordered) + 99) // 100 - 1)]


def summary(values: Sequence[float]) -> dict[str, float]:
    median = float(statistics.median(values))
    return {
        "median": median,
        "mad": float(statistics.median(abs(value - median) for value in values)),
        "p95": float(percentile95(values)),
    }


def l20_contents() -> dict[str, str]:
    return {
        f"files/{index:02}.txt": (
            f"file={index:02};value={index * index:04}\n" * 64
        )[:1024]
        for index in range(1, 21)
    }


def fixture_manifest(fixture: str) -> dict[str, str]:
    values = (
        {f"context-{key}.txt": value for key, value in E8_SEEDS.items()}
        | {"economy-output.txt": E8_OUTPUT}
        if fixture == "e8"
        else l20_contents()
    )
    return {
        path: sha256_bytes(content.encode("utf-8"))
        for path, content in sorted(values.items())
    }


def workspace_digest(root: Path) -> str:
    digest = hashlib.sha256()
    for path in sorted(value for value in root.rglob("*") if value.is_file()):
        relative = path.relative_to(root).as_posix().encode("utf-8")
        content = path.read_bytes()
        digest.update(len(relative).to_bytes(8, "big"))
        digest.update(relative)
        digest.update(len(content).to_bytes(8, "big"))
        digest.update(content)
    return digest.hexdigest()


def seed_workspace(fixture: str, workspace: Path) -> str:
    if fixture == "l20":
        for relative, content in l20_contents().items():
            path = workspace / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content, encoding="utf-8")
    return workspace_digest(workspace)


def e8_operations(fixture_binary: Path) -> list[tuple[str, str]]:
    def command(*arguments: str) -> str:
        return shlex.join((str(fixture_binary), *arguments))

    values: list[tuple[str, str]] = []
    for key, content in E8_SEEDS.items():
        values.append(
            (
                f"economy-bootstrap-{key}",
                command(
                    "write",
                    "--path",
                    f"context-{key}.txt",
                    "--content",
                    content,
                ),
            )
        )
    for key in E8_SEEDS:
        values.append(
            (
                f"economy-context-{key}",
                command("read", "--path", f"context-{key}.txt"),
            )
        )
    for key in ("a", "c", "e"):
        values.append(
            (
                f"economy-probe-{key}",
                command("read", "--path", f"context-{key}.txt"),
            )
        )
    values.extend(
        [
            (
                "economy-edit",
                command(
                    "write",
                    "--path",
                    "economy-output.txt",
                    "--content",
                    E8_OUTPUT,
                ),
            ),
            (
                "economy-verify",
                command("read", "--path", "economy-output.txt"),
            ),
            (
                "economy-iterate-one",
                command("read", "--path", "context-b.txt"),
            ),
            (
                "economy-iterate-two",
                command("read", "--path", "context-d.txt"),
            ),
        ]
    )
    if len(values) != 17:
        raise ProofError("E8 fixture operation count changed")
    return values


def tool_spec(body: Mapping[str, Any], name: str) -> Mapping[str, Any]:
    tools = body.get("tools")
    for value in tools if isinstance(tools, list) else []:
        if not isinstance(value, Mapping):
            continue
        function = value.get("function")
        candidate = function if isinstance(function, Mapping) else value
        if candidate.get("name") == name:
            return candidate
    raise ProofError(f"provider request does not advertise {name}")


def function_chunks(
    calls: Sequence[tuple[str, str, Mapping[str, Any]]]
) -> tuple[list[bytes], dict[str, Any]]:
    tool_calls = [
        {
            "index": index,
            "id": call_id,
            "type": "function",
            "function": {
                "name": name,
                "arguments": json.dumps(arguments, separators=(",", ":")),
            },
        }
        for index, (call_id, name, arguments) in enumerate(calls)
    ]
    chunks = [
        chat_chunk(MODEL_ID, {"role": "assistant"}),
        chat_chunk(MODEL_ID, {"tool_calls": tool_calls}),
        chat_chunk(MODEL_ID, {}, "tool_calls"),
        b"data: [DONE]\n\n",
    ]
    return chunks, {"tool_calls": tool_calls}


def text_chunks(content: str) -> tuple[list[bytes], dict[str, Any]]:
    chunks = [
        chat_chunk(MODEL_ID, {"role": "assistant"}),
        chat_chunk(MODEL_ID, {"content": content}),
        chat_chunk(MODEL_ID, {}, "stop"),
        b"data: [DONE]\n\n",
    ]
    return chunks, {"content": content}


def value_type(value: Any) -> dict[str, Any]:
    if value is None:
        return {"kind": "null"}
    if isinstance(value, bool):
        return {"kind": "bool"}
    if isinstance(value, int):
        return {"kind": "i64", "min": value, "max": value}
    if isinstance(value, str):
        return {"kind": "string", "max_bytes": max(1, len(value.encode("utf-8")))}
    if isinstance(value, list):
        if not value:
            raise ProofError("fixture cannot infer the item type of an empty list")
        item = value_type(value[0])
        if any(value_type(member) != item for member in value[1:]):
            raise ProofError("fixture list is not homogeneous")
        return {"kind": "list", "item": item, "max_items": len(value)}
    if isinstance(value, Mapping):
        return {
            "kind": "record",
            "fields": {
                str(key): value_type(member)
                for key, member in sorted(value.items())
            },
        }
    raise ProofError(f"unsupported fixture value type {type(value).__name__}")


def tool_script_schema(body: Mapping[str, Any]) -> tuple[str, dict[str, Mapping[str, Any]]]:
    specification = tool_spec(body, "tool_script")
    schema = specification.get("parameters")
    if not isinstance(schema, Mapping):
        schema = specification.get("input_schema")
    if not isinstance(schema, Mapping):
        raise ProofError("tool_script schema is unavailable")
    properties = schema.get("properties")
    extension = schema.get("x-haider-orchestration")
    if not isinstance(properties, Mapping) or not isinstance(extension, Mapping):
        raise ProofError("tool_script schema is missing its orchestration extension")
    digest_schema = properties.get("catalog_digest")
    digest = digest_schema.get("const") if isinstance(digest_schema, Mapping) else None
    wrappers_value = extension.get("wrappers")
    wrapper_values = wrappers_value if isinstance(wrappers_value, list) else []
    wrappers = {
        str(value["tool"]): value
        for value in wrapper_values
        if isinstance(value, Mapping) and isinstance(value.get("tool"), str)
    }
    if not isinstance(digest, str) or not wrappers:
        raise ProofError("tool_script catalog digest/wrappers are unavailable")
    return digest, wrappers


def sequential_script(
    body: Mapping[str, Any], operations: Sequence[tuple[str, Mapping[str, Any]]]
) -> dict[str, Any]:
    digest, wrappers = tool_script_schema(body)
    nodes: list[dict[str, Any]] = []
    awaits: list[int] = []
    for ordinal, (tool, arguments) in enumerate(operations):
        wrapper = wrappers.get(tool)
        if not isinstance(wrapper, Mapping) or not isinstance(
            wrapper.get("wrapper_digest"), str
        ):
            raise ProofError(f"script wrapper unavailable for {tool}")
        wrapper_digest = str(wrapper["wrapper_digest"])
        value_slot = len(nodes)
        nodes.append(
            {
                "slot": value_slot,
                "evidence_type": "OrchValueV1",
                "config": {
                    "operator": "literal",
                    "type": value_type(arguments),
                    "operand_config": {"value": arguments},
                    "region": [],
                    "origin": [ordinal],
                },
                "ports": [],
            }
        )
        argument_slot = len(nodes)
        nodes.append(
            {
                "slot": argument_slot,
                "evidence_type": "OrchArgumentV1",
                "config": {
                    "tool": tool,
                    "wrapper_digest": wrapper_digest,
                    "region": [],
                    "origin": [ordinal],
                },
                "ports": [
                    {
                        "role": "data",
                        "port": "args",
                        "source_slot": value_slot,
                        "output": "value",
                    }
                ],
            }
        )
        retry_slot = len(nodes)
        nodes.append(
            {
                "slot": retry_slot,
                "evidence_type": "OrchRetryPolicyV1",
                "config": {"max_attempts": 1},
                "ports": [],
            }
        )
        ask_slot = len(nodes)
        call_slot = ask_slot + 1
        ask_ports = [
            {
                "role": "data",
                "port": "args",
                "source_slot": argument_slot,
                "output": "value",
            }
        ]
        if awaits:
            ask_ports.append(
                {
                    "role": "control",
                    "port": "after_prior",
                    "source_slot": awaits[-1],
                    "output": "settled",
                }
            )
        nodes.append(
            {
                "slot": ask_slot,
                "evidence_type": "OrchAskPauseV1",
                "config": {
                    "tool": tool,
                    "wrapper_digest": wrapper_digest,
                    "owner_call_slot": call_slot,
                    "wait_ms": 120000,
                    "region": [],
                    "origin": [ordinal],
                },
                "ports": ask_ports,
            }
        )
        nodes.append(
            {
                "slot": call_slot,
                "evidence_type": "OrchCallV1",
                "config": {
                    "tool": tool,
                    "wrapper_digest": wrapper_digest,
                    "region": [],
                    "origin": [ordinal],
                },
                "ports": [
                    {
                        "role": "data",
                        "port": "args",
                        "source_slot": argument_slot,
                        "output": "value",
                    },
                    {
                        "role": "control",
                        "port": "permit",
                        "source_slot": ask_slot,
                        "output": "permit",
                    },
                    {
                        "role": "config",
                        "port": "retry",
                        "source_slot": retry_slot,
                        "output": "policy",
                    },
                ],
            }
        )
        await_slot = len(nodes)
        nodes.append(
            {
                "slot": await_slot,
                "evidence_type": "OrchAwaitV1",
                "config": {"on_error": "stop", "region": [], "origin": [ordinal]},
                "ports": [
                    {
                        "role": "data",
                        "port": "operation",
                        "source_slot": call_slot,
                        "output": "operation",
                    }
                ],
            }
        )
        awaits.append(await_slot)
    join_slot = len(nodes)
    opaque = {
        "kind": "opaque",
        "ref_kind": "tool_result",
        "issuer_version": "1",
        "decoder_version": "1",
    }
    nodes.append(
        {
            "slot": join_slot,
            "evidence_type": "OrchJoinV1",
            "config": {
                "mode": "all",
                "type": {"kind": "list", "item": opaque, "max_items": len(awaits)},
                "omit_inactive": False,
                "region": [],
            },
            "ports": [
                {
                    "role": "data",
                    "port": f"result_{ordinal:03}",
                    "source_slot": source,
                    "output": "value",
                }
                for ordinal, source in enumerate(awaits)
            ],
        }
    )
    exit_slot = len(nodes)
    nodes.append(
        {
            "slot": exit_slot,
            "evidence_type": "OrchExitV1",
            "config": {"mode": "return", "region": []},
            "ports": [
                {
                    "role": "data",
                    "port": "value",
                    "source_slot": join_slot,
                    "output": "value",
                }
            ],
        }
    )
    return {
        "version": 1,
        "transport": "instruct-pipe-dag-v1",
        "catalog_digest": digest,
        "graph": {
            "kind": "inline",
            "parameters": [],
            "nodes": nodes,
            "exits": [exit_slot],
            "read_groups": [],
        },
        "inputs": [],
        "limits": {"returned_bytes": 65536},
    }


@dataclass
class SampleConfig:
    fixture: str
    arm: str
    delay_ms: int
    fixture_binary: Path
    shape_ref: Mapping[str, Any] | None = None


class RecorderState:
    def __init__(self) -> None:
        self.condition = threading.Condition()
        self.config: SampleConfig | None = None
        self.requests: list[dict[str, Any]] = []
        self.generated_source: bytes = b""
        self.errors: list[str] = []
        self.fidelity_issues: list[str] = []

    def begin(self, config: SampleConfig) -> None:
        with self.condition:
            if self.config is not None:
                raise ProofError("measurement provider already owns a sample")
            self.config = config
            self.requests = []
            self.generated_source = b""
            self.errors = []
            self.fidelity_issues = []

    def finish(self) -> tuple[list[dict[str, Any]], bytes, list[str], list[str]]:
        with self.condition:
            requests = list(self.requests)
            generated = self.generated_source
            errors = list(self.errors)
            fidelity_issues = list(self.fidelity_issues)
            self.config = None
            return requests, generated, errors, fidelity_issues

    def respond(self, body: Mapping[str, Any], raw: bytes) -> tuple[list[bytes], dict[str, Any]]:
        with self.condition:
            config = self.config
            if config is None:
                raise ProofError("provider received a request outside a sample")
            request_number = len(self.requests) + 1
            try:
                chunks, semantic = response_for(config, request_number, body, self)
            except ProofError as error:
                self.errors.append(str(error))
                chunks, semantic = text_chunks("measurement fixture rejected")
            wire = b"".join(chunks)
            self.requests.append(
                {
                    "request_number": request_number,
                    "raw_bytes": len(raw),
                    "raw_sha256": sha256_bytes(raw),
                    "_raw": raw,
                    "canonical_bytes": len(canonical(body)),
                    "canonical_sha256": sha256_bytes(canonical(body)),
                    "body": dict(body),
                    "response_semantic": semantic,
                    "response_wire_bytes": len(wire),
                    "response_wire_sha256": sha256_bytes(wire),
                }
            )
            return chunks, semantic


def expected_final(fixture: str) -> str:
    if fixture == "l20":
        return json.dumps(
            [
                {"path": path, "content": content}
                for path, content in l20_contents().items()
            ],
            ensure_ascii=False,
            separators=(",", ":"),
        )
    return json.dumps(
        {
            "status": "success",
            "output_sha256": sha256_bytes(E8_OUTPUT.encode("utf-8")),
            "child_results": 17,
        },
        separators=(",", ":"),
    )


def validate_result_context(
    fixture: str,
    body: Mapping[str, Any],
    script: bool,
    cold_script: bool = False,
    allow_truncated: bool = False,
    expected_tool_messages: int | None = None,
) -> list[str]:
    def strings(value: Any, depth: int = 0) -> list[str]:
        if depth > 8:
            return []
        if isinstance(value, str):
            unnumbered = "".join(
                re.sub(r"^\d+: ", "", line) for line in value.splitlines(keepends=True)
            )
            expanded = []
            for line in value.splitlines(keepends=True):
                match = re.match(r"^(.*) \[repeated (\d+)×\]\n?$", line)
                expanded.append(
                    ((match.group(1) + "\n") * int(match.group(2)))
                    if match
                    else line
                )
            result = [value, unnumbered, "".join(expanded)]
            try:
                decoded = json.loads(value)
            except (json.JSONDecodeError, TypeError):
                return result
            if decoded != value:
                result.extend(strings(decoded, depth + 1))
            return result
        if isinstance(value, Mapping):
            return [text for member in value.values() for text in strings(member, depth + 1)]
        if isinstance(value, list):
            return [text for member in value for text in strings(member, depth + 1)]
        return []

    messages = body.get("messages")
    message_values = messages if isinstance(messages, list) else []
    tool_messages = [
        message
        for message in message_values
        if isinstance(message, Mapping) and message.get("role") == "tool"
    ]
    expected_count = expected_tool_messages
    if expected_count is None:
        expected_count = (
            (2 if cold_script else 1)
            if script
            else (20 if fixture == "l20" else 17)
        )
    if len(tool_messages) != expected_count:
        raise ProofError(
            f"{fixture} result context tool messages expected={expected_count} "
            f"actual={len(tool_messages)}"
        )
    combined = "\n".join(
        text for message in tool_messages for text in strings(message.get("content", ""))
    )
    expected_values = (
        list(l20_contents().values())
        if fixture == "l20"
        else [*E8_SEEDS.values(), E8_OUTPUT]
    )
    missing = [index for index, value in enumerate(expected_values) if value not in combined]
    issues = []
    if missing:
        issue = (
            f"{fixture} provider context does not preserve every full read result; "
            f"missing={missing} tool_content_lengths="
            f"{[len(str(message.get('content', ''))) for message in tool_messages]} "
            f"first={str(tool_messages[0].get('content', ''))[:400]!r}"
        )
        if not allow_truncated:
            raise ProofError(issue)
        issues.append(issue)
    if "[haider:truncated" in combined:
        issue = f"{fixture} provider context contains a truncated result"
        if not allow_truncated:
            raise ProofError(issue)
        issues.append(issue)
    return issues


def direct_calls(
    config: SampleConfig, request_number: int, body: Mapping[str, Any]
) -> list[tuple[str, str, Mapping[str, Any]]]:
    if config.fixture == "l20":
        if config.arm == "batch":
            indices = range(1, 21) if request_number == 1 else range(0)
        else:
            indices = [request_number] if request_number <= 20 else []
        if indices:
            tool_spec(body, "fs_read")
        return [
            (
                f"l20-read-{index:02}",
                "fs_read",
                {"path": f"files/{index:02}.txt"},
            )
            for index in indices
        ]
    operations = e8_operations(config.fixture_binary)
    stages = [operations[0:5], operations[5:10], operations[10:13]] + [
        operations[13:14],
        operations[14:15],
        operations[15:16],
        operations[16:17],
    ]
    if request_number > len(stages):
        return []
    calls = []
    for call_id, command in stages[request_number - 1]:
        name, arguments = select_exec_tool(body, command)
        if name != "process_exec":
            raise ProofError(f"E8 selected unexpected process tool {name}")
        calls.append((call_id, name, arguments))
    return calls


def script_operations(
    config: SampleConfig, body: Mapping[str, Any]
) -> list[tuple[str, Mapping[str, Any]]]:
    if config.fixture == "l20":
        tool_spec(body, "fs_read")
        return [
            ("fs_read", {"path": f"files/{index:02}.txt"})
            for index in range(1, 21)
        ]
    return [
        ("process_exec", select_exec_tool(body, command)[1])
        for _, command in e8_operations(config.fixture_binary)
    ]


def response_for(
    config: SampleConfig,
    request_number: int,
    body: Mapping[str, Any],
    state: RecorderState,
) -> tuple[list[bytes], dict[str, Any]]:
    script = config.arm in ("script", "cold_script", "ref_script")
    if config.arm == "cold_script" and request_number == 1:
        tool_spec(body, "list_tools")
        return function_chunks(
            [("measure-discover", "list_tools", {"filter": "tool_script"})]
        )
    script_request = request_number - (1 if config.arm == "cold_script" else 0)
    if script and script_request == 1:
        if config.arm == "ref_script":
            if config.shape_ref is None:
                raise ProofError("cached-ref arm has no admitted shape handle")
            digest, _ = tool_script_schema(body)
            source = {
                "version": 1,
                "transport": "instruct-pipe-dag-v1",
                "catalog_digest": digest,
                "graph": {"kind": "ref", "root": dict(config.shape_ref)},
                "inputs": [],
                "limits": {"returned_bytes": 65536},
            }
        else:
            source = sequential_script(body, script_operations(config, body))
        state.generated_source = canonical(source)
        return function_chunks([("measure-script", "tool_script", source)])
    if script:
        issues = validate_result_context(
            config.fixture,
            body,
            True,
            config.arm == "cold_script",
            expected_tool_messages=2 if config.arm == "ref_script" else None,
        )
        state.fidelity_issues.extend(issues)
        return text_chunks(expected_final(config.fixture))
    calls = direct_calls(config, request_number, body)
    if calls:
        return function_chunks(calls)
    issues = validate_result_context(
        config.fixture, body, False, allow_truncated=config.arm == "base"
    )
    state.fidelity_issues.extend(issues)
    return text_chunks(expected_final(config.fixture))


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    state: RecorderState

    def log_message(self, _format: str, *_args: object) -> None:
        return

    def do_POST(self) -> None:  # noqa: N802
        length = int(self.headers.get("Content-Length", "0"))
        raw = self.rfile.read(max(0, length))
        try:
            decoded = json.loads(raw)
        except (UnicodeDecodeError, json.JSONDecodeError):
            decoded = {}
        body = decoded if isinstance(decoded, Mapping) else {}
        chunks, _ = self.state.respond(body, raw)
        config = self.state.config
        if config is not None and config.delay_ms:
            time.sleep(config.delay_ms / 1000)
        payload = b"".join(chunks)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)
        self.wfile.flush()

    def do_GET(self) -> None:  # noqa: N802
        if self.path.rstrip("/").endswith("/models"):
            payload = canonical(
                {
                    "object": "list",
                    "data": [{"id": MODEL_ID, "object": "model", "created": 1}],
                }
            )
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
            return
        self.send_error(404)


class RecorderServer:
    def __init__(self) -> None:
        self.state = RecorderState()
        handler = type("OrchestrationMeasureHandler", (Handler,), {"state": self.state})
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    @property
    def url(self) -> str:
        host, port = self.server.server_address[:2]
        return f"http://{host}:{port}/v1"

    def __enter__(self) -> "RecorderServer":
        self.thread.start()
        return self

    def __exit__(self, *_args: object) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)


def expected_requests(fixture: str, arm: str) -> int:
    if arm == "cold_script":
        return 3
    if arm in ("script", "ref_script", "batch"):
        return 2
    return 21 if fixture == "l20" else 8


def verify_workspace(fixture: str, workspace: Path, before: str) -> str:
    after = workspace_digest(workspace)
    if fixture == "l20":
        if after != before:
            raise ProofError("L20 modified its read-only workspace")
        for relative, content in l20_contents().items():
            if (workspace / relative).read_text(encoding="utf-8") != content:
                raise ProofError(f"L20 full-content mismatch for {relative}")
    else:
        expected = {f"context-{key}.txt": value for key, value in E8_SEEDS.items()}
        expected["economy-output.txt"] = E8_OUTPUT
        for relative, content in expected.items():
            if (workspace / relative).read_text(encoding="utf-8") != content:
                raise ProofError(f"E8 final content mismatch for {relative}")
    return after


def child_rows(events: Sequence[Mapping[str, Any]], script: bool) -> list[dict[str, Any]]:
    orchestration_attempts: dict[str, dict[str, Any]] = {}
    if script:
        for event in events:
            payload = event.get("payload")
            if not isinstance(payload, Mapping) or payload.get("type") != "item":
                continue
            item = payload.get("item")
            if (
                payload.get("event") != "completed"
                or not isinstance(item, Mapping)
                or item.get("item") != "extension"
                or item.get("kind") != "orchestration_call_v1"
            ):
                continue
            data = item.get("data")
            if not isinstance(data, Mapping) or not isinstance(data.get("call_id"), str):
                continue
            call_id = str(data["call_id"])
            row = orchestration_attempts.setdefault(call_id, {})
            phase = data.get("phase")
            if phase == "started":
                row["started_at_ms"] = event.get("committed_at_ms")
            elif phase == "completed":
                row["finished_at_ms"] = event.get("committed_at_ms")
                receipt = data.get("receipt_ref")
                if isinstance(receipt, Mapping):
                    row["receipt_ledger_digest"] = receipt.get("ledger_digest")
                    row["receipt_bytes"] = receipt.get("byte_len")
    result = []
    for event in events:
        payload = event.get("payload")
        if not isinstance(payload, Mapping) or payload.get("type") != "tool_result":
            continue
        call_id = str(payload.get("call_id", ""))
        if script != call_id.startswith("orch:"):
            continue
        timing = orchestration_attempts.get(call_id, {})
        started = timing.get("started_at_ms")
        finished = timing.get("finished_at_ms")
        result.append(
            {
                "call_id": call_id,
                "seq": event.get("seq"),
                "committed_at_ms": event.get("committed_at_ms"),
                "status": (
                    payload.get("result", {}).get("status", "completed")
                    if isinstance(payload.get("result"), Mapping)
                    else "completed"
                ),
                "payload_sha256": sha256_bytes(canonical(payload)),
                **timing,
                "duration_ms": (
                    int(finished) - int(started)
                    if isinstance(started, int) and isinstance(finished, int)
                    else None
                ),
            }
        )
    return result


def orchestration_row(
    events: Sequence[Mapping[str, Any]], generated: bytes
) -> dict[str, Any] | None:
    script_data: Mapping[str, Any] | None = None
    terminal_data: Mapping[str, Any] | None = None
    script_committed_at_ms: int | None = None
    for event in events:
        payload = event.get("payload")
        if not isinstance(payload, Mapping) or payload.get("type") != "item":
            continue
        item = payload.get("item")
        if (
            payload.get("event") != "completed"
            or not isinstance(item, Mapping)
            or item.get("item") != "extension"
            or not isinstance(item.get("data"), Mapping)
        ):
            continue
        if item.get("kind") == "orchestration_script_v1":
            script_data = item["data"]
            committed = event.get("committed_at_ms")
            script_committed_at_ms = committed if isinstance(committed, int) else None
        elif item.get("kind") == "orchestration_terminal_v1":
            terminal_data = item["data"]
    if script_data is None or terminal_data is None:
        return None

    refs: dict[str, Mapping[str, Any]] = {}
    candidates = [
        script_data.get("shape_ref"),
        script_data.get("source_ref"),
        terminal_data.get("final_checkpoint"),
        terminal_data.get("terminal_ref"),
    ]
    receipts = terminal_data.get("receipt_refs")
    if isinstance(receipts, list):
        candidates.extend(receipts)
    for value in candidates:
        if isinstance(value, Mapping) and isinstance(value.get("artifact"), str):
            refs[str(value["artifact"])] = value

    source = json.loads(generated) if generated else {}
    graph = source.get("graph") if isinstance(source, Mapping) else None
    nodes = graph.get("nodes") if isinstance(graph, Mapping) else None
    node_values = nodes if isinstance(nodes, list) else []
    started_at_ms = terminal_data.get("started_at_ms")
    finished_at_ms = terminal_data.get("finished_at_ms")
    return {
        "graph_mode": graph.get("kind") if isinstance(graph, Mapping) else None,
        "shape_digest": terminal_data.get("shape_digest"),
        "source_digest": terminal_data.get("source_digest"),
        "shape_ref": script_data.get("shape_ref"),
        "source_ref": script_data.get("source_ref"),
        "terminal_ref": terminal_data.get("terminal_ref"),
        "final_checkpoint": terminal_data.get("final_checkpoint"),
        "counts": terminal_data.get("counts"),
        "selected_exit": terminal_data.get("selected_exit"),
        "expanded_nodes": len(node_values),
        "expanded_edges": sum(
            len(node.get("ports", []))
            for node in node_values
            if isinstance(node, Mapping) and isinstance(node.get("ports", []), list)
        ),
        "observable_unique_cas_bytes": sum(
            int(value.get("byte_len", 0)) for value in refs.values()
        ),
        "observable_unique_cas_objects": len(refs),
        "receipt_bytes": sum(
            int(value.get("byte_len", 0))
            for value in receipts
            if isinstance(value, Mapping)
        )
        if isinstance(receipts, list)
        else 0,
        "admission_to_script_commit_ms": (
            script_committed_at_ms - int(started_at_ms)
            if script_committed_at_ms is not None and isinstance(started_at_ms, int)
            else None
        ),
        "script_elapsed_ms": (
            int(finished_at_ms) - int(started_at_ms)
            if isinstance(started_at_ms, int) and isinstance(finished_at_ms, int)
            else None
        ),
        "wrapper_preexposed": True,
        "shape_cache_hit": (
            isinstance(graph, Mapping) and graph.get("kind") == "ref"
        ),
        "admission_type_check_us": terminal_data.get("admission_us"),
        "scheduling_checkpoint_us": terminal_data.get("scheduling_us"),
    }


def request_role(fixture: str, arm: str, request_number: int) -> str:
    if arm == "cold_script" and request_number == 1:
        return "discovery"
    if arm in ("script", "cold_script") and request_number == (
        2 if arm == "cold_script" else 1
    ):
        return "generate"
    expected = expected_requests(fixture, arm)
    return "final" if request_number == expected else "direct_tool"


def run_sample(
    bin_dir: Path,
    server: RecorderServer,
    tokenizer: ReferenceTokenizer,
    fixture_binary: Path,
    fixture: str,
    arm: str,
    delay_ms: int,
    ordinal: int,
    measured: bool,
) -> dict[str, Any]:
    config = SampleConfig(fixture, arm, delay_ms, fixture_binary)
    server.state.begin(config)
    profile = ThrowawayProfile(
        bin_dir,
        server.url,
        root=Path(tempfile.mkdtemp(prefix="orch-measure-", dir="/tmp")),
    )
    profile.env["HAIDER_TOOL_PROFILE"] = "inspection" if fixture == "l20" else "automation"
    if arm in ("direct", "script", "batch"):
        profile.env["HAIDER_TOOL_EXPOSURE"] = "tool_script"
    else:
        profile.env.pop("HAIDER_TOOL_EXPOSURE", None)
    before = seed_workspace(fixture, profile.workspace)
    stopped = False
    try:
        profile.ready()
        prompt = L20_PROMPT if fixture == "l20" else E8_PROMPT
        command = [
            "run",
            "-p",
            prompt,
            "--provider",
            PROVIDER_ID,
            "--model",
            MODEL_ID,
            "--output",
            "jsonl",
            "--timeout",
            "120s",
            "--auto-allow",
            "--allow-writes",
            "--allow-exec",
        ]
        run = profile.command(command, timeout=150)
        requests, generated, provider_errors, fidelity_issues = server.state.finish()
        documents = parse_json_lines(run.stdout, f"{fixture}/{arm} sample")
        events = documents[1:] if documents and documents[0].get("event") == "accepted" else []
        if run.returncode != 0 or run.timed_out:
            raise ProofError(
                f"{fixture}/{arm} run failed exit={run.returncode} timeout={run.timed_out} "
                f"stderr={run.stderr[-300:]!r}"
            )
        if provider_errors:
            raise ProofError("; ".join(provider_errors))
        if len(requests) != expected_requests(fixture, arm):
            raise ProofError(
                f"{fixture}/{arm} provider requests expected={expected_requests(fixture, arm)} "
                f"actual={len(requests)}"
            )
        script = arm in ("script", "cold_script")
        children = child_rows(events, script)
        expected_children = 20 if fixture == "l20" else 17
        if len(children) != expected_children or any(
            child["status"] != "completed" for child in children
        ):
            raise ProofError(
                f"{fixture}/{arm} child results expected={expected_children} "
                f"actual={len(children)} statuses={[child['status'] for child in children]}"
            )
        after = verify_workspace(fixture, profile.workspace, before)
        request_rows = []
        total_input_tokens = 0
        total_output_tokens = 0
        for request in requests:
            request_number = int(request["request_number"])
            body = request.pop("body")
            raw_bytes = request.pop("_raw")
            input_bytes = canonical(body)
            output_bytes = canonical(request.pop("response_semantic"))
            input_tokens = tokenizer.count(input_bytes)
            output_tokens = tokenizer.count(output_bytes)
            wire_tokens = tokenizer.count(raw_bytes)
            tools_bytes = canonical(body.get("tools", []))
            messages = body.get("messages", [])
            messages_bytes = canonical(messages)
            tool_messages = [
                message
                for message in messages
                if isinstance(message, Mapping) and message.get("role") == "tool"
            ]
            projection_bytes = canonical(tool_messages)
            total_input_tokens += input_tokens
            total_output_tokens += output_tokens
            request_rows.append(
                request
                | {
                    "role": request_role(fixture, arm, request_number),
                    "wire_input_tokens": wire_tokens,
                    "canonical_input_tokens": input_tokens,
                    "tool_schema_bytes": len(tools_bytes),
                    "tool_schema_tokens": tokenizer.count(tools_bytes),
                    "messages_bytes": len(messages_bytes),
                    "messages_tokens": tokenizer.count(messages_bytes),
                    "tool_result_projection_bytes": len(projection_bytes),
                    "tool_result_projection_tokens": tokenizer.count(projection_bytes),
                    "semantic_output_bytes": len(output_bytes),
                    "semantic_output_sha256": sha256_bytes(output_bytes),
                    "semantic_output_tokens": output_tokens,
                }
            )
        generated_tokens = tokenizer.count(generated) if generated else 0
        terminal = [
            event
            for event in events
            if isinstance(event.get("payload"), Mapping)
            and event["payload"].get("terminal_kind") is not None
        ]
        if len(terminal) != 1 or terminal[0]["payload"].get("terminal_kind") != "success":
            raise ProofError(f"{fixture}/{arm} has no unique success terminal")
        orchestration = orchestration_row(events, generated) if script else None
        if script and orchestration is None:
            raise ProofError(f"{fixture}/{arm} has no durable orchestration terminal")
        load = float(os.getloadavg()[0])
        result = {
            "fixture": fixture,
            "arm": arm,
            "delay_ms": delay_ms,
            "ordinal": ordinal,
            "measured": measured,
            "load_1m": load,
            "wall_ms": run.wall_ms,
            "provider_requests": len(request_rows),
            "provider_retries": 0,
            "child_attempts": len(children),
            "child_retries": 0,
            "input_tokens": total_input_tokens,
            "output_tokens": total_output_tokens,
            "total_tokens": total_input_tokens + total_output_tokens,
            "generated_source_bytes": len(generated),
            "generated_source_tokens": generated_tokens,
            "generated_source_sha256": sha256_bytes(generated) if generated else None,
            "graph_mode": "inline" if script else None,
            "wrapper_preexposed": arm == "script",
            "cold_schema_discovery": arm == "cold_script",
            "shape_cache_hit": False if script else None,
            "request_rows": request_rows,
            "child_rows": children,
            "orchestration": orchestration,
            "final_context_bytes": request_rows[-1]["canonical_bytes"],
            "final_context_tokens": request_rows[-1]["canonical_input_tokens"],
            "context_curve_tokens": [
                request["canonical_input_tokens"] for request in request_rows
            ],
            "workspace_before_sha256": before,
            "workspace_after_sha256": after,
            "fidelity_passed": not fidelity_issues,
            "fidelity_issues": fidelity_issues,
            "terminal_seq": terminal[0].get("seq"),
            "run_stdout_sha256": sha256_bytes(run.stdout.encode("utf-8")),
            "passed": True,
        }
        stop = profile.stop()
        stopped = True
        if stop.returncode != 0:
            raise ProofError(f"{fixture}/{arm} daemon stop failed")
        return result
    finally:
        if server.state.config is not None:
            server.state.finish()
        if not stopped:
            profile.stop()
        profile.dispose()


def run_cached_shape_ref(
    bin_dir: Path,
    server: RecorderServer,
    tokenizer: ReferenceTokenizer,
    fixture_binary: Path,
) -> dict[str, Any]:
    """Populate one L20 shape, then execute its ref in the same real session."""

    profile = ThrowawayProfile(
        bin_dir,
        server.url,
        root=Path(tempfile.mkdtemp(prefix="orch-ref-measure-", dir="/tmp")),
    )
    profile.env["HAIDER_TOOL_PROFILE"] = "inspection"
    profile.env["HAIDER_TOOL_EXPOSURE"] = "tool_script"
    before = seed_workspace("l20", profile.workspace)

    def execute(
        config: SampleConfig, session_id: str | None
    ) -> tuple[dict[str, Any], str]:
        server.state.begin(config)
        command = ["run", "-p", L20_PROMPT, "--output", "jsonl", "--timeout", "120s"]
        if session_id is None:
            command.extend(
                [
                    "--provider",
                    PROVIDER_ID,
                    "--model",
                    MODEL_ID,
                    "--auto-allow",
                    "--allow-writes",
                    "--allow-exec",
                ]
            )
        else:
            command.extend(["--session", session_id])
        run = profile.command(command, timeout=150)
        requests, generated, provider_errors, fidelity_issues = server.state.finish()
        if run.returncode != 0 or run.timed_out:
            raise ProofError(
                f"l20/{config.arm} failed exit={run.returncode} "
                f"timeout={run.timed_out} stdout={run.stdout[-300:]!r} "
                f"stderr={run.stderr[-300:]!r}"
            )
        documents = parse_json_lines(run.stdout, f"l20/{config.arm} cached-ref proof")
        accepted = next(
            (row for row in documents if row.get("event") == "accepted"), None
        )
        if not isinstance(accepted, Mapping) or not isinstance(
            accepted.get("session_id"), str
        ):
            raise ProofError(f"l20/{config.arm} omitted accepted session identity")
        actual_session = str(accepted["session_id"])
        if session_id is not None and actual_session != session_id:
            raise ProofError("cached-ref continuation changed session identity")
        events = documents[1:]
        if provider_errors:
            raise ProofError("; ".join(provider_errors))
        if fidelity_issues:
            raise ProofError("; ".join(fidelity_issues))
        if len(requests) != 2:
            raise ProofError(
                f"l20/{config.arm} provider requests expected=2 actual={len(requests)}"
            )
        children = child_rows(events, True)
        if len(children) != 20 or any(
            child["status"] != "completed" for child in children
        ):
            raise ProofError(
                f"l20/{config.arm} child results expected=20 actual={len(children)}"
            )
        terminals = [
            event
            for event in events
            if isinstance(event.get("payload"), Mapping)
            and event["payload"].get("terminal_kind") is not None
        ]
        if len(terminals) != 1 or terminals[0]["payload"].get("terminal_kind") != "success":
            raise ProofError(f"l20/{config.arm} has no unique success terminal")
        orchestration = orchestration_row(events, generated)
        if orchestration is None:
            raise ProofError(f"l20/{config.arm} has no orchestration evidence")
        input_tokens = sum(
            tokenizer.count(canonical(request["body"])) for request in requests
        )
        output_tokens = sum(
            tokenizer.count(canonical(request["response_semantic"]))
            for request in requests
        )
        return (
            {
                "arm": config.arm,
                "wall_ms": run.wall_ms,
                "provider_requests": len(requests),
                "child_attempts": len(children),
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "total_tokens": input_tokens + output_tokens,
                "generated_source_bytes": len(generated),
                "generated_source_tokens": tokenizer.count(generated),
                "generated_source_sha256": sha256_bytes(generated),
                "orchestration": orchestration,
                "terminal_seq": terminals[0].get("seq"),
                "fidelity_passed": True,
            },
            actual_session,
        )

    try:
        profile.ready()
        inline, session_id = execute(
            SampleConfig("l20", "script", 0, fixture_binary), None
        )
        shape_ref = inline["orchestration"].get("shape_ref")
        if not isinstance(shape_ref, Mapping):
            raise ProofError("inline cache-population turn omitted its shape ref")
        cached, continued_session = execute(
            SampleConfig("l20", "ref_script", 0, fixture_binary, shape_ref),
            session_id,
        )
        if continued_session != session_id:
            raise ProofError("cached-ref execution did not use the population session")
        if cached["orchestration"].get("shape_digest") != inline[
            "orchestration"
        ].get("shape_digest"):
            raise ProofError("cached-ref execution changed the admitted shape digest")
        after = verify_workspace("l20", profile.workspace, before)
        return {
            "fixture": "l20",
            "session_id_sha256": sha256_bytes(session_id.encode("utf-8")),
            "workspace_before_sha256": before,
            "workspace_after_sha256": after,
            "cache_population": inline,
            "cached_shape_ref": cached,
            "passed": True,
        }
    finally:
        if server.state.config is not None:
            server.state.finish()
        stop = profile.stop()
        if stop.returncode != 0:
            raise ProofError("cached-ref daemon stop failed")
        profile.dispose()


def paired_schedule(left: str, right: str) -> list[str]:
    return [value for _ in range(5) for value in (left, right, right, left)]


def comparison_summary(name: str, samples: Sequence[Mapping[str, Any]]) -> dict[str, Any]:
    arms = sorted({str(sample["arm"]) for sample in samples})
    if len(arms) != 2:
        raise ProofError(f"comparison {name} does not contain exactly two arms")
    by_arm = {arm: [sample for sample in samples if sample["arm"] == arm] for arm in arms}
    left, right = arms
    metrics = {}
    for arm, rows in by_arm.items():
        metrics[arm] = {
            "samples": len(rows),
            "wall_ms": summary([float(row["wall_ms"]) for row in rows]),
            "input_tokens": summary([float(row["input_tokens"]) for row in rows]),
            "output_tokens": summary([float(row["output_tokens"]) for row in rows]),
            "total_tokens": summary([float(row["total_tokens"]) for row in rows]),
            "provider_requests": sorted({row["provider_requests"] for row in rows}),
            "child_attempts": sorted({row["child_attempts"] for row in rows}),
            "final_context_tokens": summary(
                [float(row["final_context_tokens"]) for row in rows]
            ),
        }
    deltas: dict[str, list[float]] = {"wall_ms": [], "total_tokens": []}
    ratios: dict[str, list[float]] = {"wall_ms": [], "total_tokens": []}
    for offset in range(0, len(samples), 4):
        block = samples[offset : offset + 4]
        if [row["arm"] for row in block] != [left, right, right, left]:
            raise ProofError(f"comparison {name} has a non-canonical paired block")
        for left_row, right_row in ((block[0], block[1]), (block[3], block[2])):
            for metric in deltas:
                left_value = float(left_row[metric])
                right_value = float(right_row[metric])
                deltas[metric].append(right_value - left_value)
                ratios[metric].append(right_value / left_value)
    return {
        "name": name,
        "arms": metrics,
        "arm_names": [left, right],
        "paired_right_minus_left": {
            metric: summary(values) for metric, values in deltas.items()
        },
        "paired_right_over_left": {
            metric: summary(values) for metric, values in ratios.items()
        },
    }


def savings(direct: Mapping[str, Any], script: Mapping[str, Any]) -> dict[str, float]:
    direct_total = float(direct["total_tokens"]["median"])
    script_total = float(script["total_tokens"]["median"])
    direct_input = float(direct["input_tokens"]["median"])
    script_input = float(script["input_tokens"]["median"])
    direct_wall = float(direct["wall_ms"]["median"])
    script_wall = float(script["wall_ms"]["median"])
    return {
        "net_token_savings_fraction": (direct_total - script_total) / direct_total,
        "input_token_savings_fraction": (direct_input - script_input) / direct_input,
        "wall_savings_ms": direct_wall - script_wall,
        "wall_savings_fraction": (direct_wall - script_wall) / direct_wall,
    }


def ensure_quiet(args: argparse.Namespace) -> None:
    if not args.quiet_request.is_file() or not args.quiet_grant.is_file():
        raise ProofError("quiet-window request/grant marker disappeared")
    if args.quiet_grant.stat().st_mtime_ns < args.quiet_request.stat().st_mtime_ns:
        raise ProofError("quiet-window grant is stale relative to this lane's request")
    if os.getloadavg()[0] >= 2:
        raise ProofError("load(1m) is not below 2 during the granted quiet window")


def run_measurement(args: argparse.Namespace) -> dict[str, Any]:
    tokenizer = ReferenceTokenizer(args.vocabulary)
    raw_samples: list[dict[str, Any]] = []
    comparisons: list[dict[str, Any]] = []
    sample_ordinal = 0
    cached_shape_ref: dict[str, Any] | None = None
    with RecorderServer() as server:
        for fixture in ("e8", "l20"):
            for delay in (0, 100):
                specifications = [
                    (
                        "base-control",
                        "base",
                        "direct",
                        args.base_bin_dir,
                        args.bin_dir,
                    ),
                    (
                        "direct-script",
                        "direct",
                        "script",
                        args.bin_dir,
                        args.bin_dir,
                    ),
                    (
                        "cold-entry",
                        "cold_direct",
                        "cold_script",
                        args.bin_dir,
                        args.bin_dir,
                    ),
                ]
                if fixture == "l20":
                    specifications.append(
                        (
                            "batch-script",
                            "batch",
                            "script",
                            args.bin_dir,
                            args.bin_dir,
                        )
                    )
                for comparison, left, right, left_bin, right_bin in specifications:
                    if comparison != "cold-entry":
                        for arm, bin_dir in ((left, left_bin), (right, right_bin)):
                            for _ in range(2):
                                ensure_quiet(args)
                                sample_ordinal += 1
                                run_sample(
                                    bin_dir,
                                    server,
                                    tokenizer,
                                    args.fixture_binary,
                                    fixture,
                                    arm,
                                    delay,
                                    sample_ordinal,
                                    False,
                                )
                                # Keep process churn from becoming its own
                                # sustained machine load. This pause is outside
                                # the timed sample and is disclosed below.
                                time.sleep(INTER_SAMPLE_COOLDOWN_SECONDS)
                    measured: list[dict[str, Any]] = []
                    for arm in paired_schedule(left, right):
                        ensure_quiet(args)
                        sample_ordinal += 1
                        row = run_sample(
                            left_bin if arm == left else right_bin,
                            server,
                            tokenizer,
                            args.fixture_binary,
                            fixture,
                            arm,
                            delay,
                            sample_ordinal,
                            True,
                        )
                        raw_samples.append(row)
                        measured.append(row)
                        time.sleep(INTER_SAMPLE_COOLDOWN_SECONDS)
                    comparison_row = comparison_summary(
                        f"{fixture}-{delay}ms-{comparison}", measured
                    )
                    if comparison in ("direct-script", "cold-entry", "batch-script"):
                        left_metrics = comparison_row["arms"][left]
                        right_metrics = comparison_row["arms"][right]
                        comparison_row["right_vs_left"] = savings(left_metrics, right_metrics)
                    comparisons.append(comparison_row)
        ensure_quiet(args)
        cached_shape_ref = run_cached_shape_ref(
            args.bin_dir, server, tokenizer, args.fixture_binary
        )
    if any(sample["load_1m"] >= 2 for sample in raw_samples):
        raise ProofError("a measured sample exceeded the required load(1m) < 2 quiet bound")
    return {
        "schema": "haider.orchestration-measure.v1",
        "created_at_utc": datetime.now(timezone.utc).isoformat(),
        "schedule": {
            "warmups_per_arm": 2,
            "measured_blocks": 5,
            "block_order": ["left", "right", "right", "left"],
            "delays_ms": [0, 100],
            "inter_sample_cooldown_seconds": INTER_SAMPLE_COOLDOWN_SECONDS,
            "quiet_grant": str(args.quiet_grant),
            "quiet_grant_mtime_ns": args.quiet_grant.stat().st_mtime_ns,
            "quiet_request": str(args.quiet_request),
            "quiet_request_mtime_ns": args.quiet_request.stat().st_mtime_ns,
        },
        "reference_tokenizer": {
            "vocabulary": str(args.vocabulary.resolve()),
            "vocabulary_sha256": tokenizer.sha256,
            "implementation_sha256": sha256_file(
                Path(__file__).with_name("economydiet_measure.py")
            ),
        },
        "binaries": {
            "base": {
                "commit": args.base_commit,
                "haider_sha256": sha256_file(args.base_bin_dir / "haider"),
                "haiderd_sha256": sha256_file(args.base_bin_dir / "haiderd"),
            },
            "implementation": {
                "commit": args.implementation_commit,
                "haider_sha256": sha256_file(args.bin_dir / "haider"),
                "haiderd_sha256": sha256_file(args.bin_dir / "haiderd"),
            },
            "ahrb_fixture_sha256": sha256_file(args.fixture_binary),
        },
        "fixtures": {
            "e8": fixture_manifest("e8"),
            "l20": fixture_manifest("l20"),
        },
        "comparisons": comparisons,
        "cached_shape_ref": cached_shape_ref,
        "samples": raw_samples,
        "all_execution_passed": all(sample["passed"] for sample in raw_samples),
        "implementation_fidelity_passed": all(
            sample["fidelity_passed"]
            for sample in raw_samples
            if sample["arm"] != "base"
        ),
        "baseline_fidelity_passed": all(
            sample["fidelity_passed"]
            for sample in raw_samples
            if sample["arm"] == "base"
        ),
    }


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--base-bin-dir", type=Path, required=True)
    parser.add_argument("--fixture-binary", type=Path, required=True)
    parser.add_argument("--vocabulary", type=Path, required=True)
    parser.add_argument("--quiet-grant", type=Path, required=True)
    parser.add_argument("--quiet-request", type=Path, required=True)
    parser.add_argument("--base-commit", required=True)
    parser.add_argument("--implementation-commit", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args(sys.argv[1:] if argv is None else argv)
    try:
        ensure_quiet(args)
        report = run_measurement(args)
    except Exception as error:
        print(f"measurement failed: {type(error).__name__}: {error}", file=sys.stderr)
        return 1
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(
        json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
