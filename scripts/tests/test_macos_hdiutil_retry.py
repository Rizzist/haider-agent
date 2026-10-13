"""Exercise the real macOS installer builder with injected hdiutil busy errors."""
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path
import platform
import plistlib
import re
import shutil
import subprocess
import tempfile
import time
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
      HDIUTIL_MOUNTPOINT=$argument
    fi
    previous=$argument
  done
fi
native_hdiutil() {
  if [ "$1" != attach ]; then /usr/bin/hdiutil "$@"; return $?; fi
  python3 - "$@" <<'TRACE_PY'
import json, os, pathlib, plistlib, re, subprocess, sys
arguments = sys.argv[1:]
image = str(pathlib.Path(arguments[1]).resolve())
record = pathlib.Path(os.environ['HDIUTIL_TEST_DIR']) / 'attachments'
def append(device):
    with record.open('a') as output:
        output.write(json.dumps({'image': image, 'device': device}) + '\\n')
# Record the actual native argument before attach, including a substituted image.
append(None)
if '-plist' not in arguments: arguments.append('-plist')
result = subprocess.run(['/usr/bin/hdiutil', *arguments], capture_output=True)
sys.stdout.buffer.write(result.stdout)
sys.stderr.buffer.write(result.stderr)
try:
    attached = plistlib.loads(result.stdout)
except ValueError:
    attached = {}
for entity in attached.get('system-entities', []):
    device = entity.get('dev-entry', '')
    if re.fullmatch(r'/dev/disk[0-9]+', device): append(device)
info = plistlib.loads(subprocess.check_output(['/usr/bin/hdiutil', 'info', '-plist']))
for attached in info.get('images', []):
    if str(pathlib.Path(attached.get('image-path', '')).resolve()) == image:
        for entity in attached.get('system-entities', []):
            device = entity.get('dev-entry', '')
            if re.fullmatch(r'/dev/disk[0-9]+', device):
                append(device)
sys.exit(result.returncode)
TRACE_PY
}
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
    if [ "$1" = attach ]; then native_hdiutil "$@" || exit $?; fi
    printf 'simulated %s, attempt %s\\n' "$HDIUTIL_TEST_ERROR" "$count" >&2
    exit "$HDIUTIL_TEST_EXIT"
  fi
