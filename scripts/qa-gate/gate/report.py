"""Report schema, validation, atomic JSON writing, and two-report diff."""

from __future__ import annotations

from datetime import datetime, timezone
import hashlib
import json
import math
import ntpath
import os
from pathlib import Path
import posixpath
import re
import statistics
import subprocess
import tempfile
from typing import Any, Iterable

from .contract import (
    VERSION_QUERY,
    EVIDENCE_STATUSES,
    ContractError,
    Evidence,
    aggregate_status,
    budget_seconds,
    validate_evidence,
)

REPORT_SCHEMA = "haider.qa-gate.v1"


def utc_now() -> datetime:
    return datetime.now(timezone.utc)


def utc_text(value: datetime) -> str:
    return value.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def safe_filename_component(value: str) -> str:
    cleaned = re.sub(r"[^A-Za-z0-9._-]+", "-", value.strip()).strip("-.")
    return cleaned or "unknown-host"


def report_filename(tier: str, hostname: str, created: datetime) -> str:
    stamp = created.astimezone(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    return f"qa-gate-{safe_filename_component(tier)}-{safe_filename_component(hostname)}-{stamp}.json"


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while chunk := handle.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def binary_metadata(path: Path, product: str) -> dict[str, Any]:
    resolved = Path(os.path.realpath(os.path.abspath(path)))
    metadata: dict[str, Any] = {
        "path": str(resolved),
        "sha256": None,
        "version_output": None,
        "version": None,
    }
    if not resolved.is_file() or not os.access(resolved, os.X_OK):
        return metadata
    metadata["sha256"] = sha256_file(resolved)
    try:
        result = subprocess.run(
            [str(resolved), "--version"],
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=budget_seconds(VERSION_QUERY),
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return metadata
    output = result.stdout.strip()
    metadata["version_output"] = output
    prefix = f"{product} "
    if result.returncode == 0 and output.startswith(prefix) and len(output) > len(prefix):
        metadata["version"] = output.removeprefix(prefix)
    return metadata


def evidence_to_json(evidence: Evidence) -> dict[str, Any]:
    validate_evidence(evidence)
    return {
        "label": evidence.label,
        "status": evidence.status,
        "evidence_line": evidence.evidence_line,
        "artefacts": list(evidence.artefacts),
    }


def _require_type(document: dict[str, Any], key: str, expected: type) -> Any:
    value = document.get(key)
    if not isinstance(value, expected):
        raise ContractError(
            f"report field {key!r} expected {expected.__name__}, actual={type(value).__name__}"
        )
    return value


def _validate_binary(value: object, label: str, host_platform: str) -> None:
    if not isinstance(value, dict):
        raise ContractError(f"report {label} must be an object")
    path = value.get("path")
    if not isinstance(path, str) or not path:
        raise ContractError(f"report {label}.path must be a non-empty string")
    path_module = ntpath if host_platform.startswith("Windows") else posixpath
    if not path_module.isabs(path) or path_module.normpath(path) != path:
        raise ContractError(f"report {label}.path must be canonical absolute path")
    sha = value.get("sha256")
    if sha is not None and (not isinstance(sha, str) or re.fullmatch(r"[0-9a-f]{64}", sha) is None):
        raise ContractError(f"report {label}.sha256 must be null or lowercase SHA-256")
    for field in ("version_output", "version"):
        if value.get(field) is not None and not isinstance(value[field], str):
            raise ContractError(f"report {label}.{field} must be null or string")


def validate_report(report: object) -> dict[str, Any]:
    """Validate the exact required-key/type contract documented in README."""

    if not isinstance(report, dict):
        raise ContractError("report root must be an object")
    if report.get("schema") != REPORT_SCHEMA:
        raise ContractError(f"report schema actual={report.get('schema')!r} expected={REPORT_SCHEMA!r}")
    if not isinstance(report.get("tier"), str) or not report["tier"]:
        raise ContractError("report tier must be a non-empty string")
    created_at = report.get("created_at_utc")
    if not isinstance(created_at, str):
        raise ContractError("report created_at_utc must be UTC YYYY-MM-DDTHH:MM:SSZ")
    try:
        parsed_created = datetime.strptime(created_at, "%Y-%m-%dT%H:%M:%SZ")
    except ValueError as error:
        raise ContractError(
            "report created_at_utc must be UTC YYYY-MM-DDTHH:MM:SSZ"
        ) from error
    if parsed_created.strftime("%Y-%m-%dT%H:%M:%SZ") != created_at:
        raise ContractError("report created_at_utc is not canonical UTC")
    host = _require_type(report, "host", dict)
    for key in ("hostname", "platform", "python"):
        if not isinstance(host.get(key), str) or not host[key]:
            raise ContractError(f"report host.{key} must be a non-empty string")
    load = _require_type(report, "load", dict)
    one_minute = load.get("one_minute")
    logical_cpus = load.get("logical_cpus")
    if (
        isinstance(one_minute, bool)
        or not isinstance(one_minute, (int, float))
        or not math.isfinite(one_minute)
        or one_minute < 0
    ):
        raise ContractError("report load.one_minute must be a finite non-negative number")
    if isinstance(logical_cpus, bool) or not isinstance(logical_cpus, int) or logical_cpus < 1:
        raise ContractError("report load.logical_cpus must be a positive integer")
    root_measurement_accepted = report.get("measurement_accepted")
    if not isinstance(root_measurement_accepted, bool):
        raise ContractError("report measurement_accepted must be bool")
    reasons = _require_type(report, "measurement_reasons", list)
    if not all(isinstance(reason, str) and reason for reason in reasons):
        raise ContractError("report measurement_reasons must be non-empty strings")
    if root_measurement_accepted != (len(reasons) == 0):
        raise ContractError(
            "report measurement_accepted must be true exactly when measurement_reasons is empty"
        )
    if one_minute > logical_cpus and root_measurement_accepted:
        raise ContractError("report cannot accept timing when one-minute load exceeds CPUs")
    _validate_binary(report.get("binary"), "binary", host["platform"])
    _validate_binary(report.get("daemon_binary"), "daemon_binary", host["platform"])
    if report.get("daemon_version") is not None and not isinstance(report["daemon_version"], str):
        raise ContractError("report daemon_version must be null or string")

    warmup = _require_type(report, "warmup", dict)
    if not isinstance(warmup.get("accepted"), bool):
        raise ContractError("report warmup.accepted must be bool")
    if root_measurement_accepted and warmup["accepted"] is not True:
        raise ContractError("report cannot accept timing when warmup.accepted is false")
    if (
        isinstance(warmup.get("wall_ms"), bool)
        or not isinstance(warmup.get("wall_ms"), int)
        or warmup["wall_ms"] < 0
    ):
        raise ContractError("report warmup.wall_ms must be non-negative integer")
    if not isinstance(warmup.get("evidence_line"), str) or not warmup["evidence_line"]:
        raise ContractError("report warmup.evidence_line must be non-empty string")

    checks = _require_type(report, "checks", list)
    ids: set[str] = set()
    calculated = {status: 0 for status in EVIDENCE_STATUSES}
    for index, check in enumerate(checks):
        if not isinstance(check, dict):
            raise ContractError(f"report checks[{index}] must be an object")
        for key in ("id", "area", "status"):
            if not isinstance(check.get(key), str) or not check[key]:
                raise ContractError(f"report checks[{index}].{key} must be non-empty string")
        if check["id"] in ids:
            raise ContractError(f"report duplicate check id {check['id']!r}")
        ids.add(check["id"])
        if check["status"] not in EVIDENCE_STATUSES:
            raise ContractError(f"report check {check['id']} invalid status {check['status']!r}")
        calculated[check["status"]] += 1
        wall_ms = check.get("wall_ms")
        if isinstance(wall_ms, bool) or not isinstance(wall_ms, int) or wall_ms < 0:
            raise ContractError(f"report check {check['id']} wall_ms must be non-negative integer")
        if not isinstance(check.get("timed"), bool):
            raise ContractError(f"report check {check['id']} timed must be bool")
        expected_fail_until = check.get("expected_fail_until")
        if expected_fail_until is not None and (
            not isinstance(expected_fail_until, str) or not expected_fail_until.strip()
        ):
            raise ContractError(
                f"report check {check['id']} expected_fail_until must be null or non-empty string"
            )
        timing_accepted = check.get("measurement_accepted")
        if check["timed"]:
            if not isinstance(timing_accepted, bool):
                raise ContractError(
                    f"report timed check {check['id']} measurement_accepted must be bool"
                )
            if timing_accepted != root_measurement_accepted:
                raise ContractError(
                    f"report timed check {check['id']} measurement_accepted must equal root"
                )
        elif timing_accepted is not None:
            raise ContractError(
                f"report untimed check {check['id']} measurement_accepted must be null"
            )
        evidence_json = check.get("evidence")
        if not isinstance(evidence_json, list) or not evidence_json:
            raise ContractError(f"report check {check['id']} evidence must be non-empty list")
        evidence: list[Evidence] = []
        for item in evidence_json:
            if not isinstance(item, dict):
                raise ContractError(f"report check {check['id']} evidence item must be object")
            value = Evidence(
                label=item.get("label"),
                status=item.get("status"),
                evidence_line=item.get("evidence_line"),
                artefacts=item.get("artefacts"),
            )
            validate_evidence(value)
            evidence.append(value)
        actual_status = aggregate_status(evidence)
        if actual_status != check["status"]:
            raise ContractError(
                f"report check {check['id']} status={check['status']} evidence_status={actual_status}"
            )
        artefacts = check.get("artefacts")
        if not isinstance(artefacts, list) or not all(
            isinstance(path, str) and path for path in artefacts
        ):
            raise ContractError(f"report check {check['id']} artefacts must be strings")

    summary = _require_type(report, "summary", dict)
    expected_summary = {
        "total": len(checks),
        "pass": calculated["PASS"],
        "fail": calculated["FAIL"],
        "skip": calculated["SKIP"],
        "env_blocked": calculated["ENV_BLOCKED"],
    }
    for key, expected in expected_summary.items():
        if summary.get(key) != expected:
            raise ContractError(
                f"report summary.{key} actual={summary.get(key)!r} expected={expected}"
            )
    return report


def write_report(path: Path, report: dict[str, Any]) -> None:
    validate_report(report)
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(
        mode="w",
        encoding="utf-8",
        dir=path.parent,
        prefix=f".{path.name}.",
        suffix=".tmp",
        delete=False,
    ) as handle:
        temporary = Path(handle.name)
        json.dump(report, handle, indent=2, sort_keys=True, ensure_ascii=False, allow_nan=False)
        handle.write("\n")
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(temporary, path)


def load_report(path: Path) -> dict[str, Any]:
    try:
        with path.open(encoding="utf-8") as handle:
            report = json.load(handle)
    except (OSError, json.JSONDecodeError) as error:
        raise ContractError(f"cannot read report {path}: {error}") from error
    return validate_report(report)


def _accepted_timing(check: dict[str, Any]) -> bool:
    return check.get("timed") is True and check.get("measurement_accepted") is True


def diff_reports(previous: dict[str, Any], current: dict[str, Any]) -> list[str]:
    """Return status changes and accepted wall outliers from two reports.

    With one wall value per check, the implementable MAD is the population MAD
    across the matched signed deltas. The per-check threshold is the larger of
    3*MAD and 20% of its previous wall.
    """

    validate_report(previous)
    validate_report(current)
    prev_by_id = {check["id"]: check for check in previous["checks"]}
    cur_by_id = {check["id"]: check for check in current["checks"]}
    lines: list[str] = []
    for check_id in sorted(prev_by_id.keys() - cur_by_id.keys()):
        lines.append(f"REMOVED {check_id} previous={prev_by_id[check_id]['status']}")
    for check_id in sorted(cur_by_id.keys() - prev_by_id.keys()):
        lines.append(f"ADDED {check_id} current={cur_by_id[check_id]['status']}")
    for check_id in sorted(prev_by_id.keys() & cur_by_id.keys()):
        before = prev_by_id[check_id]["status"]
        after = cur_by_id[check_id]["status"]
        if before != after:
            lines.append(f"FLIP {check_id} {before}->{after}")

    timing_ids = [
        check_id
        for check_id in sorted(prev_by_id.keys() & cur_by_id.keys())
        if _accepted_timing(prev_by_id[check_id]) and _accepted_timing(cur_by_id[check_id])
    ]
    deltas = [cur_by_id[check_id]["wall_ms"] - prev_by_id[check_id]["wall_ms"] for check_id in timing_ids]
    median_delta = statistics.median(deltas) if deltas else 0.0
    mad = statistics.median(abs(delta - median_delta) for delta in deltas) if deltas else 0.0
    for check_id in timing_ids:
        before = prev_by_id[check_id]["wall_ms"]
        after = cur_by_id[check_id]["wall_ms"]
        delta = after - before
        threshold = max(3.0 * mad, 0.20 * before)
        if abs(delta) > threshold:
            percent = math.inf if before == 0 else 100.0 * delta / before
            percent_text = "+inf" if math.isinf(percent) and percent > 0 else f"{percent:+.1f}"
            lines.append(
                f"WALL {check_id} {before}->{after}ms delta={delta:+d}ms "
                f"percent={percent_text}% threshold={threshold:.1f}ms mad={mad:.1f}ms"
            )
    if not lines:
        lines.append(f"NO_CHANGES matched={len(prev_by_id.keys() & cur_by_id.keys())} mad={mad:.1f}ms")
    return lines


def status_change_count(lines: Iterable[str]) -> int:
    return sum(line.startswith(("FLIP ", "ADDED ", "REMOVED ")) for line in lines)
