"""Gate complete TTL=0 run lifecycles against the accepted v0.0.968 pins."""

from __future__ import annotations

import json

from gate import ENV_BLOCKED, FAIL, PASS, BudgetPart, Evidence
from turn_wall_harness import run_one_shot_harness


id = "t1.turn.one_shot_budget"
tier = "t1"
area = "performance"
needs = ("binary", "network:none")
script = []
turns_expected = 0
timed = True

# Accepted D6 / PROPOSAL2 release pins. CPU is the process-tree total
# normalized to the canonical 21 measured fresh-profile cases.
WALL_BUDGET_MS = 124.0
CPU_TOTAL_21_BUDGET_MS = 1_059.0
PEAK_RSS_BUDGET_KIB = 51.2 * 1_024

CASE_COMMANDS = BudgetPart(
    "twenty-six bounded fresh-profile turns",
    2_132.0,
    "26 * 82s turn_wall_harness.one_case command timeout",
)
CASE_PROOFS = BudgetPart(
    "twenty-six provider and no-spawn proofs",
    312.0,
    "26 * (2s provider settle + 10s status --no-spawn)",
)
EXACT_CLEANUP = BudgetPart(
    "twenty-six exact-profile cleanup fallbacks",
    936.0,
    "26 * (30s exact stop + 2s stop wait + 2s SIGTERM wait + 2s SIGKILL wait)",
)
HARNESS_FINALIZE = BudgetPart(
    "provider and provenance finalization",
    4.0,
    "2s provider thread join + 2s git commit query",
)
# Registry #94: 2,132 + 312 + 936 + 4 = 3,384 seconds. Every nested
# subprocess, provider wait, and exact-PID cleanup bound is named above.
budget = CASE_COMMANDS + CASE_PROOFS + EXACT_CLEANUP + HARNESS_FINALIZE


def _line(report) -> str:
    summary = report.get("summary", {})
    return (
        f"wall={summary.get('wall_ms', {}).get('median')}ms/{WALL_BUDGET_MS}ms "
        f"cpu21={summary.get('process_tree_cpu_total_21_normalized_ms')}ms/"
        f"{CPU_TOTAL_21_BUDGET_MS}ms "
        f"peak={summary.get('peak_rss_kib')}KiB/{PEAK_RSS_BUDGET_KIB}KiB"
    )


def run(ctx) -> list[Evidence]:
    report = run_one_shot_harness(
        ctx.bin_dir,
        budget_ms=WALL_BUDGET_MS,
        cpu_total_21_budget_ms=CPU_TOTAL_21_BUDGET_MS,
        peak_rss_budget_kib=PEAK_RSS_BUDGET_KIB,
    )
    if not report["measurement_accepted"]:
        evidence = []
        if report["correctness_failures"]:
            evidence.append(
                Evidence(
                    "turn_one_shot_correctness",
                    FAIL,
                    "failures=" + " | ".join(report["correctness_failures"]),
                )
            )
        evidence.append(
            Evidence(
                "turn_one_shot_measurement",
                ENV_BLOCKED,
                "measurement_accepted=false reasons="
                + " | ".join(report["measurement_reasons"]),
            )
        )
        return evidence

    scratch = ctx.write_artefact(
        "turn-one-shot-report.json", json.dumps(report, indent=2, sort_keys=True) + "\n"
    )
    artefact = ctx.publish_artefact("turn-one-shot-report.json", scratch)
    if report["failures"]:
        return [
            Evidence(
                "turn_one_shot_budget",
                FAIL,
                _line(report) + " failures=" + " | ".join(report["failures"]),
                [artefact],
            )
        ]
    return [
        Evidence(
            "turn_one_shot_budget",
            PASS,
            _line(report)
            + f" load={report['load_one_minute']} measured=21 fresh_profiles=true",
            [artefact],
        )
    ]
