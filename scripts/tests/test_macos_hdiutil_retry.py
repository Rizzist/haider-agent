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
if [ "$1" = create ]; then printf '%s\\0' "${@: -1}" >> "$HDIUTIL_TEST_DIR/createpaths"; fi
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
  if [ "$1" != attach ]; then
    python3 - "$@" <<'NATIVE_PY'
import json, os, pathlib, subprocess, sys
result = subprocess.run(['/usr/bin/hdiutil', *sys.argv[1:]], capture_output=True)
with (pathlib.Path(os.environ['HDIUTIL_TEST_DIR']) / 'native-results').open('a') as output:
    output.write(json.dumps({'action': sys.argv[1], 'status': result.returncode,
                             'stderr': os.fsdecode(result.stderr)}) + '\\n')
sys.stdout.buffer.write(result.stdout)
sys.stderr.buffer.write(result.stderr)
sys.exit(result.returncode)
NATIVE_PY
    return $?
  fi
  python3 - "__IDENTITY_HELPER__" "$@" <<'TRACE_PY'
import importlib.util, json, os, pathlib, plistlib, re, subprocess, sys, time
spec = importlib.util.spec_from_file_location("identity_helper", sys.argv[1])
helper = importlib.util.module_from_spec(spec); spec.loader.exec_module(helper)
arguments = sys.argv[2:]
# hdiutil accepts flags before the image, including flags with values.
value_flags = {'-mount', '-mountpoint', '-mountroot', '-mountrandom', '-shadow',
               '-imagekey', '-drivekey', '-encryption', '-recover', '-cacert'}
positional = []
index = 1
while index < len(arguments):
    argument = arguments[index]
    if argument in value_flags:
        index += 2
    elif argument.startswith('-'):
        index += 1
    else:
        positional.append(argument)
        index += 1
if len(positional) != 1:
    sys.exit('cannot identify hdiutil attach image argument')
image = str(pathlib.Path(positional[0]).resolve())
expected = helper.identity(pathlib.Path(image).stat())
record = pathlib.Path(os.environ['HDIUTIL_TEST_DIR']) / 'attachments'
def append(device):
    with record.open('a') as output:
        output.write(json.dumps({'image': image, 'device': device, 'identity': expected}) + '\\n')
def same_image(path):
    path = pathlib.Path(path).resolve()
    if str(path) == image:
        return True
    try:
        return path.samefile(image)
    except OSError:
        return False
def inventory_devices():
    query = subprocess.run(['/usr/bin/hdiutil', 'info', '-plist'], capture_output=True, check=True)
    info = plistlib.loads(query.stdout)
    if b'cannot get IO media array' in query.stderr:
        sys.exit('incomplete pre-attach inventory')
    return {entity['dev-entry'] for attached in info.get('images', [])
            if same_image(attached.get('image-path', ''))
            for entity in attached.get('system-entities', [])
            if re.fullmatch(r'/dev/disk[0-9]+', entity.get('dev-entry', ''))}
# Preserve the attempted path, but a path without a device grants no ownership.
append(None)
previous = inventory_devices()
if '-plist' not in arguments: arguments.append('-plist')
result = subprocess.run(['/usr/bin/hdiutil', *arguments], capture_output=True)
with (record.parent / 'native-results').open('a') as output:
    output.write(json.dumps({'action': arguments[0], 'status': result.returncode,
                             'stderr': os.fsdecode(result.stderr)}) + '\\n')
sys.stdout.buffer.write(result.stdout)
sys.stderr.buffer.write(result.stderr)
try:
    attached = plistlib.loads(result.stdout)
except ValueError:
    attached = {}
devices = {entity['dev-entry'] for entity in attached.get('system-entities', [])
           if re.fullmatch(r'/dev/disk[0-9]+', entity.get('dev-entry', ''))}
# Neither a recyclable plist device nor a path delta proves backing ownership.
for post_attempt in range(4):
    query = subprocess.run(['/usr/bin/hdiutil', 'info', '-plist'], capture_output=True, check=True)
    # Post-attach inventory fault-injection seam.
    info = plistlib.loads(query.stdout)
    backing = {entity['dev-entry']: attached for attached in info.get('images', [])
               for entity in attached.get('system-entities', []) if 'dev-entry' in entity}
    incomplete = b'cannot get IO media array' in query.stderr or any(
        pathlib.Path(device).exists() and device not in backing for device in devices)
    if not incomplete:
        break
    if post_attempt == 3:
        sys.exit('incomplete post-attach inventory; keeping everything')
    time.sleep(0.1 * (post_attempt + 1))
verified = {}
def is_expected(device):
    if device not in backing:
        if pathlib.Path(device).exists():
            sys.exit('live attach device missing from post-attach inventory; keeping everything')
        return False
    attached = backing[device]; pid = attached.get('hdid-pid')
    if pid not in verified: verified[pid] = helper.backing_identity(attached, expected)
    if verified[pid] is None: sys.exit("backing inode unavailable; keeping everything")
    return verified[pid]
for device in (devices if devices else inventory_devices() - previous):
    if device not in previous and is_expected(device):
        append(device)