fi
native_hdiutil "$@"
''')
        script.chmod(0o755)

    def recorded_mountpoints(self):
        record = self.base / 'mountpoints'
        if not record.exists():
            return set()
        return {Path(os.fsdecode(point)).resolve()
                for point in record.read_bytes().split(b'\0') if point}

    def recorded_attachments(self):
        record = self.base / 'attachments'
        if not record.exists():
            return []
        return [json.loads(line) for line in record.read_text().splitlines()]

    def disk_image_inventory(self):
        for attempt in range(4):
            result = subprocess.run(['/usr/bin/hdiutil', 'info', '-plist'],
                                    capture_output=True, timeout=60, check=True)
            images = plistlib.loads(result.stdout).get('images', [])
            inventory_devices = {entity.get('dev-entry') for image in images
                                 for entity in image.get('system-entities', [])}
            devices = {record['device'] for record in self.recorded_attachments()
                       if record['device']}
            missing_device = any(Path(device).exists() and device not in inventory_devices
                                 for device in devices)
            if b'cannot get IO media array' not in result.stderr and not missing_device:
                return images
            if attempt == 3:
                raise RuntimeError('hdiutil attachment inventory remained incomplete')
            time.sleep(0.1 * (attempt + 1))

    def disk_image_mountpoints(self):
        return {Path(entity['mount-point']).resolve()
                for image in self.disk_image_inventory()
                for entity in image.get('system-entities', [])
                if 'mount-point' in entity}

    def attached_test_images(self, inventory=None):
        owned = {Path(record['image']).resolve() for record in self.recorded_attachments()}
        if not owned:
            return {}
        if inventory is None:
            inventory = self.disk_image_inventory()
        def is_owned(path):
            if path in owned:
                return True
            for record in owned:
                try:
                    if path.samefile(record):
                        return True
                except OSError:
                    pass
            return False
        return {Path(image['image-path']).resolve(): image
                for image in inventory if 'image-path' in image
                and is_owned(Path(image['image-path']).resolve())}

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
        self.assertEqual(self.attached_test_images(), {}, 'owned test image attached')
        self.assertEqual(self.mounted_test_mounts(), set(), 'owned test mount retained')

    def detach_leftover_test_mounts(self):
        owned = self.recorded_mountpoints()
        # Whole disks remain in hdiutil info even after their volumes unmount.
        # Match the exact recorded image before using a recyclable device name.
        for image in self.attached_test_images().values():
            devices = [entity['dev-entry'] for entity in image.get('system-entities', [])
                       if re.fullmatch(r'/dev/disk[0-9]+', entity.get('dev-entry', ''))]
            self.assertTrue(devices, 'owned attached image has no whole-disk device')
            subprocess.run(['/usr/bin/hdiutil', 'detach', devices[0]], check=True,
                           capture_output=True, text=True, timeout=60)
        self.assertEqual(self.attached_test_images(), {}, 'owned test image attached')
        remaining = self.disk_image_mountpoints()
        self.assertEqual(self.mounted_test_mounts(remaining), set(), 'owned test mount retained')
        # A failed builder retains its work directory. Remove it only after the
        # exact recorded attachment and mount are clear; never guess from TMPDIR.
        for point in owned:
            if (point.name == 'mount' and point.parent.name.startswith('tmp.')
                    and point.parent.exists()):
                self.remove_clear_directory(point.parent, remaining, 'builder work directory')

    def remove_clear_directory(self, directory, inventory, label):
        if not directory.exists():
            return
        # Protect even foreign backing files whose volumes are already unmounted.
        self.assertFalse(any(Path(image['image-path']).resolve().is_relative_to(directory.resolve())
                             for image in self.disk_image_inventory() if 'image-path' in image),
                         f'attached image inside {label}')
        self.assertFalse(any(mount.is_relative_to(directory.resolve()) for mount in inventory),
                         f'unrecorded mount inside {label}')
        device = directory.stat().st_dev
        self.assertEqual(device, directory.parent.stat().st_dev,
                         f'mounted filesystem at {label}')

        def fail_walk(error):
            raise error

        # Check before deleting any files. This also catches mounts omitted from
        # the inventory or represented there with a different case/Unicode alias.
        for current, names, _ in os.walk(directory, onerror=fail_walk, followlinks=False):
            for name in names:
                child = Path(current) / name
                if not child.is_symlink():
                    self.assertEqual(child.stat().st_dev, device,
                                     f'mounted filesystem inside {label}')
        shutil.rmtree(directory)

    def cleanup_fixtures(self):
        # Keep fixtures if detach/audit fails: deleting them can traverse a mount.
        self.detach_leftover_test_mounts()
        remaining = self.disk_image_mountpoints()
        for directory in reversed(self.temporary_directories):
            self.remove_clear_directory(directory, remaining, 'fixture directory')

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
      native_hdiutil attach "$BLANK_DMG" -readonly -nobrowse -mountpoint "$HDIUTIL_MOUNTPOINT" -plist || exit $?
    else
      native_hdiutil "$@" || exit $?
    fi
    printf 'simulated Resource busy after mount\\n' >&2
    exit "$ATTACH_ERROR_CODE"
  fi
  mount_device=$(stat -f %d "$HDIUTIL_MOUNTPOINT")
  parent_device=$(stat -f %d "$(dirname "$HDIUTIL_MOUNTPOINT")")
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
native_hdiutil "$@"
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
                         [1, 2, 1])
        self.assertIn('simulated Invalid argument on second attach', result.stderr)

    def test_retained_native_mktemp_mount_fails_helper_assertion(self):
        # This is Astra's original sustained-busy-detach reproducer. The helper
        # must reject it even when native mktemp ignores the fixture's TMPDIR.
        with self.assertRaisesRegex(AssertionError, 'owned test (mount retained|image attached)'):
            self.build_with_mounted_attach_failure(99)
        points = self.recorded_mountpoints()
        self.assertEqual(len(points), 1)
        self.assertEqual(self.mounted_test_mounts(), points)
        # A missing/alternate inventory spelling must not hide a real mount.
        with patch.object(self, 'disk_image_mountpoints', return_value=set()):
            self.assertEqual(self.mounted_test_mounts(), points)
            with self.assertRaisesRegex(AssertionError, 'owned test (mount retained|image attached)'):
                self.assert_no_test_mounts()
            self.detach_leftover_test_mounts()
        self.assert_no_test_mounts()
        self.assertTrue(all(not point.parent.exists() for point in points))

    def test_unrecorded_mount_in_fixture_preserves_data_without_inventory(self):
        # This image belongs to an independent caller, not the builder tracker.
        self.unrecorded_mountpoint = self.base / 'tmp.unrecorded' / 'mount'
        self.unrecorded_mountpoint.mkdir(parents=True)
        foreign_image = self.base / 'foreign.dmg'
        marker = self.unrecorded_mountpoint / 'foreign-data.txt'
        try:
            subprocess.run(['/usr/bin/hdiutil', 'create', '-size', '8m', '-fs', 'HFS+',
                            '-volname', 'Foreign973', str(foreign_image)], check=True,
                           capture_output=True, timeout=60)
            subprocess.run(['/usr/bin/hdiutil', 'attach', str(foreign_image), '-nobrowse',
                            '-mountpoint', str(self.unrecorded_mountpoint)], check=True,
                           capture_output=True, timeout=60)
            marker.write_text('foreign fixture data\n')
            # The physical preflight must protect an unrecorded writable volume
            # even when an inventory spelling mismatch makes it absent here.
            with patch.object(self, 'disk_image_mountpoints', return_value=set()), \
                    patch.object(self, 'disk_image_inventory', return_value=[]):
                with self.assertRaisesRegex(AssertionError, 'mounted filesystem inside fixture'):
                    self.cleanup_fixtures()
                self.assertEqual(marker.read_text(), 'foreign fixture data\n')
                self.assertTrue(self.payload.is_dir())
        finally:
            # The independent caller ejects its own exact device, even on failure.
            for image in self.disk_image_inventory():
                if Path(image.get('image-path', '')).resolve() != foreign_image.resolve():
                    continue
                device = next(entity['dev-entry'] for entity in image['system-entities']
                              if re.fullmatch(r'/dev/disk[0-9]+', entity.get('dev-entry', '')))
                for attempt in range(4):
                    result = subprocess.run(['/usr/bin/hdiutil', 'detach', device],
                                            capture_output=True, timeout=60)
                    if result.returncode == 0:
                        break
                    if attempt == 3:
                        result.check_returncode()
                    time.sleep(0.1 * (attempt + 1))

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
                with self.assertRaisesRegex(AssertionError, 'owned test (mount retained|image attached)'):
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
        with patch.object(self, 'disk_image_inventory', return_value=plistlib.loads(info)['images']):
            self.assertEqual(self.recorded_mountpoints(), {point.resolve()})
            self.assertEqual(self.mounted_test_mounts(), {point.resolve()})

    def mock_attachment(self, point, device='/dev/disk973'):
        image = self.base / 'owned.dmg'
        (self.base / 'attachments').write_text(json.dumps({
            'image': str(image), 'device': device}) + '\n')
        return {'image-path': str(image), 'system-entities': [
            {'dev-entry': device}, {'mount-point': str(point)}]}

    def test_failed_inventory_keeps_original_attach_status_and_fixtures(self):
        shim = self.shim / 'hdiutil'
        shim.write_text(shim.read_text().replace(
            'if [ "$1" = "$HDIUTIL_TEST_OPERATION" ]; then',
            'if [ "$1" = info ]; then exit 9; fi\n'
            'if [ "$1" = "$HDIUTIL_TEST_OPERATION" ]; then'))
        output = self.base / 'output-inventory-error'
        result = subprocess.run(['bash', str(BUILD), str(self.payload), str(output),
                                 VERSION, TARGET], capture_output=True, text=True, timeout=120,
                                env={'PATH': f'{self.shim}:{os.environ.get("PATH", "/usr/bin:/bin")}',
                                     'HDIUTIL_TEST_DIR': str(self.base),
                                     'HDIUTIL_TEST_OPERATION': 'attach',
                                     'HDIUTIL_TEST_FAILURES': '1',
                                     'HDIUTIL_TEST_ERROR': 'Invalid argument',
                                     'HDIUTIL_TEST_EXIT': '5'})
        self.assertEqual(result.returncode, 5, result.stderr)
        self.assertIn('retaining temporary mount for manual cleanup', result.stderr)
        self.assertFalse(list(output.glob('*.sha256')))
        images = self.attached_test_images()
        self.assertEqual(len(images), 1)
        self.assertTrue(all(image.is_file() for image in images))
        self.detach_leftover_test_mounts()
        self.assert_no_test_mounts()

    def test_incomplete_inventory_is_requeried(self):
        info = plistlib.dumps({'images': []})
        warning = b'hdiutil: info: cannot get IO media array for IOHDIXHDDriveOutKernel'
        with patch.object(subprocess, 'run', side_effect=[
                subprocess.CompletedProcess([], 0, info, warning),
                subprocess.CompletedProcess([], 0, info, b'')]) as query:
            self.assertEqual(self.disk_image_inventory(), [])
            self.assertEqual(query.call_count, 2)

    def test_inventory_omitting_a_live_owned_device_is_requeried(self):
        mine = self.mock_attachment(self.base / 'mount')
        image = mine['image-path']
        device = mine['system-entities'][0]['dev-entry']
        missing = subprocess.CompletedProcess([], 0, plistlib.dumps({'images': []}), b'')
        complete = subprocess.CompletedProcess([], 0, plistlib.dumps({'images': [mine]}), b'')
        original_exists = Path.exists
        def exists(path):
            return str(path) == device or original_exists(path)
        with patch.object(Path, 'exists', exists), \
                patch.object(subprocess, 'run', side_effect=[missing, complete]) as query:
            self.assertEqual(set(self.attached_test_images()), {Path(image).resolve()})
            self.assertEqual(query.call_count, 2)

    def test_incomplete_inventory_preserves_fixtures(self):
        self.mock_attachment(self.base / 'mount')
        info = plistlib.dumps({'images': []})
        warning = b'hdiutil: info: cannot get IO media array for IOHDIXHDDriveOutKernel'
        with patch.object(subprocess, 'run', return_value=subprocess.CompletedProcess(
                [], 0, info, warning)) as query:
            with self.assertRaisesRegex(RuntimeError, 'inventory remained incomplete'):
                self.cleanup_fixtures()
            self.assertEqual(query.call_count, 4)
        self.assertTrue(self.payload.is_dir())

    def test_attachment_records_normalize_image_aliases_and_whitespace(self):
        image = self.base / 'image space \t\n\u03a9.dmg'
        image.touch()
        alias = self.base / 'image alias'
        alias.symlink_to(image)
        (self.base / 'attachments').write_text(json.dumps({
            'image': str(alias), 'device': '/dev/disk973'}) + '\n')
        inventory = [{'image-path': str(image), 'system-entities': [
            {'dev-entry': '/dev/disk973'}]}]
        self.assertEqual(set(self.attached_test_images(inventory)), {image.resolve()})

    def test_attachment_records_follow_filesystem_case_and_unicode_equivalence(self):
        image = self.base / 'owned-\u00e9.dmg'
        image.touch()
        alias = self.base / 'OWNED-e\u0301.DMG'
        (self.base / 'attachments').write_text(json.dumps({
            'image': str(alias), 'device': '/dev/disk973'}) + '\n')
        inventory = [{'image-path': str(image), 'system-entities': [
            {'dev-entry': '/dev/disk973'}]}]
        # Match the host filesystem: on a case-sensitive volume this spelling
        # cannot name the attached image and must not confer ownership.
        expected = {image.resolve()} if alias.exists() and alias.samefile(image) else set()
        self.assertEqual(set(self.attached_test_images(inventory)), expected)

    def test_recycled_device_on_foreign_image_does_not_confer_ownership(self):
        point = self.base / 'tmp.gone' / 'mount'
        mine = self.mock_attachment(point)
        foreign = {'image-path': str(self.base.parent / 'foreign.dmg'),
                   'system-entities': [{'dev-entry': mine['system-entities'][0]['dev-entry']}]}
        self.assertEqual(self.attached_test_images([foreign]), {})
        with patch.object(self, 'disk_image_inventory', return_value=[foreign]), \
                patch.object(subprocess, 'run') as detach:
            self.detach_leftover_test_mounts()
            detach.assert_not_called()

    def test_cleanup_ignores_unrecorded_mounts_and_prefix_siblings(self):
        point = self.base / 'tmp.owned' / 'mount'
        neighbor = self.base / 'tmp.owned-other' / 'mount'
        point.mkdir(parents=True)
        (self.base / 'mountpoints').write_bytes(os.fsencode(str(point)) + b'\0')
        mine = self.mock_attachment(point)
        foreign = {'image-path': str(self.base.parent / 'foreign.dmg'),
                   'system-entities': [{'dev-entry': '/dev/disk974'},
                                       {'mount-point': str(neighbor)}]}
        inventory = [mine, foreign]
        def detached(*args, **kwargs):
            inventory.remove(mine)
        with patch.object(self, 'disk_image_inventory', side_effect=lambda: inventory), \
                patch.object(subprocess, 'run', side_effect=detached) as detach:
            self.detach_leftover_test_mounts()
            detach.assert_called_once_with(
                ['/usr/bin/hdiutil', 'detach', '/dev/disk973'], check=True,
                capture_output=True, text=True, timeout=60)
            self.assertEqual(inventory, [foreign])

    def test_cleanup_already_gone_is_repeatable_noop(self):
        point = self.base / 'tmp.gone' / 'mount'
        (self.base / 'mountpoints').write_bytes(os.fsencode(str(point)) + b'\0')
        with patch.object(self, 'disk_image_inventory', return_value=[]), \
                patch.object(subprocess, 'run') as detach:
            self.detach_leftover_test_mounts()
            self.detach_leftover_test_mounts()
            detach.assert_not_called()

    def test_failed_detach_preserves_fixtures(self):
        point = self.base / 'tmp.retained' / 'mount'
        (self.base / 'mountpoints').write_bytes(os.fsencode(str(point)) + b'\0')
        mine = self.mock_attachment(point)
        with patch.object(self, 'disk_image_inventory', return_value=[mine]), \
                patch.object(subprocess, 'run', side_effect=subprocess.CalledProcessError(
                    16, ['/usr/bin/hdiutil', 'detach', '/dev/disk973'])):
            with self.assertRaises(subprocess.CalledProcessError):
                self.cleanup_fixtures()
        self.assertTrue(self.payload.is_dir())

    def test_failed_final_mount_audit_preserves_fixtures(self):
        point = self.base / 'tmp.retained' / 'mount'
        (self.base / 'mountpoints').write_bytes(os.fsencode(str(point)) + b'\0')
        with patch.object(self, 'disk_image_mountpoints', return_value={point.resolve()}), \
                patch.object(subprocess, 'run'):
            with self.assertRaisesRegex(AssertionError, 'owned test (mount retained|image attached)'):
                self.cleanup_fixtures()
        self.assertTrue(self.payload.is_dir())

    def test_cleanup_preserves_work_directory_with_unrecorded_mount(self):
        point = self.base / 'tmp.owned' / 'mount'
        point.mkdir(parents=True)
        neighbor = point.parent / 'mount-other'
        (self.base / 'mountpoints').write_bytes(os.fsencode(str(point)) + b'\0')
        mine = self.mock_attachment(point)
        foreign = {'image-path': str(self.base.parent / 'foreign.dmg'),
                   'system-entities': [{'dev-entry': '/dev/disk974'},
                                       {'mount-point': str(neighbor)}]}
        inventory = [mine, foreign]
        def detached(*args, **kwargs):
            inventory.remove(mine)
        with patch.object(self, 'disk_image_inventory', side_effect=lambda: inventory), \
                patch.object(subprocess, 'run', side_effect=detached) as detach:
            with self.assertRaisesRegex(AssertionError, 'unrecorded mount inside builder'):
                self.detach_leftover_test_mounts()
            self.assertEqual(detach.call_args.args[0],
                             ['/usr/bin/hdiutil', 'detach', '/dev/disk973'])
        self.assertTrue(point.parent.is_dir())

    def attach_fixture(self, *, mountpoint=True, recorded=True):
        image = self.base / 'attached image.dmg'
        subprocess.run(['/usr/bin/hdiutil', 'create', '-size', '8m', '-fs', 'HFS+',
                        '-volname', f'Owned973-{self.base.name}', str(image)], check=True,
                       capture_output=True, timeout=60)
        arguments = ['attach', str(image), '-nobrowse']
        point = self.base / 'mount'
        if mountpoint:
            point.mkdir()
            arguments += ['-mountpoint', str(point)]
        if recorded:
            (self.shim / 'hdiutil').write_text(HDIUTIL_TRACE + 'native_hdiutil "$@"\n')
            command = [str(self.shim / 'hdiutil'), *arguments]
        else:
            command = ['/usr/bin/hdiutil', *arguments]
        subprocess.run(command, check=True, capture_output=True, timeout=60, env={
            'PATH': os.environ.get('PATH', '/usr/bin:/bin'),
            'HDIUTIL_TEST_DIR': str(self.base)})
        attached = next(entry for entry in self.disk_image_inventory()
                        if Path(entry.get('image-path', '')).resolve() == image.resolve())
        device = next(entity['dev-entry'] for entity in attached['system-entities']
                      if re.fullmatch(r'/dev/disk[0-9]+', entity.get('dev-entry', '')))
        return image, device, attached

    def test_attached_unmounted_image_is_detected_and_detached_before_deletion(self):
        image, device, _ = self.attach_fixture()
        subprocess.run(['/usr/sbin/diskutil', 'unmountDisk', device], check=True,
                       capture_output=True, timeout=60)
        self.assertEqual(self.mounted_test_mounts(), set())
        with self.assertRaisesRegex(AssertionError, 'owned test image attached'):
            self.assert_no_test_mounts()
        self.assertIn({'image': str(image.resolve()), 'device': device},
                      self.recorded_attachments())
        native_run = subprocess.run
        detaches = []
        def detach_with_backing_file(*args, **kwargs):
            if args[0][:2] == ['/usr/bin/hdiutil', 'detach']:
                self.assertTrue(image.is_file(), 'backing image deleted before detach')
                self.assertEqual(args[0], ['/usr/bin/hdiutil', 'detach', device])
                detaches.append(args[0])
            return native_run(*args, **kwargs)
        with patch.object(subprocess, 'run', side_effect=detach_with_backing_file):
            self.cleanup_fixtures()
        self.assertEqual(len(detaches), 1)
        self.assertFalse(self.base.exists())
        self.assertFalse(any(entity.get('dev-entry') == device
                             for entry in self.disk_image_inventory()
                             for entity in entry.get('system-entities', [])))
        self.assert_no_test_mounts()

    def test_attach_without_mountpoint_is_recorded_and_cleaned(self):
        image, device, _ = self.attach_fixture(mountpoint=False)
        self.assertEqual(self.recorded_mountpoints(), set())
        self.assertIn({'image': str(image.resolve()), 'device': device},
                      self.recorded_attachments())
        with self.assertRaisesRegex(AssertionError, 'owned test image attached'):
            self.assert_no_test_mounts()
        self.cleanup_fixtures()
        self.assert_no_test_mounts()
        self.assertFalse(image.exists())

    def test_busy_eject_of_unmounted_image_preserves_backing_file(self):
        image, device, _ = self.attach_fixture()
        subprocess.run(['/usr/sbin/diskutil', 'unmountDisk', device], check=True,
                       capture_output=True, timeout=60)
        native_run = subprocess.run
        def busy_detach(*args, **kwargs):
            if args[0][:2] == ['/usr/bin/hdiutil', 'detach']:
                raise subprocess.CalledProcessError(16, args[0])
            return native_run(*args, **kwargs)
        with patch.object(subprocess, 'run', side_effect=busy_detach):
            with self.assertRaises(subprocess.CalledProcessError):
                self.cleanup_fixtures()
        self.assertTrue(image.is_file())
        with self.assertRaisesRegex(AssertionError, 'owned test image attached'):
            self.assert_no_test_mounts()
        self.detach_leftover_test_mounts()
        self.assert_no_test_mounts()

    def test_failed_attachment_reaudit_preserves_backing_file(self):
        image, _, _ = self.attach_fixture()
        native_run = subprocess.run
        def retain_device(*args, **kwargs):
            if args[0][:2] == ['/usr/bin/hdiutil', 'detach']:
                return subprocess.CompletedProcess(args[0], 0)
            return native_run(*args, **kwargs)
        with patch.object(subprocess, 'run', side_effect=retain_device):
            with self.assertRaisesRegex(AssertionError, 'owned test image attached'):
                self.cleanup_fixtures()
        self.assertTrue(image.is_file())
        self.detach_leftover_test_mounts()

    def test_foreign_unmounted_image_in_fixture_is_not_deleted_or_detached(self):
        image, device, _ = self.attach_fixture(recorded=False)
        try:
            subprocess.run(['/usr/sbin/diskutil', 'unmountDisk', device], check=True,
                           capture_output=True, timeout=60)
            self.assert_no_test_mounts()
            with self.assertRaisesRegex(AssertionError, 'attached image inside fixture'):
                self.cleanup_fixtures()
            self.assertTrue(image.is_file())
            self.assertTrue(any(entity.get('dev-entry') == device
                                for entry in self.disk_image_inventory()
                                for entity in entry.get('system-entities', [])))
        finally:
            subprocess.run(['/usr/bin/hdiutil', 'detach', device], check=True,
                           capture_output=True, timeout=60)

    def build_with_busy_eject(self, *, already_ejected=False, attach_error=False, failures=1):
        (self.shim / 'hdiutil').write_text(HDIUTIL_TRACE + '''
