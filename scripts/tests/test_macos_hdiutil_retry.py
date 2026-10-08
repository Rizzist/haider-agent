"""Exercise the real macOS installer builder with injected hdiutil busy errors."""
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path
import platform
import plistlib
import shutil
import subprocess
import tempfile
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[2]
BUILD = ROOT / 'packaging/installers/macos-build.sh'
TARGET = 'aarch64-apple-darwin'
VERSION = '0.0.972'
HDIUTIL_TRACE = '''#!/bin/bash
printf '%s\\n' "$1" >> "$HDIUTIL_TEST_DIR/calls"
if [ "$1" = attach ]; then
  previous=
  for argument in "$@"; do
    if [ "$previous" = -mountpoint ]; then
      printf '%s\\0' "$argument" >> "$HDIUTIL_TEST_DIR/mountpoints"
    fi
    previous=$argument
  done
fi
'''


@unittest.skipUnless(platform.system() == 'Darwin', 'requires macOS pkgbuild and hdiutil')
class MacOSHdiutilRetryTests(unittest.TestCase):
    def setUp(self):
        self.base = Path(tempfile.mkdtemp())
        self.temporary_directories = [self.base]
        self.addCleanup(self.cleanup_fixtures)
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
        script.write_text(HDIUTIL_TRACE + '''
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

    def recorded_mountpoints(self):
        record = self.base / 'mountpoints'
        if not record.exists():
            return set()
        return {Path(os.fsdecode(point)).resolve()
                for point in record.read_bytes().split(b'\0') if point}

    def disk_image_mountpoints(self):
        info = plistlib.loads(subprocess.check_output(
            ['/usr/bin/hdiutil', 'info', '-plist'], timeout=60))
        return {Path(entity['mount-point']).resolve()
                for image in info.get('images', [])
                for entity in image.get('system-entities', [])
                if 'mount-point' in entity}

    def mounted_test_mounts(self, inventory=None):
        owned = self.recorded_mountpoints()
        if not owned:
            return set()
        if inventory is None:
            inventory = self.disk_image_mountpoints()
        mounted = owned & inventory
        # realpath preserves some case/Unicode aliases on macOS. Check the
        # recorded location itself, even if the inventory uses another spelling.
        for point in owned:
            try:
                if point.stat().st_dev != point.parent.stat().st_dev:
                    mounted.add(point)
            except FileNotFoundError:
                pass  # The builder already removed this work directory.
        return mounted

    def assert_no_test_mounts(self):
        self.assertEqual(self.mounted_test_mounts(), set(), 'owned test mount retained')

    def detach_leftover_test_mounts(self):
        owned = self.recorded_mountpoints()
        if not owned:
            return
        for point in self.mounted_test_mounts():
            subprocess.run(['/usr/bin/hdiutil', 'detach', str(point)], check=True,
                           capture_output=True, text=True, timeout=60)
        remaining = self.disk_image_mountpoints()
        self.assertEqual(self.mounted_test_mounts(remaining), set(), 'owned test mount retained')
        # A failed builder retains its work directory. Remove it only after the
        # exact recorded mount is clear; never derive its location from TMPDIR.
        for point in owned:
            if (point.name == 'mount' and point.parent.name.startswith('tmp.')
                    and point.parent.exists()):
                self.assertFalse(any(mount.is_relative_to(point.parent) for mount in remaining),
                                 'unrecorded mount inside builder work directory')
                shutil.rmtree(point.parent)

    def cleanup_fixtures(self):
        # Keep fixtures if detach/audit fails: deleting them can traverse a mount.
        self.detach_leftover_test_mounts()
        for directory in reversed(self.temporary_directories):
            if directory.exists():
                shutil.rmtree(directory)

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
        self.assert_no_test_mounts()
        return result, output, calls

    def build_with_mounted_attach_failure(self, detach_failures, second_attach_error=False,
                                          attach_error_code=16, empty_image=False):
        result, output, calls = self.run_mounted_attach_failure(
            detach_failures, second_attach_error, attach_error_code, empty_image)
        self.assert_no_test_mounts()
        return result, output, calls

    def run_mounted_attach_failure(self, detach_failures, second_attach_error=False,
                                  attach_error_code=16, empty_image=False, use_tmpdir=True):
        """Fail attach after a real mount, then inject detach failures."""
        blank = self.base / 'empty.dmg'
        if empty_image:
            subprocess.run(['/usr/bin/hdiutil', 'create', '-size', '8m', '-fs', 'HFS+',
                            '-volname', 'Empty973', str(blank)], check=True,
                           capture_output=True, text=True)
        (self.shim / 'hdiutil').write_text(HDIUTIL_TRACE + '''
if [ "$1" = attach ]; then
  count=0
  [ ! -f "$HDIUTIL_TEST_DIR/attach-count" ] || count=$(cat "$HDIUTIL_TEST_DIR/attach-count")
  count=$((count + 1))
  printf '%s\\n' "$count" > "$HDIUTIL_TEST_DIR/attach-count"
  if [ "$count" = 1 ]; then
    if [ "$EMPTY_IMAGE" = 1 ]; then
      /usr/bin/hdiutil attach "$BLANK_DMG" -readonly -nobrowse -mountpoint "${@: -1}" >/dev/null || exit $?
    else
      /usr/bin/hdiutil "$@" >/dev/null || exit $?
    fi
    printf 'simulated Resource busy after mount\\n' >&2
    exit "$ATTACH_ERROR_CODE"
  fi
  mount_device=$(stat -f %d "${@: -1}")
  parent_device=$(stat -f %d "$(dirname "${@: -1}")")
  if [ "$mount_device" != "$parent_device" ]; then
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
            'HDIUTIL_TEST_DIR': str(self.base),
            'SECOND_ATTACH_ERROR': '1' if second_attach_error else '0',
            'ATTACH_ERROR_CODE': str(attach_error_code),
            'DETACH_FAILURES': str(detach_failures),
            'EMPTY_IMAGE': '1' if empty_image else '0',
            'BLANK_DMG': str(blank),
        }
        if use_tmpdir:
            env['TMPDIR'] = str(self.base) + '/'
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

    def test_empty_hfs_partial_mount_does_not_retry_attach(self):
        result, output, calls = self.build_with_mounted_attach_failure(1, empty_image=True)
        self.assertEqual(result.returncode, 16, result.stderr)
        self.assertEqual([calls.count(action) for action in ('create', 'attach', 'detach')],
                         [1, 1, 2])
        self.assertIn('simulated Resource busy after mount', result.stderr)
        self.assertNotIn('attach retried before previous mount was detached', result.stderr)
        self.assertFalse(list(output.glob('*.sha256')))

    def test_failed_detach_without_package_preserves_original_status(self):
        result, _, calls = self.build_with_mounted_attach_failure(
            1, attach_error_code=5, empty_image=True)
        self.assertEqual(result.returncode, 5, result.stderr)
        self.assertEqual([calls.count(action) for action in ('attach', 'detach')], [1, 2])
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

    def test_retained_native_mktemp_mount_fails_helper_assertion(self):
        # This is Astra's original sustained-busy-detach reproducer. The helper
        # must reject it even when native mktemp ignores the fixture's TMPDIR.
        with self.assertRaisesRegex(AssertionError, 'owned test mount retained'):
            self.build_with_mounted_attach_failure(99)
        points = self.recorded_mountpoints()
        self.assertEqual(len(points), 1)
        self.assertEqual(self.mounted_test_mounts(), points)
        # A missing/alternate inventory spelling must not hide a real mount.
        with patch.object(self, 'disk_image_mountpoints', return_value=set()):
            self.assertEqual(self.mounted_test_mounts(), points)
            with self.assertRaisesRegex(AssertionError, 'owned test mount retained'):
                self.assert_no_test_mounts()
            self.detach_leftover_test_mounts()
        self.assert_no_test_mounts()
        self.assertTrue(all(not point.parent.exists() for point in points))

    def test_external_builder_directory_with_spaces_tmpdir_set_and_unset(self):
        external = Path(tempfile.mkdtemp(prefix='hdiutil builder with spaces '))
        self.temporary_directories.append(external)
        alias = self.base / 'builder symlink with spaces'
        alias.symlink_to(external, target_is_directory=True)
        (self.base / 'builder-temp-dir').write_text(str(alias))
        # Only redirect the builder's work-dir creation. Diagnostic-file mktemp
        # calls still use the real command and the builder's supplied template.
        mktemp = self.shim / 'mktemp'
        mktemp.write_text('''#!/bin/bash
if [ "$#" = 1 ] && [ "$1" = -d ]; then
  exec /usr/bin/mktemp -d "$(cat "$HDIUTIL_TEST_DIR/builder-temp-dir")/tmp.XXXXXXXX"
fi
exec /usr/bin/mktemp "$@"
''')
        mktemp.chmod(0o755)
        for use_tmpdir in (True, False):
            with self.subTest(use_tmpdir=use_tmpdir):
                for name in ('attach-count', 'detach-count', 'calls'):
                    (self.base / name).unlink(missing_ok=True)
                previous = self.recorded_mountpoints()
                result, output, _ = self.run_mounted_attach_failure(99, use_tmpdir=use_tmpdir)
                self.assertEqual(result.returncode, 16, result.stderr)
                points = self.recorded_mountpoints() - previous
                self.assertEqual(len(points), 1)
                self.assertTrue(all(not point.is_relative_to(self.base.resolve())
                                    for point in points))
                self.assertTrue(all(' ' in str(point) for point in points))
                with self.assertRaisesRegex(AssertionError, 'owned test mount retained'):
                    self.assert_no_test_mounts()
                self.detach_leftover_test_mounts()
                self.assert_no_test_mounts()
                shutil.rmtree(output)

    def test_concurrent_builders_clean_only_their_own_mount(self):
        other = MacOSHdiutilRetryTests()
        other.setUp()
        self.addCleanup(other.cleanup_fixtures)
        with ThreadPoolExecutor(max_workers=2) as executor:
            runs = [executor.submit(case.run_mounted_attach_failure, 99)
                    for case in (self, other)]
            for run in runs:
                result, _, calls = run.result()
                self.assertEqual(result.returncode, 16, result.stderr)
                self.assertEqual([calls.count(op) for op in ('create', 'attach', 'detach')],
                                 [1, 1, 6])
        mine, theirs = self.mounted_test_mounts(), other.mounted_test_mounts()
        self.assertEqual(len(mine), 1)
        self.assertEqual(len(theirs), 1)
        self.assertTrue(mine.isdisjoint(theirs))
        self.detach_leftover_test_mounts()
        self.assert_no_test_mounts()
        self.assertEqual(other.mounted_test_mounts(), theirs)
        other.detach_leftover_test_mounts()
        other.assert_no_test_mounts()

    def test_mount_records_normalize_var_alias_symlinks_and_whitespace(self):
        directory = Path(tempfile.mkdtemp(dir='/var/tmp'))
        self.temporary_directories.append(directory)
        point = directory / 'tmp.space \t\n\u03a9' / 'mount'
        point.mkdir(parents=True)
        alias = self.base / 'symlink alias'
        alias.symlink_to(point.parent, target_is_directory=True)
        records = [point, point.resolve(), alias / 'mount']
        (self.base / 'mountpoints').write_bytes(
            b''.join(os.fsencode(str(record)) + b'\0' for record in records))
        info = plistlib.dumps({'images': [{'system-entities': [
            {'mount-point': str(point.resolve())}, {'dev-entry': '/dev/fixture'}]}]})
        with patch.object(subprocess, 'check_output', return_value=info):
            self.assertEqual(self.recorded_mountpoints(), {point.resolve()})
            self.assertEqual(self.mounted_test_mounts(), {point.resolve()})

    def test_cleanup_ignores_unrecorded_mounts_and_prefix_siblings(self):
        point = self.base / 'tmp.owned' / 'mount'
        neighbor = self.base / 'tmp.owned-other' / 'mount'
        point.mkdir(parents=True)
        (self.base / 'mountpoints').write_bytes(os.fsencode(str(point)) + b'\0')
        def info(points):
            return plistlib.dumps({'images': [{'system-entities': [
                {'mount-point': str(p)} for p in points]}]})
        with patch.object(subprocess, 'check_output', side_effect=[
                info([point, neighbor]), info([neighbor])]), \
                patch.object(subprocess, 'run') as detach:
            self.detach_leftover_test_mounts()
            detach.assert_called_once_with(
                ['/usr/bin/hdiutil', 'detach', str(point.resolve())], check=True,
                capture_output=True, text=True, timeout=60)

    def test_cleanup_already_gone_is_repeatable_noop(self):
        point = self.base / 'tmp.gone' / 'mount'
        (self.base / 'mountpoints').write_bytes(os.fsencode(str(point)) + b'\0')
        with patch.object(subprocess, 'check_output', return_value=plistlib.dumps({
                'images': []})), patch.object(subprocess, 'run') as detach:
            self.detach_leftover_test_mounts()
            self.detach_leftover_test_mounts()
            detach.assert_not_called()

    def test_failed_detach_preserves_fixtures(self):
        point = self.base / 'tmp.retained' / 'mount'
        (self.base / 'mountpoints').write_bytes(os.fsencode(str(point)) + b'\0')
        with patch.object(self, 'disk_image_mountpoints', return_value={point.resolve()}), \
                patch.object(subprocess, 'run', side_effect=subprocess.CalledProcessError(
                    16, ['/usr/bin/hdiutil', 'detach', str(point)])):
            with self.assertRaises(subprocess.CalledProcessError):
                self.cleanup_fixtures()
        self.assertTrue(self.payload.is_dir())

    def test_failed_final_mount_audit_preserves_fixtures(self):
        point = self.base / 'tmp.retained' / 'mount'
        (self.base / 'mountpoints').write_bytes(os.fsencode(str(point)) + b'\0')
        with patch.object(self, 'disk_image_mountpoints', return_value={point.resolve()}), \
                patch.object(subprocess, 'run'):
            with self.assertRaisesRegex(AssertionError, 'owned test mount retained'):
                self.cleanup_fixtures()
        self.assertTrue(self.payload.is_dir())

    def test_cleanup_preserves_work_directory_with_unrecorded_mount(self):
        point = self.base / 'tmp.owned' / 'mount'
        point.mkdir(parents=True)
        neighbor = point.parent / 'mount-other'
        (self.base / 'mountpoints').write_bytes(os.fsencode(str(point)) + b'\0')
        with patch.object(self, 'disk_image_mountpoints', side_effect=[
                {point.resolve(), neighbor.resolve()}, {neighbor.resolve()}]), \
                patch.object(subprocess, 'run') as detach:
            with self.assertRaisesRegex(AssertionError, 'unrecorded mount inside builder'):
                self.detach_leftover_test_mounts()
            self.assertEqual(detach.call_args.args[0],
                             ['/usr/bin/hdiutil', 'detach', str(point.resolve())])
        self.assertTrue(point.parent.is_dir())


if __name__ == '__main__':
    unittest.main()
