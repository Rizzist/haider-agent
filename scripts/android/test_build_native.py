import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('build_native', Path(__file__).with_name('build-native.py'))
native = importlib.util.module_from_spec(spec)
spec.loader.exec_module(native)


class WorkspaceVersionTest(unittest.TestCase):
    def test_workspace_authority_not_dependency_version(self):
        manifest = '[package]\nversion = "1.2.3"\n[workspace.package]\nversion = "0.0.970" # build\n[dependencies]\nversion = "9.9.9"\n'
        self.assertEqual(native.workspace_version(manifest), '0.0.970')

    def test_missing_or_non_numeric_version_is_rejected(self):
        for text in ('[package]\nversion="0.0.970"', '[workspace.package]\nversion="next"'):
            with self.assertRaises(ValueError):
                native.workspace_version(text)


if __name__ == '__main__':
    unittest.main()
