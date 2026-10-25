"""Cleanup exemptions require a handle inherited at CLI entry, on the same vnode."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import platform
import plistlib
import re
import shutil
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    'inherited_image', ROOT / 'packaging/installers/macos-image-identity.py')
IMAGE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(IMAGE)


ACTIONS = ('finish', 'clean', 'remove', 'withdraw-image')
PINS = ('valid', 'absent', 'closed-reused', 'other-file', 'undeclared-extra')

# The parent passes a high descriptor to keep its real pin separate from the
# native image helper's fd 3. Only the closed declaration may collide with 3.
CHILD = r'''
import json, os, plistlib, subprocess, sys
from unittest.mock import patch
helper, action, image, record, stage, pin, case, listing, helper_pid, receipt = sys.argv[1:]
pin, helper_pid = int(pin), int(helper_pid)
real = subprocess.run
# Stock Python lazily opens /dev/urandom while importing the helper. Keep
# that interpreter-owned handle off 3, then close only our reservation before
# observing CLI entry. Compile from a closed source file on both Pythons.
reserved = os.open(os.devnull, os.O_RDONLY)
assert reserved == 3
try:
    with open(helper, 'rb') as source:
        program = compile(source.read(), helper, 'exec')
    exec(program, {'__name__': 'image_identity_preload', '__file__': helper})
finally:
    os.close(reserved)
if action == 'clean':
    os.close(0)  # clean's directory must take 0, leaving 3 for the audit
if case == 'other-file':
    assert os.open(os.devnull, os.O_RDONLY) == (0 if action == 'clean' else 3)
    if action == 'clean':
        assert os.open(os.devnull, os.O_RDONLY) == 3
        os.close(0)
entry = {}
for fd in (3, pin):
    try:
        st = os.fstat(fd)
        entry[str(fd)] = [st.st_dev, st.st_ino]
    except OSError:
        entry[str(fd)] = None
rows = []
def query(command, **kwargs):
    if command == ['hdiutil', 'info', '-plist']:
        return subprocess.CompletedProcess(command, 0, plistlib.dumps({'images': []}), b'')
    assert command[:3] == ['/usr/sbin/lsof', '-nP', '-F0pfDti'], command
    if helper_pid:
        result = real(command, **kwargs)
        original = result.stdout
        lost = b'p%d\0\n' % helper_pid
        assert original.count(lost) == 1
        # Preserve every native file field and only remove one owner set.
        # Put the helper immediately after the parent, without changing files.
        groups = {}
        for line in original.split(b'\n')[:-1]:
            if line.startswith(b'p'):
                pid = int(line[1:-1])
                groups[pid] = []
            else:
                groups[pid].append(line + b'\n')
        parent = os.getppid()
        assert parent in groups and helper_pid in groups
        assert any(line.startswith(b'f3\0') for line in groups[helper_pid])
        order = [pid for pid in groups if pid != helper_pid]
        order.insert(order.index(parent) + 1, helper_pid)
        ordered = b''.join(b'p%d\0\n' % pid + b''.join(groups[pid]) for pid in order)
        result.stdout = ordered if listing == 'complete' else ordered.replace(lost, b'', 1)
        if case == 'closed-reused':
            st = os.fstat(3)
            assert entry['3'] is None and [st.st_dev, st.st_ino] == entry[str(pin)]
        rows.append({'native': original.decode(), 'filtered': result.stdout.decode(),
                     'only_owner_set_removed': result.stdout == original.replace(lost, b'', 1)})
        return result
    target = os.stat(image)
    def fields(pid, fd):
        return (b'p%d\0\nf%d\0tREG\0D0x%x\0i%d\0\n'
                % (pid, fd, target.st_dev, target.st_ino))
    own = []
    for fd in range(256):
        try:
            st = os.fstat(fd)
        except OSError:
            continue
        if (st.st_dev, st.st_ino) == (target.st_dev, target.st_ino):
            own.append(fd)
    data = b'p%d\0\n' % os.getpid()
    data += b''.join(fields(os.getpid(), fd).split(b'\n', 1)[1] for fd in own)
    # A real parent's image pin, absent or mismatched in the child, is a reader.
    if case != 'unused-nine':
        data += fields(os.getppid(), pin)
    if listing != 'own-only':
        reader = fields(os.getpid() + 100000, 3)
        data += reader if listing == 'complete' else reader.split(b'\n', 1)[1]
    rows.append({'entry': entry, 'own': own})
    return subprocess.CompletedProcess(command, 0, data, b'')
sys.argv = [helper, action, image, record, stage]
try:
    with patch('subprocess.run', side_effect=query):
        exec(program, {'__name__': '__main__', '__file__': helper})
finally:
    with open(receipt, 'w') as output:
        json.dump({'entry': entry, 'queries': rows}, output)
'''


class ImageFixture:
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='haider973-inheritance-')
        self.addCleanup(self.temp.cleanup)
        self.initialize_paths(Path(self.temp.name).resolve())

    def initialize_paths(self, root):
        self.root = root
        self.stage = self.root / 'stage'
        self.rawdir = self.stage / 'create.test'
        self.path = self.rawdir / 'image.dmg'
        self.record = self.root / 'identity.json'
        self.receipt = self.root / 'readers.json'
        self.marker = self.stage / 'retain-create'

    def prepare(self):
        self.rawdir.mkdir(parents=True, exist_ok=True)
        if not self.path.exists():
            self.path.write_bytes(b'test-created detached image')
        expected = IMAGE.identity(self.path.stat())
        self.record.write_text(json.dumps({'identity': expected, 'copying': True}))
        (self.stage / 'staged-image.json').write_text(json.dumps({
            'path': str(self.path), 'identity': expected, 'directory': IMAGE.identity(self.rawdir.stat())}))
        self.marker.write_text(str(self.path))
        return expected

    def run_action(self, action, case, listing, helper_pid=0):
        import fcntl

        low = os.open(self.path, os.O_RDONLY)
        try:
            pin = fcntl.fcntl(low, fcntl.F_DUPFD, 100)
        finally:
            os.close(low)
        try:
            environment = dict(os.environ, HAIDER_IMAGE_PARENT_FDS=str(pin))
            if case in ('closed-reused', 'other-file'):
                environment['HAIDER_IMAGE_PARENT_FDS'] += ',3'
            passed = (pin,) if case not in ('absent', 'unused-nine') else ()
            if case == 'unused-nine':
                environment['HAIDER_IMAGE_PARENT_FDS'] = '9'
                os.close(pin)
            result = subprocess.run([sys.executable, '-I', '-B', '-c', CHILD, str(SPEC.origin),
                action, str(self.path), str(self.record), str(self.stage), str(pin), case, listing,
                str(helper_pid), str(self.receipt)], capture_output=True, text=True,
                timeout=120, pass_fds=passed, env=environment)
        finally:
            if case != 'unused-nine':
                os.close(pin)
        return result, json.loads(self.receipt.read_text())

    def assert_retained(self, result, expected):
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn('retaining', result.stderr)
        self.assertNotIn('Traceback', result.stderr)
        self.assertEqual(IMAGE.identity(self.path.stat()), expected)
        self.assertTrue(self.marker.exists())


class InheritedHandleTests(ImageFixture, unittest.TestCase):
    @unittest.skipUnless(os.name == 'posix', 'requires POSIX inherited descriptors')
    def test_cleanup_pairs_require_entry_inheritance(self):
        # 40 paired cases, plus 16 own-only controls. A valid inherited pin is
        # exempt, but never a separate reader with a lost owner set.
        for action in ACTIONS:
            for case in PINS:
                for listing in ('complete', 'lost-owner'):
                    with self.subTest(action=action, pin=case, listing=listing):
                        expected = self.prepare()
                        result, observed = self.run_action(action, case, listing)
                        self.assert_retained(result, expected)
                        self.assertEqual(self.path.read_bytes(), b'test-created detached image')
                        if case == 'closed-reused':
                            self.assertIsNone(observed['entry']['3'])
                            self.assertIn(3, observed['queries'][0]['own'])
            for case in ('valid', 'closed-reused', 'other-file', 'unused-nine'):
                with self.subTest(action=action, own_only=case):
                    self.prepare()
                    result, _ = self.run_action(action, case, 'own-only')
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(self.path.exists(), action == 'finish')

    def test_entry_and_current_identity_are_both_required(self):
        from unittest.mock import patch

        self.prepare()
        with self.path.open('rb') as source, (self.root / 'other').open('wb') as other:
            fd = source.fileno()
            expected = IMAGE.identity(os.fstat(fd))
            own = os.getpid()
            parent = os.getppid()
            data = (b'p%d\0\nf%d\0tREG\0D%x\0i%d\0\n' % (own, fd, *expected)
                    + b'p%d\0\nf%d\0tREG\0D%x\0i%d\0\n' % (parent, fd, *expected))
            with patch.object(IMAGE.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0, data, b'')):
                for entry in (None, {}, {fd: IMAGE.identity(os.fstat(other.fileno()))},
                              {fd: [expected[0] + 1, expected[1]]},
                              {fd: [expected[0], expected[1] + 1]}):
                    with self.subTest(entry=entry):
                        self.assertEqual(IMAGE.image_users_snapshot(self.path, fd, entry), {parent})
                self.assertEqual(IMAGE.image_users_snapshot(self.path, fd, {fd: expected}), set())
                # The audit's own descriptor stays valid; a different declared
                # descriptor changed after entry and cannot exempt a reader.
                changed = other.fileno()
                data = data.replace(b'p%d\0\nf%d\0' % (parent, fd),
                                    b'p%d\0\nf%d\0' % (parent, changed))
                with patch.object(IMAGE.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0, data, b'')):
                    self.assertEqual(IMAGE.image_users_snapshot(self.path, fd, {changed: expected}), {parent})


@unittest.skipUnless(platform.system() == 'Darwin', 'requires native hdiutil and lsof')
class NativeInheritedHandleTests(ImageFixture, unittest.TestCase):
    def setUp(self):
        # An uncertain native generation must survive even a failed cleanup.
        self.initialize_paths(Path(tempfile.mkdtemp(prefix='haider973-native-inheritance-')).resolve())
        self.pending = None
        self.generation = 0
        self.addCleanup(self.cleanup_native)

    def native(self, *arguments):
        return subprocess.run(['/usr/bin/hdiutil', *arguments], capture_output=True,
                              timeout=120, check=True).stdout

    def attachments(self, images=None):
        if images is None:
            images = IMAGE.staging_inventory()
        return [image for image in images
                if Path(image['image-path'].removesuffix(' (deleted)')).resolve() == self.path]

    def begin_generation(self, expected):
        self.generation += 1
        self.pending = {'path': str(self.path), 'identity': expected, 'helpers': [], 'requested': [],
                        'history': str(self.root / f'clear-{self.generation}.json')}
        self.save_generation()

    def save_generation(self):
        (self.root / 'native-generation.json').write_text(json.dumps(self.pending))

    def clear_generation(self):
        """Detach verified fixture vnodes once; wait for inventory and readers."""
        state = self.pending
        deadline = time.monotonic() + 30
        last = 'native image still attached'
        while True:
            try:
                if self.path.exists() or self.path.is_symlink():
                    IMAGE.check(self.path, state['identity'])
                images = IMAGE.staging_inventory()
                for image in self.attachments(images):
                    pid = image.get('hdid-pid')
                    if not isinstance(pid, int) or pid <= 0:
                        continue
                    if pid not in state['helpers']:
                        state['helpers'].append(pid)
                    devices = [entity['dev-entry'] for entity in image['system-entities']
                               if re.fullmatch(r'/dev/disk[0-9]+', entity.get('dev-entry', ''))]
                    for device in devices:
                        request = [pid, device]
                        if request in state['requested'] or not Path(device).exists():
                            continue
                        # Reject a recycled or ambiguous device before detach.
                        owners = [entry for entry in images if any(
                            entity.get('dev-entry') == device for entity in entry['system-entities'])]
                        if len(owners) != 1 or IMAGE.backing_identity(image, state['identity']) is not True:
                            continue
                        self.native('detach', device)
                        state['requested'].append(request)
                        self.save_generation()
                if self.attachments():
                    raise RuntimeError('native attachment inventory has not cleared')
                if self.path.exists() or self.path.is_symlink():
                    IMAGE.require_unattached(self.path, state['identity'], Path(state['history']))
                elif any(not IMAGE.helper_has_exited(pid) for pid in state['helpers']):
                    raise RuntimeError('deleted fixture helper has not exited')
                return
            except (RuntimeError, OSError, subprocess.SubprocessError) as error:
                last = str(error)
                self.save_generation()
            if time.monotonic() >= deadline:
                raise RuntimeError('native fixture cleanup unverified; retaining ' + str(self.path)
                                   + ': ' + last)
            time.sleep(.1)

    def attached_helper(self, devices, expected):
        deadline = time.monotonic() + 30
        while True:
            candidates = [image for image in self.attachments() if any(
                entity.get('dev-entry') in devices for entity in image['system-entities'])
                and IMAGE.backing_identity(image, expected) is True]
            if len(candidates) == 1:
                pid = candidates[0]['hdid-pid']
                self.pending['helpers'].append(pid)
                self.save_generation()
                return pid
            if time.monotonic() >= deadline:
                raise RuntimeError('native fixture attachment unverified; retaining ' + str(self.path))
            time.sleep(.1)

    def remove_clear_fixture(self):
        self.clear_generation()
        if self.path.exists() or self.path.is_symlink():
            IMAGE.check(self.path, self.pending['identity'])
            self.path.unlink()
        self.pending = None
        self.save_generation()

    def cleanup_native(self):
        if self.pending is not None:
            self.remove_clear_fixture()
        images = IMAGE.staging_inventory()
        if any(Path(image['image-path']).resolve().is_relative_to(self.root) for image in images):
            raise RuntimeError('native fixture inventory unverified; retaining ' + str(self.root))
        shutil.rmtree(self.root)

    def prepare_attached(self):
        self.rawdir.mkdir(parents=True, exist_ok=True)
        self.native('create', '-size', '8m', '-fs', 'HFS+', '-volname',
                    'Haider973Readers', str(self.path))
        expected = self.prepare()
        self.begin_generation(expected)
        # Native create may still be closing its formatting helper.
        # Establish an unused generation before the explicit attach.
        self.clear_generation()
        digest = hashlib.sha256(self.path.read_bytes()).digest()
        self.begin_generation(expected)
        attached = plistlib.loads(self.native('attach', str(self.path), '-readonly',
                                            '-nobrowse', '-nomount', '-plist'))
        devices = [entry['dev-entry'] for entry in attached['system-entities']
                   if re.fullmatch(r'/dev/disk[0-9]+', entry.get('dev-entry', ''))]
        self.assertTrue(devices)
        return expected, digest, devices, self.attached_helper(devices, expected)

    def test_cleanup_waits_for_detach_inventory_before_unlink(self):
        from unittest.mock import patch

        self.prepare_attached()
        native, inventory = self.native, IMAGE.staging_inventory
        attached = inventory()
        requested, stale = [], []
        remaining = 3

        def detach(*arguments):
            result = native(*arguments)
            if arguments[0] == 'detach':
                requested.append(arguments[1])
            return result

        def delayed_inventory():
            nonlocal remaining
            if requested and remaining:
                self.assertTrue(self.path.exists(), 'fixture unlinked before inventory cleared')
                stale.append(remaining)
                remaining -= 1
                return attached
            return inventory()

        with patch.object(self, 'native', side_effect=detach), \
                patch.object(IMAGE, 'staging_inventory', side_effect=delayed_inventory):
            self.remove_clear_fixture()
        self.assertEqual(len(requested), 1, 'successful detach repeated against a recyclable name')
        self.assertEqual(len(stale), 3)
        self.assertFalse(self.path.exists())

    def test_attached_cleanup_pairs_preserve_native_readers(self):
        # Durable form of Astra's native F3 probe: 40 cases, all four actions.
        # Kills current-only inheritance, capture-after-open and clean-only
        # delayed capture; complete listings and undeclared collisions pair it.
        for action in ACTIONS:
            for case in PINS:
                for listing in ('complete', 'lost-owner'):
                    expected, digest, _, helper_pid = self.prepare_attached()
                    try:
                        with self.subTest(action=action, pin=case, listing=listing):
                            result, observed = self.run_action(action, case, listing, helper_pid)
                            self.assertTrue(observed['queries'])
                            for query in observed['queries']:
                                self.assertEqual(query['only_owner_set_removed'], listing == 'lost-owner')
                            if case == 'closed-reused':
                                self.assertIsNone(observed['entry']['3'])
                            self.assert_retained(result, expected)
                            self.assertEqual(hashlib.sha256(self.path.read_bytes()).digest(), digest)
                            self.assertTrue(self.attachments())
                    finally:
                        # Keep cleanup outside subTest: uncertainty stops this
                        # generation before a later case can reuse its pathname.
                        self.remove_clear_fixture()


if __name__ == '__main__':
    unittest.main()
