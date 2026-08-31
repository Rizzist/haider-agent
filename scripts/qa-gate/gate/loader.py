"""Check-module discovery, declaration validation, needs, and segment law."""

from __future__ import annotations

from dataclasses import dataclass
import hashlib
import importlib.util
import os
from pathlib import Path
import re
from types import ModuleType
from typing import Any, Callable

from .contract import BudgetSum, ContractError, ENV_BLOCKED, Evidence, validate_budget

TERMINAL_STEPS = frozenset(
    (
        "finish",
        "error",
        "error_presented",
        "hang",
        "premature_eof",
        "error_with_retryability",
        "malformed_frame",
    )
)
KNOWN_NEEDS = frozenset(("binary", "daemon", "network:none", "pty"))


@dataclass(frozen=True)
class CheckDefinition:
    path: Path
    id: str
    tier: str
    area: str
    needs: tuple[str, ...]
    script: list[dict[str, Any]]
    budget: BudgetSum
    timed: bool
    turns_expected: int
    segments: int
    expected_fail_until: str | None
    run: Callable[[Any], list[Evidence]]
    module: ModuleType


def _load_module(path: Path) -> ModuleType:
    digest = hashlib.sha256(str(path.resolve()).encode()).hexdigest()[:16]
    spec = importlib.util.spec_from_file_location(f"haider_qa_check_{digest}", path)
    if spec is None or spec.loader is None:
        raise ContractError(f"cannot load check module {path}")
    module = importlib.util.module_from_spec(spec)
    try:
        spec.loader.exec_module(module)
    except Exception as error:
        raise ContractError(f"cannot import check {path}: {type(error).__name__}: {error}") from error
    return module


def _string_field(module: ModuleType, field: str, path: Path) -> str:
    value = getattr(module, field, None)
    if not isinstance(value, str) or not value.strip():
        raise ContractError(f"check {path} must export non-empty string {field}")
    return value


def load_check(path: Path, expected_tier: str | None = None) -> CheckDefinition:
    path = Path(path)
    module = _load_module(path)
    check_id = _string_field(module, "id", path)
    tier = _string_field(module, "tier", path)
    area = _string_field(module, "area", path)
    if expected_tier is not None and tier != expected_tier:
        raise ContractError(
            f"check {check_id} tier actual={tier!r} expected={expected_tier!r} from directory"
        )
    if not check_id.startswith(f"{tier}."):
        raise ContractError(f"check id {check_id!r} must start with tier {tier!r}")

    needs = getattr(module, "needs", None)
    if not isinstance(needs, (list, tuple)) or not all(
        isinstance(need, str) and need for need in needs
    ):
        raise ContractError(f"check {check_id} needs must be a list/tuple of strings")
    if len(set(needs)) != len(needs):
        raise ContractError(f"check {check_id} declares duplicate needs")
    for need in needs:
        if need not in KNOWN_NEEDS and not need.startswith("fixture:"):
            raise ContractError(f"check {check_id} declares unknown need {need!r}")

    script = getattr(module, "script", None)
    if not isinstance(script, list):
        raise ContractError(f"check {check_id} script must be a list of fake-provider steps")
    for index, step in enumerate(script):
        if not isinstance(step, dict) or not isinstance(step.get("step"), str):
            raise ContractError(f"check {check_id} script[{index}] needs string field step")
    segments = sum(step["step"] in TERMINAL_STEPS for step in script)

    turns_expected = getattr(module, "turns_expected", None)
    if (
        isinstance(turns_expected, bool)
        or not isinstance(turns_expected, int)
        or turns_expected < 0
    ):
        raise ContractError(
            f"check {check_id} must export integer turns_expected >= 0 for the segment law"
        )
    if turns_expected > segments:
        raise ContractError(
            f"check {check_id} refused: turns_expected={turns_expected} exceeds segments={segments}"
        )

    budget = validate_budget(getattr(module, "budget", None))
    timed = getattr(module, "timed", None)
    if not isinstance(timed, bool):
        raise ContractError(f"check {check_id} timed must be bool")
    expected_fail_until = getattr(module, "expected_fail_until", None)
    if expected_fail_until is not None and (
        not isinstance(expected_fail_until, str)
        or re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?", expected_fail_until)
        is None
    ):
        raise ContractError(
            f"check {check_id} expected_fail_until must be absent or a semantic version"
        )
    run = getattr(module, "run", None)
    if not callable(run):
        raise ContractError(f"check {check_id} must export callable run(ctx)")

    expected_fail_until = getattr(module, "expected_fail_until", None)
    if expected_fail_until is not None and (
        not isinstance(expected_fail_until, str) or not expected_fail_until.strip()
    ):
        raise ContractError(
            f"check {check_id} expected_fail_until must be absent or a non-empty version string"
        )

    return CheckDefinition(
        path=path,
        id=check_id,
        tier=tier,
        area=area,
        needs=tuple(needs),
        script=script,
        budget=budget,
        timed=timed,
        turns_expected=turns_expected,
        segments=segments,
        expected_fail_until=expected_fail_until,
        run=run,
        module=module,
    )


def discover_checks(check_root: Path, tier: str) -> list[CheckDefinition]:
    tier_root = Path(check_root) / tier
    if not tier_root.is_dir():
        raise ContractError(f"unknown or empty tier directory {tier_root}")
    checks = [
        load_check(path, tier)
        for path in sorted(tier_root.glob("*.py"))
        if not path.name.startswith("_")
    ]
    if not checks:
        raise ContractError(f"tier {tier!r} has no checks")
    seen: dict[str, Path] = {}
    for check in checks:
        if check.id in seen:
            raise ContractError(
                f"duplicate check id {check.id!r}: {seen[check.id]} and {check.path}"
            )
        seen[check.id] = check.path
    return checks


def missing_needs(
    check: CheckDefinition,
    *,
    bin_dir: Path,
    fixture_root: Path,
) -> list[str]:
    reasons: list[str] = []
    binary_name = "haider.exe" if os.name == "nt" else "haider"
    daemon_name = "haiderd.exe" if os.name == "nt" else "haiderd"
    for need in check.needs:
        if need == "binary":
            path = bin_dir / binary_name
            if not path.is_file() or not os.access(path, os.X_OK):
                reasons.append(f"binary unavailable: {path}")
        elif need == "daemon":
            path = bin_dir / daemon_name
            if not path.is_file() or not os.access(path, os.X_OK):
                reasons.append(f"daemon unavailable: {path}")
        elif need == "pty":
            if os.name != "posix":
                reasons.append("pty unavailable on this platform")
            else:
                try:
                    import pty  # noqa: F401 - capability probe
                except ImportError:
                    reasons.append("stdlib pty module unavailable")
        elif need.startswith("fixture:"):
            relative = need.removeprefix("fixture:")
            fixture = (fixture_root / relative).resolve()
            try:
                within = os.path.commonpath((str(fixture), str(fixture_root.resolve()))) == str(
                    fixture_root.resolve()
                )
            except ValueError:
                within = False
            if not within:
                raise ContractError(f"check {check.id} fixture escapes fixture root: {relative!r}")
            if not fixture.exists():
                reasons.append(f"fixture unavailable: {relative}")
        elif need == "network:none":
            continue
    return reasons


def env_blocked_evidence(reasons: list[str]) -> list[Evidence]:
    if not reasons:
        raise ContractError("ENV_BLOCKED evidence requires at least one missing need")
    return [
        Evidence(
            "environment_needs",
            ENV_BLOCKED,
            "missing need: " + "; ".join(reasons),
        )
    ]