sys.exit(result.returncode)
TRACE_PY
}
'''.replace('__IDENTITY_HELPER__', str(ROOT / 'packaging/installers/macos-image-identity.py'))


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
        owned = [(Path(record['image']).resolve(), record['device'])
                 for record in self.recorded_attachments() if record['device']]
        if not owned:
            return {}
        if inventory is None:
            inventory = self.disk_image_inventory()
        def is_owned(path, device):
            for recorded_path, recorded_device in owned:
                if device != recorded_device:
                    continue
                if path == recorded_path:
                    return True
                try:
                    if path.samefile(recorded_path):
                        return True
                except OSError:
                    pass
            return False
        import importlib.util
        spec = importlib.util.spec_from_file_location('image_identity',
            ROOT / 'packaging/installers/macos-image-identity.py')
        helper = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(helper)
        attached = {}
        for image in inventory:
            if 'image-path' not in image:
                continue
            path = Path(image['image-path']).resolve()
            identities = [r.get('identity') for r in self.recorded_attachments()
                          if r.get('device') and is_owned(path, r['device'])]
            if identities and all(i is not None for i in identities):
                matching = [helper.backing_identity(image, i) for i in identities]
                if not any(matching):
                    if None in matching:
                        raise RuntimeError('recorded backing inode unavailable; keeping fixtures')
                    continue
            entities = [entity for entity in image.get('system-entities', [])
                        if is_owned(path, entity.get('dev-entry'))]
            if entities:
                attached.setdefault(path, {'image-path': str(path), 'system-entities': []})[
                    'system-entities'].extend(entities)
        return attached

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
            for device in devices:
                # Re-audit each identity before using a recyclable device name.
                current = self.attached_test_images().get(Path(image['image-path']).resolve(), {})
                if not any(entity.get('dev-entry') == device
                           for entity in current.get('system-entities', [])):
                    continue
                subprocess.run(['/usr/bin/hdiutil', 'detach', device], check=True,
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
        root = directory.resolve()
        def image_is_inside(image):
            path = Path(image['image-path']).resolve()
            if path.is_relative_to(root):
                return True
            # realpath does not normalize filesystem case/Unicode aliases.
            # An aliased parent can still be this exact directory, even when
            # an attached image's backing file has already been removed.
            for parent in path.parents:
                try:
                    if parent.samefile(root):
                        return True
                except OSError:
                    pass
            return False
        # Protect even foreign backing files whose volumes are already unmounted.
        self.assertFalse(any(image_is_inside(image) for image in self.disk_image_inventory()
                             if 'image-path' in image),
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
        created = self.base / 'createpaths'
        if created.exists():
            for raw in created.read_bytes().split(b'\0'):
                if raw:
                    directory = Path(os.fsdecode(raw)).resolve().parent.parent
                    if directory.name.startswith('tmp.'):
                        self.remove_clear_directory(directory, remaining, 'builder create directory')
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
      cat "$BLANK_DMG" > "$2" || exit $?
    fi
    native_hdiutil "$@" || exit $?
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

    def assert_busy_attempts(self, operation, calls):
        native = [json.loads(line) for line in
                  (self.base / 'native-results').read_text().splitlines()]
        relevant = {operation, 'attach'} if operation == 'detach' else {operation}
        failures = [record for record in native if record['action'] in relevant
                    and record['status'] != 0]
        for record in failures:
            self.assertTrue(record['status'] == 16 or re.search(
                r'resource busy|device busy|resource temporarily unavailable|\bebusy\b',
                record['stderr'], re.IGNORECASE), record)
        # The injected busy response needs a retry. Real additional
        # busy responses may also need retries, or partial-attach ejects.
        # Every extra call must be explained by a recorded native busy
        # failure; create/attach each still have a four-attempt ceiling.
        attempts = calls.count(operation)
        attach_failures = sum(record['action'] == 'attach' for record in failures)
        ceiling = 4 + 5 * attach_failures if operation == 'detach' else 4
        self.assertGreaterEqual(attempts, 2)
        self.assertLessEqual(attempts, ceiling)
        self.assertLessEqual(attempts - 2, len(failures))


    def test_busy_create_attach_and_detach_recover(self):
        for operation in ('create', 'attach', 'detach'):
            with self.subTest(operation=operation):
                (self.base / 'calls').unlink(missing_ok=True)
                (self.base / 'count').unlink(missing_ok=True)
                (self.base / 'native-results').unlink(missing_ok=True)
                result, output, calls = self.build(operation, 1)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn('macOS post-pack gate:', result.stdout)
                self.assertIn('macOS installer gate PASS', result.stdout)
                self.assert_busy_attempts(operation, calls)
                name = f'haider-v{VERSION}-{TARGET}'
                for extension in ('.pkg', '.dmg'):
                    artifact = output / f'{name}{extension}'
                    self.assertTrue(artifact.is_file())
                    digest = hashlib.sha256(artifact.read_bytes()).hexdigest()
                    self.assertEqual((output / f'{name}{extension}.sha256').read_text(),
                                     f'{digest}  {artifact.name}\n')

    def test_nonempty_busy_create_partial_is_cleaned_and_retried(self):
        shim = self.shim / 'hdiutil'
        shim.write_text(shim.read_text().replace(
            'if [ "$1" = create ]; then touch "${@: -1}"; fi',
            'if [ "$1" = create ]; then /bin/dd if=/dev/zero of="${@: -1}" bs=1048576 count=1 2>/dev/null; fi'))
        result, output, calls = self.build('create', 1)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(calls.count('create'), 2)
        self.assertIn('macOS installer gate PASS', result.stdout)
        paths = [Path(os.fsdecode(p)) for p in (self.base / 'createpaths').read_bytes().split(b'\0') if p]
        self.assertEqual(len(paths), 2)
        self.assertNotEqual(paths[0].parent, paths[1].parent)
        self.assertTrue(all(not p.parent.parent.exists() for p in paths), 'PASS retained work directory')
        self.assertTrue(list(output.glob('*.dmg.sha256')))
        self.assertNotIn('retaining ', result.stderr.lower())

    def test_nonempty_busy_create_is_still_bounded(self):
        shim = self.shim / 'hdiutil'
        shim.write_text(shim.read_text().replace(
            'if [ "$1" = create ]; then touch "${@: -1}"; fi',
            'if [ "$1" = create ]; then echo partial > "${@: -1}"; fi'))
        result, output, calls = self.build('create', 4)
        self.assertEqual(result.returncode, 16, result.stderr)
        self.assertEqual(calls.count('create'), 4)
        self.assertFalse(list(output.glob('*.sha256')))
        self.assertNotIn('Retaining unattributed create output in ', result.stderr)

    def test_failed_busy_create_can_be_rerun_into_the_same_output(self):
        result, output, calls = self.build('create', 4)
        self.assertEqual(result.returncode, 16, result.stderr)
        self.assertEqual(calls.count('create'), 4)
        self.assertFalse(list(output.glob('*.pkg')))
        self.assertFalse(list(output.glob('*.sha256')))
        result, rerun_output, _ = self.build('create', 0)
        self.assertEqual(output, rerun_output)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('macOS installer gate PASS', result.stdout)
        self.assertEqual(len(list(output.iterdir())), 4)

    def test_failed_package_validation_can_be_rerun_into_the_same_output(self):
        verifier = self.shim / 'pkgutil'
        verifier.write_text('#!/bin/bash\nexit 7\n')
        verifier.chmod(0o755)
        output = self.base / 'failed-validation-output'
        result = self.run_builder(output)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertFalse(list(output.iterdir()))
        verifier.unlink()
        result = self.run_builder(output)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('macOS installer gate PASS', result.stdout)

    def test_builder_preserves_a_preexisting_foreign_package(self):
        output = self.base / 'foreign-pkg-output'
        output.mkdir()
        pkg = output / f'haider-v{VERSION}-{TARGET}.pkg'
        pkg.write_bytes(b'foreign package')
        expected = (pkg.stat().st_dev, pkg.stat().st_ino)
        result = self.run_builder(output)
        self.assertNotEqual(result.returncode, 0, result.stderr)
        self.assertEqual(pkg.read_bytes(), b'foreign package')
        self.assertEqual((pkg.stat().st_dev, pkg.stat().st_ino), expected)
        self.assertFalse(list(output.glob('*.sha256')))

    def test_busy_partial_attached_before_retry_is_retained_with_original_status(self):
        # The native tool finishes before the helper records staging identity.
        # The first callback inventory then observes an independent attach.
        (self.shim / 'hdiutil').write_text(HDIUTIL_TRACE + '''
if [ "$1" = create ] && [ ! -f "$HDIUTIL_TEST_DIR/partial-path" ]; then
  native_hdiutil "$@" || exit $?
  printf '%s' "${@: -1}" > "$HDIUTIL_TEST_DIR/partial-path"
  echo 'simulated Resource busy with valid partial' >&2
  exit 16
fi
if [ "$1" = info ] && [ -f "$HDIUTIL_TEST_DIR/partial-path" ] && [ ! -f "$HDIUTIL_TEST_DIR/partial-device" ]; then
  python3 - <<'FOREIGN_PARTIAL'
import os, pathlib, plistlib, re, subprocess
base = pathlib.Path(os.environ['HDIUTIL_TEST_DIR'])
raw = pathlib.Path((base / 'partial-path').read_text())
result = subprocess.run(['/usr/bin/hdiutil', 'attach', str(raw), '-readonly', '-nobrowse', '-nomount', '-plist'],
                        capture_output=True, check=True)
entities = plistlib.loads(result.stdout)['system-entities']
device = next(e['dev-entry'] for e in entities if re.fullmatch(r'/dev/disk[0-9]+', e.get('dev-entry', '')))
(base / 'partial-device').write_text(device)
FOREIGN_PARTIAL
  [ "$?" = 0 ] || exit $?
fi
native_hdiutil "$@"
''')
        device = None
        try:
            result = self.run_builder(self.base / 'attached-partial-output')
            raw = Path((self.base / 'partial-path').read_text())
            device = (self.base / 'partial-device').read_text()
            self.assertEqual(result.returncode, 16, result.stderr)
            self.assertEqual((self.base / 'calls').read_text().splitlines().count('create'), 1)
            self.assertIn('Retaining unattributed create output', result.stderr)
            self.assertTrue(raw.exists())
            self.assertEqual(Path(self.image_for_device(device)['image-path']).resolve(), raw.resolve())
        finally:
            record = self.base / 'partial-device'
            if record.exists():
                device = record.read_text()
                self.recover_foreign(Path((self.base / 'partial-path').read_text()), device)

    def test_builder_runs_with_stock_python39(self):
        stock = Path('/usr/bin/python3')
        if not stock.exists(): self.skipTest('stock Python unavailable')
        version = subprocess.check_output([str(stock), '-c', 'import sys; print("%s.%s" % sys.version_info[:2])'], text=True).strip()
        if version != '3.9': self.skipTest('stock Python is ' + version)
        (self.shim / 'python3').symlink_to(stock)
        result = self.run_builder(self.base / 'python39-output')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('macOS installer gate PASS', result.stdout)

    def test_failed_builder_removes_verified_unattached_staging(self):
        (self.shim / 'cmp').write_text('#!/bin/bash\necho "forced cmp failure" >&2\nexit 7\n')
        (self.shim / 'cmp').chmod(0o755)
        result, output, calls = self.build_with_busy_eject()
        self.assertEqual(result.returncode, 7, result.stderr)
        self.assertEqual(calls.count('detach'), 2)
        self.assertFalse(list(output.glob('*.sha256')))
        paths = [Path(os.fsdecode(p)) for p in (self.base / 'createpaths').read_bytes().split(b'\0') if p]
        self.assertTrue(paths)
        self.assertTrue(all(not p.parent.parent.exists() for p in paths))
        self.assertNotIn('Retaining unattributed create output in ', result.stderr)
        self.assert_no_test_mounts()

    def test_successful_builder_removes_work_directory(self):
        result, _, _ = self.build('create', 0)
        self.assertEqual(result.returncode, 0, result.stderr)
        paths = [Path(os.fsdecode(p)) for p in (self.base / 'createpaths').read_bytes().split(b'\0') if p]
        self.assertTrue(paths)
        self.assertTrue(all(not p.parent.parent.exists() for p in paths))
        self.assertNotIn('Retaining unattributed create output in ', result.stderr)

    def test_package_replaced_after_expansion_is_refused(self):
        (self.shim / 'pkgutil').write_text('''#!/bin/bash
/usr/sbin/pkgutil "$@" || exit $?
if [ "$1" = --expand-full ]; then
  /bin/rm -f "$PACKAGE_OUTPUT"; printf 'foreign pkg\n' > "$PACKAGE_OUTPUT"
