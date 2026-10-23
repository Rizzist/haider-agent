"""Deterministic reader-inventory corruption must never authorize cleanup."""
import importlib.util
import json
import os
from pathlib import Path
import platform
import plistlib
import re
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    'reader_identity', ROOT / 'packaging/installers/macos-image-identity.py')
IMAGE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(IMAGE)

CLI = '''
import os, plistlib, runpy, subprocess, sys
from unittest.mock import patch
helper, listing, *arguments = sys.argv[1:]
template = open(listing, 'rb').read()
target = os.stat(arguments[1])
pin = int(os.environ.get('READER_TEST_PIN', '-1'))
def own_descriptor():
    # Like lsof: the descriptor the CLI itself holds on the audited image.
    found = []
    for fd in range(3, 256):
        try:
            st = os.fstat(fd)
        except OSError:
            continue
        if fd != pin and (st.st_dev, st.st_ino) == (target.st_dev, target.st_ino):
            found.append(fd)
    assert len(found) == 1, found
    return found[0]
def query(command, **kwargs):
    if command == ['hdiutil', 'info', '-plist']:
        return subprocess.CompletedProcess(command, 0, plistlib.dumps({'images': []}), b'')
    assert command[:3] == ['/usr/sbin/lsof', '-nP', '-F0pfDti'], command
    data = (template.replace(b'pOWN', b'p%d' % os.getpid()).replace(b'pPARENT', b'p%d' % os.getppid())
            .replace(b'fOWN', b'f%d' % own_descriptor()).replace(b'fPIN', b'f%d' % pin))
    return subprocess.CompletedProcess(command, 0, data, b'')
sys.argv = [helper, *arguments]
with patch('subprocess.run', side_effect=query):
    runpy.run_path(helper, run_name='__main__')
'''


NATIVE = '''
import os, plistlib, runpy, subprocess, sys
from unittest.mock import patch
helper, helper_pid, shape, *arguments = sys.argv[1:]
real = subprocess.run
def query(command, **kwargs):
    if command == ['hdiutil', 'info', '-plist']:
        # hdiutil info silently omits the attachment, an already supported condition.
        return subprocess.CompletedProcess(command, 0, plistlib.dumps({'images': []}), b'')
    result = real(command, **kwargs)
    if command[0] == '/usr/sbin/lsof' and shape != 'real':
        # Real records, real descriptors: only the helper's process set is lost,
        # so its file sets follow those of our own process or our parent.
        order, files, pid = [], {}, None
        for line in result.stdout.split(b'\\n')[:-1]:
            if line.startswith(b'p'):
                pid = int(line[1:-1])
                order.append(pid)
                files[pid] = []
            else:
                files[pid].append(line + b'\\n')
        lost = int(helper_pid)
        host = os.getpid() if shape == 'after-own' else os.getppid()
        assert lost in order and host in order, (shape, result.stdout)
        order.remove(lost)
        files[host] += files.pop(lost)
        result.stdout = b''.join(b'p%d\\0\\n' % pid + b''.join(files[pid]) for pid in order)
    return result
sys.argv = [helper, *arguments]
with patch('subprocess.run', side_effect=query):
    runpy.run_path(helper, run_name='__main__')
'''


class ReaderInventoryTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.path = self.root / 'image.dmg'
        self.path.write_bytes(b'test-created image bytes')
        # Real descriptors on the image: ours, a second one of ours, and the
        # copy a parent would pass down (exec 9< in the builder).
        self.source = self.path.open('rb')
        self.addCleanup(self.source.close)
        self.extra = self.path.open('rb')
        self.addCleanup(self.extra.close)
        self.pin = self.path.open('rb')
        self.addCleanup(self.pin.close)
        self.other_file = (self.root / 'other').open('wb')
        self.addCleanup(self.other_file.close)
        closed = os.open(os.devnull, os.O_RDONLY)
        os.close(closed)
        self.closed = closed
        environment = patch.dict(os.environ)
        environment.start()
        self.addCleanup(environment.stop)
        os.environ.pop('HAIDER_IMAGE_PARENT_FDS', None)
        self.dev, self.ino = IMAGE.identity(self.path.stat())
        self.own = os.getpid()
        self.parent = os.getppid()
        self.other = max(self.own, self.parent) + 100000

    def declare(self, value=None):
        os.environ['HAIDER_IMAGE_PARENT_FDS'] = str(self.pin.fileno()) if value is None else value

    def fields(self, pid, fd=None):
        if fd is None:
            fd = {self.own: self.source.fileno(), self.parent: self.pin.fileno()}.get(pid, 8)
        return [f'p{pid}'.encode(), f'f{fd}'.encode(), b'tREG',
                f'D0x{self.dev:x}'.encode(), f'i{self.ino}'.encode()]

    def listing(self, fields, native=False):
        if native:
            return fields[0] + b'\0\n' + b'\0'.join(fields[1:]) + b'\0\n'
        return b'\0'.join(fields) + b'\0'

    def orphan(self, fd):
        """A native file set whose process set was lost."""
        return b'\0'.join(self.fields(self.other, fd)[1:]) + b'\0\n'

    def users(self, output):
        with patch.object(IMAGE.subprocess, 'run',
                return_value=subprocess.CompletedProcess([], 0, output, b'')):
            return IMAGE.image_users_snapshot(self.path, self.source.fileno())

    def refuse(self, output):
        with self.assertRaisesRegex(RuntimeError, 'retaining'):
            self.users(output)

    def refuse_both_orders(self, fields):
        for native in (False, True):
            own = self.listing(self.fields(self.own), native)
            other = self.listing(fields, native)
            for own_first in (False, True):
                with self.subTest(native=native, own_first=own_first):
                    with self.assertRaisesRegex(RuntimeError, 'inventory.*retaining'):
                        self.users(own + other if own_first else other + own)

    def test_each_missing_file_field_is_uncertain(self):
        for key in (b'f', b't', b'D', b'i'):
            with self.subTest(missing=key):
                self.refuse_both_orders([field for field in self.fields(self.other) if field[:1] != key])

    def test_missing_process_header_before_and_after_own_is_uncertain(self):
        self.refuse_both_orders(self.fields(self.other)[1:])
        # Native process-set delimiters survive omission of the p field.
        own = self.listing(self.fields(self.own), True)
        missing = b'\0\n' + b'\0'.join(self.fields(self.other)[1:]) + b'\0\n'
        for data in (missing + own, own + missing):
            with self.subTest(data=data), self.assertRaisesRegex(RuntimeError, 'retaining'):
                self.users(data)

    def test_lost_process_set_cannot_borrow_own_descriptor_exclusion(self):
        # The reader's whole p set is gone, so lsof credits its file set to our
        # process. Exclusion needs each descriptor verified in-process.
        own = self.listing(self.fields(self.own), True)
        for fd in (self.closed, self.other_file.fileno(), self.source.fileno(), 2 ** 70):
            with self.subTest(fd=fd):
                self.refuse(own + self.orphan(fd))
        # Verified descriptors on the image remain excluded (negative control).
        self.assertEqual(self.users(own + self.orphan(self.extra.fileno())), set())

    def test_lost_process_set_cannot_borrow_parent_descriptor_exclusion(self):
        self.declare()
        parent = self.listing(self.fields(self.parent), True)
        own = self.listing(self.fields(self.own), True)
        self.assertEqual(self.users(parent + own), set())
        for fd in (self.closed, self.other_file.fileno(), self.source.fileno(), self.extra.fileno()):
            with self.subTest(fd=fd):
                self.assertEqual(self.users(parent + self.orphan(fd) + own), {self.parent})
        # The declared descriptor itself cannot be listed twice.
        self.refuse(parent + self.orphan(self.pin.fileno()) + own)

    def test_parent_descriptors_need_a_confirmed_declaration(self):
        own = self.listing(self.fields(self.own), True)
        parent = self.listing(self.fields(self.parent), True)
        self.assertEqual(self.users(parent + own), {self.parent})
        # A declaration names the descriptor, and our inherited copy confirms it.
        for declared in (str(self.closed), str(self.other_file.fileno())):
            with self.subTest(declared=declared):
                self.declare(declared)
                listing = self.listing(self.fields(self.parent, int(declared)), True)
                self.assertEqual(self.users(listing + own), {self.parent})
        for declared in ('9,', ',9', 'x', ' 9', '9 ', '-1', '+9', '9\n', '9;10', str(2 ** 70)):
            with self.subTest(declared=declared):
                self.declare(declared)
                if declared == str(2 ** 70):
                    self.assertEqual(self.users(parent + own), {self.parent})
                else:
                    self.refuse(parent + own)
        self.declare(f'{self.closed},{self.pin.fileno()}')
        self.assertEqual(self.users(parent + own), set())
        # Other processes are never excluded, declared descriptor or not.
        self.assertEqual(self.users(self.listing(self.fields(self.other, self.pin.fileno()), True) + own),
                         {self.other})

    def test_records_of_another_file_and_a_missing_pinned_descriptor_are_uncertain(self):
        own = self.listing(self.fields(self.own), True)
        for pid in (self.own, self.other):
            for key, value in ((b'D', f'D0x{self.dev + 1:x}'.encode()), (b'i', f'i{self.ino + 1}'.encode())):
                fields = [value if field[:1] == key else field
                          for field in self.fields(pid, self.extra.fileno() if pid == self.own else 8)]
                with self.subTest(pid=pid, key=key):
                    self.refuse(own + self.listing(fields, True))
        # Our other descriptors cannot stand in for the pinned one.
        self.refuse(self.listing(self.fields(self.own, self.extra.fileno()), True))

    def test_process_without_file_set_mid_listing_is_uncertain(self):
        own = self.listing(self.fields(self.own), True)
        for pid in (self.other, self.parent):
            with self.subTest(pid=pid):
                self.refuse(b'p%d\0\n' % pid + own)

    def test_repeated_process_header_is_uncertain(self):
        own = self.listing(self.fields(self.own), True)
        again = self.listing(self.fields(self.own, self.extra.fileno()), True)
        self.refuse(own + again)
        other = self.listing(self.fields(self.other), True)
        self.refuse(other + own + self.listing(self.fields(self.other, 9), True))

    def test_leading_orphan_file_set_is_uncertain(self):
        own = self.listing(self.fields(self.own), True)
        for fd in (8, self.extra.fileno()):
            with self.subTest(fd=fd):
                self.refuse(self.orphan(fd) + own)

    def test_trailing_unterminated_field_is_uncertain(self):
        own = self.listing(self.fields(self.own), True)
        other = self.listing(self.fields(self.other), True)
        for junk in (b'iX', b'X', b'i7', b'nname'):
            with self.subTest(junk=junk):
                self.refuse(own[:-1] + junk + b'\n')
                self.refuse(own + other[:-1] + junk + b'\n')

    def test_truncated_final_records_are_uncertain(self):
        own = self.listing(self.fields(self.own))
        other = self.listing(self.fields(self.other))
        for end in range(1, len(other)):
            with self.subTest(end=end), self.assertRaisesRegex(RuntimeError, 'retaining'):
                self.users(own + other[:end])
        native = self.listing(self.fields(self.own), True)
        for suffix in (b'p973\0\n', b'f8\0tREG\0', native[:-1]):
            with self.subTest(suffix=suffix), self.assertRaisesRegex(RuntimeError, 'retaining'):
                self.users(native + suffix)

    def test_duplicate_fields_and_records_are_uncertain(self):
        fields = self.fields(self.other)
        for index in range(len(fields)):
            for value in (fields[index], fields[index][:1] + b'0'):
                with self.subTest(index=index, value=value):
                    self.refuse_both_orders(fields[:index] + [value] + fields[index:])
        native = self.listing(self.fields(self.own), True)
        for suffix in (native, b'\0'.join(self.fields(self.own)[1:]) + b'\0\n'):
            with self.subTest(suffix=suffix), self.assertRaisesRegex(RuntimeError, 'retaining'):
                self.users(native + suffix)

    def test_out_of_order_fields_are_uncertain(self):
        fields = self.fields(self.other)
        for index in range(len(fields) - 1):
            changed = fields.copy()
            changed[index:index + 2] = reversed(changed[index:index + 2])
            with self.subTest(index=index):
                self.refuse_both_orders(changed)

    def test_unknown_fields_and_broken_set_delimiters_are_uncertain(self):
        own = self.listing(self.fields(self.own), True)
        other = self.listing(self.fields(self.other), True)
        malformed = [own + b'nignored\0\n', own + b'\n', own + b'\0\n',
                     own + other.replace(b'\0', b'', 1), own + other[:-1],
                     own + other.replace(b'\n', b'', 1), own + b'\0',
                     own + other.replace(b'tREG', b'tREG\0nunknown')]
        for data in malformed:
            with self.subTest(data=data), self.assertRaisesRegex(RuntimeError, 'retaining'):
                self.users(data)

    def test_non_numeric_descriptors_and_other_malformed_values_are_uncertain(self):
        for key, values in ((b'f', (b'?', b'txt', b'mem', b'7u', b'7?', b'-1', b'+7', b' 7', b'')),
                (b'p', (b'?', b'0', b'-1', b'+7', b' 7', b'')),
                (b't', (b'?', b'DIR', b'')),
                (b'D', (b'?', b'0x', b'-1', b'+1', b' 1', b'')),
                (b'i', (b'?', b'-1', b'+1', b' 1', b'1_0', b''))):
            for value in values:
                fields = [key + value if field[:1] == key else field for field in self.fields(self.other)]
                with self.subTest(key=key, value=value):
                    self.refuse_both_orders(fields)

    def test_own_and_parent_records_are_validated_before_exclusion(self):
        self.declare()
        own = self.listing(self.fields(self.own))
        parent = self.listing(self.fields(self.parent))
        self.assertEqual(self.users(own + parent), set())
        self.assertEqual(self.users(parent + own), set())
        for pid in (self.own, self.parent):
            for key in (b'f', b't', b'D', b'i'):
                fields = [field for field in self.fields(pid) if field[:1] != key]
                with self.subTest(pid=pid, missing=key), self.assertRaisesRegex(RuntimeError, 'retaining'):
                    self.users(own + self.listing(fields))

    def test_complete_records_and_native_multiple_descriptors_preserve_readers(self):
        for native in (False, True):
            own = self.listing(self.fields(self.own), native)
            other = self.listing(self.fields(self.other), native)
            self.assertEqual(self.users(own), set())
            self.assertEqual(self.users(own + other), {self.other})
            self.assertEqual(self.users(other + own), {self.other})
        # The builder's shape: the parent pins the image, and our process
        # holds both the inherited copy and its own audit descriptor.
        self.declare()
        own = self.listing(self.fields(self.own), True)
        second = b'\0'.join(self.fields(self.own, self.pin.fileno())[1:]) + b'\0\n'
        third = b'\0'.join(self.fields(self.own, self.extra.fileno())[1:]) + b'\0\n'
        parent = self.listing(self.fields(self.parent), True)
        self.assertEqual(self.users(parent + own + second + third), set())
        self.assertEqual(self.users(own + second + parent), set())
        # An orphan set is not a second descriptor of the preceding process.
        with self.assertRaisesRegex(RuntimeError, 'retaining'):
            self.users(own + b'\0\n' + second)

    @unittest.skipUnless(os.name == 'posix', 'requires POSIX image descriptors')
    def test_all_cleanup_actions_retain_uncertain_records_and_accept_complete_ones(self):
        record = self.root / 'identity.json'
        stage = self.root / 'stage'
        stage.mkdir()
        rawdir = stage / 'create.test'
        rawdir.mkdir()
        path = rawdir / 'image.dmg'
        listing = self.root / 'readers'
        own = self.listing([b'pOWN', b'fOWN', b'tREG', b'DDEV', b'iINO'], True)
        pinned = b'\0'.join([b'fPIN', b'tREG', b'DDEV', b'iINO']) + b'\0\n'
        parent = self.listing([b'pPARENT', b'fPIN', b'tREG', b'DDEV', b'iINO'], True)
        # A descriptor number the CLI cannot hold, so only exclusion could clear it.
        orphan = b'\0'.join([b'f4000', b'tREG', b'DDEV', b'iINO']) + b'\0\n'
        # (listing, parent declares its descriptor, expected success)
        cases = {
            'own-only': (own, False, True),
            'parent-declared': (parent + own + pinned, True, True),
            'missing-field': (own + self.listing([b'p973', b'f8', b'tREG', b'DDEV'], True), False, False),
            'lost-p-after-own': (own + orphan, False, False),
            'lost-p-after-parent': (parent + orphan + own + pinned, True, False),
            'parent-undeclared': (parent + own + pinned, False, False),
        }
        # Inject text into the real CLI, including its exit/error boundary.
        for action in ('finish', 'clean', 'remove', 'withdraw-image'):
            for name, (data, declared, clear) in cases.items():
                with self.subTest(action=action, case=name):
                    rawdir.mkdir(exist_ok=True)
                    path.write_bytes(b'retained test bytes')
                    expected = IMAGE.identity(path.stat())
                    record.write_text(json.dumps({'identity': expected, 'copying': True}))
                    (stage / 'staged-image.json').write_text(json.dumps({
                        'path': str(path), 'identity': expected, 'directory': IMAGE.identity(rawdir.stat())}))
                    marker = stage / 'retain-create'
                    marker.write_text(str(path))
                    listing.write_bytes(data.replace(b'DDEV', f'D0x{expected[0]:x}'.encode())
                                        .replace(b'iINO', f'i{expected[1]}'.encode()))
                    pin = os.open(path, os.O_RDONLY)
                    try:
                        environment = {key: value for key, value in os.environ.items()
                                       if key != 'HAIDER_IMAGE_PARENT_FDS'}
                        environment['READER_TEST_PIN'] = str(pin)
                        if declared:
                            environment['HAIDER_IMAGE_PARENT_FDS'] = str(pin)
                        result = subprocess.run([sys.executable, '-B', '-c', CLI, str(SPEC.origin),
                            str(listing), action, str(path), str(record), str(stage)],
                            capture_output=True, text=True, timeout=30, pass_fds=(pin,), env=environment)
                    finally:
                        os.close(pin)
                    self.assertEqual(result.returncode, 0 if clear else 1, result.stderr)
                    self.assertNotIn('Traceback', result.stderr)
                    if not clear:
                        self.assertIn('retaining', result.stderr)
                        self.assertEqual(path.read_bytes(), b'retained test bytes')
                        self.assertEqual(IMAGE.identity(path.stat()), expected)
                        self.assertTrue(marker.exists())
                    else:
                        self.assertEqual(path.exists(), action == 'finish')


