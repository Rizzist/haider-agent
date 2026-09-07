import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('emulator_gate', Path(__file__).with_name('emulator-gate.py'))
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)

class InstrumentationVerdictTest(unittest.TestCase):
    def test_exit_zero_without_tests_is_not_success(self):
        for text in ['', 'INSTRUMENTATION_CODE: -1', 'FAILURES!!!\nOK (2 tests)', 'INSTRUMENTATION_FAILED\nOK (1 test)']:
            self.assertFalse(gate.instrumentation_passed(text))
        self.assertTrue(gate.instrumentation_passed('OK (2 tests)\nINSTRUMENTATION_CODE: -1'))

if __name__ == '__main__':
    unittest.main()
