#!/usr/bin/env python3
"""Discover and sweep every durable turn boundary with real daemon SIGKILL."""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import signal
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
from typing import Any, Mapping, Sequence

from turnperf_support import (
    BOUNDARY_FILE_ENV,
    BOUNDARY_TARGET_ENV,
    FakeProvider,
    ProofError,
    ThrowawayProfile,
    assert_provider_ledger,
    parse_json_lines,
    parse_single_json,
    run_arguments,
    sha256_file,
    tool_effect_count,
    validate_jsonl,
    wait_session_idle,
    wait_session_settled,
)


DISCOVERY_TIMEOUT = 82.0
BOUNDARY_ARM_TIMEOUT = 20.0
RECOVERY_TIMEOUT = 40.0


class LiveClient:
    def __init__(self, profile: ThrowawayProfile, shape: str):
        self.stdout: list[str] = []
        self.stderr: list[str] = []
        self.process = subprocess.Popen(
            (profile.haider, *run_arguments(shape)),
            cwd=profile.workspace,
            env=profile.env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            errors="replace",
            bufsize=1,
            start_new_session=os.name == "posix",
        )
        self.readers = [
            threading.Thread(
                target=self._read, args=(self.process.stdout, self.stdout), daemon=True
            ),
            threading.Thread(
                target=self._read, args=(self.process.stderr, self.stderr), daemon=True
            ),
        ]
        for reader in self.readers:
            reader.start()

    @staticmethod
    def _read(stream: Any, destination: list[str]) -> None:
        if stream is None:
            return
        for line in iter(stream.readline, ""):
            destination.append(line)

    def stdout_snapshot(self) -> str:
        return "".join(list(self.stdout))

    def finish(self, timeout: float) -> tuple[int, str, str]:
        try:
            self.process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            if os.name == "posix":
                os.killpg(self.process.pid, signal.SIGKILL)
            else:
                self.process.kill()
            self.process.wait(timeout=5)
            raise ProofError("attached client did not terminate after daemon restart")
        for reader in self.readers:
            reader.join(timeout=2)
        return self.process.returncode, "".join(self.stdout), "".join(self.stderr)


def _wait_pid_gone(pid: int, timeout: float = 5) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return True
        except PermissionError:
            return False
        time.sleep(0.01)
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return True
    return False


def _boundary_rows(path: Path, *, tolerate_partial_tail: bool = False) -> list[tuple[int, int]]:
    try:
        lines = path.read_text(encoding="ascii").splitlines()
    except OSError:
        return []
    rows: list[tuple[int, int]] = []
    for index, line in enumerate(lines):
        fields = line.split("\t")
        if len(fields) != 2:
            if tolerate_partial_tail and index + 1 == len(lines):
                break
            raise ProofError(f"malformed boundary ledger row {line!r}")
        try:
            rows.append((int(fields[0]), int(fields[1])))
        except ValueError:
            if tolerate_partial_tail and index + 1 == len(lines):
                break
            raise ProofError(f"malformed boundary ledger row {line!r}") from None
    return rows


def _wait_journal_boundary(path: Path, ordinal: int, timeout: float) -> tuple[int, int]:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        for row in _boundary_rows(path, tolerate_partial_tail=True):
            if row[0] == ordinal:
                return row
        time.sleep(0.005)
    raise ProofError(f"journal boundary ordinal={ordinal} was not discovered live")


def _events(profile: ThrowawayProfile, run_id: str) -> list[dict[str, Any]]:
    result = profile.command(["events", "--no-spawn"], timeout=10)
    if result.returncode != 0:
        raise ProofError(f"events snapshot failed exit={result.returncode}")
    return [
        event
        for event in parse_json_lines(result.stdout, "events snapshot")
        if event.get("run_id") == run_id
    ]


