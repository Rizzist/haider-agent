"""Stdlib-only QA gate primitives."""

from .contract import (
    DAEMON_STARTUP,
    DAEMON_STOP,
    PASS,
    FAIL,
    SKIP,
    ENV_BLOCKED,
    PROCESS_EXIT_GRACE,
    RUN_TERMINAL_GRACE,
    RUN_TIMEOUT,
    STATUS_REQUEST,
    VERSION_QUERY,
    BudgetPart,
    BudgetSum,
    ContractError,
    Evidence,
)

__all__ = [
    "DAEMON_STARTUP",
    "DAEMON_STOP",
    "PASS",
    "FAIL",
    "SKIP",
    "ENV_BLOCKED",
    "PROCESS_EXIT_GRACE",
    "RUN_TERMINAL_GRACE",
    "RUN_TIMEOUT",
    "STATUS_REQUEST",
    "VERSION_QUERY",
    "BudgetPart",
    "BudgetSum",
    "ContractError",
    "Evidence",
]
