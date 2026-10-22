"""Deterministic reader-inventory corruption must never authorize cleanup."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
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
data = open(listing, 'rb').read().replace(b'pOWN', b'p' + str(os.getpid()).encode())
def query(command, **kwargs):
    if command == ['hdiutil', 'info', '-plist']:
        return subprocess.CompletedProcess(command, 0, plistlib.dumps({'images': []}), b'')
    assert command[:3] == ['/usr/sbin/lsof', '-nP', '-F0pfDti'], command
    return subprocess.CompletedProcess(command, 0, data, b'')
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
        self.source = self.path.open('rb')
        self.addCleanup(self.source.close)
        self.dev, self.ino = IMAGE.identity(self.path.stat())
        self.own = os.getpid()
        self.other = max(self.own, os.getppid()) + 100000

    def fields(self, pid, fd=None):
        if fd is None:
            fd = 7 if pid == self.own else 8
        return [f'p{pid}'.encode(), f'f{fd}'.encode(), b'tREG',
                f'D0x{self.dev:x}'.encode(), f'i{self.ino}'.encode()]

    def listing(self, fields, native=False):
        if native:
            return fields[0] + b'\0\n' + b'\0'.join(fields[1:]) + b'\0\n'
        return b'\0'.join(fields) + b'\0'

    def users(self, output):
        with patch.object(IMAGE.subprocess, 'run',
                return_value=subprocess.CompletedProcess([], 0, output, b'')):
            return IMAGE.image_users_snapshot(self.path, self.source.fileno())

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
        own = self.listing(self.fields(self.own))
        parent = self.listing(self.fields(os.getppid()))
        self.assertEqual(self.users(own + parent), set())
        self.assertEqual(self.users(parent + own), set())
        for pid in (self.own, os.getppid()):
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
        own = self.listing(self.fields(self.own), True)
        second = b'\0'.join(self.fields(self.own, fd=8)[1:]) + b'\0\n'
        parent = self.listing(self.fields(os.getppid()), True)
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
        # Inject text into the real CLI, including its exit/error boundary.
        for action in ('finish', 'clean', 'remove', 'withdraw-image'):
            for uncertain in (True, False):
                with self.subTest(action=action, uncertain=uncertain):
                    path.write_bytes(b'retained test bytes')
                    expected = IMAGE.identity(path.stat())
                    record.write_text(json.dumps({'identity': expected, 'copying': True}))
                    (stage / 'staged-image.json').write_text(json.dumps({
                        'path': str(path), 'identity': expected, 'directory': IMAGE.identity(rawdir.stat())}))
                    marker = stage / 'retain-create'
                    marker.write_text(str(path))
                    fields = [b'pOWN', b'f7', b'tREG', f'D0x{expected[0]:x}'.encode(),
                              f'i{expected[1]}'.encode()]
                    data = self.listing(fields)
                    if uncertain:
                        data += self.listing([b'p973', b'f8', b'tREG', fields[3]])
                    listing.write_bytes(data)
                    result = subprocess.run([sys.executable, '-B', '-c', CLI, str(SPEC.origin),
                        str(listing), action, str(path), str(record), str(stage)],
                        capture_output=True, text=True, timeout=30)
                    self.assertEqual(result.returncode, 1 if uncertain else 0, result.stderr)
                    self.assertNotIn('Traceback', result.stderr)
                    if uncertain:
                        self.assertIn('retaining', result.stderr)
                        self.assertEqual(path.read_bytes(), b'retained test bytes')
                        self.assertEqual(IMAGE.identity(path.stat()), expected)
                        self.assertTrue(marker.exists())
                    else:
                        self.assertEqual(path.exists(), action == 'finish')
                        rawdir.mkdir(exist_ok=True)


if __name__ == '__main__':
    unittest.main()