def _replay(profile: ThrowawayProfile, run_id: str) -> dict[str, Any]:
    result = profile.command(
        ["run", "--replay", run_id, "--output", "json", "--timeout", "10s"],
        timeout=15,
    )
    if result.returncode != 0:
        raise ProofError(f"durable replay failed exit={result.returncode}")
    document = parse_single_json(result.stdout, "durable replay")
    if document.get("schema") != "haider.run.replay.v1":
        raise ProofError(f"durable replay schema invalid={document.get('schema')!r}")
    if document.get("provider_requests") != 0:
        raise ProofError(
            f"replay issued provider requests actual={document.get('provider_requests')!r}"
        )
    integrity = document.get("integrity")
    if not isinstance(integrity, dict) or not all(
        integrity.get(field) is True
        for field in (
            "sequences_strictly_increasing",
            "run_id_stable",
            "exactly_one_typed_terminal",
            "terminal_seq_matches_status",
        )
    ):
        raise ProofError(f"replay integrity flags are not all true: {integrity!r}")
    return document


def _typed_terminals(events: Sequence[Mapping[str, Any]]) -> list[Mapping[str, Any]]:
    return [
        event
        for event in events
        if isinstance(event.get("payload"), Mapping)
        and event["payload"].get("terminal_kind") is not None
    ]


def _validate_recovered_jsonl(stdout: str, shape: str) -> dict[str, Any]:
    """Validate a reattached stream without inventing an ordering guarantee.

    Reconnect can race the terminal run envelope with a later session-Idle
    envelope, so arrival order need not be sequence order. Durability is
    checked below against the ordered store projection; this check proves the
    attached client saw one non-duplicated complete session stream and exactly
    one typed terminal for one run.
    """

    documents = parse_json_lines(stdout, f"{shape} recovered JSONL")
    if not documents or documents[0].get("event") != "accepted":
        raise ProofError(f"{shape} recovered JSONL first record is not accepted")
    accepted = documents[0]
    events = documents[1:]
    if not events:
        raise ProofError(f"{shape} recovered JSONL has no envelopes")
    sequences = [event.get("seq") for event in events]
    if any(isinstance(value, bool) or not isinstance(value, int) for value in sequences):
        invalid = [
            event
            for event in events
            if isinstance(event.get("seq"), bool)
            or not isinstance(event.get("seq"), int)
        ]
        raise ProofError(
            f"{shape} recovered JSONL has a non-integer sequence records={invalid!r}"
        )
    if len(set(sequences)) != len(sequences):
        raise ProofError(f"{shape} recovered JSONL has duplicate sequences={sequences!r}")
    first = accepted.get("head_seq")
    if not isinstance(first, int) or set(sequences) != set(range(first, max(sequences) + 1)):
        raise ProofError(
            f"{shape} recovered JSONL session projection is incomplete sequences={sequences!r}"
        )
    event_ids = [event.get("event_id") for event in events]
    if any(not isinstance(value, str) or not value for value in event_ids):
        raise ProofError(f"{shape} recovered JSONL has a missing event_id")
    if len(set(event_ids)) != len(event_ids):
        raise ProofError(f"{shape} recovered JSONL has duplicate event_id values")
    run_ids = {
        event.get("run_id")
        for event in events
        if isinstance(event.get("run_id"), str)
    }
    session_id = accepted.get("session_id")
    if len(run_ids) != 1 or not isinstance(session_id, str):
        raise ProofError(f"{shape} recovered run/session identity is not singular")
    run_id = next(iter(run_ids))
    run_events = [event for event in events if event.get("run_id") == run_id]
    terminals = _typed_terminals(run_events)
    if len(terminals) != 1:
        raise ProofError(
            f"{shape} recovered typed terminal count expected=1 actual={len(terminals)}"
        )
    return {
        "accepted": accepted,
        "events": events,
        "run_events": run_events,
        "session_id": session_id,
        "run_id": run_id,
        "terminal": terminals[0],
    }


def _assert_store_integrity(profile: ThrowawayProfile) -> None:
    database = profile.profile / "store.sqlite"
    connection = sqlite3.connect(f"file:{database}?mode=ro", uri=True)
    try:
        row = connection.execute("PRAGMA integrity_check").fetchone()
    finally:
        connection.close()
    if row != ("ok",):
        raise ProofError(f"SQLite integrity_check failed actual={row!r}")


