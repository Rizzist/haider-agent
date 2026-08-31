"""A token ceiling binds before the fake provider receives the request."""

from gate import (
    DAEMON_STARTUP,
    DAEMON_STOP,
    PROCESS_EXIT_GRACE,
    RUN_TERMINAL_GRACE,
    STATUS_REQUEST,
    BudgetPart,
)
from gate.budget_binding import check_budget_binding

id = "t0.budget.max_tokens_binds"
tier = "t0"
area = "budget"
needs = ("binary", "daemon", "network:none")
CONTROL = "QA_TOKENS_ABOVE_BOUND_CONTROL"
script = [
    {"step": "emit_text", "text": CONTROL},
    {
        "step": "emit_usage",
        "usage": {
            "input": 2,
            "output": 1,
            "reasoning": 0,
            "cached": 0,
            "source": "locally_exact",
        },
    },
    {"step": "finish", "reason": "end_turn"},
]
turns_expected = 1
HEADLESS_TEN_SECONDS = BudgetPart(
    "headless budget check --timeout",
    10.0,
    "haider run --timeout 10s; crates/haider-cli/src/run.rs:460-485",
)
# Registry #94: isolated control 30+10+2 plus its 60+20+2+2 cleanup;
# below-bound 30+10+2 plus parent cleanup 60+20+2+2. Total=252s.
budget = (
    DAEMON_STARTUP
    + HEADLESS_TEN_SECONDS
    + RUN_TERMINAL_GRACE
    + DAEMON_STARTUP
    + HEADLESS_TEN_SECONDS
    + RUN_TERMINAL_GRACE
    + STATUS_REQUEST
    + DAEMON_STOP
    + PROCESS_EXIT_GRACE
    + PROCESS_EXIT_GRACE
    + STATUS_REQUEST
    + DAEMON_STOP
    + PROCESS_EXIT_GRACE
    + PROCESS_EXIT_GRACE
)
timed = False


def run(ctx):
    return check_budget_binding(
        ctx,
        flag="--max-tokens",
        low_value="1",
        high_value="100000",
        dimension="tokens",
        control_sentinel=CONTROL,
        process_timeout=DAEMON_STARTUP + HEADLESS_TEN_SECONDS + RUN_TERMINAL_GRACE,
    )
