"""Measurement isolation and explicitly incomplete diagnostic streams."""
from __future__ import annotations

import json
import os
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock

import phase_attribution as attribution
from gate.loader import load_check
import turn_wall_harness as harness
from turnperf_support import ThrowawayProfile


def report(*, blocked=False, failures=()):
    return {
        "measurement_accepted": not blocked,
        "measurement_reasons": ["load start=4.50 is not below 3.00"] if blocked else [],
        "correctness_failures": list(failures), "budget_failures": [],
        "failures": list(failures), "passed": not blocked and not failures,
        "parameters": {"phase_attribution": False},
    }


def trace(root, *, count=1, dropped=0, schema=4):
    header = {"schema": schema, "pid": 1, "records": count, "dropped": dropped,
              "clock": "CLOCK_MONOTONIC", "cpu_clock": "CLOCK_THREAD_CPUTIME_ID"}
    row = {"phase": "rpc", "start_ns": 110, "end_ns": 120,
           "cpu_ns": 0, "waiting": False, "counters": {}}
    (root / "phase-1.jsonl").write_text(json.dumps(header) + "\n" +
        (json.dumps(row) + "\n") * count)


def boundary():
    return {"phase_boundary": {"start_ns": 100, "end_ns": 200, "cpu_ns": 50,
                              "expected_pids": [1], "reaped_children_cpu_ns": 0}}


