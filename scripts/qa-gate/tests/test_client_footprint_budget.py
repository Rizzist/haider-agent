from __future__ import annotations

from contextlib import redirect_stderr
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
SPEC = importlib.util.spec_from_file_location("client_footprint_budget", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError(f"cannot import {SCRIPT}")
client_footprint = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(client_footprint)


class ClientFootprintBudgetTests(unittest.TestCase):
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
