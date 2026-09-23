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
        self.addCleanup(self.detach_leftover_test_mounts)
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

    def detach_leftover_test_mounts(self):
        mounts = subprocess.check_output(['/sbin/mount'], text=True).splitlines()
        for line in mounts:
            if ' on ' not in line:
                continue
            mount_point = line.split(' on ', 1)[1].split(' (', 1)[0]
            if mount_point.startswith(str(self.base) + '/'):
                subprocess.run(['/usr/bin/hdiutil', 'detach', mount_point], check=True,
                               capture_output=True, text=True)

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

    def build_with_mounted_attach_failure(self, detach_failures, second_attach_error=False,
                                          attach_error_code=16):
        """Fail attach after a real mount, then inject detach failures."""
        (self.shim / 'hdiutil').write_text('''#!/bin/bash
printf '%s\\n' "$1" >> "$HDIUTIL_TEST_DIR/calls"
if [ "$1" = attach ]; then
  count=0
  [ ! -f "$HDIUTIL_TEST_DIR/attach-count" ] || count=$(cat "$HDIUTIL_TEST_DIR/attach-count")
  count=$((count + 1))
  printf '%s\\n' "$count" > "$HDIUTIL_TEST_DIR/attach-count"
  if [ "$count" = 1 ]; then
    /usr/bin/hdiutil "$@" >/dev/null || exit $?
    printf 'simulated Resource busy after mount\\n' >&2
    exit "$ATTACH_ERROR_CODE"
  fi
  if [ -e "${@: -1}/$HDIUTIL_TEST_PKG" ]; then
    printf 'attach retried before previous mount was detached\\n' >&2
    exit 99
  fi
  if [ "$SECOND_ATTACH_ERROR" = 1 ]; then
    printf 'simulated Invalid argument on second attach\\n' >&2
    exit 5
  fi
fi
if [ "$1" = detach ]; then
  count=0
  [ ! -f "$HDIUTIL_TEST_DIR/detach-count" ] || count=$(cat "$HDIUTIL_TEST_DIR/detach-count")
  count=$((count + 1))
  printf '%s\\n' "$count" > "$HDIUTIL_TEST_DIR/detach-count"
  if [ "$count" -le "$DETACH_FAILURES" ]; then
    printf 'simulated Resource busy on detach %s\\n' "$count" >&2
    exit 16
  fi
fi
exec /usr/bin/hdiutil "$@"
''')
        output = self.base / 'output-attach-cleanup'
        env = {
            'PATH': f'{self.shim}:{os.environ.get("PATH", "/usr/bin:/bin")}',
            'TMPDIR': str(self.base) + '/',
            'HDIUTIL_TEST_DIR': str(self.base),
            'HDIUTIL_TEST_PKG': f'haider-v{VERSION}-{TARGET}.pkg',
            'SECOND_ATTACH_ERROR': '1' if second_attach_error else '0',
            'ATTACH_ERROR_CODE': str(attach_error_code),
            'DETACH_FAILURES': str(detach_failures),
        }
        result = subprocess.run(
            ['bash', str(BUILD), str(self.payload), str(output), VERSION, TARGET],
            capture_output=True, text=True, env=env, timeout=120)
        calls = (self.base / 'calls').read_text().splitlines()
        self.assertNotIn(str(self.base), subprocess.check_output(['/sbin/mount'], text=True))
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

    def test_mounted_attach_cleanup_succeeds_before_retry(self):
        result, output, calls = self.build_with_mounted_attach_failure(0)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([calls.count(action) for action in ('create', 'attach', 'detach')],
                         [1, 2, 2])
        self.assertNotIn('attach retried before previous mount was detached', result.stderr)
        self.assertIn('macOS installer gate PASS', result.stdout)
        self.assertTrue(list(output.glob('*.dmg.sha256')))

    def test_mounted_attach_cleanup_retry_succeeds(self):
        result, _, calls = self.build_with_mounted_attach_failure(1)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([calls.count(action) for action in ('create', 'attach', 'detach')],
                         [1, 2, 3])
        self.assertNotIn('attach retried before previous mount was detached', result.stderr)

    def test_mounted_attach_cleanup_fails_then_exit_trap_detaches(self):
        # One preliminary detach and all four detach retries fail; the trap succeeds.
        result, output, calls = self.build_with_mounted_attach_failure(5)
        self.assertEqual(result.returncode, 16, result.stderr)
        self.assertEqual([calls.count(action) for action in ('create', 'attach', 'detach')],
                         [1, 1, 6])
        self.assertIn('simulated Resource busy after mount', result.stderr)
        self.assertTrue(result.stderr.rstrip().endswith('simulated Resource busy after mount'),
                        result.stderr)
        self.assertNotIn('attach retried before previous mount was detached', result.stderr)
        self.assertFalse(list(output.glob('*.sha256')))

    def test_failed_cleanup_keeps_original_attach_status(self):
        result, _, calls = self.build_with_mounted_attach_failure(5, attach_error_code=5)
        self.assertEqual(result.returncode, 5, result.stderr)
        self.assertEqual([calls.count(action) for action in ('attach', 'detach')], [1, 6])
        self.assertIn('simulated Resource busy after mount', result.stderr)
        self.assertTrue(result.stderr.rstrip().endswith('simulated Resource busy after mount'),
                        result.stderr)

    def test_failed_partial_dmg_removal_stops_create_retry(self):
        (self.shim / 'rm').write_text('''#!/bin/bash
if [ "$1" = -f ] && [ "$2" = "$FAILED_DMG" ]; then
  printf 'simulated cleanup failure\\n' >&2
  exit 13
fi
exec /bin/rm "$@"
''')
        (self.shim / 'rm').chmod(0o755)
        output = self.base / 'output-create-cleanup'
        dmg = output / f'haider-v{VERSION}-{TARGET}.dmg'
        env = {
            'PATH': f'{self.shim}:{os.environ.get("PATH", "/usr/bin:/bin")}',
            'HDIUTIL_TEST_DIR': str(self.base),
            'HDIUTIL_TEST_OPERATION': 'create',
            'HDIUTIL_TEST_FAILURES': '1',
            'HDIUTIL_TEST_ERROR': 'Resource busy',
            'HDIUTIL_TEST_EXIT': '16',
            'FAILED_DMG': str(dmg),
        }
        result = subprocess.run(
            ['bash', str(BUILD), str(self.payload), str(output), VERSION, TARGET],
            capture_output=True, text=True, env=env, timeout=120)
        calls = (self.base / 'calls').read_text().splitlines()
        self.assertEqual(result.returncode, 16, result.stderr)
        self.assertEqual(calls.count('create'), 1)
        self.assertIn('simulated Resource busy, attempt 1', result.stderr)
        self.assertIn('simulated cleanup failure', result.stderr)

    def test_successful_preliminary_detach_clears_trap_state(self):
        result, _, calls = self.build_with_mounted_attach_failure(0, second_attach_error=True)
        self.assertEqual(result.returncode, 5, result.stderr)
        self.assertEqual([calls.count(action) for action in ('create', 'attach', 'detach')],
                         [1, 2, 2])
        self.assertIn('simulated Invalid argument on second attach', result.stderr)


if __name__ == '__main__':
    unittest.main()
