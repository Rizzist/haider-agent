from __future__ import annotations

from contextlib import redirect_stderr, redirect_stdout
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


SCRIPT = Path(__file__).resolve().parents[2] / "perf" / "client-footprint-budget.py"
WORKFLOW = Path(__file__).resolve().parents[3] / ".github" / "workflows" / "ship-gate.yml"
SPEC = importlib.util.spec_from_file_location("client_footprint_budget", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError(f"cannot import {SCRIPT}")
client_footprint = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(client_footprint)


class ClientFootprintBudgetTests(unittest.TestCase):
    def test_calibration_requires_exactly_five_runs(self):
        required = [
            "--haider",
            "/tmp/haider",
            "--surface",
            "status-post-command",
            "--output",
            "/tmp/client-footprint",
            "--calibrate",
        ]
        for runs in (None, "4", "6"):
            argv = list(required)
            if runs is not None:
                argv.extend(("--runs", runs))
            with self.subTest(runs=runs), redirect_stderr(io.StringIO()):
                with self.assertRaisesRegex(SystemExit, "2"):
                    client_footprint.parse_args(argv)

        args = client_footprint.parse_args(required + ["--runs", "5"])
        self.assertTrue(args.calibrate)
        self.assertEqual(args.runs, client_footprint.CALIBRATION_RUNS)

    def test_calibration_path_records_runner_median_and_headroom(self):
        footprints = [1_000, 1_100, 1_200, 1_300, 1_400]

        def measured(**_kwargs):
            footprint = footprints[measured.call_count]
            measured.call_count += 1
            return {
                "phys_footprint_bytes": footprint,
                "cpu_total_us": footprint // 10,
                "threads": 1,
                "load_before_read": 0.5,
            }

        measured.call_count = 0
        with tempfile.TemporaryDirectory(dir="/tmp") as directory:
            root = Path(directory)
            haider = root / "haider"
            haider.touch()
            haider.with_name("haiderd").touch()
            output = root / "artefact"
            argv = [
                "--haider",
                str(haider),
                "--surface",
                "status-post-command",
                "--output",
                str(output),
                "--calibrate",
                "--runs",
                "5",
            ]
            ci_environment = {
                "GITHUB_RUN_ID": "33617313643",
                "GITHUB_RUN_ATTEMPT": "2",
                "GITHUB_SHA": "abc123",
                "RUNNER_OS": "macOS",
                "RUNNER_ARCH": "ARM64",
            }
            with (
                mock.patch.object(client_footprint, "DarwinProcessMetrics"),
                mock.patch.object(client_footprint, "wait_for_load", return_value=0.5),
                mock.patch.object(client_footprint, "measure_status", side_effect=measured),
                mock.patch.dict(os.environ, ci_environment, clear=True),
                redirect_stdout(io.StringIO()),
            ):
                self.assertEqual(client_footprint.main(argv), 0)

            summary = json.loads(
                (output / "summary.json").read_text(encoding="utf-8")
            )
            self.assertEqual(summary["mode"], "calibration")
            self.assertEqual(summary["runs"], 5)
            self.assertEqual(summary["phys_footprint_bytes"]["median"], 1_200)
            self.assertEqual(summary["derived_budget_bytes"], 1_320)
            self.assertEqual(
                summary["budget_basis"],
                {
                    "metric": "phys_footprint_bytes.median",
                    "headroom_percent": 10,
                    "formula": "ceil(median * 1.10)",
                },
            )
            self.assertEqual(
                summary["run_context"],
                {
                    "github_run_attempt": "2",
                    "github_run_id": "33617313643",
                    "github_sha": "abc123",
                    "runner_arch": "ARM64",
                    "runner_os": "macOS",
                },
            )
            self.assertEqual(measured.call_count, 5)
            self.assertEqual(
                sorted(path.name for path in output.glob("run-*/sample.json")),
                ["sample.json"] * 5,
            )

    def test_ship_gate_calibrates_all_three_surfaces_at_n_five(self):
        workflow = WORKFLOW.read_text(encoding="utf-8")
        self.assertIn(
            "calibrate and enforce settled client footprint budgets (N=5)", workflow
        )
        self.assertIn("--calibrate", workflow)
        self.assertIn("--runs 5", workflow)
        self.assertIn(
            "calibrate_surface status-post-command status 2938637", workflow
        )
        self.assertIn("calibrate_surface run-post-command run 3794948", workflow)
        self.assertIn(
            "calibrate_surface tui-demo-sixel tui-sixel 6344334", workflow
        )

    def test_registry_44_uses_proc_rusage_without_retrying_vmmap(self):
        source = SCRIPT.read_text(encoding="utf-8")
        self.assertNotIn('"/usr/bin/vmmap"', source)
        self.assertNotIn("diagnostic-allow-missing-vmmap", source)
        self.assertIn("proc_pid_rusage", source)

    def test_tui_cpu_workload_is_exactly_twenty_turns(self):
        required = [
            "--haider",
            "/tmp/haider",
            "--surface",
            "tui-demo-sixel",
            "--output",
            "/tmp/client-footprint",
            "--budget-bytes",
            "1000",
        ]
        args = client_footprint.parse_args(required + ["--tui-turns", "20"])
        self.assertEqual(args.tui_turns, 20)
        for invalid in ("1", "19", "21"):
            with self.subTest(invalid=invalid), redirect_stderr(io.StringIO()):
                with self.assertRaisesRegex(SystemExit, "2"):
                    client_footprint.parse_args(required + ["--tui-turns", invalid])

        headless = list(required)
        headless[3] = "status-post-command"
        with redirect_stderr(io.StringIO()), self.assertRaisesRegex(SystemExit, "2"):
            client_footprint.parse_args(headless + ["--tui-turns", "20"])

    def test_cpu_ticks_are_converted_with_the_mach_timebase(self):
        metrics = object.__new__(client_footprint.DarwinProcessMetrics)
        metrics.timebase_numer = 125
        metrics.timebase_denom = 3
        self.assertEqual(metrics.ticks_to_ns(3), 125)

    def test_cpu_read_is_load_gated_immediately_before_clock_read(self):
        events = []
        metrics = mock.Mock()
        metrics.read.side_effect = lambda pid: events.append(("read", pid)) or {
            "cpu_total_ns": 7
        }
        with mock.patch.object(
            client_footprint,
            "require_load_below",
            side_effect=lambda limit, stage: events.append(("load", limit, stage)),
        ):
            sample = client_footprint.read_cpu_at_calibrated_load(
                metrics, 42, 4.0, "before-test"
            )
        self.assertEqual(sample["cpu_total_ns"], 7)
        self.assertEqual(
            events,
            [("load", 4.0, "before-test"), ("read", 42)],
        )

    def test_tui_turn_deadline_reuses_derived_headless_deadline(self):
        self.assertEqual(
            client_footprint.TUI_TURN_TIMEOUT_SECONDS,
            client_footprint.HEADLESS_TERMINAL_DEADLINE_SECONDS,
        )

    def test_twenty_turn_cpu_summary_reports_median_and_mad(self):
        samples = [
            {
                "phys_footprint_bytes": 1_000 + index,
                "cpu_total_us": 100 + index,
                "turn_20_cpu_ns": value,
            }
            for index, value in enumerate((100, 110, 120, 130, 500))
        ]
        summary = client_footprint.summarize("tui-demo-sixel", samples)
        self.assertEqual(
            summary["turn_20_cpu_ns"],
            {"min": 100, "median": 120, "max": 500, "mad": 10.0},
        )

    def test_hermetic_env_removes_all_proxy_routes_and_pins_loopback_bypass(self):
        inherited = {
            "HTTP_PROXY": "http://upper-http.invalid",
            "HTTPS_PROXY": "http://upper-https.invalid",
            "ALL_PROXY": "socks5://upper-all.invalid",
            "http_proxy": "http://lower-http.invalid",
            "https_proxy": "http://lower-https.invalid",
            "all_proxy": "socks5://lower-all.invalid",
            "NO_PROXY": "inherited.invalid",
            "no_proxy": "inherited.invalid",
        }
        with tempfile.TemporaryDirectory(dir="/tmp") as directory:
            with mock.patch.dict(os.environ, inherited, clear=False):
                env = client_footprint.hermetic_env(Path(directory))
        for key in client_footprint.PROXY_ENV_KEYS:
            self.assertNotIn(key, env)
        self.assertEqual(env["NO_PROXY"], "127.0.0.1,localhost")
        self.assertEqual(env["no_proxy"], "127.0.0.1,localhost")

    def test_stub_is_exact_ipv4_loopback_and_subprocess_reachability_is_recorded(self):
        with tempfile.TemporaryDirectory(dir="/tmp") as directory:
            root = Path(directory)
            artefacts = root / "artefacts"
            artefacts.mkdir()
            fixture = root / "fixture"
            fixture.mkdir()
            env = client_footprint.hermetic_env(fixture)
            stub = client_footprint.openai_stub()
            try:
                self.assertTrue(stub.base_url.startswith("http://127.0.0.1:"))
                client_footprint.verify_stub_reachable(
                    stub.base_url, env, artefacts
                )
                evidence = json.loads(
                    (artefacts / "stub-reachability.json").read_text(encoding="utf-8")
                )
                self.assertEqual(evidence["exit"], 0)
                self.assertEqual(evidence["stdout"].strip(), "200")
                self.assertEqual(
                    [(request["method"], request["path"]) for request in stub.requests],
                    [("GET", "/v1/models")],
                )
            finally:
                stub.close()

    def test_stub_url_guard_rejects_localhost_and_ipv6_spelling(self):
        for url in ("http://localhost:1/v1", "http://[::1]:1/v1"):
            with self.subTest(url=url):
                with self.assertRaisesRegex(RuntimeError, "exact IPv4 loopback"):
                    client_footprint.require_ipv4_loopback_base_url(url)

    def test_exit_before_terminal_keeps_streams_stub_log_and_daemon_logs(self):
        class Stub:
            requests = [
                {"method": "GET", "path": "/v1/models", "body": ""},
                {
                    "method": "POST",
                    "path": "/v1/chat/completions",
                    "body": "{}",
                },
            ]
            chat_count = 1

        with tempfile.TemporaryDirectory(dir="/tmp") as directory:
            root = Path(directory)
            artefacts = root / "artefacts"
            artefacts.mkdir()
            fixture = root / "fixture"
            fixture.mkdir()
            env = client_footprint.hermetic_env(fixture)
            profile = Path(env["HAIDER_PROFILE_DIR"])
            (profile / "daemon.log").write_text("daemon stable tail\n", encoding="utf-8")
            process_logs = profile / "daemon-logs"
            process_logs.mkdir()
            (process_logs / "haiderd-test.log").write_text(
                "daemon process tail\n", encoding="utf-8"
            )
            job_log = io.StringIO()
            with redirect_stderr(job_log):
                client_footprint.persist_run_failure_diagnostics(
                    artefact_dir=artefacts,
                    env=env,
                    stub=Stub(),
                    failure=RuntimeError("headless run exited before its terminal"),
                    child_stdout='{"event":"accepted"}\n',
                    child_stderr="client stderr tail\n",
                    child_exit=1,
                    terminal=None,
                    cleanup_errors=[],
                )
            self.assertEqual(
                (artefacts / "run.stdout").read_text(encoding="utf-8"),
                '{"event":"accepted"}\n',
            )
            self.assertEqual(
                (artefacts / "run.stderr").read_text(encoding="utf-8"),
                "client stderr tail\n",
            )
            stub_log = json.loads(
                (artefacts / "stub-requests.json").read_text(encoding="utf-8")
            )
            self.assertEqual(stub_log["request_count"], 2)
            self.assertEqual(stub_log["chat_request_count"], 1)
            failure = json.loads(
                (artefacts / "failure.json").read_text(encoding="utf-8")
            )
            self.assertFalse(failure["terminal_seen"])
            self.assertIsNone(failure["terminal_kind"])
            self.assertEqual(
                sorted(path.name for path in (artefacts / "daemon-logs").glob("*.log")),
                ["daemon.log", "haiderd-test.log"],
            )
            excerpt = job_log.getvalue()
            self.assertIn("stub_requests=2 chat_requests=1", excerpt)
            self.assertIn("child stderr tail", excerpt)
            self.assertIn("daemon process tail", excerpt)

    def test_measure_run_exit_before_terminal_wires_the_diagnostics_path(self):
        with tempfile.TemporaryDirectory(dir="/tmp") as directory:
            root = Path(directory)
            fixture = root / "fixture"
            fixture.mkdir()
            artefacts = root / "artefacts"
            artefacts.mkdir()
            env = client_footprint.hermetic_env(fixture)
            profile = Path(env["HAIDER_PROFILE_DIR"])
            (profile / "daemon.log").write_text(
                "integration daemon tail\n", encoding="utf-8"
            )
            with (
                mock.patch.object(client_footprint, "ensure_profile_daemon_ready"),
                mock.patch.object(client_footprint, "stop_profile_daemon"),
            ):
                with self.assertRaisesRegex(RuntimeError, "before its terminal"):
                    client_footprint.measure_run(
                        haider=Path(sys.executable),
                        env=env,
                        settle_seconds=1,
                        load_limit=100,
                        load_wait_seconds=1,
                        metrics=mock.Mock(),
                        artefact_dir=artefacts,
                    )
            failure = json.loads(
                (artefacts / "failure.json").read_text(encoding="utf-8")
            )
            self.assertFalse(failure["terminal_seen"])
            self.assertIn("before its terminal", failure["error"])
            self.assertIn("can't open file", (artefacts / "run.stderr").read_text())
            stub_log = json.loads(
                (artefacts / "stub-requests.json").read_text(encoding="utf-8")
            )
            self.assertEqual(stub_log["request_count"], 1)
            self.assertEqual(stub_log["chat_request_count"], 0)
            self.assertEqual(
                (artefacts / "daemon-logs" / "daemon.log").read_text(),
                "integration daemon tail\n",
            )

    def test_coalesced_jsonl_write_exposes_terminal_without_text_buffer_race(self):
        ordinary = {"event": "accepted", "payload": {"type": "accepted"}}
        terminal = {
            "seq": 24,
            "payload": {"type": "run_state", "terminal_kind": "success"},
        }
        wire = "".join(json.dumps(item) + "\n" for item in (ordinary, terminal))
        child = subprocess.Popen(
            [
                sys.executable,
                "-I",
                "-c",
                "import os, sys; os.write(1, sys.argv[1].encode())",
                wire,
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            bufsize=0,
            start_new_session=True,
        )
        stdout_parts: list[bytes] = []
        observed = client_footprint.read_headless_terminal(child, stdout_parts)
        child.wait(timeout=5)
        assert child.stdout is not None
        child.stdout.close()
        self.assertEqual(observed, terminal)
        self.assertEqual(b"".join(stdout_parts).decode(), wire)

    def test_provider_error_terminal_is_seen_but_surface_still_fails(self):
        terminal = {
            "seq": 24,
            "payload": {
                "type": "run_state",
                "terminal_kind": "failure",
                "error_code": "provider_error",
            },
        }
        with self.assertRaisesRegex(RuntimeError, "surface requires success"):
            client_footprint.require_successful_headless_terminal(terminal)

    def test_headless_deadline_is_twice_the_45_second_product_timeout(self):
        self.assertEqual(client_footprint.HEADLESS_RUN_TIMEOUT_SECONDS, 45)
        self.assertEqual(client_footprint.HEADLESS_TERMINAL_DEADLINE_SECONDS, 90)


if __name__ == "__main__":
    unittest.main()
