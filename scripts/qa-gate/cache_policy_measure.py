#!/usr/bin/env python3
"""Measure prompt-cache key economics through the real CLI/daemon adapter.

The loopback grants a cache read only when both ``prompt_cache_key`` and the
exact leading provider-visible components match a prior request. It retains a
sanitized per-request ledger for independent arithmetic while leaving the
canonical deep fixture's response and journal sequence unchanged.
"""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import shutil
import sys
import tempfile
from typing import Any, Mapping, Sequence

import deep_turn_harness as deep
from turnperf_support import (
    ProofError,
    ThrowawayProfile,
    parse_json_lines,
    parse_single_json,
    profile_daemon_pid,
    sha256_file,
    wait_pid_gone,
    wait_session_idle,
)


FRESH_SESSIONS = 12
PRICE_SOURCE = "https://developers.openai.com/api/docs/models/gpt-5.6-sol"
INPUT_USD_PER_MTOK = 4.0
CACHE_READ_USD_PER_MTOK = 0.4
CACHE_WRITE_USD_PER_MTOK = 5.0


def _totals(records: Sequence[Mapping[str, Any]], turns: int) -> dict[str, Any]:
    if turns <= 0 or not records:
        raise ProofError("cache summary requires positive turns and request records")
    usage = [record.get("cache_usage") for record in records]
    if not all(isinstance(value, Mapping) for value in usage):
        raise ProofError("recording loopback omitted cache usage")
    counters = {
        name: sum(int(value[name]) for value in usage)
        for name in ("logical", "cache_read", "cache_write", "fresh")
    }
    if counters["logical"] != (
        counters["cache_read"] + counters["cache_write"] + counters["fresh"]
    ):
        raise ProofError(f"cache counters do not conserve logical input: {counters}")
    request_hits = sum(int(value["cache_read"]) > 0 for value in usage)
    input_cost = (
        counters["fresh"] * INPUT_USD_PER_MTOK
        + counters["cache_read"] * CACHE_READ_USD_PER_MTOK
        + counters["cache_write"] * CACHE_WRITE_USD_PER_MTOK
    ) / 1_000_000
    scale = 1_000_000 / turns
    return {
        **counters,
        "turns": turns,
        "requests": len(records),
        "request_hit_rate": request_hits / len(records),
        "token_hit_rate": counters["cache_read"] / counters["logical"],
        "observed_input_cost_usd": input_cost,
        "tokens_per_turn": {
            name: counters[name] / turns
            for name in ("logical", "cache_read", "cache_write", "fresh")
        },
        "projected_per_million_turns": {
            name: counters[name] * scale
            for name in ("logical", "cache_read", "cache_write", "fresh")
        }
        | {"input_cost_usd": input_cost * scale},
    }


def _fresh_arguments(index: int) -> list[str]:
    unique_tail = " ".join(f"fresh-{index:02d}-{word:03d}" for word in range(48))
    return [
        "run",
        "-p",
        (
            "Fresh-session cache matrix. Preserve the shared system and tool prefix, "
            f"then answer this session-specific suffix: {unique_tail}"
        ),
        "--provider",
        deep.PROVIDER_ID,
        "--model",
        deep.MODEL_ID,
        "--output",
        "jsonl",
        "--timeout",
        "30s",
    ]


def _accepted_session(stdout: str, label: str) -> str:
    records = parse_json_lines(stdout, label)
    accepted = next((row for row in records if row.get("event") == "accepted"), None)
    if not isinstance(accepted, Mapping) or not isinstance(
        accepted.get("session_id"), str
    ):
        raise ProofError(f"{label} omitted its accepted session identity")
    terminals = [
        row
        for row in records[1:]
        if isinstance(row.get("payload"), Mapping)
        and row["payload"].get("terminal_kind") is not None
    ]
    if len(terminals) != 1 or terminals[0] is not records[-1]:
        raise ProofError(f"{label} omitted its one final typed terminal: {terminals!r}")
    return str(accepted["session_id"])


def _configure_profile(
    bin_dir: Path, provider: deep.DeepProvider, root: Path
) -> ThrowawayProfile:
    profile = ThrowawayProfile(bin_dir, provider.base_url, root=root)
    (profile.profile / "providers.json").write_text(
        json.dumps(deep.provider_catalog(provider.base_url), separators=(",", ":")) + "\n",
        encoding="utf-8",
    )
    os.chmod(profile.profile / "providers.json", 0o600)
    return profile