def _assert_no_duplicate_provider_request(
    ledger: Sequence[Mapping[str, Any]], shape: str
) -> None:
    allowed = {1} if shape == "single" else {1, 2}
    counts: dict[int, int] = {}
    for entry in ledger:
        ordinal = entry.get("logical_ordinal")
        if isinstance(ordinal, bool) or not isinstance(ordinal, int) or ordinal not in allowed:
            raise ProofError(f"invalid physical provider ordinal {ordinal!r}")
        counts[ordinal] = counts.get(ordinal, 0) + 1
    duplicates = {ordinal: count for ordinal, count in counts.items() if count > 1}
    if duplicates:
        raise ProofError(f"duplicate physical provider requests detected={duplicates}")


def _assert_event_identity(events: Sequence[Mapping[str, Any]]) -> None:
    event_ids = [event.get("event_id") for event in events]
    if any(not isinstance(value, str) or not value for value in event_ids):
        raise ProofError("replay contains a missing event_id")
    if len(set(event_ids)) != len(event_ids):
        raise ProofError("replay contains duplicate event_id values")
    sequences = [event.get("seq") for event in events]
    if any(
        isinstance(value, bool) or not isinstance(value, int) for value in sequences
    ) or any(after <= before for before, after in zip(sequences, sequences[1:])):
        raise ProofError(f"replay sequences are not strictly increasing: {sequences!r}")
    tool_results: dict[str, int] = {}
    for event in events:
        payload = event.get("payload")
        if isinstance(payload, Mapping) and payload.get("type") == "tool_result":
            call_id = payload.get("call_id")
            if isinstance(call_id, str):
                tool_results[call_id] = tool_results.get(call_id, 0) + 1
    duplicates = {call_id: count for call_id, count in tool_results.items() if count > 1}
    if duplicates:
        raise ProofError(f"duplicate durable tool results detected={duplicates}")


def _assert_tool_effect_result_bounds(effects: int, tool_results: int) -> None:
    """Prove at-most-once effect execution across an ambiguous crash.

    A crash after a durable successful Effect::Outcome but before ToolResult
    can legitimately retain one observable effect behind the typed recovery
    door. The matrix probes that door and abandons only to seal the run. A
    durable tool result, when present, must still correspond to exactly one
    observable effect. Neither coordinate may duplicate.
    """

    if effects > 1 or tool_results > 1 or tool_results > effects:
        raise ProofError(
            "tool effect/result at-most-once violated "
            f"actual_effects={effects} actual_results={tool_results}"
        )


def _validate_probe_receipt(
    document: Mapping[str, Any], session_id: str
) -> tuple[str, str]:
    menu_id = document.get("menu_id")
    resolution_seq = document.get("resolution_seq")
    replacement_menu_id = document.get("replacement_menu_id")
    expected_replacement = (
        f"{menu_id}-probe-{resolution_seq}"
        if isinstance(menu_id, str)
        and menu_id
        and isinstance(resolution_seq, int)
        and not isinstance(resolution_seq, bool)
        else None
    )
    if (
        document.get("schema") != "haider.session_recovery.v1"
        or document.get("session_id") != session_id
        or document.get("chosen_option") != "probe"
        or document.get("completed") is not True
        or document.get("resulting_run_state") != "effect_unknown"
        or expected_replacement is None
        or replacement_menu_id != expected_replacement
    ):
        raise ProofError(
            "typed recovery probe did not preserve the parked retry-pending state "
            f"document={document!r}"
        )
    return menu_id, replacement_menu_id


