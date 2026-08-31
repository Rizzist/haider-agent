from __future__ import annotations

import json
import copy
import io
import os
from pathlib import Path
import platform
import signal
import tempfile
import textwrap
import unittest
from unittest import mock

from gate.context import (
    CheckContext,
    CommandResult,
    canonical_paths_equal,
    path_is_within,
    status_socket_path_valid,
)
from gate.contract import PASS, ContractError, Evidence, validate_evidence_list
from gate.headless import provider_request_ordinals
from gate.loader import load_check
from gate.report import diff_reports, load_report, validate_report, write_report
import runner


VALID_MODULE = """
from gate import DAEMON_STARTUP, DAEMON_STOP, Evidence, PASS
id = "t0.test.valid"
tier = "t0"
area = "runner"
needs = ("network:none",)
script = [{"step": "finish", "reason": "end_turn"}]
turns_expected = 1
budget = DAEMON_STARTUP + DAEMON_STOP
timed = False
def run(ctx):
    return [Evidence("valid", PASS, "valid evidence")]
"""


class LoaderContractTests(unittest.TestCase):
    def write_module(self, directory: Path, source: str) -> Path:
        path = directory / "check.py"
        path.write_text(textwrap.dedent(source), encoding="utf-8")
        return path

    def test_literal_budget_is_rejected_at_load(self):
        with tempfile.TemporaryDirectory() as directory_text:
            directory = Path(directory_text)
            source = VALID_MODULE.replace(
                "budget = DAEMON_STARTUP + DAEMON_STOP", "budget = 50"
            )
            with self.assertRaisesRegex(ContractError, "literal-only"):
                load_check(self.write_module(directory, source), "t0")

    def test_turns_expected_over_segment_count_is_refused(self):
        with tempfile.TemporaryDirectory() as directory_text:
            directory = Path(directory_text)
            source = VALID_MODULE.replace("turns_expected = 1", "turns_expected = 2")
            with self.assertRaisesRegex(
                ContractError, "turns_expected=2 exceeds segments=1"
            ):
                load_check(self.write_module(directory, source), "t0")

    def test_missing_need_is_env_blocked_and_never_fail(self):
        with tempfile.TemporaryDirectory() as directory_text:
            directory = Path(directory_text)
            source = VALID_MODULE.replace(
                'needs = ("network:none",)',
                'needs = ("fixture:definitely-missing",)\nCALLED = False',
            ).replace(
                'return [Evidence("valid", PASS, "valid evidence")]',
                'global CALLED\n    CALLED = True\n    raise AssertionError("must not run")',
            )
            check = load_check(self.write_module(directory, source), "t0")
            row, versions = runner.execute_check(
                check,
                bin_dir=directory,
                measurement_accepted=True,
            )
            self.assertEqual(row["status"], "ENV_BLOCKED")
            self.assertNotEqual(row["status"], "FAIL")
            self.assertFalse(check.module.CALLED)
            self.assertEqual(versions, set())

    def test_expected_fail_until_is_validated_and_reported_without_rewriting_status(self):
        with tempfile.TemporaryDirectory() as directory_text:
            directory = Path(directory_text)
            source = VALID_MODULE.replace(
                'needs = ("network:none",)',
                'needs = ("fixture:definitely-missing",)\nexpected_fail_until = "0.0.968"',
            )
            check = load_check(self.write_module(directory, source), "t0")
            row, _versions = runner.execute_check(
                check,
                bin_dir=directory,
                measurement_accepted=True,
            )
            self.assertEqual(check.expected_fail_until, "0.0.968")
            self.assertEqual(row["expected_fail_until"], "0.0.968")
            self.assertEqual(row["status"], "ENV_BLOCKED")

    def test_malformed_expected_fail_until_is_rejected_at_load(self):
        with tempfile.TemporaryDirectory() as directory_text:
            directory = Path(directory_text)
            source = VALID_MODULE.replace(
                "timed = False", 'expected_fail_until = "968"\ntimed = False'
            )
            with self.assertRaisesRegex(ContractError, "semantic version"):
                load_check(self.write_module(directory, source), "t0")

    def test_cleanup_exception_becomes_fail_row_and_does_not_escape(self):
        with tempfile.TemporaryDirectory() as directory_text:
            directory = Path(directory_text)
            check = load_check(self.write_module(directory, VALID_MODULE), "t0")

            class BrokenCleanupContext:
                def __init__(self, **_kwargs):
                    self.daemon_versions = set()
                    self.root = directory

                def cleanup(self):
                    raise OSError("cleanup transport unavailable")

                def emergency_cleanup(self):
                    return "no_status_observed_pid"

                def write_artefact(self, _name, _content):
                    return str(directory / "cleanup-error.txt")

                def dispose(self, *, keep):
                    self.kept = keep

            with mock.patch.object(runner, "CheckContext", BrokenCleanupContext):
                row, _versions = runner.execute_check(
                    check,
                    bin_dir=directory,
                    measurement_accepted=True,
                )
            self.assertEqual(row["status"], "FAIL")
            self.assertIn("cleanup_runner_error type=OSError", row["evidence"][-1]["evidence_line"])

    def test_empty_evidence_line_is_runner_contract_error(self):
        with self.assertRaisesRegex(ContractError, "empty evidence_line"):
            validate_evidence_list([Evidence("empty", "PASS", "")])

    def test_shipped_check_budget_sums_cover_every_nested_bound(self):
        expected = {
            "t0.account.alias_selects": 306_000,
            "t0.budget.max_cost_binds_before_request": 252_000,
            "t0.budget.max_tokens_binds": 252_000,
            "t0.daemon.status_stop": 310_000,
            "t0.headless.input_required_is_typed": 118_000,
            "t0.run.exit_codes": 336_000,
            "t0.run.jsonl_contract": 146_000,
            "t0.run.replay_resume_recover": 445_000,
            "t0.sessions.wait_ready_n": 506_000,
        }
        checks = runner.discover_checks(runner.CHECK_ROOT, "t0")
        self.assertEqual(
            {check.id: check.budget.milliseconds for check in checks}, expected
        )
        by_id = {check.id: check for check in checks}
        for check_id in (
            "t0.budget.max_cost_binds_before_request",
            "t0.budget.max_tokens_binds",
        ):
            self.assertEqual(by_id[check_id].segments, 1)
            self.assertEqual(by_id[check_id].turns_expected, 1)

    def test_provider_request_counter_uses_completed_attempt_once(self):
        document = {
            "events": [
                {
                    "payload": {
                        "type": "item",
                        "event": event,
                        "item": {
                            "item": "extension",
                            "kind": "cache_request_attempt_v1",
                            "data": {"ordinal": 1},
                        },
                    }
                }
                for event in ("started", "completed", "completed")
            ]
        }
        self.assertEqual(provider_request_ordinals(document), {1})

    def test_account_stub_stays_small_stdlib_fixture(self):
        source = (runner.HERE / "gate" / "openai_stub.py").read_text(encoding="utf-8")
        self.assertLessEqual(len(source.splitlines()), 150)

    def test_status_check_never_stops_without_trusted_status_pid(self):
        check = load_check(
            runner.CHECK_ROOT / "t0" / "t0.daemon.status_stop.py", "t0"
        )

        class EmptyStatusContext:
            ownership_refused = False
            daemon_pids = set()

            def __init__(self):
                self.calls = []

            def run_haider(self, args, *, timeout):
                del timeout
                self.calls.append(tuple(args))
                if args == ["--version"]:
                    return CommandResult(tuple(args), 0, "haider 0.0.967\n", "", False, 1)
                if args == ["status", "--json"]:
                    return CommandResult(tuple(args), 0, "{}\n", "", False, 1)
                raise AssertionError(f"untrusted check attempted command {args!r}")

            def command_artefact(self, name, result):
                del result
                return f"/tmp/{name}.txt"

        context = EmptyStatusContext()
        evidence = check.run(context)
        self.assertEqual(evidence[0].status, "FAIL")
        self.assertEqual(context.calls, [("--version",), ("status", "--json")])
        self.assertIn("trusted status PID actual=none", evidence[0].evidence_line)