fi
''')
        (self.shim / 'pkgutil').chmod(0o755)
        output = self.base / 'early-pkg-output'
        pkg = output / f'haider-v{VERSION}-{TARGET}.pkg'
        result = self.run_builder(output, PACKAGE_OUTPUT=str(pkg))
        self.assertNotEqual(result.returncode, 0, result.stderr)
        self.assertIn('identity changed', result.stderr)
        self.assertEqual(pkg.read_text(), 'foreign pkg\n')
        self.assertFalse(list(output.glob('*.sha256')))
        self.assertFalse(list(output.glob('*.dmg')))

    def test_package_replaced_after_detach_is_refused(self):
        (self.shim / 'hdiutil').write_text(HDIUTIL_TRACE + '''
if [ "$1" = detach ] && [ ! -f "$HDIUTIL_TEST_DIR/pkg-swapped" ]; then
  touch "$HDIUTIL_TEST_DIR/pkg-swapped"
  /bin/rm -f "$PACKAGE_OUTPUT"; printf 'foreign pkg\n' > "$PACKAGE_OUTPUT"
fi
native_hdiutil "$@"
''')
        output = self.base / 'late-pkg-output'
        pkg = output / f'haider-v{VERSION}-{TARGET}.pkg'
        result = self.run_builder(output, PACKAGE_OUTPUT=str(pkg))
        self.assertNotEqual(result.returncode, 0, result.stderr)
        self.assertIn('identity changed', result.stderr)
        self.assertEqual(pkg.read_text(), 'foreign pkg\n')
        self.assertFalse(list(output.glob('*.sha256')))
        self.assert_no_test_mounts()

    def test_package_inplace_change_after_validation_is_refused(self):
        (self.shim / 'hdiutil').write_text(HDIUTIL_TRACE + """
if [ "$1" = detach ]; then
  printf 'foreign in-place package' > "$PACKAGE_OUTPUT"
fi
native_hdiutil "$@"
""")
        output = self.base / 'inplace-pkg-output'
        pkg = output / f'haider-v{VERSION}-{TARGET}.pkg'
        result = self.run_builder(output, PACKAGE_OUTPUT=str(pkg))
        self.assertNotEqual(result.returncode, 0, result.stderr)
        self.assertIn('package bytes changed since validation', result.stderr)
        self.assertEqual(pkg.read_text(), 'foreign in-place package')
        self.assertFalse(list(output.glob('*.sha256')))
        self.assert_no_test_mounts()

    def test_builder_refuses_each_preplanted_sidecar_symlink(self):
        for extension in ('pkg', 'dmg'):
            with self.subTest(extension=extension):
                output = self.base / f'sidecar-output-{extension}'
                output.mkdir()
                victim = self.base / f'victim-{extension}'
                victim.write_bytes(b'leave me alone')
                sidecar = output / f'haider-v{VERSION}-{TARGET}.{extension}.sha256'
                sidecar.symlink_to(victim)
                result = self.run_builder(output)
                self.assertNotEqual(result.returncode, 0, result.stderr)
                self.assertNotIn('macOS installer gate PASS', result.stdout)
                self.assertEqual(victim.read_bytes(), b'leave me alone')
                self.assertTrue(sidecar.is_symlink())
                self.assert_no_test_mounts()

    def test_builder_dmg_replaced_between_detach_and_checksum_is_refused(self):
        (self.shim / 'hdiutil').write_text(HDIUTIL_TRACE + '''
if [ "$1" = detach ] && [ ! -f "$HDIUTIL_TEST_DIR/dmg-swapped" ]; then
  touch "$HDIUTIL_TEST_DIR/dmg-swapped"
  /bin/rm -f "$IMAGE_OUTPUT"; printf 'foreign dmg\n' > "$IMAGE_OUTPUT"
fi
native_hdiutil "$@"
''')
        output = self.base / 'late-dmg-output'
        dmg = output / f'haider-v{VERSION}-{TARGET}.dmg'
        result = self.run_builder(output, IMAGE_OUTPUT=str(dmg))
        self.assertNotEqual(result.returncode, 0, result.stderr)
        self.assertIn('identity changed', result.stderr)
        self.assertNotIn('macOS installer gate PASS', result.stdout)
        self.assertEqual(dmg.read_text(), 'foreign dmg\n')
        self.assertFalse(Path(str(dmg) + '.sha256').exists())
        self.assert_no_test_mounts()

    def test_builder_dmg_inplace_change_after_detach_is_refused(self):
        (self.shim / 'hdiutil').write_text(HDIUTIL_TRACE + '''
if [ "$1" = detach ] && [ ! -f "$HDIUTIL_TEST_DIR/dmg-edited" ]; then
  native_hdiutil "$@" || exit $?
  touch "$HDIUTIL_TEST_DIR/dmg-edited"
  python3 - "$IMAGE_OUTPUT" <<'EDIT_DMG'
