import json
import subprocess
import unittest
from unittest.mock import patch

from daemon_recovery_probe import COMPONENT, MARKER, PACKAGE, STATE_FILE, RecoveryProbe


class RecoveryProbeTest(unittest.TestCase):
    def setUp(self):
        self.commands = []
        self.output = "No services match: " + COMPONENT
        self.responses = []

        def run(command, **kwargs):
            self.commands.append(command)
            self.assertFalse(kwargs["check"])
            self.assertEqual(15, kwargs["timeout"])
            code, output, error = self.responses.pop(0) if self.responses else (0, self.output, "")
            return subprocess.CompletedProcess(command, code, stdout=output, stderr=error)

        self.probe = RecoveryProbe("owned-emulator", run)

    def test_cleanup_never_instruments_or_force_stops_the_package(self):
        self.responses = [(255, "Stopping service\n", "Service stopped\n")]
        result = self.probe.cleanup()
        self.assertEqual(255, result["commandExit"])
        self.assertTrue(result["serviceAbsent"])
        self.assertEqual([["adb", "-s", "owned-emulator", "shell", "run-as", PACKAGE,
                           "am", "stopservice", "--user", "0", "-n", COMPONENT],
                          ["adb", "-s", "owned-emulator", "shell", "dumpsys", "activity",
                           "service", COMPONENT]], self.commands)
        self.assertFalse(any("force-stop" in c or "instrument" in c for c in self.commands))

    def test_cleanup_accepts_already_absent_only_with_observation(self):
        self.responses = [(255, "", "Service not stopped: was not running.\n")]
        self.assertTrue(self.probe.cleanup()["serviceAbsent"])
        self.assertEqual(2, len(self.commands))

    def test_cleanup_never_converts_permission_or_other_errors_to_success(self):
        for code, message in [(255, "SecurityException: Permission Denial"),
                              (1, "Service stopped")]:
            self.responses = [(code, "", message)]
            with self.assertRaises(subprocess.CalledProcessError):
                self.probe.cleanup()

    def test_cleanup_rejects_service_still_present(self):
        self.responses = [(255, "", "Service stopped\n")]
        self.output = MARKER + json.dumps(dict(phase="STOPPING"))
        with patch("daemon_recovery_probe.time.monotonic", side_effect=[0, 6]):
            with self.assertRaisesRegex(AssertionError, "still present"):
                self.probe.cleanup()

    def test_missing_diagnostics_is_not_treated_as_absence(self):
        self.output = "Permission Denial"
        with self.assertRaisesRegex(RuntimeError, "diagnostics unavailable"):
            self.probe.snapshot()

    def test_missing_service_is_not_created_or_cleaned_up(self):
        self.assertIsNone(self.probe.snapshot())
        self.assertEqual([["adb", "-s", "owned-emulator", "shell", "dumpsys", "activity",
                           "service", COMPONENT]], self.commands)

    def test_ready_requires_foreground_lifetime_and_endpoint(self):
        state = dict(phase="READY", enabled=True, started=True, hasEndpoint=True,
                     generation=1, destroyed=False)
        self.output = "  " + MARKER + json.dumps(state)
        self.assertEqual(state, self.probe.assert_ready(timeout=0))
        state["started"] = False
        self.output = MARKER + json.dumps(state)
        with self.assertRaisesRegex(AssertionError, "Invalid Ready lifetime"):
            self.probe.assert_ready(timeout=0)

    def test_disabled_reads_only_lifecycle_state_and_rejects_active(self):
        self.output = json.dumps(dict(enabled=False, active=False))
        self.probe.assert_disabled()
        self.assertEqual([["adb", "-s", "owned-emulator", "shell", "run-as", PACKAGE,
                           "cat", STATE_FILE]], self.commands)
        self.output = json.dumps(dict(enabled=False, active=True))
        with self.assertRaises(AssertionError):
            self.probe.assert_disabled()

    def test_absence_times_out_without_becoming_a_pass(self):
        with self.assertRaisesRegex(AssertionError, "did not reach Ready"):
            self.probe.assert_ready(timeout=0)


if __name__ == "__main__":
    unittest.main()