def sample_report(*, current: bool) -> dict:
    statuses = ("PASS", "FAIL", "PASS") if current else ("PASS", "PASS", "PASS")
    walls = (100, 100, 200) if current else (100, 100, 100)
    checks = []
    for index, (status, wall_ms) in enumerate(zip(statuses, walls), start=1):
        checks.append(
            {
                "id": f"t0.test.{index}",
                "area": "runner",
                "status": status,
                "evidence": [
                    {
                        "label": "fixture",
                        "status": status,
                        "evidence_line": f"fixture status={status}",
                        "artefacts": [],
                    }
                ],
                "wall_ms": wall_ms,
                "artefacts": [],
                "timed": True,
                "measurement_accepted": True,
            }
        )
    return {
        "schema": "haider.qa-gate.v1",
        "tier": "t0",
        "created_at_utc": "2026-08-31T00:00:00Z",
        "host": {"hostname": "test-host", "platform": "test-os", "python": "3.11.0"},
        "load": {"one_minute": 0.25, "logical_cpus": 8},
        "measurement_accepted": True,
        "measurement_reasons": [],
        "binary": {
            "path": "/installed/haider",
            "sha256": "a" * 64,
            "version_output": "haider 0.0.967",
            "version": "0.0.967",
        },
        "daemon_binary": {
            "path": "/installed/haiderd",
            "sha256": "b" * 64,
            "version_output": "haiderd 0.0.967",
            "version": "0.0.967",
        },
        "daemon_version": "0.0.967",
        "warmup": {"accepted": True, "wall_ms": 5, "evidence_line": "warmup ok"},
        "checks": checks,
        "summary": {
            "total": 3,
            "pass": statuses.count("PASS"),
            "fail": statuses.count("FAIL"),
            "skip": 0,
            "env_blocked": 0,
        },
    }


