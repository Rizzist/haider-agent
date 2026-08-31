#!/usr/bin/env python3
"""Run installed Haider artefacts through CPU-only functional checks."""

from __future__ import annotations

import argparse
from datetime import datetime
import os
from pathlib import Path
import platform
import socket
import sys
import time
import traceback
import unittest
from typing import Any

from gate.context import CheckContext, canonical_path
from gate.contract import (
    DAEMON_STARTUP,
    FAIL,
    PASS,
    ContractError,
    Evidence,
    aggregate_status,
    validate_evidence,
    validate_evidence_list,
)
from gate.loader import (
    CheckDefinition,
    discover_checks,
    env_blocked_evidence,
    missing_needs,
)
from gate.report import (
    REPORT_SCHEMA,
    binary_metadata,
    diff_reports,
    evidence_to_json,
    load_report,
    report_filename,
    status_change_count,
    utc_now,
    utc_text,
    validate_report,
    write_report,
)

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parents[1]
CHECK_ROOT = HERE / "checks"
FIXTURE_ROOT = HERE / "fixtures"


def _artifact_names(evidence: list[Evidence]) -> list[str]:
    return sorted({path for item in evidence for path in item.artefacts})


def execute_check(
    check: CheckDefinition,
    *,
    bin_dir: Path,
    measurement_accepted: bool,
) -> tuple[dict[str, Any], set[str]]:
    started = time.monotonic()
    unavailable = missing_needs(check, bin_dir=bin_dir, fixture_root=FIXTURE_ROOT)
    if unavailable:
        evidence = env_blocked_evidence(unavailable)
        return (
            {
                "id": check.id,
                "area": check.area,
                "status": aggregate_status(evidence),
                "evidence": [evidence_to_json(item) for item in evidence],
                "wall_ms": round((time.monotonic() - started) * 1_000),
                "artefacts": [],
                "timed": check.timed,
                "measurement_accepted": measurement_accepted if check.timed else None,
                "budget": {
                    "total_ms": check.budget.milliseconds,
                    "parts": [
                        {
                            "name": part.name,
                            "milliseconds": round(part.seconds * 1_000),
                            "source": part.source,
                        }
                        for part in check.budget.parts
                    ],
                },
                "segments": check.segments,
                "turns_expected": check.turns_expected,
            },
            set(),
        )

    context: CheckContext | None = None
    evidence: list[Evidence] = []
    try:
        context = CheckContext(check_id=check.id, bin_dir=bin_dir, script=check.script)
        evidence.extend(validate_evidence_list(check.run(context)))
    except Exception as error:  # runner errors must become diagnostic FAIL rows
        detail = f"runner_error type={type(error).__name__} actual={str(error)!r}"
        artefacts: list[str] = []
        if context is not None:
            artefacts.append(context.write_artefact("runner-error.txt", traceback.format_exc()))
        evidence.append(Evidence("runner_error", FAIL, detail, artefacts))
    finally:
        if context is not None:
            try:
                cleanup = context.cleanup()
            except Exception as error:
                emergency = context.emergency_cleanup()
                artefacts: list[str] = []
                try:
                    artefacts.append(
                        context.write_artefact("cleanup-runner-error.txt", traceback.format_exc())
                    )
                except Exception:
                    pass
                cleanup = Evidence(
                    "no_orphan_daemons",
                    FAIL,
                    f"cleanup_runner_error type={type(error).__name__} actual={str(error)!r} "
                    f"emergency_cleanup={emergency}",
                    artefacts,
                )
            validate_evidence(cleanup)
            evidence.append(cleanup)

    wall_ms = round((time.monotonic() - started) * 1_000)
    if wall_ms > check.budget.milliseconds:
        evidence.append(
            Evidence(
                "derived_budget",
                FAIL,
                f"derived_budget exceeded actual={wall_ms}ms limit={check.budget.milliseconds}ms",
            )
        )
    status = aggregate_status(evidence)
    artefacts = _artifact_names(evidence)
    daemon_versions: set[str] = set()
    if context is not None:
        daemon_versions.update(context.daemon_versions)
        if status == FAIL:
            artefacts.append(str(context.root))
        context.dispose(keep=status == FAIL)
    artefacts = sorted(set(artefacts))
    row = {
        "id": check.id,
        "area": check.area,
        "status": status,
        "evidence": [evidence_to_json(item) for item in evidence],
        "wall_ms": wall_ms,
        "artefacts": artefacts,
        "timed": check.timed,
        "measurement_accepted": measurement_accepted if check.timed else None,
        "budget": {
            "total_ms": check.budget.milliseconds,
            "parts": [
                {
                    "name": part.name,
                    "milliseconds": round(part.seconds * 1_000),
                    "source": part.source,
                }
                for part in check.budget.parts
            ],
        },
        "segments": check.segments,
        "turns_expected": check.turns_expected,
    }
    return row, daemon_versions


