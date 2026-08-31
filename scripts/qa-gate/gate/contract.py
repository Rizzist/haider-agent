"""The declaration and evidence contract shared by every QA check."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Iterable

PASS = "PASS"
FAIL = "FAIL"
SKIP = "SKIP"
ENV_BLOCKED = "ENV_BLOCKED"
EVIDENCE_STATUSES = frozenset((PASS, FAIL, SKIP, ENV_BLOCKED))


class ContractError(RuntimeError):
    """A check or report violated the QA gate's machine contract."""


@dataclass(frozen=True)
class BudgetPart:
    """One named product or harness budget with provenance."""

    name: str
    seconds: float
    source: str

    def __add__(self, other: object) -> "BudgetSum":
        if isinstance(other, BudgetPart):
            return BudgetSum((self, other))
        if isinstance(other, BudgetSum):
            return BudgetSum((self, *other.parts))
        return NotImplemented


@dataclass(frozen=True)
class BudgetSum:
    """A deadline derived by summing named budgets; never a bare literal."""

    parts: tuple[BudgetPart, ...]

    def __add__(self, other: object) -> "BudgetSum":
        if isinstance(other, BudgetPart):
            return BudgetSum((*self.parts, other))
        if isinstance(other, BudgetSum):
            return BudgetSum((*self.parts, *other.parts))
        return NotImplemented

    @property
    def seconds(self) -> float:
        return sum(part.seconds for part in self.parts)

    @property
    def milliseconds(self) -> int:
        return round(self.seconds * 1_000)


# Registry #94: outer bounds are arithmetic over the nested product budgets.
DAEMON_STARTUP = BudgetPart(
    "daemon startup deadline",
    30.0,
    "crates/haider-client/src/spawn.rs:58 STARTUP_DEADLINE",
)
VERSION_QUERY = BudgetPart(
    "cold binary version query",
    30.0,
    "qa-gate --version ceiling; registry #42 cold-inode allowance is nested here",
)
STATUS_REQUEST = BudgetPart(
    "client request timeout",
    60.0,
    "crates/haider-client/src/client.rs:41-46 REQUEST_TIMEOUT",
)
RUN_TIMEOUT = BudgetPart(
    "qa attached run --timeout",
    30.0,
    "haider run --timeout 30s (crates/haider-client/src/headless.rs:2583-2591)",
)
RUN_TERMINAL_GRACE = BudgetPart(
    "terminal cancellation grace",
    2.0,
    "crates/haider-client/src/headless.rs:67-75 DEFAULT_TERMINAL_GRACE",
)
DAEMON_STOP = BudgetPart(
    "daemon stop timeout",
    20.0,
    "crates/haider-cli/src/daemon.rs:22 DEFAULT_STOP_TIMEOUT",
)
PROCESS_EXIT_GRACE = BudgetPart(
    "post-stop process-exit observation grace",
    2.0,
    "qa-gate bounded kill-0 observation after daemon stop reports process_exited",
)


@dataclass(frozen=True)
class Evidence:
    """One labelled machine-verifiable observation returned by a check."""

    label: str
    status: str
    evidence_line: str
    artefacts: list[str] = field(default_factory=list)


def validate_budget(value: object) -> BudgetSum:
    """Accept only a real sum of at least two named, positive budgets."""

    if not isinstance(value, BudgetSum):
        raise ContractError(
            "budget must be a sum of named BudgetPart values; literal-only budgets are rejected"
        )
    if len(value.parts) < 2:
        raise ContractError("budget must sum at least two named budget parts")
    for part in value.parts:
        if (
            not isinstance(part.name, str)
            or not part.name.strip()
            or not isinstance(part.source, str)
            or not part.source.strip()
        ):
            raise ContractError("every budget part needs a non-empty name and source")
        if isinstance(part.seconds, bool) or not isinstance(part.seconds, (int, float)):
            raise ContractError(f"budget part {part.name!r} must have numeric seconds")
        if part.seconds <= 0:
            raise ContractError(f"budget part {part.name!r} must be positive")
    return value


def budget_seconds(value: BudgetPart | BudgetSum) -> float:
    if isinstance(value, BudgetPart):
        return value.seconds
    if isinstance(value, BudgetSum):
        validate_budget(value)
        return value.seconds
    raise ContractError("subprocess timeout must be a named BudgetPart or BudgetSum")


def validate_evidence(evidence: Evidence) -> None:
    if not isinstance(evidence, Evidence):
        raise ContractError(f"run(ctx) returned non-Evidence value {type(evidence).__name__}")
    if not isinstance(evidence.label, str) or not evidence.label.strip():
        raise ContractError("Evidence.label must be non-empty")
    if not isinstance(evidence.status, str) or evidence.status not in EVIDENCE_STATUSES:
        raise ContractError(
            f"Evidence {evidence.label!r} has invalid status {evidence.status!r}"
        )
    if not isinstance(evidence.evidence_line, str) or not evidence.evidence_line.strip():
        raise ContractError(f"Evidence {evidence.label!r} has an empty evidence_line")
    if "\n" in evidence.evidence_line or "\r" in evidence.evidence_line:
        raise ContractError(f"Evidence {evidence.label!r} evidence_line must be one line")
    if not isinstance(evidence.artefacts, list) or not all(
        isinstance(path, str) and path for path in evidence.artefacts
    ):
        raise ContractError(f"Evidence {evidence.label!r} artefacts must be non-empty strings")


def validate_evidence_list(values: object) -> list[Evidence]:
    if not isinstance(values, list) or not values:
        raise ContractError("run(ctx) must return a non-empty list[Evidence]")
    for value in values:
        validate_evidence(value)
    return values


def aggregate_status(evidence: Iterable[Evidence]) -> str:
    statuses = {item.status for item in evidence}
    if FAIL in statuses:
        return FAIL
    if ENV_BLOCKED in statuses:
        return ENV_BLOCKED
    if SKIP in statuses:
        return SKIP
    return PASS