class ReportAndPathTests(unittest.TestCase):
    def test_report_validate_write_read_and_diff_round_trip(self):
        previous = sample_report(current=False)
        current = sample_report(current=True)
        validate_report(previous)
        validate_report(current)
        with tempfile.TemporaryDirectory() as directory:
            previous_path = Path(directory) / "previous.json"
            current_path = Path(directory) / "current.json"
            write_report(previous_path, previous)
            write_report(current_path, current)
            self.assertEqual(load_report(previous_path), previous)
            self.assertEqual(load_report(current_path), current)
            lines = diff_reports(load_report(previous_path), load_report(current_path))
        self.assertIn("FLIP t0.test.2 PASS->FAIL", lines)
        self.assertTrue(any(line.startswith("WALL t0.test.3 100->200ms") for line in lines))

    def test_report_validator_rejects_nonfinite_load_bad_utc_and_negative_warmup(self):
        mutations = (
            ("nonfinite load", lambda report: report["load"].__setitem__("one_minute", float("nan"))),
            ("bad UTC", lambda report: report.__setitem__("created_at_utc", "yesterday")),
            ("negative warmup", lambda report: report["warmup"].__setitem__("wall_ms", -1)),
        )
        for label, mutate in mutations:
            with self.subTest(label=label):
                report = copy.deepcopy(sample_report(current=False))
                mutate(report)
                with self.assertRaises(ContractError):
                    validate_report(report)

    def test_report_validator_enforces_measurement_rejection_consistency(self):
        def overload_accepted(report):
            report["load"] = {"one_minute": 9.0, "logical_cpus": 1}

        def rejected_root_with_accepted_rows(report):
            report["measurement_accepted"] = False
            report["measurement_reasons"] = ["busy"]

        def rejection_without_reason(report):
            report["measurement_accepted"] = False
            for check in report["checks"]:
                check["measurement_accepted"] = False

        def failed_warmup_with_accepted_timing(report):
            report["warmup"]["accepted"] = False
            report["warmup"]["evidence_line"] = "warmup failed"

        for label, mutate in (
            ("overload accepted", overload_accepted),
            ("root rejected rows accepted", rejected_root_with_accepted_rows),
            ("rejected without reason", rejection_without_reason),
            ("failed warmup accepted", failed_warmup_with_accepted_timing),
        ):
            with self.subTest(label=label):
                report = copy.deepcopy(sample_report(current=False))
                mutate(report)
                with self.assertRaises(ContractError):
                    validate_report(report)

    def test_report_validator_rejects_relative_binary_path(self):
        report = copy.deepcopy(sample_report(current=False))
        report["binary"]["path"] = "relative/../haider"
        with self.assertRaisesRegex(ContractError, "canonical absolute path"):
            validate_report(report)

    def test_report_validator_rejects_nonstring_expected_fail_until(self):
        report = copy.deepcopy(sample_report(current=False))
        report["checks"][0]["expected_fail_until"] = 968
        with self.assertRaisesRegex(ContractError, "expected_fail_until"):
            validate_report(report)

        report = copy.deepcopy(sample_report(current=False))
        report["checks"][0]["expected_fail_until"] = "968"
        with self.assertRaisesRegex(ContractError, "expected_fail_until"):
            validate_report(report)

    def test_canonical_path_compare_accepts_symlink_alias_and_descendant(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            actual = root / "actual"
            actual.mkdir()
            alias = root / "alias"
            try:
                alias.symlink_to(actual, target_is_directory=True)
            except (OSError, NotImplementedError) as error:
                self.skipTest(f"symlinks unavailable: {error}")
            self.assertTrue(canonical_paths_equal(actual, alias))
            self.assertTrue(path_is_within(alias / "child", actual))
        if platform.system() == "Darwin" and Path("/private/tmp").exists():
            self.assertTrue(canonical_paths_equal("/tmp", "/private/tmp"))

    def test_windows_status_socket_is_named_pipe_not_runtime_descendant(self):
        self.assertTrue(
            status_socket_path_valid(
                r"\\.\pipe\haider-0123456789abcdef",
                r"C:\runtime\profile",
                platform_name="nt",
            )
        )
        self.assertFalse(
            status_socket_path_valid(
                r"C:\runtime\profile\h.sock",
                r"C:\runtime\profile",
                platform_name="nt",
            )
        )

    def test_context_pins_short_hermetic_environment(self):
        previous = os.environ.get("NO_COLOR")
        os.environ["NO_COLOR"] = "1"
        context = None
        try:
            context = CheckContext(
                check_id="t0.test.context",
                bin_dir=Path(tempfile.gettempdir()),
                script=[{"step": "finish", "reason": "end_turn"}],
            )
            self.assertNotIn("NO_COLOR", context.env)
            self.assertEqual(context.env["TERM"], "xterm-256color")
            self.assertEqual(context.env["HAIDER_DISCOVERY_DISABLED"], "1")
            self.assertEqual(context.env["HAIDER_NO_UPDATE_CHECK"], "1")
            self.assertTrue(path_is_within(context.profile_dir, context.root))
            self.assertTrue(path_is_within(context.runtime_root, context.root))
            if os.name != "nt":
                self.assertLessEqual(len(str(context.root).encode()), 64)
        finally:
            if context is not None:
                context.dispose(keep=False)
            if previous is None:
                os.environ.pop("NO_COLOR", None)
            else:
                os.environ["NO_COLOR"] = previous

    def test_spawn_tracking_covers_new_headless_command_families(self):
        self.assertTrue(CheckContext._may_spawn(("account", "add")))
        self.assertTrue(CheckContext._may_spawn(("resume", "session-id")))
        self.assertTrue(CheckContext._may_spawn(("session", "session-id", "--json")))
        self.assertTrue(CheckContext._may_spawn(("sessions", "wait-ready")))
        self.assertFalse(
            CheckContext._may_spawn(("sessions", "wait-ready", "--no-spawn"))
        )

    def test_sigint_helper_arms_on_stdout_and_signals_only_the_client(self):
        context = CheckContext(
            check_id="t0.test.sigint",
            bin_dir=Path(tempfile.gettempdir()),
            script=[{"step": "hang"}],
        )

        class FakeProcess:
            pid = 12345

            def __init__(self):
                self.stdout = io.StringIO('{"state":"streaming"}\n')
                self.stderr = io.StringIO("")
                self.returncode = None
                self.signals = []

            def poll(self):
                return self.returncode

            def send_signal(self, value):
                self.signals.append(value)
                self.returncode = 130

            def wait(self, timeout=None):
                del timeout
                return self.returncode

        process = FakeProcess()
        try:
            with mock.patch("gate.context.subprocess.Popen", return_value=process):
                result = context.interrupt_haider_after_stdout(
                    ["run", "--output", "jsonl"],
                    marker='"state":"streaming"',
                    arm_timeout=runner.DAEMON_STARTUP,
                    terminal_timeout=runner.DAEMON_STARTUP,
                )
            self.assertEqual(result.returncode, 130)
            self.assertFalse(result.timed_out)
            self.assertEqual(process.signals, [signal.SIGINT])
        finally:
            context.dispose(keep=False)

    def test_isolated_subcase_uses_fresh_context_and_returns_cleanup_evidence(self):
        context = CheckContext(
            check_id="t0.test.isolated",
            bin_dir=Path(tempfile.gettempdir()),
            script=[{"step": "finish", "reason": "end_turn"}],
        )
        observed = {}

        def fake_run(child, args, *, timeout):
            observed["root"] = child.root
            observed["args"] = tuple(args)
            observed["timeout"] = timeout
            observed["script"] = child.fake_script
            return CommandResult(tuple(args), 0, "{}\n", "", False, 1)

        try:
            with mock.patch.object(CheckContext, "run_haider", fake_run), mock.patch.object(
                CheckContext,
                "cleanup",
                lambda _child: Evidence(
                    "no_orphan_daemons", PASS, "no_orphan_daemons pids=1 alive_after=false"
                ),
            ):
                result, cleanup = context.run_isolated_haider(
                    "control",
                    ["run", "-p", "control"],
                    timeout=runner.DAEMON_STARTUP,
                )
            self.assertEqual(result.returncode, 0)
            self.assertNotEqual(observed["root"], context.root)
            self.assertEqual(observed["script"], context.fake_script)
            self.assertEqual(cleanup.status, PASS)
            self.assertEqual(cleanup.label, "control_no_orphan_daemons")
            self.assertIn("isolated_subcase=control", cleanup.evidence_line)
        finally:
            context.dispose(keep=False)

    def test_spawn_capable_cleanup_without_status_pid_fails(self):
        context = CheckContext(
            check_id="t0.test.no-pid",
            bin_dir=Path(tempfile.gettempdir()),
            script=[{"step": "finish", "reason": "end_turn"}],
        )
        context.spawn_possible = True

        def fake_run(args, *, timeout):
            del timeout
            if args[0] == "status":
                return CommandResult(tuple(args), 69, "", "unavailable", False, 1)
            return CommandResult(
                tuple(args),
                69,
                '{"schema":"haider.daemon-stop.v1","outcome":"not_running","elapsed_ms":0}\n',
                "",
                False,
                1,
            )

        context.run_haider = fake_run
        try:
            evidence = context.cleanup()
            self.assertEqual(evidence.status, "FAIL")
            self.assertIn("no status-observed daemon pid", evidence.evidence_line)
        finally:
            context.dispose(keep=False)

    def test_foreign_status_pid_is_never_stopped_or_signalled(self):
        context = CheckContext(
            check_id="t0.test.foreign-pid",
            bin_dir=Path(tempfile.gettempdir()),
            script=[{"step": "finish", "reason": "end_turn"}],
        )
        context.spawn_possible = True
        foreign_pid = 424242
        foreign = {
            "schema": "haider.observe.v1",
            "profile_path": "/Users/owner/.haider",
            "runtime_dir": "/Users/owner/.haider/runtime",
            "daemon": {
                "pid": foreign_pid,
                "version": "0.0.967",
                "socket_path": "/Users/owner/.haider/runtime/h.sock",
                "pid_file_path": "/Users/owner/.haider/runtime/haiderd.pid",
            },
        }
        calls = []

        def fake_run(args, *, timeout):
            del timeout
            calls.append(tuple(args))
            return CommandResult(tuple(args), 0, json.dumps(foreign) + "\n", "", False, 1)

        context.run_haider = fake_run
        try:
            evidence = context.cleanup()
            self.assertEqual(evidence.status, "FAIL")
            self.assertEqual(context.daemon_pids, set())
            self.assertEqual(context.untrusted_status_pids, {foreign_pid})
            self.assertEqual(calls, [("status", "--json", "--no-spawn")])
            self.assertIn("refused daemon stop for untrusted status", evidence.evidence_line)
        finally:
            context.dispose(keep=False)


if __name__ == "__main__":
    unittest.main()