class WallTraceCapacityTests(unittest.TestCase):
    def test_wall_arm_and_short_attribution_have_independent_parameters(self):
        measured = report()
        diagnostic = report(blocked=True)
        diagnostic["phase_trace"] = {"status": "complete"}
        diagnostic["summary"] = {"single": {"wall_ms": {"median": 999}}}
        with mock.patch.object(harness, "_run_warm_harness", side_effect=[measured, diagnostic]) as run:
            result = harness.run_harness(Path("/bin"), budget_ms={"single": 56.7})
        wall, attr = run.call_args_list
        self.assertFalse(wall.kwargs["phases"])
        self.assertEqual((wall.kwargs["warmups"], wall.kwargs["measured"]), (5, 25))
        self.assertEqual(wall.kwargs["budget_ms"], {"single": 56.7})
        self.assertTrue(attr.kwargs["phases"])
        self.assertEqual((attr.kwargs["warmups"], attr.kwargs["measured"]), (1, 1))
        self.assertNotIn("budget_ms", attr.kwargs)
        self.assertNotIn("resource_baseline", attr.kwargs)
        self.assertTrue(result["passed"])
        self.assertEqual(result["attribution"]["status"], "complete")
        self.assertNotIn("summary", result)

    def test_wall_daemon_environment_has_no_phase_trace_even_when_parent_does(self):
        # Exercise the actual environment handed to the --ready launcher, which
        # propagates it to its daemon, rather than testing a report flag.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in ("haider", "haiderd"):
                (root / name).touch()
            with mock.patch.dict(os.environ, {attribution.ENV: "parent-trace"}):
                profile = ThrowawayProfile(root, "http://127.0.0.1:1", root=root / "profile")
                captured = []
                def command(args, **kwargs):
                    captured.append(kwargs["env"])
                    return SimpleNamespace(timed_out=False, returncode=0, stderr="")
                with mock.patch("turnperf_support.run_command", side_effect=command):
                    profile.ready({attribution.ENV: None})
                self.assertNotIn(attribution.ENV, captured[0])

    def test_actual_wall_launcher_removes_phase_environment(self):
        class Started(Exception):
            pass
        fake_profile = mock.Mock()
        fake_profile.ready.side_effect = Started
        fake_profile.stop.return_value = SimpleNamespace(returncode=0, stdout='{"outcome":"stopped_cleanly"}')
        with tempfile.TemporaryDirectory() as directory:
            with mock.patch.object(harness, "cpu_accounting_self_check"), \
                 mock.patch.object(harness.tempfile, "mkdtemp", return_value=directory), \
                 mock.patch.object(harness, "FakeProvider"), \
                 mock.patch.object(harness, "ThrowawayProfile", return_value=fake_profile):
                with self.assertRaises(Started):
                    harness.run_harness(Path("/bin"), phases=False)
            self.assertIsNone(fake_profile.ready.call_args.args[0][attribution.ENV])

    def test_gate_explicitly_requests_no_attribution_and_budget_stays_pinned(self):
        check = load_check(Path(__file__).parents[1] / "checks/t1/t1.turn.wall_budget.py", "t1")
        with mock.patch.object(check.module, "run_harness", return_value=report(blocked=True)) as run:
            result = check.run(mock.Mock(bin_dir=Path("/bin")))
        self.assertFalse(run.call_args.kwargs["phases"])
        self.assertEqual(check.budget.milliseconds, 6_142_000)
        self.assertEqual([item.status for item in result], ["ENV_BLOCKED"])

    def test_overload_remains_blocked_with_complete_or_truncated_attribution(self):
        for status in ("complete", "truncated", "unavailable"):
            with self.subTest(status=status):
                measured = report(blocked=True)
                diagnostic = report(blocked=True)
                diagnostic["phase_trace"] = {"status": status, "reason": "record_capacity"}
                with mock.patch.object(harness, "_run_warm_harness", side_effect=[measured, diagnostic]):
                    result = harness.run_harness(Path("/bin"))
                self.assertFalse(result["measurement_accepted"])
                self.assertFalse(result["passed"])
                self.assertEqual(result["failures"], [])

    def test_diagnostic_correctness_failure_is_never_ignored(self):
        diagnostic = report(failures=["provider ledger mismatch"])
        diagnostic["phase_trace"] = {"status": "complete"}
        with mock.patch.object(harness, "_run_warm_harness", side_effect=[report(), diagnostic]):
            result = harness.run_harness(Path("/bin"))
        self.assertFalse(result["passed"])
        self.assertEqual(result["correctness_failures"], ["provider ledger mismatch"])

    def test_no_phases_skips_diagnostic_for_any_measurement_length(self):
        for length in (25, 175_000, 400_000):
            with self.subTest(length=length), mock.patch.object(harness, "_run_warm_harness", return_value=report()) as run:
                result = harness.run_harness(Path("/bin"), measured=length, phases=False)
                run.assert_called_once()
                self.assertFalse(run.call_args.kwargs["phases"])
                self.assertEqual(result["attribution"], {"status": "not_requested"})

    def test_synthetic_long_streams_are_complete_or_typed_truncated(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for total in (175_000, 400_000):
                for dropped in (0, total - 100_000):
                    with self.subTest(total=total, dropped=dropped):
                        trace(root, count=total if not dropped else 100_000, dropped=dropped)
                        result = harness._attribute_warm_rows(root, [boundary()])
                        self.assertEqual(result["status"], "truncated" if dropped else "complete")
                        if dropped:
                            self.assertEqual(result["reason"], "record_capacity")

    def test_missing_old_and_damaged_traces_have_typed_reasons(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.assertEqual(harness._attribute_warm_rows(root, [boundary()])["reason"], "missing_process_traces")
            trace(root, schema=0)
            self.assertEqual(harness._attribute_warm_rows(root, [boundary()])["reason"], "unsupported_schema")
            trace(root)
            path = root / "phase-1.jsonl"
            lines = path.read_text().splitlines()
            path.write_text(lines[0] + "\n")
            self.assertEqual(harness._attribute_warm_rows(root, [boundary()])["reason"], "incomplete_trace")
            path.write_text("{invalid json\n")
            self.assertEqual(harness._attribute_warm_rows(root, [boundary()])["reason"], "invalid_trace")

    def test_parser_recursion_is_a_typed_diagnostic_failure(self):
        # The system Python's recursive JSON decoder can hit its stack limit.
        # Use the standard library's Python scanner on every runtime so this
        # exercises a real parse failure even with an iterative C decoder.
        decoder = json.JSONDecoder()
        decoder.scan_once = json.scanner.py_make_scanner(decoder)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for depth in (10, 1500):
                with self.subTest(depth=depth):
                    trace(root)
                    path = root / "phase-1.jsonl"
                    lines = path.read_text().splitlines()
                    lines[0] = (lines[0][:-1] + ',"extension":' +
                                "[" * depth + "0" + "]" * depth + "}")
                    path.write_text("\n".join(lines) + "\n")
                    with mock.patch.object(attribution.json, "loads", side_effect=decoder.decode):
                        result = harness._attribute_warm_rows(root, [boundary()])
                        if depth == 10:
                            self.assertEqual(result["status"], "complete")
                        else:
                            self.assertEqual(result["status"], "unavailable")
                            self.assertEqual(result["reason"], "invalid_trace")
                            with self.assertRaises(attribution.PhaseTraceError) as error:
                                attribution.read_records(root)
                            self.assertEqual(error.exception.reason, "invalid_trace")

    def test_unsupported_clocks_are_unavailable_not_capacity_truncation(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            trace(root)
            path = root / "phase-1.jsonl"
            lines = path.read_text().splitlines()
            header = json.loads(lines[0])
            header["clock"] = "other_clock"
            path.write_text(json.dumps(header) + "\n" + lines[1] + "\n")
            result = harness._attribute_warm_rows(root, [boundary()])
            self.assertEqual(result["status"], "unavailable")
            self.assertEqual(result["reason"], "unsupported_clock")

    def test_legacy_supported_schemas_still_parse(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for schema in (1, 2, 3, 4):
                with self.subTest(schema=schema):
                    trace(root, schema=schema)
                    rows, pids = attribution.read_records(root)
                    self.assertEqual(pids, [1])
                    self.assertEqual(len(rows), 1)

    def test_truncated_diagnostic_does_not_claim_complete_phase_summary(self):
        diagnostic = report()
        diagnostic["phase_trace"] = {"status": "truncated", "reason": "record_capacity"}
        diagnostic["phase_summary"] = None
        with mock.patch.object(harness, "_run_warm_harness", side_effect=[report(), diagnostic]):
            result = harness.run_harness(Path("/bin"))
        self.assertTrue(result["passed"])
        self.assertEqual(result["attribution"]["status"], "truncated")
        self.assertIsNone(result["attribution"]["report"]["phase_summary"])
