from __future__ import annotations

import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

from turn_wall_harness import (
    ONE_SHOT_MEASURED,
    ONE_SHOT_WARMUPS,
    _abba,
    _arguments,
    _lifecycle_trace_records,
    _one_shot_parameter_reasons,
    _stage_summary,
    _trace_records,
)
from turnperf_sigkill_matrix import (
    _assert_tool_effect_result_bounds,
    _validate_probe_receipt,
    _validate_recovered_jsonl,
)
from gate.loader import load_check
from turnperf_support import (
    ProofError,
    ProxyState,
    _select_exec_tool,
    assert_provider_ledger,
    median_mad,
    tool_effect_count,
)


class TurnPerformanceHarnessTests(unittest.TestCase):
    def test_abba_has_exact_per_shape_cardinality(self):
        self.assertEqual(
            _abba(4),
            ["single", "tool", "tool", "single"] * 2,
        )
        self.assertEqual(_abba(25).count("single"), 25)
        self.assertEqual(_abba(25).count("tool"), 25)

    def test_median_and_mad_are_untrimmed(self):
        self.assertEqual(median_mad([1.0, 2.0, 100.0]), (2.0, 1.0))

    def test_proxy_case_reset_requires_zero_active_handlers(self):
        with tempfile.TemporaryDirectory(dir="/tmp") as directory:
            state = ProxyState(Path(directory) / "ledger.jsonl")
            state.enter()
            with self.assertRaisesRegex(ProofError, "zero active handlers"):
                state.begin_case("single")
            state.leave()
            state.begin_case("single")
            state.record({"model": "turnperf-model", "messages": []}, "/v1/chat")
            assert_provider_ledger(state.snapshot_case(), "single")
            self.assertEqual(state.read_disk_ledger(), state.snapshot_all())

    def test_absent_provider_ledger_is_an_empty_external_log(self):
        with tempfile.TemporaryDirectory(dir="/tmp") as directory:
            state = ProxyState(Path(directory) / "ledger.jsonl")
            self.assertEqual(state.read_disk_ledger(), [])

    def test_tool_effect_counter_finds_auto_hermetic_sandbox(self):
        with tempfile.TemporaryDirectory(dir="/tmp") as directory:
            root = Path(directory)
            nested = root / "h" / ".haider" / "lockdown" / "digest"
            nested.mkdir(parents=True)
            (nested / "turnperf-effect-7.txt").write_text(
                "turnperf-effect-7\n", encoding="utf-8"
            )
            with self.assertRaisesRegex(ProofError, "non-monotonic fs_write"):
                tool_effect_count(root, 7)

    def test_tool_shape_refuses_non_monotonic_fs_write_fallback(self):
        body = {
            "tools": [
                {
                    "function": {
                        "name": "fs_write",
                        "parameters": {
                            "type": "object",
                            "properties": {"path": {}, "content": {}},
                        },
                    }
                }
            ]
        }
        with self.assertRaisesRegex(ProofError, "monotonic process-exec"):
            _select_exec_tool(body, "turnperf-effect-1")

    def test_tool_shape_prefers_local_process_over_ssh_shell(self):
        body = {
            "tools": [
                {
                    "function": {
                        "name": "ssh_shell",
                        "parameters": {
                            "type": "object",
                            "properties": {"profile": {}, "command": {}},
                            "required": ["profile", "command"],
                        },
                    }
                },
                {
                    "function": {
                        "name": "process_exec",
                        "parameters": {
                            "type": "object",
                            "properties": {"command": {}},
                            "required": ["command"],
                        },
                    }
                },
            ]
        }
        name, arguments = _select_exec_tool(body, "turnperf-effect-1")
        self.assertEqual(name, "process_exec")
        self.assertEqual(set(arguments), {"command"})

    def test_warm_ci_budget_uses_the_accepted_release_pins(self):
        path = (
            Path(__file__).parents[1]
            / "checks"
            / "t1"
            / "t1.turn.wall_budget.py"
        )
        check = load_check(path, "t1")
        self.assertEqual(check.module.WALL_BUDGET_MS, {"single": 56.7, "tool": 78.0})
        for baseline in check.module.BASELINE.values():
            self.assertGreater(baseline["combined_cpu_mad_ms"], 0)
            self.assertEqual(baseline["combined_peak_rss_tolerance_kib"], 64.0)

    def test_overload_suppresses_timing_failure_but_not_correctness_failure(self):
        path = (
            Path(__file__).parents[1]
            / "checks"
            / "t1"
            / "t1.turn.wall_budget.py"
        )
        check = load_check(path, "t1")
        rejected = {
            "measurement_accepted": False,
            "measurement_reasons": ["load start=4.50 is not below 4.00"],
            "correctness_failures": [],
            "budget_failures": ["single wall median exceeds budget"],
            "failures": ["single wall median exceeds budget"],
        }
        ctx = mock.Mock(bin_dir=Path("/tmp"))
        with mock.patch.object(check.module, "run_harness", return_value=rejected):
            evidence = check.run(ctx)
        self.assertEqual([item.status for item in evidence], ["ENV_BLOCKED"])

        rejected["correctness_failures"] = ["provider count expected=1 actual=2"]
        with mock.patch.object(check.module, "run_harness", return_value=rejected):
            evidence = check.run(ctx)
        self.assertEqual([item.status for item in evidence], ["FAIL", "ENV_BLOCKED"])

    def test_recovered_jsonl_allows_sequence_order_race_but_no_gap(self):
        accepted = {"event": "accepted", "session_id": "s", "head_seq": 1}
        values = [
            accepted,
            {"seq": 1, "event_id": "e1", "payload": {"type": "session_state"}},
            {
                "seq": 2,
                "event_id": "e2",
                "run_id": "r",
                "payload": {"type": "run_state", "state": "queued"},
            },
            {
                "seq": 4,
                "event_id": "e4",
                "run_id": "r",
                "payload": {"type": "session_state", "state": "idle"},
            },
            {
                "seq": 3,
                "event_id": "e3",
                "run_id": "r",
                "payload": {
                    "type": "run_state",
                    "state": "errored",
                    "terminal_kind": "failure",
                },
            },
        ]
        parsed = _validate_recovered_jsonl(
            "".join(json.dumps(value) + "\n" for value in values), "single"
        )
        self.assertEqual(parsed["run_id"], "r")
        self.assertEqual(parsed["terminal"]["seq"], 3)

    def test_tool_effect_can_precede_parked_recovery_without_tool_result(self):
        _assert_tool_effect_result_bounds(1, 0)
        _assert_tool_effect_result_bounds(1, 1)
        with self.assertRaisesRegex(ProofError, "at-most-once"):
            _assert_tool_effect_result_bounds(2, 1)
        with self.assertRaisesRegex(ProofError, "at-most-once"):
            _assert_tool_effect_result_bounds(0, 1)

    def test_probe_receipt_requires_exact_retry_pending_replacement(self):
        receipt = {
            "schema": "haider.session_recovery.v1",
            "session_id": "s",
            "menu_id": "m",
            "chosen_option": "probe",
            "resolution_seq": 9,
            "completed": True,
            "resulting_run_state": "effect_unknown",
            "replacement_menu_id": "m-probe-9",
        }
        self.assertEqual(_validate_probe_receipt(receipt, "s"), ("m", "m-probe-9"))
        receipt["replacement_menu_id"] = "m-probe-8"
        with self.assertRaisesRegex(ProofError, "retry-pending"):
            _validate_probe_receipt(receipt, "s")

    def test_trace_parser_keeps_only_numeric_allowlist(self):
        records = _trace_records(
            "haider: trace level=TRACE target=haider.turn phase=provider_open "
            "operation_micros=7 turn_ordinal=2 request_ordinal=1 txn_ordinal=0 "
            "start_us_from_accept=10 end_us_from_accept=17 prompt=secret\n"
        )
        self.assertEqual(len(records), 1)
        self.assertEqual(records[0]["operation_micros"], 7)
        self.assertNotIn("prompt", records[0])

    def test_one_shot_proof_requires_twenty_one_samples_and_five_warmups(self):
        self.assertEqual(_one_shot_parameter_reasons(5, 21, 3.0), [])
        reasons = _one_shot_parameter_reasons(4, 20, 3.0)
        self.assertEqual(len(reasons), 2)
        self.assertIn("warmups=4", reasons[0])
        self.assertIn("measured=20", reasons[1])

    def test_one_shot_lifecycle_trace_accepts_stage_boundaries_only(self):
        records = _lifecycle_trace_records(
            "haider: trace level=TRACE target=haider.lifecycle phase=spawn_ready "
            "operation_micros=31415 unix_micros=123456 secret=nope\n"
            "haider: trace level=TRACE target=haider.lifecycle phase=client_accepted_seen "
            "operation_micros=2718 unix_micros=126174 secret=nope\n"
            "haider: trace level=TRACE target=haider.lifecycle phase=other "
            "operation_micros=1 unix_micros=2\n"
        )
        self.assertEqual(
            records,
            [
                {
                    "level": "TRACE",
                    "target": "haider.lifecycle",
                    "phase": "spawn_ready",
                    "operation_micros": 31415,
                    "unix_micros": 123456,
                },
                {
                    "level": "TRACE",
                    "target": "haider.lifecycle",
                    "phase": "client_accepted_seen",
                    "operation_micros": 2718,
                    "unix_micros": 126174,
                },
            ],
        )

    def test_one_shot_ci_gate_enforces_all_accepted_release_pins(self):
        path = (
            Path(__file__).parents[1]
            / "checks"
            / "t1"
            / "t1.turn.one_shot_budget.py"
        )
        check = load_check(path, "t1")
        self.assertEqual(check.module.WALL_BUDGET_MS, 124.0)
        self.assertEqual(check.module.CPU_TOTAL_21_BUDGET_MS, 1_059.0)
        self.assertEqual(check.module.PEAK_RSS_BUDGET_KIB, 51.2 * 1_024)

    def test_one_shot_cli_defaults_to_proof_sample_counts(self):
        args = _arguments(["--bin-dir", "/tmp/bin", "--one-shot"])
        warmups = ONE_SHOT_WARMUPS if args.warmups is None else args.warmups
        measured = ONE_SHOT_MEASURED if args.measured is None else args.measured
        self.assertEqual((warmups, measured), (5, 21))

    def test_one_shot_stage_summary_reports_untrimmed_median_and_mad(self):
        self.assertEqual(
            _stage_summary([1.0, 2.0, 100.0]),
            {"count": 3, "median": 2.0, "mad": 1.0},
        )


if __name__ == "__main__":
    unittest.main()
