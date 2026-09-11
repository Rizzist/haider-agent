#!/usr/bin/env python3
"""Real PTY regression for multiline tool-output secrets (throwaway profile only)."""

from __future__ import annotations

import argparse
import base64
import json
import os
from pathlib import Path
import shutil
import time
import traceback

from gate.context import CheckContext
from gate.contract import BudgetPart, STATUS_REQUEST
from gate.tui_probe import TuiProcess, start_daemon


CARRIERS = [
    "HEAD:752dfaa79475887978ffeb8eaa73134d7a933c7d",
    "urn:uuid:01a0e893-52bc-7def-89ab-0123456789cd",
    "thread-01a0e893-52bc-7def-89ab-0123456789cd",
    "01a0e893-52bc-7def-89ab-0123456789cd.result.md",
    "QmYwAPJzv5CZsnAzt8auVZRnGi2CQCqK4HCb2jdPrFAgDq",
    "run_id=aB3dE5fG7hI9jK1mN3pQ5rS7tU9vW1xY",
]
URLS = [
    ("https://owner:fixturepass@example.test/repo", "fixturepass"),
    ("postgres://owner:p%40ssw0rd@db.test/app", "p%40ssw0rd"),
]
MULTILINE = [
    'password="abc\nSYNTHETICTAIL987" after',
    "password='abc\nSYNTHETICTAIL987' after",
    'password="abc\\\nSYNTHETICTAIL987" after',
    'password="abc\r\nSYNTHETICTAIL987" after',
    "password='abc\\\r\nSYNTHETICTAIL987' after",
    'password="abc\n\\"SYNTHETICTAIL987" after',
    "password='abc\n\\'SYNTHETICTAIL987' after",
]


def command_chunks(value):
    if isinstance(value, dict):
        if isinstance(value.get("chunk_b64"), str):
            yield base64.b64decode(value["chunk_b64"])
        for child in value.values():
            yield from command_chunks(child)
    elif isinstance(value, list):
        for child in value:
            yield from command_chunks(child)


def run(bin_dir: Path, evidence: Path) -> int:
    evidence.mkdir(parents=True, exist_ok=True)
    # Short POSIX socket paths; CheckContext refuses ordinary user profiles.
    os.environ["TMPDIR"] = "/tmp"
    steps = [
        {"step": "emit_text", "text": "TUI_SESSION_READY"},
        {"step": "finish", "reason": "end_turn"},
    ]
    ctx = CheckContext(
        check_id="multiline-redaction-tui",
        bin_dir=bin_dir.resolve(),
        script=steps,
        report_artefact_root=evidence,
    )
    fixture = "\n".join([
        "BEGIN_MULTILINE_OUTPUT", *MULTILINE,
        *(url for url, _ in URLS), *CARRIERS, "END_MULTILINE_OUTPUT", "",
    ])
    (ctx.workspace_dir / "multiline.txt").write_bytes(fixture.encode())
    (evidence / "synthetic-input.txt").write_bytes(fixture.encode())
    report = {"checks": []}
    tui = None

    def check(name, passed, detail=None):
        report["checks"].append({"name": name, "passed": bool(passed), "detail": detail})
        print(name, bool(passed), flush=True)

    try:
        report["status"] = start_daemon(ctx)
        setup = ctx.run_haider([
            "run", "--provider", "fake", "--model", "fake-model",
            "--output", "jsonl", "--timeout", "30s", "-p", "Prepare terminal regression.",
        ], timeout=BudgetPart("TUI setup", 40, "30s turn plus startup"))
        (evidence / "setup.jsonl").write_text(setup.stdout)
        (evidence / "setup.stderr").write_text(setup.stderr)
        check("setup CLI", setup.returncode == 0 and not setup.timed_out)
        docs = [json.loads(line) for line in setup.stdout.splitlines() if line.startswith("{")]
        session = next(doc["session_id"] for doc in docs if doc.get("session_id"))
        report["session_id"] = session
        tui = TuiProcess(ctx, session_id=session)
        tui.type_slow("!cat multiline.txt")
        tui.enter()
        deadline = time.monotonic() + 20
        while True:
            frame = tui.repaint(180, 60)
            if "END_MULTILINE_OUTPUT" in frame.text or time.monotonic() >= deadline:
                break
            tui.settle(0.3)
        (evidence / "frame.txt").write_text(frame.text)
        (evidence / "frame.ansi").write_bytes(frame.raw)
        check("actual command and full fixture visible", all(text in frame.text for text in [
            "cat multiline.txt", "BEGIN_MULTILINE_OUTPUT", "END_MULTILINE_OUTPUT",
        ]))
        secrets = ["SYNTHETICTAIL987", *(secret for _, secret in URLS)]
        check("no synthetic secret in frame or PTY history", all(
            secret not in frame.text and secret.encode() not in tui.sink[0]
            for secret in secrets
        ))
        check("all multiline values visibly replaced", frame.text.count(
            "[REDACTED:secret_value] after") == len(MULTILINE))
        check("URL usernames retained", all(
            url.replace(secret, "[REDACTED:secret_value]") in frame.text
            for url, secret in URLS
        ))
        check("six public carriers visible", all(value in frame.text for value in CARRIERS))
        events = ctx.run_haider(["events", "--no-spawn"], timeout=STATUS_REQUEST)
        (evidence / "journal.jsonl").write_text(events.stdout)
        decoded = b"".join(
            chunk for line in events.stdout.splitlines() if line.startswith("{")
            for chunk in command_chunks(json.loads(line))
        )
        (evidence / "journal-output.txt").write_bytes(decoded)
        check("durable command output present and safe", events.returncode == 0
              and b"END_MULTILINE_OUTPUT" in decoded
              and all(secret.encode() not in decoded and secret not in events.stdout for secret in secrets))
    except Exception:
        report["error"] = traceback.format_exc()
    finally:
        if tui is not None:
            clean, audit = tui.close()
            (evidence / "pty.ansi").write_bytes(tui.sink[0])
            check("TUI clean exit", clean, audit)
        cleanup = ctx.cleanup()
        check("owned daemon stopped", str(cleanup.status) == "PASS", str(cleanup))
        report["owned_daemon_pids"] = sorted(ctx.daemon_pids)
        if str(cleanup.status) == "PASS":
            shutil.rmtree(ctx.root)
        report["scratch_removed"] = not ctx.root.exists()
        (evidence / "result.json").write_text(json.dumps(report, indent=2))
    return int("error" in report or not all(check["passed"] for check in report["checks"]))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", required=True, type=Path)
    parser.add_argument("--evidence-dir", required=True, type=Path)
    args = parser.parse_args()
    raise SystemExit(run(args.bin_dir, args.evidence_dir.resolve()))
