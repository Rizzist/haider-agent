import importlib.util
import contextlib
import io
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('emulator_gate', Path(__file__).with_name('emulator-gate.py'))
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)

class InstrumentationVerdictTest(unittest.TestCase):
    def test_exit_zero_without_tests_is_not_success(self):
        for text in ['', 'INSTRUMENTATION_CODE: -1', 'FAILURES!!!\nOK (2 tests)', 'INSTRUMENTATION_FAILED\nOK (1 test)']:
            self.assertFalse(gate.instrumentation_passed(text))
        self.assertTrue(gate.instrumentation_passed('OK (2 tests)\nINSTRUMENTATION_CODE: -1'))


class EmulatorOwnershipTest(unittest.TestCase):
    def invoke(self, root, *extra):
        with contextlib.redirect_stderr(io.StringIO()):
            return gate.main(['--apks', str(root / 'apks'), '--tier', 'pr', '--evidence', str(root / 'evidence'),
                              '--serial', 'emulator-5554', *extra])

    def test_invalid_inputs_never_call_adb_or_cleanup(self):
        with tempfile.TemporaryDirectory() as directory, patch.object(gate.subprocess, 'run') as run, \
                patch.object(gate.subprocess, 'check_output') as output:
            root = Path(directory)
            for options in ([], ['--owned-emulator'], ['--owned-emulator', '--serial', 'physical-device']):
                self.assertEqual(self.invoke(root, *options), 1)
            run.assert_not_called()
            output.assert_not_called()
            result = json.loads((root / 'evidence/result.json').read_text())
            self.assertEqual(result['steps'], [])

    def test_wrong_package_never_calls_adb(self):
        with tempfile.TemporaryDirectory() as directory, patch.object(gate.subprocess, 'run') as run, \
                patch.object(gate.subprocess, 'check_output', side_effect=["package: name='wrong'", "package: name='wrong.test'"]):
            root = Path(directory)
            (root / 'apks').mkdir()
            for name in ('app.apk', 'tests.apk'):
                (root / 'apks' / name).touch()
            self.assertEqual(self.invoke(root, '--owned-emulator'), 1)
            run.assert_not_called()

    def exercise(self, failure=None, nightly=False, already_idle=False):
        commands = []
        def process(command, **kwargs):
            commands.append(command)
            tail = command[3:] if command[:1] == ['adb'] and '-s' in command else command[1:]
            code, output = 0, ''
            if tail == ['devices', '-l']:
                output = 'List of devices attached\nemulator-5554\tdevice product:test'
            elif 'getconf' in tail:
                output = '4096'
            elif tail[:1] == ['install'] and failure == 'install':
                code = 1
            elif 'instrument' in tail:
                output = 'FAILURES!!!' if failure == 'instrumentation' else 'OK (2 tests)'
            elif 'services' in tail:
                output = 'HaiderDaemonService isForeground=true'
            elif 'sys.boot_completed' in tail:
                output = '1'
            elif tail == ['shell', 'dumpsys', 'deviceidle']:
                output = 'mState=IDLE mForceIdle=' + str(already_idle).lower()
            elif 'force-idle' in tail and failure == 'doze':
                raise subprocess.TimeoutExpired(command, 120)
            elif 'screencap' in tail and failure == 'screenshot':
                raise subprocess.TimeoutExpired(command, 30)
            return subprocess.CompletedProcess(command, code, output.encode(), b'')
        with tempfile.TemporaryDirectory() as directory, patch.object(gate, 'preflight', return_value=(Path('app.apk'), Path('test.apk'))), \
                patch.object(gate.subprocess, 'run', side_effect=process):
            root = Path(directory)
            options = ['--owned-emulator'] + (['--tier', 'nightly'] if nightly else [])
            code = self.invoke(root, *options)
            result = json.loads((root / 'evidence/result.json').read_text())
        return code, commands, result

    def test_failed_install_never_runs_cleanup(self):
        code, commands, result = self.exercise(failure='install')
        self.assertEqual(code, 1)
        self.assertFalse(any('force-stop' in c or 'unforce' in c or 'screencap' in c for c in commands))
        self.assertEqual(result['verdict'], 'NO_SHIP')

    def test_instrumentation_failure_stops_only_installed_app(self):
        code, commands, _ = self.exercise(failure='instrumentation')
        self.assertEqual(code, 1)
        self.assertEqual(sum('force-stop' in c for c in commands), 1)
        self.assertFalse(any('unforce' in c for c in commands))

    def test_only_owned_doze_is_restored(self):
        for failure in (None, 'doze'):
            code, commands, _ = self.exercise(failure=failure, nightly=True)
            self.assertEqual(code, int(failure is not None))
            self.assertEqual(sum('force-idle' in c for c in commands), 1)
            self.assertEqual(sum('unforce' in c for c in commands), 1)
        code, commands, _ = self.exercise(nightly=True, already_idle=True)
        self.assertEqual(code, 1)
        self.assertFalse(any('force-idle' in c or 'unforce' in c for c in commands))

    def test_screenshot_timeout_preserves_result(self):
        code, _, result = self.exercise(failure='screenshot')
        self.assertEqual(code, 1)
        self.assertEqual(result['verdict'], 'NO_SHIP')
        self.assertEqual(next(s for s in result['steps'] if s['name'] == 'screen')['exit_code'], 124)

if __name__ == '__main__':
    unittest.main()
