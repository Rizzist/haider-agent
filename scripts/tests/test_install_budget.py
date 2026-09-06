"""Installer resource accounting and timeout cleanup regressions."""
from pathlib import Path
import subprocess
import json
import sys
import tempfile
import time
import unittest
from unittest import mock

from scripts.tests.test_install_bundle_native import ROOT, run_owned_install
from gate.contract import VERSION_QUERY, validate_budget
from gate.install_budget import (install_budget, native_member_sizes, fetch_attempt_seconds,
                                 FETCH_ARCHIVE_BYTES, FETCH_CHECKSUM_BYTES, FETCH_ATTEMPTS, INSTALL_FETCH)


class InstallBudgetTests(unittest.TestCase):
    def test_budget_scales_with_member_count_and_bytes(self):
        one = validate_budget(install_budget((1024 * 1024,)))
        three = validate_budget(install_budget((1024 * 1024,) * 3))
        bigger = validate_budget(install_budget((2 * 1024 * 1024,) * 3))
        self.assertGreater(three.seconds, one.seconds)
        self.assertGreater(bigger.seconds, three.seconds)
        # Seven hash and eight IO passes add a measured-byte allowance, while
        # fixed self-tests do not multiply when another member is present.
        self.assertEqual(bigger.seconds - three.seconds, 7 * 3 / 8 + 8 * 3 / 16)
        self.assertEqual(install_budget((1024 * 1024,), old_bytes=8 * 1024 * 1024).seconds - one.seconds, 1)
        with self.assertRaises(ValueError):
            install_budget((0,))

    def test_shell_npm_and_qa_share_capacity_derived_fetch_budgets(self):
        # Execute the production shell declarations without downloading; check
        # both capacities and formula against npm and the QA BudgetPart.
        script = (ROOT / "scripts/install.sh").read_text().split("fetch() (", 1)[0]
        script += '\nprintf "%s %s %s %s %s" "$FETCH_ATTEMPTS" "$FETCH_CONNECT_SECONDS" "$FETCH_TRANSFER_BYTES_PER_SECOND" "$FETCH_ARCHIVE_BYTES" "$FETCH_CHECKSUM_BYTES"'
        result = subprocess.run(["sh", "-c", script], capture_output=True, text=True, timeout=VERSION_QUERY.seconds, check=True)
        attempts, connect, rate, archive, checksum = map(int, result.stdout.split())
        self.assertEqual((archive, checksum, attempts), (FETCH_ARCHIVE_BYTES, FETCH_CHECKSUM_BYTES, FETCH_ATTEMPTS))
        for capacity in (archive, checksum):
            self.assertEqual(connect + (capacity + rate - 1) // rate, fetch_attempt_seconds(capacity))
        code = "const i=require(process.argv[1]);console.log(JSON.stringify(process.argv.slice(2).map(n=>i.downloadAttemptMs(Number(n)))));"
        result = subprocess.run(["node", "-e", code, str(ROOT / "packaging/npm/install.js"), str(FETCH_ARCHIVE_BYTES), str(FETCH_CHECKSUM_BYTES)],
                                capture_output=True, text=True, timeout=VERSION_QUERY.seconds, check=True)
        self.assertEqual(json.loads(result.stdout), [1000 * fetch_attempt_seconds(n) for n in (FETCH_ARCHIVE_BYTES, FETCH_CHECKSUM_BYTES)])
        self.assertEqual(INSTALL_FETCH.seconds, FETCH_ATTEMPTS * fetch_attempt_seconds(FETCH_ARCHIVE_BYTES))

    def test_linux_budget_includes_required_portal_bytes(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            for index, name in enumerate(("haider", "haider-tui", "haiderd", "haider-wayland-portal"), 1):
                (directory / name).write_bytes(b"x" * index)
            with mock.patch("gate.install_budget.sys.platform", "linux"):
                self.assertEqual(native_member_sizes(directory), (1, 2, 3, 4))
            with mock.patch("gate.install_budget.sys.platform", "darwin"):
                self.assertEqual(native_member_sizes(directory), (1, 2, 3))

    def test_watchdog_kills_nested_helper_and_preserves_diagnostics(self):
        with tempfile.TemporaryDirectory() as temporary:
            marker = Path(temporary) / "escaped-child"
            child = "import pathlib,time;time.sleep(1);pathlib.Path(" + repr(str(marker)) + ").touch()"
            parent = "import subprocess,sys,time;subprocess.Popen([sys.executable,'-c'," + repr(child) + "]);print('helper started',flush=True);time.sleep(10)"
            started = time.monotonic()
            with self.assertRaises(subprocess.TimeoutExpired) as caught:
                run_owned_install([sys.executable, "-c", parent], timeout=.3)
            self.assertIn("helper started", caught.exception.output)
            self.assertLess(time.monotonic() - started, .3 + VERSION_QUERY.seconds)
            time.sleep(1.1)
            self.assertFalse(marker.exists(), "nested helper survived the timed-out shell/node wrapper")