def run_deep_fixture(bin_dir: Path) -> dict[str, Any]:
    root = Path(tempfile.mkdtemp(prefix="hcpd-", dir="/tmp"))
    profile: ThrowawayProfile | None = None
    foreign_start = deep.foreign_haiderd_snapshot()
    try:
        with deep.DeepProvider(cache_accounting=True) as provider:
            profile = _configure_profile(bin_dir, provider, root / "x")
            pre_stop = deep._pre_stop(profile)
            profile.ready()
            pid, generation, _status = profile.status()
            session_id: str | None = None
            records: list[dict[str, Any]] = []
            for turn in range(1, deep.TURNS + 1):
                before = len(provider.state.snapshot())
                arguments = (
                    deep._initial_arguments()
                    if session_id is None
                    else deep._continuation_arguments(session_id, turn)
                )
                result = profile.command(arguments, timeout=90, observe_pid=pid)
                if result.timed_out or result.returncode != 0:
                    raise ProofError(
                        f"deep turn {turn} failed exit={result.returncode} "
                        f"timeout={result.timed_out}: {result.stderr[-500:]}"
                    )
                actual_session = _accepted_session(result.stdout, f"deep turn {turn}")
                if session_id is None:
                    session_id = actual_session
                elif actual_session != session_id:
                    raise ProofError(
                        f"deep session identity changed {session_id!r} -> {actual_session!r}"
                    )
                wait_session_idle(profile, session_id)
                if not provider.state.wait_idle(2):
                    raise ProofError(f"deep turn {turn} provider did not settle")
                added = provider.state.snapshot()[before:]
                expected = 2 if deep.turn_is_tool(turn) else 1
                if len(added) != expected:
                    raise ProofError(
                        f"deep turn {turn} expected {expected} provider requests, got {len(added)}"
                    )
                records.extend(added)
                if deep.turn_is_tool(turn):
                    effects = deep._effect_count(profile.root, deep.effect_token(turn))
                    if effects != 1:
                        raise ProofError(
                            f"deep turn {turn} tool effect expected once, got {effects}"
                        )
            sizes = [
                records[sum(2 if deep.turn_is_tool(turn) else 1 for turn in range(1, depth))][
                    "body_bytes"
                ]
                for depth in deep.CHECKPOINTS
            ]
            if any(after <= before for before, after in zip(sizes, sizes[1:])):
                raise ProofError(f"deep provider transcript did not grow: {sizes}")
            stop = profile.stop()
            document = parse_single_json(stop.stdout, "cache-policy deep stop")
            if stop.returncode != 0 or document.get("outcome") != "stopped_cleanly":
                raise ProofError(f"deep profile stop failed: {document}")
            if not wait_pid_gone(pid, 5):
                raise ProofError(f"deep profile daemon pid {pid} survived stop")
            return {
                "turns": deep.TURNS,
                "daemon_generation": generation,
                "pre_stop": pre_stop,
                "checkpoint_body_bytes": sizes,
                "records": records,
                "summary": _totals(records, deep.TURNS),
                "foreign_daemons": {
                    "start": foreign_start,
                    "end": deep.foreign_haiderd_snapshot(exclude_pids=(pid,)),
                },
            }
    finally:
        if profile is not None:
            profile.dispose(remove_root=True)
        elif root.exists():
            shutil.rmtree(root)