def _reconcile_open_recovery(
    profile: ThrowawayProfile,
    proxy: FakeProvider,
    session_id: str,
) -> dict[str, Any]:
    card_deadline = time.monotonic() + 5
    while True:
        card_result = profile.command(
            ["session", session_id, "recover", "--json"], timeout=10
        )
        card = parse_single_json(card_result.stdout, "effect recovery card")
        if card_result.returncode == 0:
            break
        error = card.get("error")
        code = error.get("code") if isinstance(error, Mapping) else None
        message = error.get("message") if isinstance(error, Mapping) else None
        if code == "no_recovery":
            return {"outcome": "not_needed", "document": card}
        if code == "recovery_incomplete" and isinstance(message, str) and any(
            f"state={state}" in message for state in ("errored", "cancelled")
        ):
            return {"outcome": "terminal_without_card", "document": card}
        if code == "recovery_incomplete" and time.monotonic() < card_deadline:
            time.sleep(0.02)
            continue
        raise ProofError(
            f"effect recovery card failed exit={card_result.returncode} document={card!r}"
        )
    options = card.get("options")
    option_values = options if isinstance(options, list) else []
    option_keys = {
        option.get("key")
        for option in option_values
        if isinstance(option, Mapping)
    }
    if (
        card.get("schema") != "haider.session_recovery.v1"
        or card.get("session_id") != session_id
        or card.get("run_state") not in {"running", "effect_unknown"}
        or not isinstance(card.get("menu_id"), str)
        or not {"probe", "abandon"}.issubset(option_keys)
    ):
        raise ProofError(f"invalid typed recovery card document={card!r}")

    ledger_before_probe = proxy.state.snapshot_case()
    probe_result = profile.command(
        ["session", session_id, "recover", "--probe", "--json"], timeout=10
    )
    probe = parse_single_json(probe_result.stdout, "effect recovery probe")
    if probe_result.returncode != 0:
        raise ProofError(
            f"effect recovery probe failed exit={probe_result.returncode} document={probe!r}"
        )
    menu_id, replacement_menu_id = _validate_probe_receipt(probe, session_id)
    if menu_id != card.get("menu_id"):
        raise ProofError(
            "effect recovery probe answered a different card "
            f"card={card.get('menu_id')!r} probe={menu_id!r}"
        )
    ledger_after_probe = proxy.state.snapshot_case()
    if ledger_after_probe != ledger_before_probe:
        raise ProofError(
            "parked-admission probe issued a duplicate physical provider request "
            f"before={ledger_before_probe!r} after={ledger_after_probe!r}"
        )

    abandon_result = profile.command(
        ["session", session_id, "recover", "--abandon", "--json"], timeout=10
    )
    abandon = parse_single_json(abandon_result.stdout, "effect recovery abandon")
    if (
        abandon_result.returncode != 0
        or abandon.get("schema") != "haider.session_recovery.v1"
        or abandon.get("session_id") != session_id
        or abandon.get("menu_id") != replacement_menu_id
        or abandon.get("chosen_option") != "abandon"
        or abandon.get("completed") is not True
        or abandon.get("resulting_run_state") != "errored"
    ):
        raise ProofError(
            f"effect recovery abandon failed exit={abandon_result.returncode} "
            f"document={abandon!r}"
        )
    ledger_after_abandon = proxy.state.snapshot_case()
    if ledger_after_abandon != ledger_before_probe:
        raise ProofError(
            "typed recovery resolution mutated the physical provider ledger "
            f"before={ledger_before_probe!r} after={ledger_after_abandon!r}"
        )
    return {
        "outcome": "probed_then_abandoned",
        "card": card,
        "probe": probe,
        "abandon": abandon,
        "provider_ledger_unchanged": True,
    }


def _case_root(label: str) -> Path:
    safe = "".join(character if character.isalnum() else "-" for character in label)
    return Path(tempfile.mkdtemp(prefix=f"htp-kill-{safe[:18]}-", dir="/tmp"))


