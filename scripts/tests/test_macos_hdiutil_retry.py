"""Exercise the real macOS installer builder with injected hdiutil busy errors."""
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
BUILD = ROOT / 'packaging/installers/macos-build.sh'
TARGET = 'aarch64-apple-darwin'
VERSION = '0.0.972'


@unittest.skipUnless(platform.system() == 'Darwin', 'requires macOS pkgbuild and hdiutil')
class MacOSHdiutilRetryTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name)
        self.payload = self.base / 'payload'
        self.payload.mkdir()
        members = {'haider': b'fixture cli\n', 'haiderd': b'fixture daemon\n'}
        for name, data in members.items():
            (self.payload / name).write_bytes(data)
        (self.payload / 'manifest.json').write_text(json.dumps({
            'version': VERSION, 'target': TARGET,
            'members': {name: hashlib.sha256(data).hexdigest()
                        for name, data in members.items()},
        }))
        (self.payload / 'uninstall-haider.sh').write_text('#!/bin/sh\n')
        self.shim = self.base / 'shim'
        self.shim.mkdir()
        script = self.shim / 'hdiutil'
        script.write_text('''#!/bin/bash
printf '%s\\n' "$1" >> "$HDIUTIL_TEST_DIR/calls"
if [ "$1" = "$HDIUTIL_TEST_OPERATION" ]; then
  count_file="$HDIUTIL_TEST_DIR/count"
  count=0
  [ ! -f "$count_file" ] || count=$(cat "$count_file")
  count=$((count + 1))
  printf '%s\\n' "$count" > "$count_file"
  if [ "$count" -le "$HDIUTIL_TEST_FAILURES" ]; then
    if [ "$1" = create ]; then touch "${@: -1}"; fi
    if [ "$1" = attach ]; then /usr/bin/hdiutil "$@" >/dev/null || exit $?; fi
    printf 'simulated %s, attempt %s\\n' "$HDIUTIL_TEST_ERROR" "$count" >&2
    exit "$HDIUTIL_TEST_EXIT"
  fi
fi
exec /usr/bin/hdiutil "$@"
''')
        script.chmod(0o755)

    def build(self, operation, failures, error='Resource busy', error_exit=16):
        output = self.base / f'output-{operation}'
        env = {
            'PATH': f'{self.shim}:{os.environ.get("PATH", "/usr/bin:/bin")}',
            'HDIUTIL_TEST_DIR': str(self.base),
            'HDIUTIL_TEST_OPERATION': operation,
            'HDIUTIL_TEST_FAILURES': str(failures),
            'HDIUTIL_TEST_ERROR': error,
            'HDIUTIL_TEST_EXIT': str(error_exit),
        }
        result = subprocess.run(
            ['bash', str(BUILD), str(self.payload), str(output), VERSION, TARGET],
            capture_output=True, text=True, env=env, timeout=120)
        calls = (self.base / 'calls').read_text().splitlines()
        return result, output, calls

    def test_busy_create_attach_and_detach_recover(self):
        for operation in ('create', 'attach', 'detach'):
            with self.subTest(operation=operation):
                (self.base / 'calls').unlink(missing_ok=True)
                (self.base / 'count').unlink(missing_ok=True)
                result, output, calls = self.build(operation, 1)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn('macOS post-pack gate:', result.stdout)
                self.assertIn('macOS installer gate PASS', result.stdout)
                self.assertEqual(calls.count(operation), 2)
                name = f'haider-v{VERSION}-{TARGET}'
                for extension in ('.pkg', '.dmg'):
                    artifact = output / f'{name}{extension}'
                    self.assertTrue(artifact.is_file())
                    digest = hashlib.sha256(artifact.read_bytes()).hexdigest()
                    self.assertEqual((output / f'{name}{extension}.sha256').read_text(),
                                     f'{digest}  {artifact.name}\n')

    def test_persistent_create_failure_keeps_last_error(self):
        result, output, calls = self.build('create', 4)
        self.assertEqual(result.returncode, 16)
        self.assertEqual(calls.count('create'), 4)
        self.assertIn('simulated Resource busy, attempt 4', result.stderr)
        self.assertIn('hdiutil create failed after 4 attempts (last exit 16)', result.stderr)
        self.assertFalse(list(output.glob('*.dmg')))
        self.assertFalse(list(output.glob('*.sha256')))

    def test_permanent_create_failure_returns_original_status_immediately(self):
        result, output, calls = self.build('create', 4, 'Invalid argument', 5)
        self.assertEqual(result.returncode, 5)
        self.assertEqual(calls.count('create'), 1)
        self.assertIn('simulated Invalid argument, attempt 1', result.stderr)
        self.assertNotIn('failed after 4 attempts', result.stderr)
        self.assertFalse(list(output.glob('*.dmg')))
        self.assertFalse(list(output.glob('*.sha256')))


if __name__ == '__main__':
    unittest.main()