if [ "$1" = attach ] && [ "$ATTACH_ERROR" = 1 ]; then
  native_hdiutil "$@" || exit $?
  printf 'simulated Resource busy after attach\\n' >&2
  exit 16
fi
if [ "$1" = detach ]; then
  count=0
  [ ! -f "$HDIUTIL_TEST_DIR/eject-count" ] || count=$(cat "$HDIUTIL_TEST_DIR/eject-count")
  count=$((count + 1))
  printf '%s\\n' "$count" > "$HDIUTIL_TEST_DIR/eject-count"
  if [ "$count" -le "$EJECT_FAILURES" ]; then
  if [ "$ALREADY_EJECTED" = 1 ]; then
    native_hdiutil "$@" || exit $?
    printf 'simulated already-ejected error\\n' >&2
    exit 1
  fi
  if [ "$count" = 1 ]; then /usr/sbin/diskutil unmountDisk "$2" >/dev/null || exit $?; fi
  printf 'simulated could not eject %s - Resource busy\\n' "$2" >&2
  exit 16
  fi
fi
native_hdiutil "$@"
''')
        output = self.base / 'output-busy-eject'
        result = subprocess.run(['bash', str(BUILD), str(self.payload), str(output),
                                 VERSION, TARGET], capture_output=True, text=True, timeout=120,
                                env={'PATH': f'{self.shim}:{os.environ.get("PATH", "/usr/bin:/bin")}',
                                     'HDIUTIL_TEST_DIR': str(self.base),
                                     'ALREADY_EJECTED': '1' if already_ejected else '0',
                                     'ATTACH_ERROR': '1' if attach_error else '0',
                                     'EJECT_FAILURES': str(failures)})
        return result, output, (self.base / 'calls').read_text().splitlines()

    def test_builder_retries_busy_eject_by_device_after_unmount(self):
        result, output, calls = self.build_with_busy_eject()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('Resource busy', result.stderr)
        self.assertEqual(calls.count('detach'), 2)
        self.assertTrue(list(output.glob('*.dmg.sha256')))
        self.assert_no_test_mounts()

    def test_builder_persistent_busy_eject_is_bounded_and_retains_attachment(self):
        result, output, calls = self.build_with_busy_eject(failures=99)
        self.assertEqual(result.returncode, 16, result.stderr)
        self.assertEqual(calls.count('detach'), 5)  # Four attempts plus the exit trap.
        self.assertIn('failed after 4 attempts', result.stderr)
        self.assertIn('retaining temporary mount for manual cleanup', result.stderr)
        self.assertFalse(list(output.glob('*.sha256')))
        images = self.attached_test_images()
        self.assertEqual(len(images), 1)
        self.assertTrue(all(image.is_file() for image in images))
        self.assertEqual(self.mounted_test_mounts(), set())
        self.detach_leftover_test_mounts()
        self.assert_no_test_mounts()

    def test_builder_already_ejected_device_is_success(self):
        result, _, calls = self.build_with_busy_eject(already_ejected=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(calls.count('detach'), 1)
        self.assert_no_test_mounts()

    def test_partial_attach_retries_busy_eject_after_unmount(self):
        # Both attach attempts report busy after attachment. Each is fully
        # detached before retry, including the first unmount-then-busy eject.
        result, _, calls = self.build_with_busy_eject(attach_error=True)
        self.assertEqual(result.returncode, 16, result.stderr)
        self.assertEqual(calls.count('attach'), 4)
        self.assertEqual(calls.count('detach'), 5)
        self.assert_no_test_mounts()


if __name__ == '__main__':
    unittest.main()
