#!/usr/bin/env python3
"""Split a read-only AHRB economy capture into exact prefix/envelope evidence.

The reference tokenizer is independently implemented using AHRB's read-only
vocabulary. Every full request and the combined stable prefix must agree with
the authoritative AHRB report before this program publishes a measurement.
This is the provider-neutral AHRB reference BPE, not a provider token estimate.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
from pathlib import Path
import re
import shlex
import statistics
import unicodedata


def canonical(value: object) -> bytes:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode()


class ReferenceTokenizer:
    """AHRB's merge algorithm and Unicode pretokenizer, over observed text.

    Python's stdlib re has no Unicode property escapes. Build equivalent
    character classes from the characters actually present in the measured
    text; unobserved Unicode code points cannot affect these matches.
    """

    def __init__(self, vocabulary: Path):
        data = vocabulary.read_bytes()
        self.sha256 = hashlib.sha256(data).hexdigest()
        self.ranks = {bytes([byte]): byte for byte in range(256)}
        for line in data.decode().splitlines():
            if not line.strip() or line.startswith("#"):
                continue
            token, rank = line.split()
            self.ranks[base64.b64decode(token)] = int(rank)
        self.piece_cache: dict[bytes, int] = {}

    def piece_count(self, piece: bytes) -> int:
        if piece in self.piece_cache:
            return self.piece_cache[piece]
        if piece in self.ranks:
            return 1
        tokens = [bytes([byte]) for byte in piece]
        while len(tokens) > 1:
            candidates = [(self.ranks[a + b], i) for i, (a, b) in
                          enumerate(zip(tokens, tokens[1:])) if a + b in self.ranks]
            if not candidates:
                break
            _, i = min(candidates)
            tokens[i:i + 2] = [tokens[i] + tokens[i + 1]]
        self.piece_cache[piece] = len(tokens)
        return len(tokens)

    def count(self, data: bytes) -> int:
        text = data.decode()
        chars = sorted(set(text))

        def properties(*names: str) -> str:
            return "".join(re.escape(char) for char in chars if any(
                unicodedata.category(char).startswith(name) for name in names))

        letters, numbers = properties("L"), properties("N")
        upper = properties("Lu", "Lt", "Lm", "Lo", "M") or r"\uFFFF"
        lower = properties("Ll", "Lm", "Lo", "M") or r"\uFFFF"
        contraction = r"(?i:'s|'t|'re|'ve|'m|'ll|'d)?"
        pattern = re.compile(
            rf"[^\r\n{letters}{numbers}]?[{upper}]*[{lower}]+{contraction}"
            rf"|[^\r\n{letters}{numbers}]?[{upper}]+[{lower}]*{contraction}"
            rf"|[{numbers or chr(0xFFFF)}]{{1,3}}"
            rf"| ?[^\s{letters}{numbers}]+[\r\n/]*"
            r"|\s*[\r\n]+|\s+")
        pieces = pattern.findall(text)
        if "".join(pieces) != text:
            raise ValueError("reference pretokenizer did not cover every input byte")
        return sum(self.piece_count(piece.encode()) for piece in pieces)


def common_prefix(sequences: list[list[dict]]) -> list[dict]:
    prefix = []
    for values in zip(*sequences):
        if any(canonical(value) != canonical(values[0]) for value in values[1:]):
            break
        prefix.append(values[0])
    return prefix


def instructions(body: dict) -> list[dict]:
    result = []
    for message in body["messages"]:
        if message.get("role") not in ("system", "developer"):
            break
        result.append(message)
    return result


def semantic_output(content: str) -> str:
    # The one retained truncation line is envelope metadata on both versions.
    # Plain-text results gained this shape when the JSON receipt was removed.
    plain = re.sub(r"\n\[haider:truncated [^\n]*\]$", "", content)
    try:
        payload, end = json.JSONDecoder().raw_decode(content)
        suffix = content[end:].strip()
        if suffix and not suffix.startswith("[haider:truncated "):
            return plain
    except ValueError:
        return plain
    if isinstance(payload, dict):
        if isinstance(payload.get("output"), str) and any(
                field in payload for field in ("effect_id", "exit_code", "status", "limits")):
            return payload["output"]
        if "workspace_mutation" in payload and isinstance(payload.get("result"), str):
            return payload["result"]
    return plain


def captured_arguments(call: dict) -> dict:
    arguments = json.loads(call["function"]["arguments"])
    if call["function"]["name"] == "process_exec":
        # The original adapter injected path/content as undeclared properties.
        # With complete schema constraints those are absent; the actual shell
        # argv still supplies the same values to ahrb-fixture. Read the argv,
        # never trust the separate AHRB_FIXTURE_HEX routing comment as execution.
        argv = shlex.split(arguments.get("command", ""), comments=True)
        for field in ("path", "content"):
            flag = f"--{field}"
            if flag in argv and argv.index(flag) + 1 < len(argv):
                arguments[field] = argv[argv.index(flag) + 1]
    return arguments


def correlated_effect_evidence(report: dict, bodies: list[dict], results: dict) -> dict:
    """Supplement, never rewrite, the old adapter's missing normalized args.

    Join native call IDs from captured provider history to the first model
    result and AHRB's out-of-process filesystem receipt. This is separate
    evidence; it cannot change the official completion or wasted-call score.
    """
    calls = {}
    for body in bodies:
        for message in body["messages"]:
            for call in message.get("tool_calls", []):
                calls.setdefault(call["id"], call)
    evidence = report["economy_summary"].get("effects_verified", {})
    if not evidence.get("expected"):
        return {"available": False}
    expected = evidence["expected"][0]
    observed = evidence["observed"][0]
    edit = captured_arguments(calls[expected["edit_call_id"]])
    read = captured_arguments(calls[expected["read_back_call_id"]])
    result = semantic_output(results[expected["read_back_call_id"]]["content"])
    checks = {
        "output_absent_before": observed["before_content_sha256"] is None,
        "external_output_sha256_matches": observed["after_content_sha256"] == expected["content_sha256"],
        "captured_edit_path_matches": edit.get("path") == expected["path"],
        "captured_edit_content_sha256_matches": hashlib.sha256(edit.get("content", "").encode()).hexdigest() == expected["content_sha256"],
        "captured_read_path_matches": read.get("path") == expected["path"],
        "correlated_model_readback_sha256_matches": hashlib.sha256(result.encode()).hexdigest() == expected["content_sha256"],
        "workspace_receipt_changed": evidence["workspace_receipt_before_sha256"] != evidence["workspace_receipt_after_sha256"],
    }
    return {"available": True, "checks": checks, "all_verified": all(checks.values()),
            "label": "independent captured-call/model-readback/filesystem join; does not replace AHRB completion or waste metrics"}


def measure(report_path: Path, bench_root: Path) -> dict:
    report_bytes = report_path.read_bytes()
    report = json.loads(report_bytes)
    official = report["economy_summary"]
    tokenizer = ReferenceTokenizer(bench_root / "assets/ahrb_o200k_base_style_v1.tiktoken")
    if official["reference_tokenizer"]["vocabulary_sha256"] != tokenizer.sha256:
        raise ValueError("AHRB report and reference vocabulary differ")
    records = sorted((record for record in report["model_requests"] if record["role"] == "primary"),
                     key=lambda record: (record["received_ns"], record["semantic_ordinal"], record["attempt"]))
    if len(records) < 5:
        raise ValueError("at least five primary physical requests required")
    if {record["request"]["dialect"] for record in records} != {"openai-chat-completions"}:
        raise ValueError("this split measurement supports the captured chat-completions fixture")
    bodies = [record["request"]["canonical"] for record in records]
    system = common_prefix([instructions(body) for body in bodies])
    tools = common_prefix([body.get("tools", []) for body in bodies])
    stable = {"messages": system, "tools": tools}
    curve = [tokenizer.count(canonical(body)) for body in bodies]
    fixed_tokens = tokenizer.count(canonical(stable))
    if curve != official["context_token_curve"]:
        raise ValueError(f"independent tokenizer disagrees with AHRB curve: {curve}")
    if fixed_tokens != official["per_turn_fixed_overhead_tokens"]:
        raise ValueError(f"independent exact common-block prefix disagrees with AHRB: {fixed_tokens}")
    slopes = [(b - a) / (j - i) for i, a in enumerate(curve)
              for j, b in enumerate(curve) if j > i]
    if statistics.median(slopes) != official["context_token_curve_slope"]:
        raise ValueError("independent Theil-Sen slope disagrees with AHRB")
    unique_results = {}
    for body in bodies:
        for message in body["messages"]:
            if message.get("role") == "tool":
                unique_results.setdefault(message["tool_call_id"], message)
    envelope_rows = []
    for call_id, message in unique_results.items():
        content = message["content"]
        if not isinstance(content, str):
            raise ValueError(f"non-text tool result: {call_id}")
        output = semantic_output(content)
        content_bytes = len(content.encode())
        output_bytes = len(str(output).encode())
        envelope_rows.append({"call_id": call_id, "content_bytes": content_bytes,
                              "output_bytes": output_bytes,
                              "envelope_overhead_bytes": content_bytes - output_bytes,
                              "wire_message_bytes": len(canonical(message))})
    content_sum = sum(row["content_bytes"] for row in envelope_rows)
    overhead_sum = sum(row["envelope_overhead_bytes"] for row in envelope_rows)
    prompt = system[0]["content"] if system else ""
    boundary = prompt.find("\n\nTool manual")
    policy = prompt if boundary == -1 else prompt[:boundary]
    manual = "" if boundary == -1 else prompt[boundary:]
    system_bytes = canonical({"messages": system})
    tool_bytes = canonical({"tools": tools})
    return {
        "schema": "haider.economydiet.measure.v1",
        "report_sha256": hashlib.sha256(report_bytes).hexdigest(),
        "task": official["task"], "reference_tokenizer": official["reference_tokenizer"],
        "tokenizer_crosscheck": "all primary request counts and exact-prefix total match AHRB",
        "python_unicode_version": unicodedata.unidata_version,
        "model_turns": len(records), "policy_bytes": len(policy.encode()),
        "manual_bytes": len(manual.encode()), "system_content_bytes": len(prompt.encode()),
        "system_envelope_bytes": len(system_bytes), "system_side_tokens": tokenizer.count(system_bytes),
        "tool_schema_array_bytes": len(canonical(tools)),
        "tool_envelope_bytes": len(tool_bytes), "tool_side_tokens": tokenizer.count(tool_bytes),
        "combined_stable_prefix_bytes": len(canonical(stable)),
        "per_turn_fixed_overhead_tokens": fixed_tokens,
        "independent_side_framing_note": "system/tools side counts each include their own JSON object framing; combined frames once",
        "stable_system_sha256": hashlib.sha256(canonical(system)).hexdigest(),
        "stable_tools_sha256": hashlib.sha256(canonical(tools)).hexdigest(),
        "stable_combined_sha256": hashlib.sha256(canonical(stable)).hexdigest(),
        "system_identical_every_request": all(instructions(body) == system for body in bodies),
        "tools_identical_every_request": all(body.get("tools", []) == tools for body in bodies),
        "default_tool_names": [tool.get("function", tool)["name"] for tool in tools],
        "context_token_curve": curve, "context_token_curve_slope": statistics.median(slopes),
        "wasted_tool_call_count": official["wasted_tool_call_count"],
        "wasted_tool_call_label": official["wasted_tool_call_count_label"],
        "completion": official["completion"],
        "effects_verified": official.get("effects_verified", {}).get("all_verified"),
        "independent_effect_evidence": correlated_effect_evidence(report, bodies, unique_results),
        "unique_tool_results": len(envelope_rows), "tool_result_content_bytes_total": content_sum,
        "tool_result_content_bytes_per_result": content_sum / len(envelope_rows),
        "tool_result_envelope_overhead_bytes_total": overhead_sum,
        "tool_result_envelope_overhead_bytes_per_result": overhead_sum / len(envelope_rows),
        "tool_result_envelopes": envelope_rows,
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--report", required=True, type=Path)
    parser.add_argument("--bench-root", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    result = measure(args.report, args.bench_root)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    print(json.dumps({key: result[key] for key in (
        "model_turns", "per_turn_fixed_overhead_tokens", "system_side_tokens", "tool_side_tokens",
        "tool_result_content_bytes_per_result", "tool_result_envelope_overhead_bytes_per_result",
        "context_token_curve_slope", "wasted_tool_call_count", "completion", "effects_verified")}, sort_keys=True))


if __name__ == "__main__":
    main()
