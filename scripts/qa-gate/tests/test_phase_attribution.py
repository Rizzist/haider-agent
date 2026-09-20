from __future__ import annotations
import json
from pathlib import Path
import tempfile
import unittest

from phase_attribution import partition, read_records, summarize


def record(phase, start, end, cpu=0, waiting=False):
    return {"phase": phase, "start_ns": start, "end_ns": end,
            "cpu_ns": cpu, "waiting": waiting, "pid": 1}


class PhaseAttributionTests(unittest.TestCase):
    def build(self, records, **kwargs):
        return partition(records, start_ns=100, end_ns=200, cpu_ns=50,
                         expected_pids=[1], available_pids=[1], **kwargs)

    def test_overlap_waits_and_residual_reconcile_exactly(self):
        result = self.build([record("rpc", 110, 190, waiting=True),
                             record("store_journal", 120, 150, 20),
                             record("projection_digest", 125, 130, 5)])
        self.assertEqual(result["detail"]["rpc"]["wall_ns"], 50)
        self.assertEqual(result["detail"]["store_journal"]["wall_ns"], 25)
        self.assertEqual(result["detail"]["projection_digest"]["wall_ns"], 5)
        self.assertEqual(result["residual"], {"wall_ns": 20, "cpu_ns": 25})
        for key in ("wall_ns", "cpu_ns"):
            self.assertEqual(sum(row[key] or 0 for row in result["phases"].values())
                             + result["residual"][key], result["total"][key])

    def test_cpu_overcount_is_error_never_clamped(self):
        with self.assertRaisesRegex(ValueError, "exceeds independent total"):
            self.build([record("store_journal", 110, 190, 51)])

    def test_boundary_cpu_is_not_prorated_and_other_processes_are_excluded(self):
        other = {**record("store_journal", 110, 190, 50), "pid": 2}
        result = self.build([record("rpc", 90, 150, 30), other])
        self.assertEqual(result["boundary_cpu_records"], 1)
        self.assertEqual(result["residual"]["cpu_ns"], 50)
        self.assertEqual(result["residual"]["wall_ns"], 50)

    def test_cold_unknown_loader_is_not_a_fake_zero(self):
        result = self.build([record("rpc", 105, 106), record("provider_assembly", 110, 150, 10)], cold=True)
        self.assertIsNone(result["phases"]["dynamic_link"]["cpu_ns"])
        self.assertEqual(result["phases"]["first_request"]["cpu_ns"], 10)
        self.assertEqual(result["residual"], {"wall_ns": 59, "cpu_ns": 40})

    def test_cold_startup_mutations_are_not_called_first_request(self):
        result = self.build([record("store_journal", 105, 110, 5),
                             record("rpc", 120, 130, 5),
                             record("teardown", 180, 190, 5),
                             record("completion_render", 190, 195, 5)], cold=True)
        self.assertEqual(result["phases"]["first_request"]["cpu_ns"], 5)
        self.assertEqual(result["excluded_cold_request_records"], 2)
        self.assertEqual(len(result["records"]), 4)

    def test_lockdown_binding_is_visible_in_cold_and_warm_attribution(self):
        records = [record("rpc", 105, 190, waiting=True),
                   record("turn_setup", 110, 180, 20),
                   record("lockdown_bind_activate", 130, 145, 7)]
        warm = self.build(records)
        cold = self.build(records, cold=True)

        self.assertEqual(warm["detail"]["lockdown_bind_activate"]["wall_ns"], 15)
        self.assertEqual(cold["phases"]["lockdown_bind_activate"]["wall_ns"], 15)
        self.assertEqual(cold["phases"]["lockdown_bind_activate"]["cpu_ns"], 7)

    def test_independent_reaped_cpu_is_separate_from_thread_scopes(self):
        result = self.build([record("tool_dispatch", 110, 150, 20)], reaped_children_cpu_ns=15)
        self.assertEqual(result["phases"]["daemon_reaped_children"]["cpu_ns"], 15)
        self.assertEqual(result["phases"]["daemon_reaped_children"]["wall_ns"], 0)
        self.assertEqual(result["residual"]["cpu_ns"], 15)

    def test_missing_binary_trace_is_visible(self):
        result = partition([], start_ns=100, end_ns=200, cpu_ns=50,
                           expected_pids=[1, 2], available_pids=[1])
        self.assertEqual(result["status"], "missing_process_traces")
        self.assertEqual(result["missing_pids"], [2])
        self.assertEqual(result["residual"], result["total"])

    def test_pooled_summary_adds_even_when_phase_medians_would_not(self):
        maps = [self.build([record("rpc", 100, 180, 40)]),
                self.build([record("store_journal", 120, 200, 35)])]
        summary = summarize(maps)
        for key in ("wall_ns", "cpu_ns"):
            self.assertEqual(sum(row[key] or 0 for row in summary["phases"].values())
                             + summary["residual"][key], summary["total"][key])

    def test_cas_counters_and_nested_wall_are_attributed_per_turn(self):
        cas = record("cas_reverify", 120, 150, 15)
        cas["counters"] = {
            "bytes_read": 131_072,
            "blocks_hashed": 2,
            "reverify_calls": 3,
        }
        result = self.build([record("store_access", 110, 180, 20), cas])
        self.assertEqual(result["detail"]["cas_reverify"]["wall_ns"], 30)
        self.assertEqual(result["detail"]["store_access"]["wall_ns"], 40)
        self.assertEqual(result["detail"]["cas_reverify"]["bytes_read"], 131_072)
        self.assertEqual(result["detail"]["cas_reverify"]["blocks_hashed"], 2)
        self.assertEqual(result["detail"]["cas_reverify"]["reverify_calls"], 3)

    def test_reader_rejects_dropped_records_and_cpu_on_waits(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / "phase-1.jsonl"
            header = {"schema": 1, "pid": 1, "dropped": 1, "records": 0, "clock": "CLOCK_MONOTONIC",
                      "cpu_clock": "CLOCK_THREAD_CPUTIME_ID"}
            path.write_text(json.dumps(header) + "\n")
            with self.assertRaisesRegex(ValueError, "truncated"):
                read_records(root)
            header["dropped"] = 0
            header["records"] = 1
            path.write_text(json.dumps(header) + "\n")
            with self.assertRaisesRegex(ValueError, "truncated"):
                read_records(root)
            row = record("rpc", 1, 2, 1, True)
            row.pop("pid")
            path.write_text(json.dumps(header) + "\n" + json.dumps(row) + "\n")
            with self.assertRaisesRegex(ValueError, "wait interval charged CPU"):
                read_records(root)

    def test_reader_accepts_v1_without_counters_and_requires_them_in_v2(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / "phase-1.jsonl"
            header = {"schema": 1, "pid": 1, "dropped": 0, "records": 1,
                      "clock": "CLOCK_MONOTONIC",
                      "cpu_clock": "CLOCK_THREAD_CPUTIME_ID"}
            row = record("rpc", 1, 2)
            row.pop("pid")
            path.write_text(json.dumps(header) + "\n" + json.dumps(row) + "\n")
            records, _ = read_records(root)
            self.assertEqual(records[0]["counters"], dict.fromkeys(
                ("bytes_read", "blocks_hashed", "reverify_calls"), 0))
            header["schema"] = 2
            path.write_text(json.dumps(header) + "\n" + json.dumps(row) + "\n")
            with self.assertRaisesRegex(ValueError, "v2 record has no counters"):
                read_records(root)

    def test_json_cli_enables_phases_and_explicit_off_is_available(self):
        from turn_wall_harness import _arguments
        self.assertTrue(_arguments(["--bin-dir", "/tmp/bin"]).phases)
        self.assertFalse(_arguments(["--bin-dir", "/tmp/bin", "--no-phases"]).phases)
