"""Gate warm attached turns against the accepted v0.0.968 baseline."""

from __future__ import annotations

import json

from gate import ENV_BLOCKED, FAIL, PASS, BudgetPart, Evidence
from turn_wall_harness import run_harness


id = "t1.turn.wall_budget"
tier = "t1"
area = "performance"
needs = ("binary", "daemon", "network:none")
script = []
turns_expected = 0
timed = True

# Accepted on d75a8ea with the installed v0.0.968 pair and the final proxy,
# 2026-09-01: 5 warm-ups + 25 retained samples per shape, loads
# 2.53/2.41/2.41. The full raw report is
# docs/testing/v0.0.969/turnperf/baseline-v0.0.968-confirmation.json.
BASELINE = {
    "single": {
        "wall_ms": 56.022125,
        "combined_cpu_ms": 4.898857999999969,
        "combined_cpu_mad_ms": 0.23781100000003086,
        "combined_peak_rss_kib": 27072.0,
        "combined_peak_rss_tolerance_kib": 64.0,
    },
    "tool": {
        "wall_ms": 92.692834,
        "combined_cpu_ms": 5.881752000000031,
        "combined_cpu_mad_ms": 0.3600260000000457,
        "combined_peak_rss_kib": 27536.0,
        "combined_peak_rss_tolerance_kib": 64.0,
    },
}
# PROPOSAL2 §4 accepted release ceilings. The R2-03 candidate clears these;
# unlike the earlier general gate, this lane does not permit 10% wall slack.
WALL_BUDGET_MS = {"single": 56.7, "tool": 78.0}

TURN_CASES = BudgetPart(
    "sixty bounded attached turns",
    4_920.0,
    "60 * (20s headless timeout + 60s request ceiling + 2s terminal grace); "
    "turnperf_support.run_arguments and turn_wall_harness.one_case",
)
SETTLE_AND_IDENTITY = BudgetPart(
    "sixty durable-idle/provider/PID gates",
    1_020.0,
    "60 * (5s durable Idle + 2s provider settle + 10s status identity)",
)
DAEMON_LIFECYCLE = BudgetPart(
    "harness and runner daemon lifecycle",
    202.0,
    "60s ready + 10s initial status + 10s final status + 30s exact stop + "
    "runner cleanup (60s status + 20s stop + 2s PID observation)",
)
# Registry #94: 4,920 + 1,020 + 202 = 6,142 seconds. Every nested
# subprocess/poll bound is covered by one named term above.
budget = TURN_CASES + SETTLE_AND_IDENTITY + DAEMON_LIFECYCLE


def _line(report) -> str:
    values = []
    for shape in ("single", "tool"):
        summary = report.get("summary", {}).get(shape, {})
        values.append(
            f"{shape}:wall={summary.get('wall_ms', {}).get('median')}ms/"
            f"{summary.get('budget_ms')}ms "
            f"cpu={summary.get('combined_cpu_ms', {}).get('median')}ms "
            f"peak={summary.get('combined_peak_rss_kib')}KiB"
        )
    return " ".join(values)


def run(ctx) -> list[Evidence]:
    report = run_harness(
        ctx.bin_dir,
        budget_ms=WALL_BUDGET_MS,
    )
    if not report["measurement_accepted"]:
        evidence = []
        if report["correctness_failures"]:
            evidence.append(
                Evidence(
                    "turn_wall_correctness",
                    FAIL,
                    "failures=" + " | ".join(report["correctness_failures"]),
                )
            )
        evidence.append(
            Evidence(
                "turn_wall_measurement",
                ENV_BLOCKED,
                "measurement_accepted=false reasons="
                + " | ".join(report["measurement_reasons"]),
            )
        )
        return evidence

    scratch = ctx.write_artefact(
        "turn-wall-report.json", json.dumps(report, indent=2, sort_keys=True) + "\n"
    )
    artefact = ctx.publish_artefact("turn-wall-report.json", scratch)
    if report["failures"]:
        return [
            Evidence(
                "turn_wall_budget",
                FAIL,
                _line(report) + " failures=" + " | ".join(report["failures"]),
                [artefact],
            )
        ]
    return [
        Evidence(
            "turn_wall_budget",
            PASS,
            _line(report)
            + f" load={report['load_one_minute']} pid={report['daemon']['pid']} "
            "same_generation=true cleanup=stopped_cleanly",
            [artefact],
        )
    ]