def discover(bin_dir: Path, proxy: FakeProvider, shape: str) -> dict[str, Any]:
    root = _case_root(f"discover-{shape}")
    profile = ThrowawayProfile(bin_dir, proxy.base_url, root=root / "profile")
    boundary_file = profile.profile / "journal-boundaries.tsv"
    try:
        profile.ready({BOUNDARY_FILE_ENV: str(boundary_file), BOUNDARY_TARGET_ENV: None})
        identity = profile.status()[:2]
        proxy.state.begin_case(shape)
        result = profile.command(run_arguments(shape), timeout=DISCOVERY_TIMEOUT)
        if result.returncode != 0 or result.timed_out:
            raise ProofError(
                f"{shape} discovery failed exit={result.returncode} timed_out={result.timed_out}"
            )
        parsed = validate_jsonl(result.stdout, shape)
        wait_session_idle(profile, parsed["session_id"])
        assert_provider_ledger(proxy.state.snapshot_case(), shape)
        if profile.status()[:2] != identity:
            raise ProofError(f"{shape} discovery daemon identity changed")
        rows = _boundary_rows(boundary_file)
        if not rows or [row[0] for row in rows] != list(range(1, len(rows) + 1)):
            raise ProofError(f"{shape} discovery ordinals are not contiguous: {rows!r}")
        events_result = profile.command(["events", "--no-spawn"], timeout=10)
        if events_result.returncode != 0 or events_result.timed_out:
            raise ProofError(
                f"{shape} discovery events failed exit={events_result.returncode} "
                f"timed_out={events_result.timed_out}"
            )
        all_events = parse_json_lines(events_result.stdout, "discovery events")
        previous = 0
        boundaries = []
        for ordinal, through_seq in rows:
            suffix = [
                event
                for event in all_events
                if isinstance(event.get("seq"), int)
                and previous < event["seq"] <= through_seq
            ]
            boundaries.append(
                {
                    "ordinal": ordinal,
                    "from_seq": previous + 1,
                    "through_seq": through_seq,
                    "event_types": [
                        event.get("payload", {}).get("type")
                        if isinstance(event.get("payload"), dict)
                        else None
                        for event in suffix
                    ],
                }
            )
            previous = through_seq
        return {
            "shape": shape,
            "boundaries": boundaries,
            "provider_requests": proxy.state.snapshot_case(),
            "terminal_seq": parsed["terminal_seq"],
        }
    finally:
        stop = profile.stop()
        if stop.returncode != 0:
            raise ProofError(f"{shape} discovery exact cleanup failed exit={stop.returncode}")
        profile.dispose()