import os, pathlib, sys
path = pathlib.Path(sys.argv[1]); before = path.stat()
with path.open('r+b') as image:
    image.seek(before.st_size // 2); image.write(b'foreign in-place bytes')
assert (path.stat().st_dev, path.stat().st_ino) == (before.st_dev, before.st_ino)
EDIT_DMG
  exit $?
fi
native_hdiutil "$@"
''')
        output = self.base / 'inplace-dmg-output'
        dmg = output / f'haider-v{VERSION}-{TARGET}.dmg'
        result = self.run_builder(output, IMAGE_OUTPUT=str(dmg))
        self.assertNotEqual(result.returncode, 0, result.stderr)
        self.assertIn('disk image bytes changed since validation', result.stderr)
        self.assertNotIn('macOS installer gate PASS', result.stdout)
        self.assertFalse(Path(str(dmg) + '.sha256').exists())
        self.assertFalse(list(output.glob('*.pkg')))
        self.assertFalse(list(output.glob('*.pkg.sha256')))
        self.assert_no_test_mounts()

    def test_builder_changed_dmg_is_refused_before_comparison(self):
        (self.shim / 'hdiutil').write_text(HDIUTIL_TRACE + '''
if [ "$1" = attach ]; then
  native_hdiutil "$@" || exit $?
  /bin/rm -f "$2"; printf 'foreign dmg\n' > "$2"
  exit 0
fi
native_hdiutil "$@"
''')
        (self.shim / 'cmp').write_text('''#!/bin/bash
touch "$HDIUTIL_TEST_DIR/comparison-ran"
exec /usr/bin/cmp "$@"
''')
        (self.shim / 'cmp').chmod(0o755)
        result = self.run_builder(self.base / 'changed-before-comparison')
        self.assertNotEqual(result.returncode, 0, result.stderr)
        self.assertFalse((self.base / 'comparison-ran').exists())
        self.assert_no_test_mounts()

    def test_builder_pins_created_dmg_until_attachment_finishes(self):
        (self.shim / 'hdiutil').write_text(HDIUTIL_TRACE + '''
if [ "$1" = attach ]; then
  /usr/sbin/lsof -a -p "$PPID" -d 9 -Fn > "$HDIUTIL_TEST_DIR/pinned-dmg"
fi
native_hdiutil "$@"
''')
        result = self.run_builder(self.base / 'pinned-dmg-output')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('haider-v' + VERSION + '-' + TARGET + '.dmg',
                      (self.base / 'pinned-dmg').read_text())

    def test_plist_capture_excludes_other_new_expected_inode_device(self):
        # Inject an additional inventory entry sharing the real helper's inode.
        # The nonexistent device is never passed to native detach. Plist-present
        # attribution must record only devices returned by this attach.
        (self.shim / 'hdiutil').write_text(HDIUTIL_TRACE + '''
if [ "$1" = attach ]; then
  native_hdiutil "$@" > "$HDIUTIL_TEST_DIR/real-attach.plist" || exit $?
  touch "$HDIUTIL_TEST_DIR/extra-inode-entry"
  cat "$HDIUTIL_TEST_DIR/real-attach.plist"
  exit 0
fi
if [ "$1" = info ] && [ -f "$HDIUTIL_TEST_DIR/extra-inode-entry" ]; then
  python3 - <<'EXTRA'
import copy, os, pathlib, plistlib, subprocess, sys
root = pathlib.Path(os.environ['HDIUTIL_TEST_DIR'])
info = plistlib.loads(subprocess.check_output(['/usr/bin/hdiutil', 'info', '-plist']))
original = plistlib.loads((root / 'real-attach.plist').read_bytes())
devices = {e.get('dev-entry') for e in original['system-entities']}
for image in list(info.get('images', [])):
    if any(e.get('dev-entry') in devices for e in image.get('system-entities', [])):
        extra = copy.deepcopy(image)
        extra['system-entities'] = [{'dev-entry': '/dev/disk999973'}]
        info['images'].append(extra)
plistlib.dump(info, sys.stdout.buffer)
EXTRA
  exit 0
fi
if [ "$1" = detach ] && [ "$2" = /dev/disk999973 ]; then exit 99; fi
native_hdiutil "$@"
''')
        (self.shim / 'cmp').write_text('''#!/bin/bash
work=$(dirname "$(dirname "$2")")
cp "$work/attached-image.json" "$HDIUTIL_TEST_DIR/captured-ownership.json"
exec /usr/bin/cmp "$@"
''')
        (self.shim / 'cmp').chmod(0o755)
        result = self.run_builder(self.base / 'plist-only-output')
        self.assertEqual(result.returncode, 0, result.stderr)
        saved = json.loads((self.base / 'captured-ownership.json').read_text())
        self.assertTrue(saved)
        self.assertNotIn('/dev/disk999973', [r['device'] for r in saved])
        self.assert_no_test_mounts()

    def test_changed_publication_is_refused_before_native_attach(self):
        # Replace after the first post-create check. The next pre-attach check
        # must refuse before any native attach observes the replacement.
        (self.shim / 'python3').write_text('''#!/bin/bash
"$REAL_PYTHON" "$@" || exit $?
if [ "$2" = check ] && [ ! -f "$HDIUTIL_TEST_DIR/pre-attach-swapped" ]; then
  touch "$HDIUTIL_TEST_DIR/pre-attach-swapped"
  /bin/rm -f "$3"; printf 'foreign before attach\n' > "$3"
fi
''')
        (self.shim / 'python3').chmod(0o755)
        result = self.run_builder(self.base / 'pre-attach-output', REAL_PYTHON=__import__('sys').executable)
        self.assertNotEqual(result.returncode, 0, result.stderr)
        calls = (self.base / 'calls').read_text().splitlines()
        self.assertNotIn('attach', calls)
        self.assertIn('identity changed', result.stderr)

    def test_busy_recovery_rejects_unexplained_or_excessive_attempts(self):
        for attempts, native_busy in ((3, 0), (5, 3)):
            with self.subTest(attempts=attempts, native_busy=native_busy):
                records = [{'action': 'create', 'status': 16,
                            'stderr': 'Resource busy'}] * native_busy
                records.append({'action': 'create', 'status': 0, 'stderr': ''})
                (self.base / 'native-results').write_text(
                    ''.join(json.dumps(record) + '\n' for record in records))
                with self.assertRaisesRegex(AssertionError, 'not less than or equal'):
                    self.assert_busy_attempts('create', ['create'] * attempts)

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
        self.assertTrue('\n'.join(line for line in result.stderr.splitlines()
                                 if not line.startswith('Retaining unattributed create output'))
                        .rstrip().endswith('simulated Resource busy after mount'),
                        result.stderr)
        self.assertNotIn('attach retried before previous mount was detached', result.stderr)
        self.assertFalse(list(output.glob('*.sha256')))

    def test_failed_cleanup_keeps_original_attach_status(self):
        result, _, calls = self.build_with_mounted_attach_failure(5, attach_error_code=5)
        self.assertEqual(result.returncode, 5, result.stderr)
        self.assertEqual([calls.count(action) for action in ('attach', 'detach')], [1, 6])
        self.assertIn('simulated Resource busy after mount', result.stderr)
        self.assertTrue('\n'.join(line for line in result.stderr.splitlines()
                                 if not line.startswith('Retaining unattributed create output'))
                        .rstrip().endswith('simulated Resource busy after mount'),
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
        self.assertTrue('\n'.join(line for line in result.stderr.splitlines()
                                 if not line.startswith('Retaining unattributed create output'))
                        .rstrip().endswith('simulated Resource busy after mount'),
                        result.stderr)

    def test_failed_partial_dmg_removal_stops_create_retry(self):
        # Replace the reserved publication inode during a failing create.
        # Identity refusal is a cleanup failure and must preserve the status.
        script = self.shim / 'hdiutil'
        script.write_text(script.read_text().replace(
            'if [ "$1" = create ]; then touch "${@: -1}"; fi',
            'if [ "$1" = create ]; then touch "${@: -1}"; '
            '/bin/rm -f "$FAILED_DMG"; echo foreign > "$FAILED_DMG"; fi'))
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
        self.assertIn('disk image identity changed', result.stderr)
        self.assertEqual(dmg.read_text(), 'foreign\n')

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
        foreign_device = None
        try:
            subprocess.run(['/usr/bin/hdiutil', 'create', '-size', '8m', '-fs', 'HFS+',
                            '-volname', 'Foreign973', str(foreign_image)], check=True,
                           capture_output=True, timeout=60)
            attached = plistlib.loads(subprocess.check_output(
                ['/usr/bin/hdiutil', 'attach', str(foreign_image), '-nobrowse',
                 '-mountpoint', str(self.unrecorded_mountpoint), '-plist'], timeout=60))
            foreign_device = next(entity['dev-entry'] for entity in attached['system-entities']
                                  if re.fullmatch(r'/dev/disk[0-9]+', entity.get('dev-entry', '')))
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
            if foreign_device:
                self.recover_foreign(foreign_image, foreign_device)

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
                                 [1, 1, 9])
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
            'if [ "$1" = info ] && [ -f "$HDIUTIL_TEST_DIR/attachments" ]; then exit 9; fi\n'
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
        arguments = ['attach', str(image), '-nobrowse', '-plist']
        point = self.base / 'mount'
        if mountpoint:
            point.mkdir()
            arguments += ['-mountpoint', str(point)]
        if recorded:
            (self.shim / 'hdiutil').write_text(HDIUTIL_TRACE + 'native_hdiutil "$@"\n')
            command = [str(self.shim / 'hdiutil'), *arguments]
        else:
            command = ['/usr/bin/hdiutil', *arguments]
        result = subprocess.run(command, check=True, capture_output=True, timeout=60, env={
            'PATH': os.environ.get('PATH', '/usr/bin:/bin'),
            'HDIUTIL_TEST_DIR': str(self.base)})
        own = plistlib.loads(result.stdout)
        device = next(entity['dev-entry'] for entity in own['system-entities']
                      if re.fullmatch(r'/dev/disk[0-9]+', entity.get('dev-entry', '')))
        attached = next(entry for entry in self.disk_image_inventory()
                        if Path(entry.get('image-path', '')).resolve() == image.resolve()
                        and any(entity.get('dev-entry') == device
                                for entity in entry.get('system-entities', [])))
        return image, device, attached

    def test_attached_unmounted_image_is_detected_and_detached_before_deletion(self):
        image, device, _ = self.attach_fixture()
        subprocess.run(['/usr/sbin/diskutil', 'unmountDisk', device], check=True,
                       capture_output=True, timeout=60)
        self.assertEqual(self.mounted_test_mounts(), set())
        with self.assertRaisesRegex(AssertionError, 'owned test image attached'):
            self.assert_no_test_mounts()
        self.assertIn({'image': str(image.resolve()), 'device': device,
                       'identity': [image.stat().st_dev, image.stat().st_ino]},
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
                             if Path(entry.get('image-path', '')).resolve() == image.resolve()
                             for entity in entry.get('system-entities', [])))
        self.assert_no_test_mounts()

    def test_attach_without_mountpoint_is_recorded_and_cleaned(self):
        image, device, _ = self.attach_fixture(mountpoint=False)
        self.assertEqual(self.recorded_mountpoints(), set())
        self.assertIn({'image': str(image.resolve()), 'device': device,
                       'identity': [image.stat().st_dev, image.stat().st_ino]},
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
            self.recover_foreign(image, device)

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
        self.assertEqual(calls.count('detach'), 8)  # Four attempts plus four in the exit trap.
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

    def builder_environment(self, **extra):
        return {'PATH': f'{self.shim}:{os.environ.get("PATH", "/usr/bin:/bin")}',
                'HDIUTIL_TEST_DIR': str(self.base), **extra}

    def run_builder(self, output, **extra):
        return subprocess.run(['bash', str(BUILD), str(self.payload), str(output),
                               VERSION, TARGET], capture_output=True, text=True,
                              timeout=180, env=self.builder_environment(**extra))

    def foreign_at_output_path(self, output, *, mounted):
        output.mkdir()
        image = output / f'haider-v{VERSION}-{TARGET}.dmg'
        point = self.base / f'foreign-{output.name}'
        point.mkdir()
        subprocess.run(['/usr/bin/hdiutil', 'create', '-size', '8m', '-fs', 'HFS+',
                        '-volname', 'Foreign973', str(image)], check=True, capture_output=True)
        attached = plistlib.loads(subprocess.check_output(
            ['/usr/bin/hdiutil', 'attach', str(image), '-readonly', '-nobrowse',
             '-mountpoint', str(point), '-plist']))
        device = next(entity['dev-entry'] for entity in attached['system-entities']
                      if re.fullmatch(r'/dev/disk[0-9]+', entity.get('dev-entry', '')))
        if not mounted:
            subprocess.run(['/usr/sbin/diskutil', 'unmountDisk', device],
                           check=True, capture_output=True)
        image.unlink()
        return image, device, point

    def image_for_device(self, device):
        for attempt in range(4):
            image = next((image for image in self.disk_image_inventory()
                          if any(entity.get('dev-entry') == device
                                 for entity in image.get('system-entities', []))), None)
            if image is not None or not Path(device).exists():
                return image
            if attempt == 3:
                raise RuntimeError('live recovery device missing from inventory: ' + device)
            time.sleep(.1 * (attempt + 1))

    def test_recovery_requeries_a_live_foreign_device_missing_from_inventory(self):
        foreign = {'image-path': str(self.base / 'foreign.dmg'),
                   'system-entities': [{'dev-entry': '/dev/disk973'}]}
        with patch.object(self, 'disk_image_inventory', side_effect=[[], [foreign]]) as query, \
                patch.object(Path, 'exists', return_value=True):
            self.assertEqual(self.image_for_device('/dev/disk973'), foreign)
            self.assertEqual(query.call_count, 2)

    def recover_foreign(self, image, device):
        for attempt in range(4):
            attached = self.image_for_device(device)
            if attached is None:
                return
            path = Path(attached['image-path']).resolve()
            same_image = path == image.resolve()
            if not same_image:
                try:
                    same_image = path.samefile(image)
                except OSError:
                    pass
            if not same_image:
                return
            result = subprocess.run(['/usr/bin/hdiutil', 'detach', device],
                                    capture_output=True, timeout=60)
            if result.returncode == 0:
                return
            if attempt == 3 or (result.returncode != 16 and not re.search(
                    rb'resource busy|device busy|resource temporarily unavailable|\bebusy\b',
                    result.stderr, re.IGNORECASE)):
                result.check_returncode()
            time.sleep(0.1 * (attempt + 1))

    def test_builder_leaves_same_path_foreign_mounted_and_unmounted(self):
        for mounted in (True, False):
            for no_plist in (False, True):
                with self.subTest(mounted=mounted, no_plist=no_plist):
                    output = self.base / f'foreign-output-{mounted}-{no_plist}'
                    image, device, point = self.foreign_at_output_path(output, mounted=mounted)
                    try:
                        (self.shim / 'hdiutil').write_text(HDIUTIL_TRACE + '''
if [ "$1" = attach ] && [ "$NO_PLIST" = 1 ] && [ ! -f "$HDIUTIL_TEST_DIR/no-plist-once" ]; then
  touch "$HDIUTIL_TEST_DIR/no-plist-once"
  native_hdiutil "$@" >/dev/null || exit $?
  printf 'simulated Resource busy without plist\\n' >&2
  exit 16
fi
native_hdiutil "$@"
''')
                        (self.base / 'no-plist-once').unlink(missing_ok=True)
                        result = self.run_builder(output, NO_PLIST='1' if no_plist else '0')
                        self.assertEqual(result.returncode, 0, result.stderr)
                        self.assertIn('macOS installer gate PASS', result.stdout)
                        self.assertTrue(self.image_for_device(device), 'foreign attachment ejected')
                        self.assertEqual(point.stat().st_dev != point.parent.stat().st_dev, mounted)
                        self.assertNotIn(device, {row['device'] for row in self.recorded_attachments()
                                                 if Path(row['image']).resolve() == image.resolve()})
                        self.assert_no_test_mounts()
                    finally:
                        self.recover_foreign(image, device)

    def test_trace_flags_before_image_excludes_same_path_foreign(self):
        output = self.base / 'trace-output'
        image, foreign_device, _ = self.foreign_at_output_path(output, mounted=False)
        try:
            subprocess.run(['/usr/bin/hdiutil', 'create', '-size', '8m', '-fs', 'HFS+',
                            '-volname', 'Owned973', str(image)], check=True, capture_output=True)
            point = self.base / 'trace-owned-mount'
            point.mkdir()
            # Exercise both plist and no-plist recording with flags before the image.
            for no_plist in (False, True):
                with self.subTest(no_plist=no_plist):
                    trace = HDIUTIL_TRACE
                    if no_plist:
                        trace = trace.replace(
                            "result = subprocess.run(['/usr/bin/hdiutil', *arguments], capture_output=True)",
                            "result = subprocess.run(['/usr/bin/hdiutil', *arguments], capture_output=True)\n"
                            "result.stdout = b''")
                    (self.shim / 'hdiutil').write_text(trace + 'native_hdiutil "$@"\n')
                    subprocess.run([str(self.shim / 'hdiutil'), 'attach', '-readonly',
                                    '-mount', 'required',
                                    '-mountpoint', str(point), '-nobrowse', str(image)],
                                   check=True, capture_output=True, env=self.builder_environment())
                    self.assertNotIn(foreign_device,
                                     {row['device'] for row in self.recorded_attachments()})
                    self.assertEqual(len(self.attached_test_images()), 1)
                    self.detach_leftover_test_mounts()
                    self.assertTrue(self.image_for_device(foreign_device))
                    self.assert_no_test_mounts()
        finally:
            self.recover_foreign(image, foreign_device)

    def test_same_path_foreign_device_is_not_owned_by_helper(self):
        image = self.base / 'same.dmg'
        (self.base / 'attachments').write_text(json.dumps({
            'image': str(image), 'device': '/dev/disk973'}) + '\n')
        foreign = {'image-path': str(image), 'system-entities': [{'dev-entry': '/dev/disk974'}]}
        with patch.object(self, 'disk_image_inventory', return_value=[foreign]), \
                patch.object(subprocess, 'run') as detach:
            self.assertEqual(self.attached_test_images(), {})
            self.detach_leftover_test_mounts()
            detach.assert_not_called()

    def test_foreign_unmounted_image_with_case_unicode_alias_preserves_fixture(self):
        image = self.base / 'foreign-\u00e9.dmg'
        alias = self.base.parent / self.base.name.upper() / 'FOREIGN-e\u0301.DMG'
        subprocess.run(['/usr/bin/hdiutil', 'create', '-size', '8m', '-fs', 'HFS+',
                        '-volname', 'AliasForeign973', str(image)], check=True, capture_output=True)
        if not alias.exists() or not alias.samefile(image):
            self.skipTest('filesystem has no case/Unicode aliases')
        attached = plistlib.loads(subprocess.check_output(
            ['/usr/bin/hdiutil', 'attach', str(alias), '-readonly', '-nomount', '-plist']))
        device = next(entity['dev-entry'] for entity in attached['system-entities']
                      if re.fullmatch(r'/dev/disk[0-9]+', entity.get('dev-entry', '')))
        # The independent creator records the actual native identity, including
        # its spelling, so recovery stays safe even if a mutant removes backing.
        identity = Path(self.image_for_device(device)['image-path']).resolve()
        try:
            self.assert_no_test_mounts()
            with self.assertRaisesRegex(AssertionError, 'attached image inside fixture'):
                self.cleanup_fixtures()
            self.assertTrue(image.exists())
            self.assertTrue(self.payload.exists())
            self.assertTrue(self.image_for_device(device))
        finally:
            self.recover_foreign(identity, device)

    def test_builder_exit_trap_retries_busy_eject_after_forced_failure(self):
        (self.shim / 'cmp').write_text('#!/bin/bash\necho "forced cmp failure" >&2\nexit 7\n')
        (self.shim / 'cmp').chmod(0o755)
        result, output, calls = self.build_with_busy_eject()
        self.assertEqual(result.returncode, 7, result.stderr)
        self.assertIn('forced cmp failure', result.stderr)
        self.assertEqual(calls.count('detach'), 2)
        self.assertFalse(list(output.glob('*.sha256')))
        self.assert_no_test_mounts()

    def test_builder_post_detach_refusal_names_owned_device(self):
        (self.shim / 'hdiutil').write_text(HDIUTIL_TRACE + '''
if [ "$1" = detach ]; then exit 0; fi
native_hdiutil "$@"
''')
        output = self.base / 'output-refusal'
        result = self.run_builder(output)
        self.assertEqual(result.returncode, 1, result.stderr)
        devices = {row['device'] for row in self.recorded_attachments() if row['device']}
        self.assertTrue(devices)
        self.assertIn('Owned image still attached as', result.stderr)
        for device in devices:
            self.assertIn(device, result.stderr)
        self.assertIn('retaining temporary mount', result.stderr)
        self.assertFalse(list(output.glob('*.sha256')))
        self.detach_leftover_test_mounts()

    def test_builder_post_detach_inventory_failure_clears_stale_device(self):
        (self.shim / 'hdiutil').write_text(HDIUTIL_TRACE + '''
if [ "$1" = detach ]; then
  native_hdiutil "$@" || exit $?
  touch "$HDIUTIL_TEST_DIR/fail-post-detach-info"
  exit 0
fi
if [ "$1" = info ] && [ -f "$HDIUTIL_TEST_DIR/fail-post-detach-info" ]; then
  echo 'simulated post-detach inventory failure' >&2
  exit 5
fi
if [ "$1" = create ]; then
  native_hdiutil "$@" -fs HFS+
  exit $?
fi
native_hdiutil "$@"
''')
        result = self.run_builder(self.base / 'post-detach-inventory-failure')
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn('Attachment ownership unverified after detach', result.stderr)
        self.assertIn('Attachment ownership unverified; no unverified device was detached', result.stderr)
        self.assertNotIn('Owned image still attached as', result.stderr)
        self.assertNotIn('Could not detach /dev/', result.stderr)
        self.assert_no_test_mounts()

    def test_builder_recycled_device_does_not_confer_ownership(self):
        # A deterministic inventory shim replaces the ejected disk by a different
        # backing path under the same device ID. No foreign disk is really ejected.
        (self.shim / 'hdiutil').write_text(HDIUTIL_TRACE + '''
if [ "$1" = detach ] && [ -f "$HDIUTIL_TEST_DIR/recycled-device" ]; then
  printf 'attempted detach of recycled foreign device\\n' >&2
  exit 99
fi
if [ "$1" = detach ]; then
  native_hdiutil "$@" || exit $?
  printf '%s' "$2" > "$HDIUTIL_TEST_DIR/recycled-device"
  exit 0
fi
if [ "$1" = info ] && [ -f "$HDIUTIL_TEST_DIR/recycled-device" ]; then
  python3 - <<'RECYCLED'
import os, pathlib, plistlib, subprocess
base = pathlib.Path(os.environ['HDIUTIL_TEST_DIR'])
info = plistlib.loads(subprocess.check_output(['/usr/bin/hdiutil', 'info', '-plist']))
info['images'].append({'image-path': str(base / 'foreign-recycled.dmg'),
    'system-entities': [{'dev-entry': (base / 'recycled-device').read_text()}]})
plistlib.dump(info, __import__('sys').stdout.buffer)
RECYCLED
  exit 0
fi
native_hdiutil "$@"
''')
        result = self.run_builder(self.base / 'output-recycled')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn('attempted detach of recycled foreign', result.stderr)
        self.assertIn('macOS installer gate PASS', result.stdout)
        self.assert_no_test_mounts()

    def test_builder_missing_live_recorded_device_preserves_attachment(self):
        # After a real unmount, omit the still-live device from info, with no
        # warning. The builder must retry inventory and retain its work tree.
        (self.shim / 'hdiutil').write_text(HDIUTIL_TRACE + '''
if [ "$1" = detach ]; then
  /usr/sbin/diskutil unmountDisk "$2" >/dev/null || exit $?
  touch "$HDIUTIL_TEST_DIR/omit-live-device"
  exit 0
fi
if [ "$1" = info ] && [ -f "$HDIUTIL_TEST_DIR/omit-live-device" ]; then
  echo info-omitted >> "$HDIUTIL_TEST_DIR/omissions"
  python3 -c 'import plistlib,sys; plistlib.dump({"images": []},sys.stdout.buffer)'
  exit 0
fi
native_hdiutil "$@"
''')
        result = self.run_builder(self.base / 'output-missing-live')
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn('inventory remained incomplete', result.stderr)
        self.assertIn('retaining temporary mount', result.stderr)
        self.assertGreaterEqual(len((self.base / 'omissions').read_text().splitlines()), 4)
        images = self.attached_test_images()
        self.assertEqual(len(images), 1)
        self.assertTrue(all(image.exists() for image in images))
        self.assertEqual(self.mounted_test_mounts(), set())
        self.detach_leftover_test_mounts()


    def test_builder_unavailable_recorded_backing_inode_keeps_attachment(self):
        (self.shim / 'hdiutil').write_text(HDIUTIL_TRACE + '''
if [ "$1" = detach ]; then
  /usr/sbin/diskutil unmountDisk "$2" >/dev/null || exit $?
  touch "$HDIUTIL_TEST_DIR/hide-backing-pid"
  exit 0
fi
if [ "$1" = info ] && [ -f "$HDIUTIL_TEST_DIR/hide-backing-pid" ]; then
  python3 - "$HDIUTIL_TEST_DIR" <<'UNKNOWN_INODE'
import os, pathlib, plistlib, subprocess, sys
info = plistlib.loads(subprocess.check_output(['/usr/bin/hdiutil', 'info', '-plist']))
root = pathlib.Path(sys.argv[1]).resolve()
for image in info.get('images', []):
    if pathlib.Path(image.get('image-path', '')).resolve().is_relative_to(root):
        image.pop('hdid-pid', None)
plistlib.dump(info, sys.stdout.buffer)
UNKNOWN_INODE
  exit 0
fi
native_hdiutil "$@"
''')
        result = self.run_builder(self.base / 'unknown-inode-output')
        self.assertNotEqual(result.returncode, 0, result.stderr)
        self.assertIn('backing inode unavailable', result.stderr)
        self.assertIn('retaining temporary mount', result.stderr)
        self.assertTrue(self.attached_test_images())
        self.assertEqual(self.mounted_test_mounts(), set())
        self.assertFalse(list((self.base / 'unknown-inode-output').glob('*.sha256')))
        self.detach_leftover_test_mounts()

    def test_creation_attachment_without_precreated_inode_is_retained(self):
        (self.shim / 'hdiutil').write_text(HDIUTIL_TRACE + '''
if [ "$1" = create ]; then
  native_hdiutil "$@" || exit $?
  printf '%s\\n' "${@: -1}" > "$HDIUTIL_TEST_DIR/creation-path"
  /usr/bin/hdiutil attach "${@: -1}" -readonly -nomount -plist > "$HDIUTIL_TEST_DIR/creation-attach.plist" || exit $?
  exit 0
fi
native_hdiutil "$@"
''')
        try:
            result = self.run_builder(self.base / 'creation-output')
            self.assertNotEqual(result.returncode, 0, result.stderr)
            self.assertIn('unattributed create output attached as', result.stderr)
            self.assertNotIn('macOS installer gate PASS', result.stdout)
            raw = Path((self.base / 'creation-path').read_text().strip())
            attached = plistlib.loads((self.base / 'creation-attach.plist').read_bytes())
            for entity in attached['system-entities']:
                device = entity.get('dev-entry', '')
                if re.fullmatch(r'/dev/disk[0-9]+', device):
                    current = self.image_for_device(device)
                    self.assertTrue(current and Path(current['image-path']).resolve() == raw.resolve())
            self.assertTrue(raw.exists())
            self.assert_no_test_mounts()
        finally:
            ledger = self.base / 'creation-attach.plist'
            if ledger.exists():
                raw = Path((self.base / 'creation-path').read_text().strip())
                for entity in plistlib.loads(ledger.read_bytes())['system-entities']:
                    device = entity.get('dev-entry', '')
                    if re.fullmatch(r'/dev/disk[0-9]+', device):
                        self.recover_foreign(raw, device)

    def test_builder_initial_capture_cannot_adopt_recycled_foreign_device(self):
        foreign = self.base / 'capture-foreign.dmg'
        subprocess.run(['/usr/bin/hdiutil', 'create', '-size', '8m', '-fs', 'HFS+',
                        '-volname', 'CaptureForeign973', str(foreign)], check=True,
                       capture_output=True)
        (self.shim / 'hdiutil').write_text(HDIUTIL_TRACE + """
if [ "$1" = attach ]; then
  native_hdiutil "$@" > "$HDIUTIL_TEST_DIR/capture-original.plist" || exit $?
  python3 - "$HDIUTIL_TEST_DIR" "$2" <<'CAPTURE'
import pathlib, plistlib, re, subprocess, sys
root = pathlib.Path(sys.argv[1])
original = plistlib.loads((root/'capture-original.plist').read_bytes())
for entity in original['system-entities']:
    device = entity.get('dev-entry', '')
    if re.fullmatch(r'/dev/disk[0-9]+', device):
        inventory = plistlib.loads(subprocess.check_output(['/usr/bin/hdiutil', 'info', '-plist']))
        if not any(pathlib.Path(i.get('image-path', '')).resolve() == pathlib.Path(sys.argv[2]).resolve()
                   and any(e.get('dev-entry') == device for e in i.get('system-entities', []))
                   for i in inventory.get('images', [])):
            continue
        subprocess.run(['/usr/bin/hdiutil', 'detach', device], capture_output=True)
result = subprocess.run(['/usr/bin/hdiutil', 'attach', str(root/'capture-foreign.dmg'),
                         '-readonly', '-nomount', '-plist'], capture_output=True, check=True)
(root/'capture-foreign.plist').write_bytes(result.stdout)
CAPTURE
  cat "$HDIUTIL_TEST_DIR/capture-original.plist"
  exit 0
fi
native_hdiutil "$@"
""")
        try:
            result = self.run_builder(self.base / 'capture-output')
            self.assertNotEqual(result.returncode, 0, result.stderr)
            attached = plistlib.loads((self.base / 'capture-foreign.plist').read_bytes())
            device = next(e['dev-entry'] for e in attached['system-entities']
                          if re.fullmatch(r'/dev/disk[0-9]+', e.get('dev-entry', '')))
            self.assertTrue(self.image_for_device(device), 'foreign replacement ejected')
            self.assertNotIn('Could not detach ' + device, result.stderr)
            self.assert_no_test_mounts()
        finally:
            ledger = self.base / 'capture-foreign.plist'
            if ledger.exists():
                for entity in plistlib.loads(ledger.read_bytes())['system-entities']:
                    device = entity.get('dev-entry', '')
                    if re.fullmatch(r'/dev/disk[0-9]+', device):
                        self.recover_foreign(foreign, device)

    def test_successful_create_identical_foreign_replacement_is_preserved(self):
        for mounted in (False, True):
            with self.subTest(mounted=mounted):
                (self.shim / 'hdiutil').write_text(HDIUTIL_TRACE + """
if [ "$1" = create ]; then
  native_hdiutil "$@" || exit $?
  python3 - "${@: -1}" "$HDIUTIL_TEST_DIR" "$FOREIGN_MOUNTED" <<'CLONE'
import hashlib, json, pathlib, shutil, subprocess, sys
image, root = map(pathlib.Path, sys.argv[1:3])
original = image.stat().st_ino
clone = image.with_name('independent-copy.dmg')
shutil.copyfile(image, clone)
image.unlink(); clone.rename(image)
assert image.stat().st_ino != original
args = ['/usr/bin/hdiutil', 'attach', str(image), '-readonly', '-plist']
if sys.argv[3] == '1':
    point = root / 'clone-mount'; point.mkdir(exist_ok=True)
    args += ['-nobrowse', '-mountpoint', str(point)]
else:
    args += ['-nomount']
attached = subprocess.run(args, capture_output=True, check=True)
(root / 'clone.plist').write_bytes(attached.stdout)
(root / 'clone.json').write_text(json.dumps({
    'path': str(image), 'sha256': hashlib.sha256(image.read_bytes()).hexdigest()}))
CLONE
  exit 0
fi
native_hdiutil "$@"
""")
                try:
                    result = self.run_builder(self.base / f'clone-output-{mounted}',
                                              FOREIGN_MOUNTED=str(int(mounted)))
                    self.assertNotEqual(result.returncode, 0, result.stderr)
                    self.assertIn('unattributed create output attached as', result.stderr)
                    self.assertNotIn('macOS installer gate PASS', result.stdout)
                    record = json.loads((self.base / 'clone.json').read_text())
                    image = Path(record['path'])
                    self.assertEqual(hashlib.sha256(image.read_bytes()).hexdigest(), record['sha256'])
                    attached = plistlib.loads((self.base / 'clone.plist').read_bytes())
                    for entity in attached['system-entities']:
                        device = entity.get('dev-entry', '')
                        if re.fullmatch(r'/dev/disk[0-9]+', device):
                            self.assertTrue(self.image_for_device(device), 'foreign clone ejected')
                    self.assert_no_test_mounts()
                finally:
                    ledger = self.base / 'clone.plist'
                    if ledger.exists():
                        image = Path(json.loads((self.base / 'clone.json').read_text())['path'])
                        for entity in plistlib.loads(ledger.read_bytes())['system-entities']:
                            device = entity.get('dev-entry', '')
                            if re.fullmatch(r'/dev/disk[0-9]+', device):
                                self.recover_foreign(image, device)
                        ledger.unlink()

    def test_builder_noplist_same_path_capture_preserves_foreign_inode(self):
        for mounted in (False, True):
            with self.subTest(mounted=mounted):
                (self.shim / 'hdiutil').write_text(HDIUTIL_TRACE + """
if [ "$1" = attach ]; then
  native_hdiutil "$@" > "$HDIUTIL_TEST_DIR/boundary-original.plist" || exit $?
  python3 - "$2" "$HDIUTIL_TEST_DIR" "$FOREIGN_MOUNTED" <<'BOUNDARY'
import pathlib, plistlib, subprocess, sys
image, root = map(pathlib.Path, sys.argv[1:3])
image.unlink()
subprocess.run(['/usr/bin/hdiutil', 'create', '-size', '8m', '-fs', 'HFS+',
                '-volname', 'BoundaryForeign973', str(image)], check=True, capture_output=True)
arguments = ['/usr/bin/hdiutil', 'attach', str(image), '-readonly', '-plist']
if sys.argv[3] == '1':
    point = root/'boundary-foreign-mount'; point.mkdir(exist_ok=True)
    arguments += ['-nobrowse', '-mountpoint', str(point)]
else:
    arguments += ['-nomount']
attached = subprocess.run(arguments, capture_output=True, check=True)
(root/'boundary-foreign.plist').write_bytes(attached.stdout)
(root/'boundary-path').write_text(str(image))
BOUNDARY
  echo 'scheduled partial-attach Invalid argument' >&2
  exit 5
fi
native_hdiutil "$@"
""")
                try:
                    result = self.run_builder(self.base / f'boundary-output-{mounted}',
                                              FOREIGN_MOUNTED=str(int(mounted)))
                    self.assertEqual(result.returncode, 5, result.stderr)
                    foreign = plistlib.loads((self.base / 'boundary-foreign.plist').read_bytes())
                    device = next(e['dev-entry'] for e in foreign['system-entities']
                                  if re.fullmatch(r'/dev/disk[0-9]+', e.get('dev-entry', '')))
                    self.assertTrue(self.image_for_device(device), 'foreign inode ejected')
                    image = Path((self.base / 'boundary-path').read_text())
                    self.assertTrue(image.exists(), 'foreign backing removed')
                    self.assert_no_test_mounts()
                finally:
                    ledger = self.base / 'boundary-foreign.plist'
                    if ledger.exists():
                        image = Path((self.base / 'boundary-path').read_text())
                        for entity in plistlib.loads(ledger.read_bytes())['system-entities']:
                            device = entity.get('dev-entry', '')
                            if re.fullmatch(r'/dev/disk[0-9]+', device):
                                self.recover_foreign(image, device)
                        ledger.unlink()

    def test_tracer_incomplete_before_inventory_refuses_attach(self):
        image = self.base / 'before-incomplete.dmg'
        subprocess.run(['/usr/bin/hdiutil', 'create', '-size', '8m', '-fs', 'HFS+',
                        '-volname', 'BeforeIncomplete973', str(image)], check=True,
                       capture_output=True)
        anchor = "    info = plistlib.loads(query.stdout)"
        trace = HDIUTIL_TRACE.replace(anchor, anchor + "\n    query.stderr = b'cannot get IO media array'")
        (self.shim / 'hdiutil').write_text(trace + 'native_hdiutil "$@"\n')
        result = subprocess.run([str(self.shim / 'hdiutil'), 'attach', str(image),
                                 '-readonly', '-nomount', '-plist'], capture_output=True,
                                env=self.builder_environment())
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b'incomplete pre-attach inventory', result.stderr)
        self.assertFalse(any(r['device'] for r in self.recorded_attachments()))
        self.assertFalse((self.base / 'native-results').exists(), 'native attach was attempted')
        self.assert_no_test_mounts()

    def test_tracer_incomplete_post_inventory_preserves_attachment(self):
        image = self.base / 'post-inventory.dmg'
        subprocess.run(['/usr/bin/hdiutil', 'create', '-size', '8m', '-fs', 'HFS+',
                        '-volname', 'PostInventory973', str(image)], check=True, capture_output=True)
        anchor = "# Neither a recyclable plist device nor a path delta proves backing ownership."
        for missing in (False, True):
            with self.subTest(missing=missing):
                hook = "\n(record.parent / 'post-real.plist').write_bytes(result.stdout)\n"
                if missing:
                    post = "    # Post-attach inventory fault-injection seam.\n"
                    trace = HDIUTIL_TRACE.replace(post, post + "    query.stdout = plistlib.dumps({'images': []})\n")
                else:
                    post = "    # Post-attach inventory fault-injection seam.\n"
                    trace = HDIUTIL_TRACE.replace(post, post + "    query.stderr = b'cannot get IO media array'\n")
                (self.shim / 'hdiutil').write_text(trace.replace(anchor, hook + anchor)
                                                 + 'native_hdiutil "$@"\n')
                try:
                    result = subprocess.run([str(self.shim / 'hdiutil'), 'attach', str(image),
                                             '-readonly', '-nomount', '-plist'],
                                            env=self.builder_environment(), capture_output=True)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn(b'keeping everything', result.stderr)
                    self.assertFalse([r for r in self.recorded_attachments() if r['device']])
                    ledger = plistlib.loads((self.base / 'post-real.plist').read_bytes())
                    devices = [e['dev-entry'] for e in ledger['system-entities']
                               if re.fullmatch(r'/dev/disk[0-9]+', e.get('dev-entry', ''))]
                    self.assertTrue(any(self.image_for_device(d) for d in devices))
                    self.assertTrue(image.exists())
                finally:
                    for entity in plistlib.loads((self.base / 'post-real.plist').read_bytes())['system-entities']:
                        device = entity.get('dev-entry', '')
                        if re.fullmatch(r'/dev/disk[0-9]+', device):
                            self.recover_foreign(image, device)

    def test_tracer_transient_post_inventory_recovers_before_recording(self):
        image = self.base / 'transient-post.dmg'
        subprocess.run(['/usr/bin/hdiutil', 'create', '-size', '8m', '-fs', 'HFS+',
                        '-volname', 'TransientPost973', str(image)], check=True, capture_output=True)
        seam = "    # Post-attach inventory fault-injection seam.\n"
        for missing in (False, True):
            with self.subTest(missing=missing):
                fault = "query.stdout = plistlib.dumps({'images': []})" if missing else "query.stderr = b'cannot get IO media array'"
                trace = HDIUTIL_TRACE.replace(seam, seam + "    if post_attempt == 0: " + fault + "\n")
                (self.shim / 'hdiutil').write_text(trace + 'native_hdiutil "$@"\n')
                result = subprocess.run([str(self.shim / 'hdiutil'), 'attach', str(image),
                                         '-readonly', '-nomount', '-plist'],
                                        env=self.builder_environment(), capture_output=True)
                try:
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertTrue(self.attached_test_images())
                    self.detach_leftover_test_mounts()
                    self.assert_no_test_mounts()
                finally:
                    if result.stdout:
                        for entity in plistlib.loads(result.stdout)['system-entities']:
                            device = entity.get('dev-entry', '')
                            if re.fullmatch(r'/dev/disk[0-9]+', device):
                                self.recover_foreign(image, device)

    def test_tracer_noplist_captures_only_the_original_backing_inode(self):
        image = self.base / 'tracer-race.dmg'
        subprocess.run(['/usr/bin/hdiutil', 'create', '-size', '8m', '-fs', 'HFS+',
                        '-volname', 'TracerOwned973', str(image)], check=True, capture_output=True)
        hook = """
if result.returncode == 0:
    original = plistlib.loads(result.stdout)
    pathlib.Path(image).unlink()
    subprocess.run(['/usr/bin/hdiutil', 'create', '-size', '8m', '-fs', 'HFS+',
                    '-volname', 'TracerForeign973', image], check=True, capture_output=True)
    foreign = subprocess.run(['/usr/bin/hdiutil', 'attach', image, '-readonly', '-nomount',
                              '-plist'], check=True, capture_output=True)
    (record.parent/'tracer-race.json').write_text(json.dumps({
        'original': original, 'foreign': plistlib.loads(foreign.stdout)}))
    result.stdout = b''
"""
        anchor = "result = subprocess.run(['/usr/bin/hdiutil', *arguments], capture_output=True)"
        (self.shim / 'hdiutil').write_text(HDIUTIL_TRACE.replace(anchor, anchor + hook)
                                          + 'native_hdiutil "$@"\n')
        try:
            subprocess.run([str(self.shim / 'hdiutil'), 'attach', str(image), '-readonly',
                            '-nomount', '-plist'], check=True, capture_output=True,
                           env=self.builder_environment())
            ledger = json.loads((self.base / 'tracer-race.json').read_text())
            original = {e['dev-entry'] for e in ledger['original']['system-entities']
                        if re.fullmatch(r'/dev/disk[0-9]+', e.get('dev-entry', ''))}
            foreign = {e['dev-entry'] for e in ledger['foreign']['system-entities']
                       if re.fullmatch(r'/dev/disk[0-9]+', e.get('dev-entry', ''))}
            recorded = {r['device'] for r in self.recorded_attachments() if r['device']}
            self.assertEqual(recorded, original)
            self.assertTrue(recorded.isdisjoint(foreign))
            self.detach_leftover_test_mounts()
            self.assertTrue(all(self.image_for_device(d) for d in foreign))
        finally:
            ledger_path = self.base / 'tracer-race.json'
            if ledger_path.exists():
                ledger = json.loads(ledger_path.read_text())
                for group in ('original', 'foreign'):
                    for entity in ledger[group]['system-entities']:
                        device = entity.get('dev-entry', '')
                        if re.fullmatch(r'/dev/disk[0-9]+', device):
                            self.recover_foreign(image, device)

    def test_builder_preserves_existing_foreign_backing_file(self):
        for mounted in (True, False):
            with self.subTest(mounted=mounted):
                output = self.base / f'existing-foreign-{mounted}'
                output.mkdir()
                image = output / f'haider-v{VERSION}-{TARGET}.dmg'
                point = self.base / f'existing-mount-{mounted}'
                point.mkdir()
                subprocess.run(['/usr/bin/hdiutil', 'create', '-size', '8m', '-fs', 'HFS+',
                                '-volname', 'ExistingForeign973', str(image)],
                               check=True, capture_output=True)
                arguments = ['/usr/bin/hdiutil', 'attach', str(image), '-readonly', '-plist']
                arguments += ['-mountpoint', str(point)] if mounted else ['-nomount']
                attached = plistlib.loads(subprocess.check_output(arguments))
                device = next(entity['dev-entry'] for entity in attached['system-entities']
                              if re.fullmatch(r'/dev/disk[0-9]+', entity.get('dev-entry', '')))
                try:
                    original = hashlib.sha256(image.read_bytes()).hexdigest()
                    result = self.run_builder(output)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn('Refusing to replace existing disk image', result.stderr)
                    self.assertTrue(image.exists(), 'foreign backing file deleted')
                    self.assertEqual(hashlib.sha256(image.read_bytes()).hexdigest(), original)
                    self.assertTrue(self.image_for_device(device))
                    self.assertEqual(point.stat().st_dev != point.parent.stat().st_dev, mounted)
                    self.assertEqual(self.recorded_attachments(), [])
                finally:
                    self.recover_foreign(image, device)


    def test_builder_preserves_foreign_file_created_during_create(self):
        (self.shim / 'hdiutil').write_text(HDIUTIL_TRACE + '''
if [ "$1" = create ]; then
  image=${@: -1}
  /usr/bin/hdiutil create -size 8m -fs HFS+ -volname LateForeign973 "$image" >/dev/null || exit $?
  /usr/bin/hdiutil attach "$image" -readonly -nomount -plist > "$HDIUTIL_TEST_DIR/late-foreign.plist" || exit $?
  shasum -a 256 "$image" > "$HDIUTIL_TEST_DIR/late-foreign.sha256"
fi
native_hdiutil "$@"
''')
        output = self.base / 'late-foreign-output'
        image = output / f'haider-v{VERSION}-{TARGET}.dmg'
        try:
            result = self.run_builder(output)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('File exists', result.stderr)
            attached = plistlib.loads((self.base / 'late-foreign.plist').read_bytes())
            device = next(e['dev-entry'] for e in attached['system-entities']
                          if re.fullmatch(r'/dev/disk[0-9]+', e.get('dev-entry', '')))
            image = Path(self.image_for_device(device)['image-path'])
            self.assertTrue(image.exists(), 'concurrent foreign backing file deleted')
            original = (self.base / 'late-foreign.sha256').read_text().split()[0]
            self.assertEqual(hashlib.sha256(image.read_bytes()).hexdigest(), original)
        finally:
            record = self.base / 'late-foreign.plist'
            if record.exists():
                attached = plistlib.loads(record.read_bytes())
                device = next(entity['dev-entry'] for entity in attached['system-entities']
                              if re.fullmatch(r'/dev/disk[0-9]+', entity.get('dev-entry', '')))
                current = self.image_for_device(device)
                if current:
                    self.recover_foreign(Path(current['image-path']), device)


    def test_valid_inplace_publication_postprocessing_keeps_the_created_inode(self):
        (self.shim / 'hdiutil').write_text(HDIUTIL_TRACE + """
if [ "$1" = create ]; then
  native_hdiutil "$@" || exit $?
  touch "$HDIUTIL_TEST_DIR/created"
  exit 0
fi
if [ "$1" = info ] && [ -f "$HDIUTIL_TEST_DIR/created" ] && [ ! -f "$HDIUTIL_TEST_DIR/transformed" ]; then
  touch "$HDIUTIL_TEST_DIR/transformed"
  /usr/bin/hdiutil convert "$PUBLICATION_DMG" -format UDZO -imagekey zlib-level=1 -o "$HDIUTIL_TEST_DIR/converted.dmg" >/dev/null || exit $?
  python3 - "$PUBLICATION_DMG" "$HDIUTIL_TEST_DIR" <<'TRANSFORM'
import hashlib,json,pathlib,sys
image,root=map(pathlib.Path,sys.argv[1:]);before=image.stat();original=hashlib.sha256(image.read_bytes()).hexdigest()
image.write_bytes((root/'converted.dmg').read_bytes())
after=image.stat();changed=hashlib.sha256(image.read_bytes()).hexdigest()
(root/'transformed.json').write_text(json.dumps({'before_inode':before.st_ino,'after_inode':after.st_ino,'before_sha256':original,'after_sha256':changed}))
TRANSFORM
fi
native_hdiutil "$@"
""")
        output = self.base / 'postprocess-output'
        image = output / f'haider-v{VERSION}-{TARGET}.dmg'
        result = self.run_builder(output, PUBLICATION_DMG=str(image))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('macOS installer gate PASS', result.stdout)
        changed = json.loads((self.base / 'transformed.json').read_text())
        self.assertEqual(changed['before_inode'], changed['after_inode'])
        self.assertNotEqual(changed['before_sha256'], changed['after_sha256'])
        self.assertEqual(hashlib.sha256(image.read_bytes()).hexdigest(), changed['after_sha256'])
        self.assertEqual(image.with_suffix('.dmg.sha256').read_text().split()[0], changed['after_sha256'])
        self.assert_no_test_mounts()


if __name__ == '__main__':
    unittest.main()
