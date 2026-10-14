"""Backing identity and publication controls, including mutable output paths."""
import importlib.util
import json
import os
from pathlib import Path
import platform
import subprocess
import tempfile
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    'image_identity', ROOT / 'packaging/installers/macos-image-identity.py')
IMAGE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(IMAGE)


@unittest.skipUnless(platform.system() == 'Darwin', 'requires macOS backing-inode lifecycle semantics')
class ImageIdentityTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.path = self.root / 'publication.dmg'
        self.record = self.root / 'identity.json'

    def test_create_owns_the_publication_inode_before_native_create(self):
        def native(arguments, **kwargs):
            expected = json.loads(self.record.read_text())
            self.assertEqual(IMAGE.identity(self.path.stat()), expected)
            if arguments[1] == 'create':
                self.assertEqual((self.root / 'retain-create').read_text().strip(), arguments[-1])
                Path(arguments[-1]).write_bytes(b'created image')
                return subprocess.CompletedProcess(arguments, 0)
            import plistlib
            return subprocess.CompletedProcess(arguments, 0, plistlib.dumps({'images': []}), b'')
        with patch.object(IMAGE.subprocess, 'run', side_effect=native):
            self.assertEqual(IMAGE.create(self.path, self.record, self.root, []), 0)
            raw = Path(json.loads((self.root / 'staged-image.json').read_text())['path'])
            self.assertTrue(raw.exists(), 'unvalidated native output deleted')
            IMAGE.check(self.path, json.loads(self.record.read_text()))
            IMAGE.finish(self.root)
            self.assertTrue(raw.exists(), 'post-create identity conferred deletion authority')
        self.assertEqual(self.path.read_bytes(), b'created image')
        self.assertEqual(IMAGE.identity(self.path.stat()), json.loads(self.record.read_text()))

    def test_failed_create_cannot_delete_a_replacement(self):
        def native(arguments):
            raw = Path(arguments[-1])
            raw.write_bytes(b'original')
            raw.unlink()
            raw.write_bytes(b'independent replacement')
            return subprocess.CompletedProcess(arguments, 16)
        with patch.object(IMAGE.subprocess, 'run', side_effect=native):
            self.assertEqual(IMAGE.create(self.path, self.record, self.root, []), 16)
        raw = Path((self.root / 'retain-create').read_text().strip())
        self.assertEqual(raw.read_bytes(), b'independent replacement')

    def test_successful_create_does_not_own_an_identical_replacement(self):
        import plistlib
        def native(arguments, **kwargs):
            if arguments[1] == 'create':
                raw = Path(arguments[-1])
                raw.write_bytes(b'valid bytes')
                replacement = raw.with_name('replacement.dmg')
                replacement.write_bytes(raw.read_bytes())
                raw.unlink()
                replacement.rename(raw)
                return subprocess.CompletedProcess(arguments, 0)
            return subprocess.CompletedProcess(arguments, 0, plistlib.dumps({'images': []}), b'')
        with patch.object(IMAGE.subprocess, 'run', side_effect=native):
            self.assertEqual(IMAGE.create(self.path, self.record, self.root, []), 0)
            IMAGE.check(self.path, json.loads(self.record.read_text()))
            IMAGE.finish(self.root)
        raw = Path((self.root / 'retain-create').read_text().strip())
        self.assertEqual(raw.read_bytes(), b'valid bytes')

    def test_successful_create_attachment_never_confers_eject_authority(self):
        import plistlib
        raw = self.root / 'raw.dmg'
        raw.write_bytes(b'valid bytes')
        (self.root / 'staged-image.json').write_text(json.dumps({
            'path': str(raw), 'identity': IMAGE.identity(raw.stat())}))
        image = {'image-path': str(raw), 'system-entities': [{'dev-entry': '/dev/disk973'}]}
        result = subprocess.CompletedProcess([], 0, plistlib.dumps({'images': [image]}), b'')
        with patch.object(IMAGE.subprocess, 'run', return_value=result) as native:
            with self.assertRaisesRegex(RuntimeError, 'unattributed create output attached as /dev/disk973'):
                IMAGE.finish(self.root)
            self.assertEqual(native.call_count, 1)
            self.assertEqual(native.call_args.args[0], ['hdiutil', 'info', '-plist'])
        self.assertTrue(raw.exists())

    def test_create_does_not_read_a_symlinked_native_output(self):
        foreign = self.root / 'foreign.bin'
        foreign.write_bytes(b'foreign bytes')
        def native(arguments):
            Path(arguments[-1]).symlink_to(foreign)
            return subprocess.CompletedProcess(arguments, 0)
        with patch.object(IMAGE.subprocess, 'run', side_effect=native):
            with self.assertRaises(OSError):
                IMAGE.create(self.path, self.record, self.root, [])
        self.assertEqual(self.path.read_bytes(), b'')
        self.assertEqual(foreign.read_bytes(), b'foreign bytes')

    def test_create_rejects_a_fifo_without_waiting_for_a_writer(self):
        def native(arguments):
            os.mkfifo(arguments[-1])
            return subprocess.CompletedProcess(arguments, 0)
        with patch.object(IMAGE.subprocess, 'run', side_effect=native):
            with self.assertRaisesRegex(RuntimeError, 'not a regular file'):
                IMAGE.create(self.path, self.record, self.root, [])
        self.assertEqual(self.path.read_bytes(), b'')

    def test_create_directory_replacement_cannot_redirect_the_source_copy(self):
        foreign = self.root / 'foreign-directory'
        foreign.mkdir()
        (foreign / 'image.dmg').write_bytes(b'foreign directory bytes')
        def native(arguments):
            raw = Path(arguments[-1])
            raw.write_bytes(b'native bytes')
            raw.parent.rename(raw.parent.with_name('original-directory'))
            raw.parent.symlink_to(foreign, target_is_directory=True)
            return subprocess.CompletedProcess(arguments, 0)
        with patch.object(IMAGE.subprocess, 'run', side_effect=native):
            self.assertEqual(IMAGE.create(self.path, self.record, self.root, []), 0)
        self.assertEqual(self.path.read_bytes(), b'native bytes')
        self.assertEqual((foreign / 'image.dmg').read_bytes(), b'foreign directory bytes')

    def test_finish_refuses_an_unattached_staging_replacement(self):
        raw = self.root / 'raw.dmg'
        raw.touch()
        self.record.write_text(json.dumps(IMAGE.identity(raw.stat())))
        (self.root / 'stage-validated').touch()
        (self.root / 'staged-image.json').write_text(json.dumps({
            'path': str(raw), 'identity': IMAGE.identity(raw.stat()), 'retain_before': False}))
        import plistlib
        with raw.open('rb'):
            raw.unlink()
            raw.write_bytes(b'foreign replacement')
            with patch.object(IMAGE.subprocess, 'run', return_value=
                    subprocess.CompletedProcess([], 0, plistlib.dumps({'images': []}), b'')):
                with self.assertRaisesRegex(RuntimeError, 'identity changed'):
                    IMAGE.finish(self.root)
        self.assertEqual(raw.read_bytes(), b'foreign replacement')

    def test_create_replacement_of_publication_is_refused_and_preserved(self):
        def native(arguments, **kwargs):
            self.path.unlink()
            self.path.write_bytes(b'foreign publication')
            Path(arguments[-1]).write_bytes(b'native output')
            return subprocess.CompletedProcess(arguments, 0)
        with patch.object(IMAGE.subprocess, 'run', side_effect=native):
            with self.assertRaisesRegex(RuntimeError, 'identity changed'):
                IMAGE.create(self.path, self.record, self.root, [])
        self.assertEqual(self.path.read_bytes(), b'foreign publication')

    def test_checksum_rejects_replacement_since_validation(self):
        self.path.write_bytes(b'validated image')
        expected = IMAGE.identity(self.path.stat())
        self.record.write_text(json.dumps(expected))
        with self.path.open('rb'):
            IMAGE.check(self.path, expected)
            self.path.unlink()
            self.path.write_bytes(b'foreign replacement')
            with self.assertRaisesRegex(RuntimeError, 'identity changed'):
                IMAGE.checksum(self.path, self.record, self.root / 'digest')
        self.assertFalse((self.root / 'digest').exists())

    def test_checksum_uses_the_open_file_and_rechecks_publication(self):
        self.path.write_bytes(b'validated image')
        self.record.write_text(json.dumps(IMAGE.identity(self.path.stat())))
        digest = IMAGE.hashlib.file_digest
        def replace_after_hash(source, algorithm):
            result = digest(source, algorithm)
            self.path.unlink()
            self.path.write_bytes(b'foreign replacement')
            return result
        with patch.object(IMAGE.hashlib, 'file_digest', side_effect=replace_after_hash):
            with self.assertRaisesRegex(RuntimeError, 'identity changed'):
                IMAGE.checksum(self.path, self.record, self.root / 'digest')
        self.assertFalse((self.root / 'digest').exists())

    def test_checksum_rejects_foreign_descriptor_after_publication_path_is_restored(self):
        self.path.write_bytes(b'validated image')
        self.record.write_text(json.dumps(IMAGE.identity(self.path.stat())))
        foreign = self.root / 'independent.dmg'
        foreign.write_bytes(b'independent replacement')
        native_open = Path.open
        def open_replaced_generation(path, *args, **kwargs):
            # The pathname was replaced while opening, then restored before
            # the audit. Only the returned descriptor reveals the wrong file.
            if path == self.path and args == ('rb',):
                return native_open(foreign, *args, **kwargs)
            return native_open(path, *args, **kwargs)
        with patch.object(Path, 'open', open_replaced_generation):
            with self.assertRaisesRegex(RuntimeError, 'identity changed before checksum'):
                IMAGE.checksum(self.path, self.record, self.root / 'digest')
        self.assertFalse((self.root / 'digest').exists())

    def test_backing_identity_requires_an_open_numeric_descriptor(self):
        for output, expected in ((b'f3\ntREG\nD0x10\ni42\nn/x/image.dmg\n', True),
                                 (b'f3\ntREG\nD0x10\ni43\nn/x/image.dmg\n', False),
                                 (b'f3\ntREG\nD0x11\ni42\nn/x/image.dmg\n', False),
                                 (b'ftxt\nD0x10\ni42\n', None)):
            with self.subTest(output=output), patch.object(IMAGE.subprocess, 'run',
                    return_value=subprocess.CompletedProcess([], 0, output)):
                self.assertEqual(IMAGE.backing_identity({'hdid-pid': 123, 'image-path': '/x/image.dmg'}, [16, 42]), expected)
        self.assertFalse(IMAGE.backing_identity({}, [16, 42]))

    def test_backing_inode_matches_without_reopening_a_replaced_path(self):
        output = b'f3\0tREG\0D0x10\0i42\0n/a name with\nnewline.dmg\0'
        with patch.object(IMAGE.subprocess, 'run', return_value=
                subprocess.CompletedProcess([], 0, output)), \
                patch.object(Path, 'stat', side_effect=OSError('path replaced')):
            self.assertTrue(IMAGE.backing_identity(
                {'hdid-pid': 123, 'image-path': '/another-current-path'}, [16, 42]))

    def test_unavailable_backing_descriptor_is_not_disowned(self):
        with patch.object(IMAGE.subprocess, 'run', return_value=
                subprocess.CompletedProcess([], 1, b'')):
            self.assertIsNone(IMAGE.backing_identity({'hdid-pid': 123}, [16, 42]))
        with patch.object(IMAGE.subprocess, 'run', return_value=
                subprocess.CompletedProcess([], 0, b'f3\0tREG\0D0x10\0i43\0n/another/file\0')):
            self.assertIsNone(IMAGE.backing_identity(
                {'hdid-pid': 123, 'image-path': '/expected/file'}, [16, 42]))

    def test_identity_check_accepts_hardlink_and_refuses_symlink_and_replacement(self):
        self.path.touch()
        expected = IMAGE.identity(self.path.stat())
        alias = self.root / 'alias.dmg'
        os.link(self.path, alias)
        IMAGE.check(alias, expected)
        alias.unlink()
        alias.symlink_to(self.path)
        with self.assertRaisesRegex(RuntimeError, 'identity changed'):
            IMAGE.check(alias, expected)
        with self.path.open('rb'):
            self.path.unlink()
            self.path.touch()
            with self.assertRaisesRegex(RuntimeError, 'identity changed'):
                IMAGE.check(self.path, expected)


if __name__ == '__main__':
    unittest.main()