def warm_up(bin_dir: Path) -> tuple[dict[str, Any], set[str]]:
    started = time.monotonic()
    context: CheckContext | None = None
    accepted = False
    line = "warmup not run"
    versions: set[str] = set()
    try:
        context = CheckContext(
            check_id="qa-gate.warmup",
            bin_dir=bin_dir,
            script=[{"step": "finish", "reason": "end_turn"}],
        )
        ready = context.run_haider(["--ready"], timeout=DAEMON_STARTUP)
        cleanup = context.cleanup()
        versions.update(context.daemon_versions)
        accepted = not ready.timed_out and ready.returncode == 0 and cleanup.status == PASS
        line = (
            f"warmup ready_exit={ready.returncode} timed_out={str(ready.timed_out).lower()} "
            f"{cleanup.evidence_line}"
        )
        if not accepted:
            context.command_artefact("warmup-ready", ready)
    except Exception as error:
        emergency = context.emergency_cleanup() if context is not None else "no_context"
        line = (
            f"warmup runner_error type={type(error).__name__} actual={str(error)!r} "
            f"emergency_cleanup={emergency}"
        )
    finally:
        if context is not None:
            context.dispose(keep=not accepted)
    return (
        {
            "accepted": accepted,
            "wall_ms": round((time.monotonic() - started) * 1_000),
            "evidence_line": line,
        },
        versions,
    )


def _load_snapshot() -> tuple[float, int, list[str]]:
    cpus = os.cpu_count() or 1
    reasons: list[str] = []
    try:
        one_minute = float(os.getloadavg()[0])
    except (AttributeError, OSError):
        one_minute = 0.0
        reasons.append("os.getloadavg unavailable")
    if one_minute > cpus:
        reasons.append(f"one-minute load {one_minute:.2f} exceeds logical CPUs {cpus}")
    return one_minute, cpus, reasons


def _render_check(row: dict[str, Any]) -> str:
    details = "; ".join(item["evidence_line"] for item in row["evidence"])
    line = f"{row['status']} {row['id']} {details}"
    if row["status"] == FAIL and row["artefacts"]:
        line += " artefacts=" + ",".join(row["artefacts"])
    return line


def _default_report_dir(version: str | None) -> Path:
    label = f"v{version}" if version else "unknown-version"
    return REPO_ROOT / "docs" / "testing" / label


