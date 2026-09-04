from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import unittest


SCRIPT = Path(__file__).parents[1] / "daemon-footprint-budget.py"
SPEC = importlib.util.spec_from_file_location("daemon_footprint_budget", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class DaemonFootprintBudgetTests(unittest.TestCase):
    def test_compaction_script_has_one_summary_request_per_interval(self) -> None:
        steps = json.loads(MODULE.fake_script(200, 1, 50))
        self.assertEqual(
            [step["text"] for step in steps if step["step"] == "emit_text"],
            [
                "summary through session 1 turn 50",
                "summary through session 1 turn 100",
                "summary through session 1 turn 150",
                "summary through session 1 turn 200",
            ],
        )
        self.assertEqual(
            sum(step["step"] == "emit_tool_call" for step in steps), 200
        )

    def test_fleet_script_has_unique_calls_for_every_session_turn(self) -> None:
        steps = json.loads(MODULE.fake_script(10, 100, None))
        calls = [
            step["call_id"] for step in steps if step["step"] == "emit_tool_call"
        ]
        self.assertEqual(len(calls), 1_000)
        self.assertEqual(len(set(calls)), 1_000)
        self.assertEqual(calls[0], "memdaemon-1-1")
        self.assertEqual(calls[-1], "memdaemon-100-10")

    def test_median_and_mad_use_the_same_population_center(self) -> None:
        self.assertEqual(MODULE.median_and_mad([10, 11, 12, 100]), (11.5, 1.0))


if __name__ == "__main__":
    unittest.main()