def run_fresh_matrix(bin_dir: Path, sessions: int) -> dict[str, Any]:
    root = Path(tempfile.mkdtemp(prefix="hcp-", dir="/tmp"))
    profile: ThrowawayProfile | None = None
    stopped = False
    try:
        with deep.DeepProvider(cache_accounting=True) as provider:
            profile = _configure_profile(bin_dir, provider, root / "x")
            pre_stop = deep._pre_stop(profile)
            profile.ready()
            pid, generation, _status = profile.status()
            session_ids: list[str] = []
            records: list[dict[str, Any]] = []
            for index in range(1, sessions + 1):
                before = len(provider.state.snapshot())
                result = profile.command(_fresh_arguments(index), timeout=90, observe_pid=pid)
                if result.timed_out or result.returncode != 0:
                    raise ProofError(
                        f"fresh session {index} failed exit={result.returncode} "
                        f"timeout={result.timed_out}: {result.stderr[-500:]}"
                    )
                session_id = _accepted_session(result.stdout, f"fresh session {index}")
                if session_id in session_ids:
                    raise ProofError(f"fresh session identity repeated: {session_id}")
                session_ids.append(session_id)
                wait_session_idle(profile, session_id)
                if not provider.state.wait_idle(2):
                    raise ProofError(f"fresh session {index} provider did not settle")
                added = provider.state.snapshot()[before:]
                if len(added) != 1:
                    raise ProofError(
                        f"fresh session {index} expected one provider request, got {len(added)}"
                    )
                records.extend(added)
            if len({record["session_id"] for record in records}) != sessions:
                raise ProofError("fresh-session ledger lost session isolation")
            stop = profile.stop()
            document = parse_single_json(stop.stdout, "cache-policy fresh stop")
            if stop.returncode != 0 or document.get("outcome") != "stopped_cleanly":
                raise ProofError(f"fresh profile stop failed: {document}")
            if not wait_pid_gone(pid, 5):
                raise ProofError(f"fresh profile daemon pid {pid} survived stop")
            stopped = True
            keys = {
                record["cache_usage"]["prompt_cache_key_sha256"] for record in records
            }
            return {
                "sessions": sessions,
                "daemon_generation": generation,
                "pre_stop": pre_stop,
                "distinct_prompt_cache_keys": len(keys),
                "records": records,
                "summary": _totals(records, sessions),
            }
    finally:
        if profile is not None:
            profile.dispose(remove_root=True)
        elif root.exists():
            shutil.rmtree(root)
        if profile is not None and not stopped and profile_daemon_pid(profile.profile) is not None:
            raise ProofError("fresh-matrix cleanup left its owned daemon")


def run_measurement(bin_dir: Path, fresh_sessions: int, candidate: str | None) -> dict[str, Any]:
    deep_run = run_deep_fixture(bin_dir.resolve())
    fresh = run_fresh_matrix(bin_dir.resolve(), fresh_sessions)
    return {
        "schema": "haider.prompt-cache-policy-measurement.v1",
        "created_at_utc": datetime.now(timezone.utc).isoformat(),
        "candidate": candidate or deep._git_commit(),
        "binary_dir": str(bin_dir.resolve()),
        "binaries": {
            name: sha256_file(bin_dir.resolve() / name) for name in ("haider", "haiderd")
        },
        "oracle": {
            "rule": "same prompt_cache_key plus exact leading provider-visible components",
            "minimum_cache_tokens": deep.PromptCacheOracle.MINIMUM_CACHE_TOKENS,
            "token_estimator": "ceil(canonical provider-visible UTF-8 bytes / 4)",
            "provider_tokenizer": False,
        },
        "pricing": {
            "source": PRICE_SOURCE,
            "model": "gpt-5.6-sol",
            "usd_per_million_tokens": {
                "ordinary_input": INPUT_USD_PER_MTOK,
                "cache_read": CACHE_READ_USD_PER_MTOK,
                "cache_write": CACHE_WRITE_USD_PER_MTOK,
            },
        },
        "deep_fixture": {
            **deep_run,
            "binary_provenance": {
                name: sha256_file(bin_dir.resolve() / name)
                for name in ("haider", "haiderd")
            },
        },
        "fresh_session_matrix": fresh,
    }


def _arguments(argv: Sequence[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", required=True, type=Path)
    parser.add_argument("--fresh-sessions", type=int, default=FRESH_SESSIONS)
    parser.add_argument("--candidate")
    parser.add_argument("--output", required=True, type=Path)
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = _arguments(sys.argv[1:] if argv is None else argv)
    try:
        if args.fresh_sessions < 3:
            raise ProofError("fresh-session matrix requires at least three sessions")
        report = run_measurement(args.bin_dir, args.fresh_sessions, args.candidate)
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
        print(json.dumps({
            "deep": report["deep_fixture"]["summary"],
            "fresh": report["fresh_session_matrix"]["summary"],
            "fresh_distinct_keys": report["fresh_session_matrix"]["distinct_prompt_cache_keys"],
        }, indent=2, sort_keys=True))
        return 0
    except (OSError, ProofError, ValueError) as error:
        print(f"cache-policy-measure: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
