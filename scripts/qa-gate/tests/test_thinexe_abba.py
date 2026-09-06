from __future__ import annotations

import argparse
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock

import thinexe_abba as thin
import turnperf_support as support


def row():
    return {"wall_ms": 1.0, "client_cpu_ms": 0.5,
            "client_sampled_cpu_lower_bound_ms": 0.4, "client_cpu_sample_count": 3,
            "process_tree_cpu_ms": 10.0, "client_peak_rss_kib": 1000}


def harness(kind):
    return {"passed": True, "measurement_accepted": True, "measurement_reasons": [],
            "samples": {"single": [row()], "tool": [row()]} if kind == "warm" else [row()]}


def conformance(failed=False):
    return {"overall": "FAIL" if failed else "PASS",
            "measurement": {"accepted": not failed},
            "cases": [{"name": str(index), "status": "FAIL" if failed and index == 0 else "PASS",
                       "evidence": {"harness_wall_ms": 5, "cpu_total_ms": 2, "peak_rss_bytes": 1024}}
                      for index in range(21)]}


class ThinexeProofTests(unittest.TestCase):
    def test_dependency_gate_denies_transitive_payload_and_decoder_crates(self):
        names = thin.dependency_names("haider-cli v0.0.969 (/repo/cli)\nimage v0.25.9\nhaider-stt v0.0.969 (*)\n")
        self.assertEqual(thin.forbidden_dependencies(names), ["haider-stt", "image"])
        self.assertEqual(thin.forbidden_dependencies({"haider-cli", "haider-protocol", "serde"}), [])
        with self.assertRaises(support.ProofError):
            thin.dependency_names("")

    def test_boundary_uses_only_normal_edges_and_detects_linked_code(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / ("haider.exe" if thin.os.name == "nt" else "haider")
            binary.write_bytes(b"missing sibling haider-tui; install both executables")
            graph = SimpleNamespace(returncode=0, stdout="haider-cli v0.0.969\nserde v1.0.0\n", stderr="")
            with mock.patch.object(thin.subprocess, "run", return_value=graph) as run, mock.patch.object(thin.shutil, "which", return_value=None):
                self.assertTrue(thin.boundary_report(Path(directory))["passed"])
                argv = run.call_args.args[0]
                self.assertEqual(argv[argv.index("--edges") + 1], "normal")
                self.assertEqual(argv[argv.index("--target") + 1], "all")
                self.assertNotIn("--all-features", argv)
                binary.write_bytes(b"_ZN11haider_core10image_path")
                report = thin.boundary_report(Path(directory))
                self.assertFalse(report["passed"])
                self.assertTrue(any("linked code marker" in item for item in report["failures"]))

    def test_overload_and_builds_block_before_samples(self):
        with mock.patch.object(thin, "load_one_minute", return_value=thin.LOAD_LIMIT):
            with self.assertRaises(thin.EnvironmentBlocked):
                thin.require_quiet_host()
        with mock.patch.object(thin, "load_one_minute", return_value=0), mock.patch.object(
            thin.subprocess, "run", return_value=SimpleNamespace(returncode=0, stdout="/bin/rustc\n/bin/python3\n")
        ):
            with self.assertRaisesRegex(thin.EnvironmentBlocked, "rustc"):
                thin.require_quiet_host()

    def test_one_shot_client_cpu_is_not_renamed_tree_cpu(self):
        metrics = thin.sample_metrics("one_shot", harness("one_shot"))
        self.assertEqual(metrics["one_shot/process_tree_cpu_ms"], [10.0])
        self.assertEqual(metrics["one_shot/client_sampled_cpu_lower_bound_ms"], [0.4])
        self.assertAlmostEqual(metrics["one_shot/client_sampled_cpu_lower_bound_total_21_normalized_ms"][0], 8.4)
        missing = harness("one_shot")
        missing["samples"][0]["client_sampled_cpu_lower_bound_ms"] = None
        with self.assertRaises(thin.EnvironmentBlocked):
            thin.require_client_rows("one_shot", missing)

    def test_process_inventory_permission_denial_blocks_before_measurement(self):
        with mock.patch.object(thin, "load_one_minute", return_value=0), mock.patch.object(
            thin.subprocess, "run", side_effect=PermissionError(1, "Operation not permitted", "/bin/ps")
        ):
            with self.assertRaisesRegex(thin.EnvironmentBlocked, "process inventory denied.*Operation not permitted"):
                thin.require_quiet_host()

    def test_conformance_command_and_failure_evidence_remain_unchanged(self):
        argv = thin.conformance_command(Path("/peer"), Path("/bin/haider"), Path("/tmp/report.json"))
        self.assertEqual(argv[1:3], ["-m", "bench.conformance"])
        self.assertEqual(argv[argv.index("--round-index") + 1], "0")
        self.assertEqual(argv[argv.index("--process-timeout") + 1], "15")
        self.assertEqual(argv[argv.index("--proxy-timeout") + 1], "20")
        report = conformance(True)
        thin.require_client_rows("conformance", report)
        self.assertIn("conformance/0/cpu_total_ms", thin.sample_metrics("conformance", report))
        self.assertEqual(report["cases"][0]["status"], "FAIL")
        self.assertFalse(report["measurement"]["accepted"])
        report["cases"].pop()
        with self.assertRaises(support.ProofError):
            thin.require_client_rows("conformance", report)

    def test_comparison_retains_outliers_and_missing_sides(self):
        report = thin.comparisons({"A": {"wall": [1, 2, 100], "missing": [1]}, "B": {"wall": [1, 3, 90]}})
        self.assertEqual(report["wall"]["baseline_median"], 2)
        self.assertEqual(report["wall"]["baseline_max"], 100)
        self.assertEqual(report["wall"]["baseline_total"], 103)
        self.assertFalse(report["missing"]["complete"])

    def test_fixed_abba_runs_every_authority_without_reclassifying_rejection(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            peer = root / "peer"
            for name in ("bench/conformance/__main__.py", "bench/conformance/runner.py",
                         "bench/adapters/haider-agent/adapter.toml", "bench/adapters/normalize.py"):
                path = peer / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("fixture")
            order = []
            def turn(kind):
                def run(path):
                    order.append((path.name, kind))
                    return harness(kind)
                return run
            def run_conformance(_peer, path, output):
                order.append((path.name, "conformance"))
                raw = conformance(True)
                thin.write_json(output, raw)
                return raw, {"passed": False, "measurement_accepted": False,
                             "strict_load_accepted": True, "raw_statuses": {"0": "FAIL"}}
            args = argparse.Namespace(output_dir=root / "results", baseline=root / "A", candidate=root / "B", conformance_root=peer)
            with mock.patch.object(thin, "require_quiet_host", return_value=0), \
                 mock.patch.object(thin, "inventory", return_value={}), \
                 mock.patch.object(thin, "boundary_report", return_value={"passed": True}), \
                 mock.patch.object(thin, "exec_floor", side_effect=turn("exec")), \
                 mock.patch.object(thin, "run_one_shot_harness", side_effect=turn("one_shot")), \
                 mock.patch.object(thin, "run_harness", side_effect=turn("warm")), \
                 mock.patch.object(thin, "run_conformance", side_effect=run_conformance):
                self.assertEqual(thin.measure(args), 1)
            expected = [(label, kind) for label in "ABBA" for kind in ("exec", "one_shot", "warm", "conformance")]
            self.assertEqual(order, expected)
            final = json.loads((args.output_dir / "abba.json").read_text())
            self.assertTrue(final["measurements_complete"])
            self.assertFalse(final["measurement_accepted"])
            self.assertEqual(final["status"], "FAIL")
            self.assertEqual(len(final["runs"]), 16)

    def test_native_sampler_retains_own_cpu_without_relabeling_waited_tree(self):
        class Event:
            checks = 0
            def is_set(self):
                self.checks += 1
                return self.checks > 1
            def set(self):
                pass
            def wait(self, _timeout):
                return False
        class Thread:
            def __init__(self, *, target, **_kwargs):
                self.target = target
            def start(self):
                self.target()
            def join(self, **_kwargs):
                pass
        child = SimpleNamespace(pid=70, returncode=0, communicate=lambda **kwargs: ("ok", ""))
        usages = [SimpleNamespace(ru_utime=1, ru_stime=1), SimpleNamespace(ru_utime=1.02, ru_stime=1.03)]
        resource = SimpleNamespace(RUSAGE_CHILDREN=1, getrusage=mock.Mock(side_effect=usages))
        with mock.patch.object(support.subprocess, "Popen", return_value=child), \
             mock.patch.object(support.threading, "Event", Event), \
             mock.patch.object(support.threading, "Thread", Thread), \
             mock.patch.object(support, "resource", resource), \
             mock.patch.object(support, "_process_usage", return_value=(5.5, 1000, 1200)) as native:
            result = support.run_command(["fake"], env={}, cwd=Path("/tmp"), timeout=1)
        native.assert_called_once_with(70)
        self.assertAlmostEqual(result.cpu_ms, 50)
        self.assertEqual(result.sampled_client_cpu_ms, 5.5)
        self.assertEqual(result.client_cpu_sample_count, 1)
        self.assertEqual(result.child_peak_rss_kib, 1000)

    def test_version_floor_uses_exact_child_rusage_and_platform_rss_units(self):
        def launch(_argv, **kwargs):
            kwargs["stdout"].write(b"haider 0.0.969\n")
            return SimpleNamespace(pid=71, returncode=None)
        with tempfile.TemporaryDirectory() as directory:
            for system, raw_peak in (("darwin", 1_024_000), ("linux", 1_000)):
                usage = SimpleNamespace(ru_utime=0.002, ru_stime=0.001, ru_maxrss=raw_peak)
                with mock.patch.object(thin.subprocess, "Popen", side_effect=launch), \
                     mock.patch.object(thin.os, "wait4", return_value=(71, 0, usage), create=True) as wait, \
                     mock.patch.object(thin.threading, "Timer"), \
                     mock.patch.object(thin.time, "monotonic_ns", side_effect=[1_000_000, 7_000_000]), \
                     mock.patch.object(thin.sys, "platform", system):
                    result = thin.version_sample(Path("/fake/haider"), Path(directory))
                wait.assert_called_once_with(71, 0)
                self.assertEqual(result["client_cpu_ms"], 3)
                self.assertEqual(result["wall_ms"], 6)
                self.assertEqual(result["client_peak_rss_kib"], 1000)


if __name__ == "__main__":
    unittest.main()