@unittest.skipUnless(platform.system() == 'Darwin' and Path('/usr/bin/hdiutil').exists(),
                     'requires native hdiutil and lsof')
class NativeReaderTests(unittest.TestCase):
    """Real attached images and real lsof listings; only a process set is lost."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.stage = self.root / 'stage'
        self.rawdir = self.stage / 'create.native'
        self.path = self.rawdir / 'image.dmg'
        self.record = self.root / 'identity.json'
        self.marker = self.stage / 'retain-create'

    def prepare(self):
        self.rawdir.mkdir(parents=True, exist_ok=True)
        if not self.path.exists():
            self.path.write_bytes(b'detached test image bytes')
        expected = IMAGE.identity(self.path.stat())
        self.record.write_text(json.dumps({'identity': expected, 'copying': True}))
        (self.stage / 'staged-image.json').write_text(json.dumps({
            'path': str(self.path), 'identity': expected, 'directory': IMAGE.identity(self.rawdir.stat())}))
        self.marker.write_text(str(self.path))
        return expected

    def run_action(self, action, shape='real', helper_pid=0, declare=True):
        # Like the builder, the parent pins the image and the CLI inherits it.
        pin = os.open(self.path, os.O_RDONLY)
        try:
            environment = {key: value for key, value in os.environ.items()
                           if key != 'HAIDER_IMAGE_PARENT_FDS'}
            if declare:
                environment['HAIDER_IMAGE_PARENT_FDS'] = str(pin)
            return subprocess.run([sys.executable, '-B', '-c', NATIVE, str(SPEC.origin), str(helper_pid), shape,
                                   action, str(self.path), str(self.record), str(self.stage)],
                                  capture_output=True, text=True, timeout=120, pass_fds=(pin,), env=environment)
        finally:
            os.close(pin)

    def attachments(self):
        info = plistlib.loads(subprocess.run(['/usr/bin/hdiutil', 'info', '-plist'], capture_output=True,
                                             check=True, timeout=60).stdout)
        return [image for image in info.get('images', [])
                if Path(image.get('image-path', '')).resolve() == self.path]

    def detach(self, device):
        for attempt in range(4):
            if not any(entity.get('dev-entry') == device for image in self.attachments()
                       for entity in image.get('system-entities', [])):
                return
            arguments = ['/usr/bin/hdiutil', 'detach', device] + (['-force'] if attempt == 3 else [])
            if subprocess.run(arguments, capture_output=True, timeout=60).returncode == 0:
                return
            time.sleep(.5 * (attempt + 1))

    def test_lost_helper_process_set_never_authorizes_cleanup_of_an_attached_image(self):
        self.rawdir.mkdir(parents=True)
        subprocess.run(['/usr/bin/hdiutil', 'create', '-size', '8m', '-fs', 'HFS+', '-volname', 'Readers973',
                        str(self.path)], check=True, capture_output=True, timeout=120)
        expected = self.prepare()
        attached = plistlib.loads(subprocess.run(
            ['/usr/bin/hdiutil', 'attach', str(self.path), '-readonly', '-nobrowse', '-nomount', '-plist'],
            check=True, capture_output=True, timeout=120).stdout)
        devices = [entity['dev-entry'] for entity in attached['system-entities']
                   if re.fullmatch(r'/dev/disk[0-9]+', entity.get('dev-entry', ''))]
        self.assertTrue(devices)
        for device in devices:
            self.addCleanup(self.detach, device)
        helper = self.attachments()[0]['hdid-pid']
        for action in ('finish', 'clean', 'remove', 'withdraw-image'):
            for shape in ('real', 'after-own', 'after-parent'):
                with self.subTest(action=action, shape=shape):
                    result = self.run_action(action, shape, helper)
                    self.assertEqual(result.returncode, 1, result.stderr)
                    self.assertIn('retaining', result.stderr)
                    self.assertNotIn('Traceback', result.stderr)
                    self.assertEqual(IMAGE.identity(self.path.stat()), expected)
                    self.assertTrue(self.marker.exists())
                    self.assertTrue(self.attachments(), 'image detached during audit')
        for device in devices:
            self.detach(device)
        self.assertEqual(self.attachments(), [])
        # Negative controls: the real, complete listing of an unattached image.
        for action in ('finish', 'clean', 'remove', 'withdraw-image'):
            with self.subTest(action=action, declared=False):
                self.prepare()
                result = self.run_action(action, declare=False)
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertIn('backing file still open', result.stderr)
                self.assertTrue(self.path.exists())
            with self.subTest(action=action, declared=True):
                self.prepare()
                result = self.run_action(action)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(self.path.exists(), action == 'finish')


if __name__ == '__main__':
    unittest.main()
