from __future__ import annotations

import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

from turn_wall_harness import (
    _abba,
    _build_phase_table,
    _client_process_residual_micros,
    _contains_turn_trace,
    _interval_union_micros,
    _render_phase_table,
    _trace_record_error,
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

    def test_ci_budget_is_exactly_baseline_times_one_point_one(self):
        path = (
            Path(__file__).parents[1]
            / "checks"
            / "t1"
            / "t1.turn.wall_budget.py"
        )
        check = load_check(path, "t1")
        for shape, baseline in check.module.BASELINE.items():
            self.assertEqual(
                check.module.WALL_BUDGET_MS[shape], baseline["wall_ms"] * 1.10
            )
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

    def test_trace_parser_separates_daemon_accept_and_client_exec_clocks(self):
        records = _trace_records(
            "haider: trace level=TRACE target=haider.turn phase=provider_open "
            "operation_micros=7 turn_ordinal=2 request_ordinal=1 txn_ordinal=0 "
            "start_us_from_accept=10 end_us_from_accept=17\n"
            "haider: trace level=TRACE target=haider.turn side=client clock=client_exec "
            "phase=exit operation_micros=4 turn_ordinal=2 request_ordinal=0 txn_ordinal=0 "
            "start_us_from_exec=30 end_us_from_exec=34 path=secret\n"
        )
        self.assertEqual(
            [(record["side"], record["clock"]) for record in records],
            [("daemon", "daemon_accept"), ("client", "client_exec")],
        )
        self.assertNotIn("path", records[1])

    def test_trace_off_detector_rejects_any_turn_target_line(self):
        self.assertFalse(_contains_turn_trace("ordinary stderr\n"))
        self.assertTrue(
            _contains_turn_trace("level=TRACE target=haider.turn phase=accept\n")
        )

    def test_phase_table_sums_records_per_turn_before_median(self):
        samples = {
            "single": [
                {"turn_ordinal": 1, "wall_ms": 1.0},
                {"turn_ordinal": 2, "wall_ms": 1.0},
            ],
            "tool": [],
        }

        def record(turn: int, start: int, end: int) -> dict[str, int | str]:
            return {
                "side": "daemon",
                "clock": "daemon_accept",
                "phase": "read_bundle",
                "turn_ordinal": turn,
                "request_ordinal": 0,
                "txn_ordinal": 0,
                "operation_micros": end - start,
                "start_us_from_accept": start,
                "end_us_from_accept": end,
            }

        table = _build_phase_table(
            samples,
            {
                1: [record(1, 0, 5), record(1, 5, 12)],
                2: [record(2, 0, 20)],
            },
        )
        row = next(row for row in table["single"] if row["phase"] == "read_bundle")
        self.assertEqual(row["records_per_present_turn"], 1.5)
        self.assertEqual(row["operation_micros_per_turn"], {"median": 16.0, "mad": 4.0})
        rendered = _render_phase_table(table)
        self.assertIn("per-turn sums", rendered)
        self.assertIn("read_bundle", rendered)

    def test_trace_record_validation_rejects_clock_and_duration_mismatch(self):
        valid = {
            "side": "daemon",
            "clock": "daemon_accept",
            "phase": "lockdown",
            "turn_ordinal": 9,
            "request_ordinal": 0,
            "txn_ordinal": 0,
            "operation_micros": 7,
            "start_us_from_accept": 10,
            "end_us_from_accept": 17,
        }
        self.assertIsNone(_trace_record_error(valid))
        wrong_clock = dict(valid, side="client")
        self.assertIn("side/clock", _trace_record_error(wrong_clock) or "")
        wrong_duration = dict(valid, operation_micros=8)
        self.assertIn("does not match", _trace_record_error(wrong_duration) or "")
        negative_ordinal = dict(valid, request_ordinal=-1)
        self.assertIn("request_ordinal", _trace_record_error(negative_ordinal) or "")

    def test_phase_table_splits_setup_and_request_prompt_assembly(self):
        def prompt(request: int, start: int, end: int) -> dict[str, int | str]:
            return {
                "side": "daemon",
                "clock": "daemon_accept",
                "phase": "prompt_assembly",
                "turn_ordinal": 1,
                "request_ordinal": request,
                "txn_ordinal": 0,
                "operation_micros": end - start,
                "start_us_from_accept": start,
                "end_us_from_accept": end,
            }

        table = _build_phase_table(
            {"single": [{"turn_ordinal": 1, "wall_ms": 1.0}], "tool": []},
            {1: [prompt(0, 0, 3), prompt(1, 4, 9)]},
        )
        rows = {row["phase"]: row for row in table["single"]}
        self.assertEqual(
            rows["prompt_assembly.setup"]["operation_micros_per_turn"]["median"],
            3.0,
        )
        self.assertEqual(
            rows["prompt_assembly.request"]["operation_micros_per_turn"]["median"],
            5.0,
        )

    def test_provider_start_residual_uses_interval_union_and_excludes_async_hooks(self):
        def daemon(
            phase: str, start: int, end: int, request: int = 0
        ) -> dict[str, int | str]:
            return {
                "side": "daemon",
                "clock": "daemon_accept",
                "phase": phase,
                "turn_ordinal": 1,
                "request_ordinal": request,
                "txn_ordinal": 0,
                "operation_micros": end - start,
                "start_us_from_accept": start,
                "end_us_from_accept": end,
            }

        records = [
            daemon("accept", 0, 3),
            daemon("read_bundle", 2, 7),
            daemon("hooks_discovery", 7, 10),
            daemon("provider_open", 10, 20, 1),
        ]
        self.assertEqual(_interval_union_micros([(0, 3), (2, 7)], 10), 7)
        table = _build_phase_table(
            {"single": [{"turn_ordinal": 1, "wall_ms": 1.0}], "tool": []},
            {1: records},
        )
        rows = {row["phase"]: row for row in table["single"]}
        self.assertEqual(
            rows["daemon_accept_to_provider_open_start"]["operation_micros_per_turn"][
                "median"
            ],
            10.0,
        )
        self.assertEqual(
            rows["daemon_unattributed_to_provider_open_start"][
                "operation_micros_per_turn"
            ]["median"],
            3.0,
        )

    def test_client_process_residual_rejects_exit_beyond_process_wall(self):
        self.assertEqual(_client_process_residual_micros(1.0, 900), 100.0)
        with self.assertRaisesRegex(ValueError, "exceeds process wall"):
            _client_process_residual_micros(1.0, 1_001)


if __name__ == "__main__":
    unittest.main()
