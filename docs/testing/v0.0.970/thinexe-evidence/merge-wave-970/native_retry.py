import subprocess
import unittest

class DiagnosticResult(unittest.TextTestResult):
    def addError(self, test, err):
        super().addError(test, err)
        exception = err[1]
        if isinstance(exception, subprocess.TimeoutExpired):
            for name in ('stdout', 'stderr'):
                data = getattr(exception, name, None)
                if isinstance(data, bytes):
                    data = data.decode('utf-8', 'replace')
                self.stream.writeln(f'Captured timeout {name}: {data}')

suite = unittest.defaultTestLoader.loadTestsFromName('scripts.tests.test_install_bundle_native')
result = unittest.TextTestRunner(verbosity=2, resultclass=DiagnosticResult).run(suite)
raise SystemExit(not result.wasSuccessful())