def _run_kill_case(
    bin_dir: Path,
    proxy: FakeProvider,
    *,
    shape: str,
    label: str,
    journal_ordinal: int | None = None,
    expected_through_seq: int | None = None,
    provider_gate: tuple[int, str] | None = None,
) -> dict[str, Any]:
    root = _case_root(label)
    profile = ThrowawayProfile(bin_dir, proxy.base_url, root=root / "profile")
    boundary_file = profile.profile / "journal-boundaries.tsv"
    old_pid: int | None = None
    restarted_pid: int | None = None
    client: LiveClient | None = None
    stopped = False
    try:
        observer = {
            BOUNDARY_FILE_ENV: str(boundary_file) if journal_ordinal is not None else None,
            BOUNDARY_TARGET_ENV: str(journal_ordinal) if journal_ordinal is not None else None,
        }
        profile.ready(observer)
        old_pid, old_generation, _ = profile.status()
        case_id = proxy.state.begin_case(shape, provider_gate)
        client = LiveClient(profile, shape)
        if journal_ordinal is not None:
            reached = _wait_journal_boundary(
                boundary_file, journal_ordinal, BOUNDARY_ARM_TIMEOUT
            )
            if expected_through_seq is None or reached[1] != expected_through_seq:
                raise ProofError(
                    f"journal boundary drift ordinal={journal_ordinal} "
                    f"expected_through_seq={expected_through_seq} actual={reached[1]}"
                )
        elif provider_gate is not None:
            if not proxy.state.wait_gate(BOUNDARY_ARM_TIMEOUT):
                raise ProofError(f"provider gate was not reached: {provider_gate!r}")
            reached = provider_gate
        else:
            raise ProofError("kill case has no target")
        pre_kill = parse_json_lines(client.stdout_snapshot(), f"{label} pre-kill JSONL")
        accepted_records = [value for value in pre_kill if value.get("event") == "accepted"]
        accepted_session_id = (
            accepted_records[0].get("session_id") if len(accepted_records) == 1 else None
        )
        if accepted_session_id is not None and not isinstance(accepted_session_id, str):
            raise ProofError(f"{label} pre-kill accepted session coordinate is invalid")
        os.kill(old_pid, signal.SIGKILL)
        if not _wait_pid_gone(old_pid):
            raise ProofError(f"status-owned daemon PID survived SIGKILL pid={old_pid}")
        proxy.state.release_gate()
        profile.ready({BOUNDARY_FILE_ENV: None, BOUNDARY_TARGET_ENV: None})
        restarted_pid, restarted_generation, _ = profile.status()
        if restarted_pid == old_pid or restarted_generation <= old_generation:
            raise ProofError(
                "daemon restart identity did not advance "
                f"old={(old_pid, old_generation)} new={(restarted_pid, restarted_generation)}"
            )
        if not proxy.state.wait_idle(5):
            raise ProofError("provider handler did not settle before recovery probe")
        recovery_action = (
            _reconcile_open_recovery(profile, proxy, accepted_session_id)
            if accepted_session_id is not None
            else {"outcome": "pre_accept_boundary"}
        )
        returncode, stdout, stderr = client.finish(RECOVERY_TIMEOUT)
        parsed = _validate_recovered_jsonl(stdout, shape)
        settled_state = wait_session_settled(profile, parsed["session_id"])
        if not proxy.state.wait_idle(2):
            raise ProofError("provider handler did not settle after daemon kill")
        ledger_before_replay = proxy.state.snapshot_case()
        disk_ledger = proxy.state.read_disk_ledger()
        if disk_ledger != proxy.state.snapshot_all():
            raise ProofError("on-disk provider ledger diverged from proxy memory")
        _assert_no_duplicate_provider_request(ledger_before_replay, shape)
        source_events = _events(profile, parsed["run_id"])
        replay = _replay(profile, parsed["run_id"])
        replay_events = replay.get("events")
        if not isinstance(replay_events, list) or replay_events != source_events:
            raise ProofError(
                f"replay parity failed source={len(source_events)} replay="
                f"{len(replay_events) if isinstance(replay_events, list) else replay_events!r}"
            )
        ledger_after_replay = proxy.state.snapshot_case()
        if ledger_after_replay != ledger_before_replay:
            raise ProofError("durable replay mutated the external provider ledger")
        live_events = sorted(parsed["run_events"], key=lambda event: event["seq"])
        if live_events != source_events:
            raise ProofError(
                f"recovered live prefix+suffix parity failed live={len(live_events)} "
                f"source={len(source_events)}"
            )
        pre_kill_events = [
            value
            for value in pre_kill
            if value.get("event") != "accepted" and value.get("run_id") == parsed["run_id"]
        ]
        if pre_kill_events != source_events[: len(pre_kill_events)]:
            raise ProofError("committed pre-kill live prefix differs from replay prefix")
        terminals = _typed_terminals(live_events)
        if len(terminals) != 1:
            raise ProofError(f"typed terminal count expected=1 actual={len(terminals)}")
        terminal_error_code = terminals[0]["payload"].get("error_code")
        if recovery_action.get("outcome") == "probed_then_abandoned" and (
            terminals[0]["payload"].get("terminal_kind") != "failure"
            or terminal_error_code != "input_required"
        ):
            raise ProofError(
                "effect recovery changed the established blocking terminal "
                f"payload={terminals[0]['payload']!r}"
            )
        _assert_event_identity(source_events)
        tool_results = sum(
            isinstance(event.get("payload"), Mapping)
            and event["payload"].get("type") == "tool_result"
            for event in source_events
        )
        effects = tool_effect_count(profile.root, case_id)
        if shape == "tool":
            _assert_tool_effect_result_bounds(effects, tool_results)
        terminal_kind = terminals[0]["payload"].get("terminal_kind")
        expected_exit_zero = terminal_kind == "success"
        if (returncode == 0) != expected_exit_zero:
            raise ProofError(
                f"client exit/terminal mismatch exit={returncode} terminal_kind={terminal_kind!r}"
            )
        _assert_store_integrity(profile)
        stop = profile.stop()
        stopped = True
        if stop.returncode != 0:
            raise ProofError(f"exact daemon cleanup failed exit={stop.returncode}")
        return {
            "label": label,
            "shape": shape,
            "target": reached,
            "old_pid": old_pid,
            "new_pid": restarted_pid,
            "client_exit": returncode,
            "client_stderr_tail": stderr[-200:],
            "pre_kill_events": len(pre_kill_events),
            "replay_events": len(source_events),
            "terminal_kind": terminal_kind,
            "terminal_error_code": terminal_error_code,
            "settled_state": settled_state,
            "effect_recovery": recovery_action,
            "tool_effects": effects,
            "tool_results": tool_results,
            "provider_requests": ledger_before_replay,
            "store_integrity": "ok",
            "passed": True,
        }
    finally:
        proxy.state.release_gate()
        if client is not None and client.process.poll() is None:
            if os.name == "posix":
                os.killpg(client.process.pid, signal.SIGKILL)
            else:
                client.process.kill()
        if not stopped:
            stop = profile.stop()
            if stop.returncode != 0:
                error = ProofError(f"exact daemon cleanup failed exit={stop.returncode}")
                active = sys.exc_info()[1]
                if active is None:
                    raise error
                active.add_note(str(error))
        profile.dispose()