def run_tier(args: argparse.Namespace) -> int:
    if sys.version_info < (3, 11):
        raise ContractError(f"Python 3.11+ required, actual={platform.python_version()}")
    checks = discover_checks(CHECK_ROOT, args.tier)
    bin_dir = Path(canonical_path(args.bin_dir))
    executable = "haider.exe" if os.name == "nt" else "haider"
    daemon_executable = "haiderd.exe" if os.name == "nt" else "haiderd"

    created = utc_now()
    hostname = socket.gethostname() or platform.node() or "unknown-host"
    one_minute, cpus, measurement_reasons = _load_snapshot()
    binary = binary_metadata(bin_dir / executable, "haider")
    daemon_binary = binary_metadata(bin_dir / daemon_executable, "haiderd")

    required_pair_present = all(
        value.get("sha256") is not None and value.get("version") is not None
        for value in (binary, daemon_binary)
    )
    daemon_versions: set[str] = set()
    if required_pair_present and any(check.timed for check in checks):
        warmup, warm_versions = warm_up(bin_dir)
        daemon_versions.update(warm_versions)
        if not warmup["accepted"]:
            measurement_reasons.append("untimed daemon warmup failed")
    else:
        warmup = {
            "accepted": not any(check.timed for check in checks),
            "wall_ms": 0,
            "evidence_line": (
                "warmup unnecessary: no timed checks"
                if not any(check.timed for check in checks)
                else "warmup unavailable: installed binary pair missing or has invalid --version"
            ),
        }
        if any(check.timed for check in checks):
            measurement_reasons.append("untimed daemon warmup unavailable")
    measurement_accepted = not measurement_reasons

    rows: list[dict[str, Any]] = []
    for check in checks:
        row, observed = execute_check(
            check,
            bin_dir=bin_dir,
            measurement_accepted=measurement_accepted,
        )
        rows.append(row)
        daemon_versions.update(observed)

    counts = {
        "total": len(rows),
        "pass": sum(row["status"] == "PASS" for row in rows),
        "fail": sum(row["status"] == "FAIL" for row in rows),
        "skip": sum(row["status"] == "SKIP" for row in rows),
        "env_blocked": sum(row["status"] == "ENV_BLOCKED" for row in rows),
    }
    daemon_version = "|".join(sorted(daemon_versions)) or None
    report = {
        "schema": REPORT_SCHEMA,
        "tier": args.tier,
        "created_at_utc": utc_text(created),
        "host": {
            "hostname": hostname,
            "platform": platform.platform(),
            "python": platform.python_version(),
        },
        "load": {"one_minute": one_minute, "logical_cpus": cpus},
        "measurement_accepted": measurement_accepted,
        "measurement_reasons": measurement_reasons,
        "binary": binary,
        "daemon_binary": daemon_binary,
        "daemon_version": daemon_version,
        "warmup": warmup,
        "checks": rows,
        "summary": counts,
    }
    validate_report(report)
    report_dir = Path(args.report_dir) if args.report_dir else _default_report_dir(binary["version"])
    report_path = report_dir / report_filename(args.tier, hostname, created)
    write_report(report_path, report)

    for row in rows:
        print(_render_check(row))
    print(f"report {report_path}")
    measurement = "accepted" if measurement_accepted else "rejected(" + "; ".join(measurement_reasons) + ")"
    version = binary["version"] or "unknown-version"
    print(
        f"qa-gate {args.tier} {version}: {counts['pass']}/{counts['total']} PASS, "
        f"{counts['fail']} FAIL, {counts['skip']} SKIP, {counts['env_blocked']} ENV-BLOCKED, "
        f"measurement {measurement}"
    )
    return 0 if counts["fail"] == 0 else 1


def run_tests() -> int:
    suite = unittest.defaultTestLoader.discover(str(HERE / "tests"), pattern="test_*.py")
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    return 0 if result.wasSuccessful() else 1


def parse_run_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="run.sh",
        description="Run CPU-only QA checks against an installed haider/haiderd pair.",
    )
    parser.add_argument("--tier", required=True)
    parser.add_argument("--bin-dir", required=True)
    parser.add_argument("--report-dir")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    argv = list(sys.argv[1:] if argv is None else argv)
    try:
        if argv and argv[0] == "diff":
            if len(argv) != 3:
                raise ContractError("usage: run.sh diff <previous.json> <current.json>")
            previous = load_report(Path(argv[1]))
            current = load_report(Path(argv[2]))
            lines = diff_reports(previous, current)
            for line in lines:
                print(line)
            return 1 if status_change_count(lines) else 0
        if argv and argv[0] == "validate":
            if len(argv) != 2:
                raise ContractError("usage: run.sh validate <report.json>")
            report = load_report(Path(argv[1]))
            print(
                f"VALID {argv[1]} schema={report['schema']} checks={len(report['checks'])}"
            )
            return 0
        if argv and argv[0] == "test":
            if len(argv) != 1:
                raise ContractError("usage: run.sh test")
            return run_tests()
        return run_tier(parse_run_args(argv))
    except ContractError as error:
        print(f"qa-gate: contract error: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