def run_matrix(bin_dir: Path) -> dict[str, Any]:
    if os.name != "posix" or not hasattr(signal, "SIGKILL"):
        raise ProofError("SIGKILL boundary matrix requires POSIX")
    root = Path(tempfile.mkdtemp(prefix="htp-matrix-", dir="/tmp"))
    cases: list[dict[str, Any]] = []
    failures: list[str] = []
    with FakeProvider(root / "provider-ledger.jsonl") as proxy:
        discovery = [discover(bin_dir, proxy, shape) for shape in ("single", "tool")]
        specifications: list[dict[str, Any]] = []
        for item in discovery:
            shape = item["shape"]
            for boundary in item["boundaries"]:
                specifications.append(
                    {
                        "shape": shape,
                        "label": f"{shape}-journal-{boundary['ordinal']}",
                        "journal_ordinal": boundary["ordinal"],
                        "expected_through_seq": boundary["through_seq"],
                    }
                )
            requests = 1 if shape == "single" else 2
            for request in range(1, requests + 1):
                for phase in ("after_post", "before_headers", "between_chunks"):
                    specifications.append(
                        {
                            "shape": shape,
                            "label": f"{shape}-provider-{request}-{phase}",
                            "provider_gate": (request, phase),
                        }
                    )
        for specification in specifications:
            try:
                cases.append(_run_kill_case(bin_dir, proxy, **specification))
            except Exception as error:
                label = specification["label"]
                failures.append(f"{label}: {type(error).__name__}: {error}")
                cases.append(
                    {
                        "label": label,
                        "shape": specification["shape"],
                        "passed": False,
                        "error": f"{type(error).__name__}: {error}",
                    }
                )
        disk_ledger = proxy.state.read_disk_ledger()
        ledger_sha256 = sha256_file(root / "provider-ledger.jsonl")
    report = {
        "schema": "haider.turn-sigkill-matrix.v1",
        "created_at_utc": datetime.now(timezone.utc).isoformat(),
        "binaries": {
            "haider": {
                "path": str((bin_dir / "haider").resolve()),
                "sha256": sha256_file(bin_dir / "haider"),
            },
            "haiderd": {
                "path": str((bin_dir / "haiderd").resolve()),
                "sha256": sha256_file(bin_dir / "haiderd"),
            },
            "proxy_source_sha256": sha256_file(
                Path(__file__).with_name("turnperf_support.py")
            ),
            "matrix_source_sha256": sha256_file(Path(__file__)),
        },
        "discovery": discovery,
        "cases": cases,
        "summary": {
            "total": len(cases),
            "passed": sum(case["passed"] for case in cases),
            "failed": sum(not case["passed"] for case in cases),
        },
        "failures": failures,
        "provider_ledger": disk_ledger,
        "provider_ledger_sha256": ledger_sha256,
        "passed": not failures,
    }
    shutil.rmtree(root, ignore_errors=True)
    return report


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args(sys.argv[1:] if argv is None else argv)
    try:
        report = run_matrix(args.bin_dir)
    except Exception as error:
        print(f"SIGKILL matrix failed: {type(error).__name__}: {error}", file=sys.stderr)
        return 1
    rendered = json.dumps(report, indent=2, sort_keys=True) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(rendered, encoding="utf-8")
    else:
        print(rendered, end="")
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
