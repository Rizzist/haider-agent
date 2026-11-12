"""Backing identity and publication controls, including mutable output paths."""
import importlib.util
import hashlib
import json
import os
from pathlib import Path
import platform
import plistlib
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    'image_identity', ROOT / 'packaging/installers/macos-image-identity.py')
IMAGE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(IMAGE)
INVALID_DATE_INVENTORY = (
    b'<plist><dict><key>images</key><array/><key>date</key>'
    b'<date>invalid</date></dict></plist>')


class InventoryTests(unittest.TestCase):
    def response(self, status=0, output=None, error=b''):
        return subprocess.CompletedProcess([], status,
            plistlib.dumps({'images': []}) if output is None else output, error)

    def test_busy_and_incomplete_inventory_retry_by_count(self):
        for status, diagnostic in ((16, b''), (0, b'Resource busy'), (1, b'Resource busy'), (9, b'DEVICE BUSY'),
                (1, b'Resource temporarily unavailable'), (1, b'error: EBUSY.'),
                (0, b'cannot get IO media array')):
            for failures in (1, 3, 4):
                with self.subTest(status=status, diagnostic=diagnostic, failures=failures), \
                        patch.object(IMAGE.subprocess, 'run', side_effect=
                            [self.response(status, error=diagnostic)] * failures + [self.response()]) as query, \
                        patch.object(IMAGE.time, 'sleep') as sleep:
                    if failures == 4:
                        with self.assertRaisesRegex(IMAGE.InventoryError, 'retaining'):
                            IMAGE.staging_inventory()
                    else:
                        self.assertEqual(IMAGE.staging_inventory(), [])
                    self.assertEqual(query.call_count, min(failures + 1, 4))
                    self.assertEqual(sleep.call_count, min(failures, 3))
                    self.assertTrue(all(call.kwargs['timeout'] == 60 for call in query.call_args_list))

    def test_other_failed_or_invalid_inventory_stops_immediately(self):
        responses = [self.response(9, error=b'Invalid argument'),
                     self.response(1, error=b'not_ebusy_suffix'),
                     self.response(error=b'unexpected inventory warning'),
                     self.response(output=INVALID_DATE_INVENTORY),
                     self.response(output=b'truncated plist'),
                     self.response(output=b'<?xml version="1.0"?><plist><dict>')]
        responses += [self.response(output=plistlib.dumps(value)) for value in
                      ({}, [], {'images': {}}, {'images': [False]}, {'images': [{}]},
                       {'images': [{'system-entities': {}}]},
                       {'images': [{'system-entities': [False]}]},
                       {'images': [{'image-path': 1}]},
                       {'images': [{'image-path': '/a', 'system-entities': [], 'hdid-pid': []}]},
                       {'images': [{'system-entities': [{'dev-entry': 1}]}]})]
        for response in responses:
            with self.subTest(response=response), \
                    patch.object(IMAGE.subprocess, 'run', return_value=response) as query, \
                    patch.object(IMAGE.time, 'sleep') as sleep:
                with self.assertRaisesRegex(IMAGE.InventoryError, 'retaining'):
                    IMAGE.staging_inventory()
                self.assertEqual(query.call_count, 1)
                sleep.assert_not_called()
        for error in (AttributeError('invalid date'), OverflowError('invalid value'),
                      RecursionError('nested plist')):
            with self.subTest(parser_error=error), \
                    patch.object(IMAGE.subprocess, 'run', return_value=self.response()) as query, \
                    patch.object(IMAGE.plistlib, 'loads', side_effect=error), \
                    patch.object(IMAGE.time, 'sleep') as sleep:
                with self.assertRaisesRegex(IMAGE.InventoryError, 'retaining'):
                    IMAGE.staging_inventory()
                self.assertEqual(query.call_count, 1)
                sleep.assert_not_called()

    def test_unavailable_inventory_and_reader_queries_fail_closed(self):
        for error in (OSError('unavailable'), subprocess.TimeoutExpired('inventory', 60)):
            with self.subTest(error=error), \
                    patch.object(IMAGE.subprocess, 'run', side_effect=error) as query:
                with self.assertRaisesRegex(IMAGE.InventoryError, 'retaining'):
                    IMAGE.staging_inventory()
                self.assertEqual(query.call_count, 1)
                self.assertIsNone(IMAGE.backing_identity({'hdid-pid': 973}, [1, 2]))
                with patch.object(IMAGE.time, 'sleep'):
                    with self.assertRaisesRegex(RuntimeError, 'retaining'):
                        IMAGE.open_image_users(Path('/unused'), 0)
        for response in (subprocess.CompletedProcess([], 9, b'', b''),
                subprocess.CompletedProcess([], 0, b'f3\0tREG\0Dinvalid\0i2\0', b''),
                subprocess.CompletedProcess([], 0, b'f3\0tREG\0D0x1\0i2\0', b'warning')):
            with self.subTest(response=response), patch.object(IMAGE.subprocess, 'run', return_value=response):
                self.assertIsNone(IMAGE.backing_identity({'hdid-pid': 973}, [1, 2]))

    def test_complete_inventory_preserves_legitimate_entries(self):
        images = [{'image-path': '/tmp/É image.dmg', 'hdid-pid': 973,
                   'system-entities': [{'dev-entry': '/dev/disk973'}]}]
        for expected in ([], images):
            with self.subTest(images=expected), patch.object(IMAGE.subprocess, 'run',
                    return_value=self.response(output=plistlib.dumps({'images': expected}))) as query:
                self.assertEqual(IMAGE.staging_inventory(), expected)
                self.assertEqual(query.call_count, 1)
        valid_date = INVALID_DATE_INVENTORY.replace(b'invalid', b'2026-10-20T00:00:00Z')
        with patch.object(IMAGE.subprocess, 'run', return_value=self.response(output=valid_date)) as query:
            self.assertEqual(IMAGE.staging_inventory(), [])
            self.assertEqual(query.call_count, 1)

    def prepare_cli_image(self, root, *, invalid_record=False):
        path = root / 'image.dmg'
        path.write_bytes(b'retained image bytes')
        record = root / 'identity.json'
        expected = IMAGE.identity(path.stat())
        record.write_text(json.dumps({} if invalid_record else {'identity': expected, 'copying': True}))
        saved = {'path': str(path), 'directory': IMAGE.identity(root.stat())}
        if not invalid_record:
            saved['identity'] = expected
        (root / 'staged-image.json').write_text(json.dumps(saved))
        (root / 'retain-create').write_text(str(path))
        return path, record

    @unittest.skipUnless(os.name == 'posix', 'requires POSIX image descriptors and a shell fixture')
    def test_cli_inventory_refusals_retain_files_without_tracebacks(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            shim = root / 'hdiutil'
            count = root / 'calls'
            for status, output in ((16, 'Resource busy'), (9, 'Invalid argument'),
                                   (0, 'invalid plist'), (0, INVALID_DATE_INVENTORY.decode())):
                shim.write_text('#!/bin/sh\necho info >> "' + str(count) + '"\n'
                                'echo "' + output + '" >&' + ('1' if status == 0 else '2') + '\n'
                                'exit ' + str(status) + '\n')
                shim.chmod(0o755)
                for action in ('finish', 'clean', 'remove', 'withdraw-image'):
                    with self.subTest(action=action, status=status, output=output):
                        count.unlink(missing_ok=True)
                        path, record = self.prepare_cli_image(root)
                        result = subprocess.run([sys.executable, str(SPEC.origin), action,
                            str(path), str(record), str(root)], capture_output=True, text=True,
                            env={**os.environ, 'PATH': str(root) + os.pathsep + os.environ['PATH']}, timeout=30)
                        self.assertEqual(result.returncode, 1, result.stderr)
                        self.assertIn('retaining', result.stderr)
                        self.assertNotIn('Traceback', result.stderr)
                        self.assertEqual(path.read_bytes(), b'retained image bytes')
                        self.assertTrue((root / 'retain-create').exists())
                        self.assertEqual(len(count.read_text().splitlines()), 4 if status == 16 else 1)

    @unittest.skipUnless(os.name == 'posix', 'requires POSIX image descriptors and a shell fixture')
    def test_cli_invalid_identity_records_retain_files_before_inventory(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            count = root / 'calls'
            shim = root / 'hdiutil'
            shim.write_text('#!/bin/sh\necho info >> "' + str(count) + '"\nexit 9\n')
            shim.chmod(0o755)
            for action in ('finish', 'clean', 'remove', 'withdraw-image'):
                with self.subTest(action=action):
                    path, record = self.prepare_cli_image(root, invalid_record=True)
                    result = subprocess.run([sys.executable, str(SPEC.origin), action,
                        str(path), str(record), str(root)], capture_output=True, text=True,
                        env={**os.environ, 'PATH':str(root) + os.pathsep + os.environ['PATH']}, timeout=30)
                    self.assertEqual(result.returncode, 1, result.stderr)
                    self.assertIn('retaining', result.stderr)
                    self.assertNotIn('Traceback', result.stderr)
                    self.assertEqual(path.read_bytes(), b'retained image bytes')
                    self.assertTrue((root / 'retain-create').exists())
                    self.assertFalse(count.exists(), 'queried inventory before proving the image identity')


@unittest.skipUnless(platform.system() == 'Darwin', 'requires macOS backing-inode lifecycle semantics')
class ImageIdentityTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.path = self.root / 'publication.dmg'
        self.record = self.root / 'identity.json'
        # Most unit fixtures synthesize hdiutil output. Supply the independent
        # reader inventory too; native builder tests exercise the real lsof.
        self.users_patch = patch.object(IMAGE, 'open_image_users', return_value=set())
        self.users = self.users_patch.start()
        self.addCleanup(self.users_patch.stop)

    def test_attachment_aliases_are_matched_by_file_identity(self):
        raw = self.root / 'É-image.dmg'
        raw.write_bytes(b'image')
        hardlink = self.root / 'hardlink.dmg'
        os.link(raw, hardlink)
        symlink = self.root / 'symlink.dmg'
        symlink.symlink_to(raw)
        for alias in (raw, self.root / 'é-IMAGE.DMG', self.root / 'E\u0301-image.dmg', hardlink, symlink):
            with self.subTest(alias=alias):
                self.assertTrue(alias.samefile(raw))
                image = {'image-path': str(alias), 'hdid-pid': 973,
                         'system-entities': [{'dev-entry': '/dev/disk973'}]}
                history = self.root / 'seen.json'
                with patch.object(IMAGE, 'staging_inventory', return_value=[image]):
                    with self.assertRaisesRegex(RuntimeError, 'attached as /dev/disk973'):
                        IMAGE.require_unattached(raw, IMAGE.identity(raw.stat()), history)
                self.assertEqual(json.loads(history.read_text()), ['/dev/disk973'])
                self.assertEqual(json.loads(history.with_suffix('.helpers.json').read_text()),
                                 {'/dev/disk973': [973]})
                self.assertTrue(raw.exists())

    def test_later_inventory_cannot_forget_a_live_observed_device(self):
        self.path.write_bytes(b'image')
        history = self.root / 'seen.json'
        history.write_text(json.dumps(['/dev/disk973']))
        exists = Path.exists
        def live_device(path):
            return str(path) == '/dev/disk973' or exists(path)
        with patch.object(IMAGE, 'staging_inventory', return_value=[]), \
                patch.object(Path, 'exists', live_device):
            with self.assertRaisesRegex(RuntimeError, 'previously observed'):
                IMAGE.require_unattached(self.path, IMAGE.identity(self.path.stat()), history)
        self.assertEqual(json.loads(history.read_text()), ['/dev/disk973'])
        self.assertTrue(self.path.exists())

    def test_always_omitted_attachment_reader_prevents_cleanup(self):
        self.path.write_bytes(b'image')
        self.users.return_value = {973}
        with patch.object(IMAGE, 'staging_inventory', return_value=[]):
            with self.assertRaisesRegex(RuntimeError, 'backing file still open'):
                IMAGE.require_unattached(self.path, IMAGE.identity(self.path.stat()), self.root / 'seen')
        self.assertTrue(self.path.exists())

    def test_recycled_name_requires_departed_helper_and_clear_pinned_reader_audit(self):
        self.path.write_bytes(b'image')
        history = self.root / 'seen.json'
        history.write_text(json.dumps(['/dev/disk973']))
        history.with_suffix('.helpers.json').write_text(json.dumps({'/dev/disk973': [973]}))
        exists = Path.exists
        for exited, readers in ((False, set()), (True, {974}), (True, set())):
            with self.subTest(exited=exited, readers=readers), \
                    patch.object(IMAGE, 'staging_inventory', return_value=[]), \
                    patch.object(IMAGE, 'helper_has_exited', return_value=exited), \
                    patch.object(Path, 'exists', lambda p: str(p) == '/dev/disk973' or exists(p)):
                self.users.return_value = readers
                if exited and not readers:
                    IMAGE.require_unattached(self.path, IMAGE.identity(self.path.stat()), history)
                else:
                    with self.assertRaisesRegex(RuntimeError, 'inventory'):
                        IMAGE.require_unattached(self.path, IMAGE.identity(self.path.stat()), history)
                self.assertEqual(json.loads(history.read_text()), ['/dev/disk973'])

        self.users.return_value = set()
        with patch.object(IMAGE, 'staging_inventory', return_value=[]), \
                patch.object(IMAGE, 'helper_has_exited', side_effect=[False, True]) as query, \
                patch.object(IMAGE.time, 'sleep'), \
                patch.object(Path, 'exists', lambda p: str(p) == '/dev/disk973' or exists(p)):
            IMAGE.require_unattached(self.path, IMAGE.identity(self.path.stat()), history)
            self.assertEqual(query.call_count, 2)

    def test_helper_exit_requires_positive_process_absence(self):
        for error, exited in ((None, False), (PermissionError(), False), (ProcessLookupError(), True)):
            with self.subTest(error=error), patch.object(IMAGE.os, 'kill', side_effect=error) as query:
                self.assertEqual(IMAGE.helper_has_exited(973), exited)
                query.assert_called_once_with(973, 0)

    def test_reader_inventory_retries_incomplete_snapshots_but_not_unknown_readers(self):
        self.users_patch.stop()
        with patch.object(IMAGE, 'image_users_snapshot', side_effect=[RuntimeError('incomplete'), set()]) as query, \
                patch.object(IMAGE.time, 'sleep'):
            self.assertEqual(IMAGE.open_image_users(self.path, 0), set())
            self.assertEqual(query.call_count, 2)
        with patch.object(IMAGE, 'image_users_snapshot', side_effect=RuntimeError('incomplete')) as query, \
                patch.object(IMAGE.time, 'sleep'):
            with self.assertRaisesRegex(RuntimeError, 'incomplete'):
                IMAGE.open_image_users(self.path, 0)
            self.assertEqual(query.call_count, 4)
        with patch.object(IMAGE, 'image_users_snapshot', return_value={973}) as query:
            self.assertEqual(IMAGE.open_image_users(self.path, 0), {973})
            self.assertEqual(query.call_count, 1)

    def test_reader_inventory_requires_the_pinned_vnode_and_preserves_unknown_readers(self):
        self.users_patch.stop()
        self.path.write_bytes(b'image')
        with self.path.open('rb') as source:
            dev, ino = IMAGE.identity(os.fstat(source.fileno()))
            own = f'p{os.getpid()}\0f{source.fileno()}\0tREG\0D0x{dev:x}\0i{ino}\0'.encode()
            other = f'p973\0f3\0tREG\0D0x{dev:x}\0i{ino}\0'.encode()
            for rc, output, error, expected in ((0, own, b'', set()), (0, own + other, b'', {973}),
                    (0, other, b'', None), (1, b'', b'', None), (0, own, b'warning', None)):
                with self.subTest(rc=rc, output=output, error=error), patch.object(IMAGE.subprocess, 'run',
                        return_value=subprocess.CompletedProcess([], rc, output, error)):
                    if expected is None:
                        with self.assertRaisesRegex(RuntimeError, 'inventory'):
                            IMAGE.open_image_users(self.path, source.fileno())
                    else:
                        self.assertEqual(IMAGE.open_image_users(self.path, source.fileno()), expected)

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

    def test_clean_staging_unlinks_only_the_saved_file_and_directory(self):
        stage = self.root / 'create.owned'
        stage.mkdir()
        raw = stage / 'image.dmg'
        raw.write_bytes(b'partial output')
        (self.root / 'staged-image.json').write_text(json.dumps({
            'path': str(raw), 'identity': IMAGE.identity(raw.stat()),
            'directory': IMAGE.identity(stage.stat())}))
        (self.root / 'retain-create').write_text(str(raw))
        with patch.object(IMAGE, 'staging_inventory', return_value=[]):
            IMAGE.clean_staging(self.root)
        self.assertFalse(stage.exists())
        self.assertFalse((self.root / 'retain-create').exists())

    def test_clean_staging_refuses_replaced_file_or_directory(self):
        for replace_directory in (False, True):
            with self.subTest(replace_directory=replace_directory):
                stage = self.root / f'create.{replace_directory}'
                stage.mkdir()
                raw = stage / 'image.dmg'
                raw.write_bytes(b'partial output')
                saved = {'path': str(raw), 'identity': IMAGE.identity(raw.stat()),
                         'directory': IMAGE.identity(stage.stat())}
                (self.root / 'staged-image.json').write_text(json.dumps(saved))
                (self.root / 'retain-create').write_text(str(raw))
                with raw.open('rb'):
                    if replace_directory:
                        stage.rename(stage.with_name(stage.name + '.original'))
                        stage.mkdir()
                    else:
                        raw.unlink()
                    raw.write_bytes(b'foreign output')
                    with patch.object(IMAGE, 'staging_inventory', return_value=[]):
                        with self.assertRaisesRegex(RuntimeError, 'identity changed'):
                            IMAGE.clean_staging(self.root)
                self.assertEqual(raw.read_bytes(), b'foreign output')
                self.assertTrue((self.root / 'retain-create').exists())

    def test_sidecar_exclusive_create_preserves_symlink_hardlink_and_regular_file(self):
        self.path.write_bytes(b'validated image')
        self.record.write_text(json.dumps(IMAGE.identity(self.path.stat())))
        victim = self.root / 'victim'
        victim.write_bytes(b'foreign sidecar')
        for kind in ('symlink', 'hardlink', 'regular'):
            with self.subTest(kind=kind):
                sidecar = self.root / kind
                if kind == 'symlink': sidecar.symlink_to(victim)
                elif kind == 'hardlink': os.link(victim, sidecar)
                else: sidecar.write_bytes(b'foreign sidecar')
                with self.assertRaises(FileExistsError):
                    IMAGE.checksum(self.path, self.record, sidecar)
                self.assertEqual(victim.read_bytes(), b'foreign sidecar')
                self.assertEqual(sidecar.read_bytes(), b'foreign sidecar')

    def test_checksum_refuses_sidecar_or_artifact_replacement_during_write(self):
        for replace_sidecar in (False, True):
            with self.subTest(replace_sidecar=replace_sidecar):
                self.path.write_bytes(b'validated image')
                self.record.write_text(json.dumps(IMAGE.identity(self.path.stat())))
                sidecar = self.root / f'digest-{replace_sidecar}'
                fdopen = IMAGE.os.fdopen
                def replace_after_write(fd, mode):
                    output = fdopen(fd, mode)
                    class Writer:
                        def __enter__(self): return self
                        def __exit__(self, *args): return output.__exit__(*args)
                        def __getattr__(self, name): return getattr(output, name)
                        def write(self, text):
                            result = output.write(text)
                            output.flush()
                            target = sidecar if replace_sidecar else self_path
                            target.unlink()
                            target.write_bytes(b'foreign replacement')
                            return result
                    return Writer()
                self_path = self.path
                with patch.object(IMAGE.os, 'fdopen', side_effect=replace_after_write):
                    with self.assertRaisesRegex(RuntimeError, 'identity changed'):
                        IMAGE.checksum(self.path, self.record, sidecar)
                target = sidecar if replace_sidecar else self.path
                self.assertEqual(target.read_bytes(), b'foreign replacement')

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
        digest = IMAGE.file_digest
        def replace_after_hash(source, algorithm):
            result = digest(source, algorithm)
            self.path.unlink()
            self.path.write_bytes(b'foreign replacement')
            return result
        with patch.object(IMAGE, 'file_digest', side_effect=replace_after_hash):
            with self.assertRaisesRegex(RuntimeError, 'identity changed'):
                IMAGE.checksum(self.path, self.record, self.root / 'digest')
        self.assertFalse((self.root / 'digest').exists())

    def test_chunked_digest_matches_native_for_empty_and_multichunk_files(self):
        for data in (b'', b'abc', b'0123456789' * 250000):
            with self.subTest(size=len(data)):
                self.path.write_bytes(data)
                with self.path.open('rb') as source, patch.object(IMAGE.hashlib, 'file_digest', None, create=True):
                    self.assertEqual(IMAGE.file_digest(source, 'sha256').digest(),
                                     hashlib.sha256(data).digest())

    def test_sealed_image_refuses_inplace_change_and_accepts_unchanged_bytes(self):
        self.path.write_bytes(b'validated image')
        expected = IMAGE.identity(self.path.stat())
        self.record.write_text(json.dumps(expected))
        with self.path.open('rb') as pinned:
            pinned.read()  # A retry may inherit the pin's previous read offset.
            IMAGE.seal(self.path, self.record, Path('/dev/fd/' + str(pinned.fileno())))
        IMAGE.checksum(self.path, self.record, self.root / 'good-digest')
        self.path.write_bytes(b'modified image')
        self.assertEqual(IMAGE.identity(self.path.stat()), expected)
        with self.assertRaisesRegex(RuntimeError, 'disk image bytes changed since validation'):
            IMAGE.checksum(self.path, self.record, self.root / 'bad-digest')
        self.assertFalse((self.root / 'bad-digest').exists())

    def test_seal_refuses_a_foreign_descriptor_even_with_original_path(self):
        self.path.write_bytes(b'created image')
        self.record.write_text(json.dumps(IMAGE.identity(self.path.stat())))
        foreign = self.root / 'foreign.dmg'
        foreign.write_bytes(b'foreign bytes')
        with foreign.open('rb') as pinned:
            with self.assertRaisesRegex(RuntimeError, 'identity changed before validation'):
                IMAGE.seal(self.path, self.record, Path('/dev/fd/' + str(pinned.fileno())))

    def test_package_snapshots_the_private_source_even_if_publication_is_edited(self):
        source = self.root / 'built.pkg'
        source.write_bytes(b'private signed bytes')
        (self.root / 'image').mkdir()
        copy = IMAGE.shutil.copyfileobj
        def edit_publication(reader, writer):
            copy(reader, writer)
            writer.flush()
            if writer.name == str(self.path):
                self.path.write_bytes(b'foreign publication')
        with patch.object(IMAGE.shutil, 'copyfileobj', side_effect=edit_publication), \
                patch.object(IMAGE.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0)):
            with self.assertRaisesRegex(RuntimeError, 'published package bytes changed since validation'):
                IMAGE.package(self.path, self.record, source, self.root, self.root, '1.2.3',
                              'aarch64-apple-darwin', self.root / 'verify.py')
        self.assertEqual((self.root / 'validated.pkg').read_bytes(), source.read_bytes())
        self.assertEqual((self.root / 'image' / self.path.name).read_bytes(), source.read_bytes())

    def test_failed_package_withdrawal_removes_only_our_unchanged_publication(self):
        for kind in ('unchanged', 'partial', 'replaced', 'edited', 'symlink'):
            with self.subTest(kind=kind):
                path = self.root / (kind + '.pkg')
                path.write_bytes(b'our bytes')
                saved = {'identity': IMAGE.identity(path.stat()),
                         'sha256': hashlib.sha256(path.read_bytes()).hexdigest()}
                with path.open('rb'):
                    if kind == 'partial': saved = {'identity': saved['identity'], 'copying': True}
                    if kind in ('replaced', 'symlink'):
                        path.unlink()
                    if kind in ('replaced', 'edited'): path.write_bytes(b'foreign bytes')
                    if kind == 'symlink': path.symlink_to(self.root / 'missing-foreign')
                    self.record.write_text(json.dumps(saved))
                    if kind in ('unchanged', 'partial'):
                        IMAGE.withdraw(path, self.record)
                        self.assertFalse(path.exists())
                    else:
                        with self.assertRaisesRegex(RuntimeError, 'changed'):
                            IMAGE.withdraw(path, self.record)
                        self.assertTrue(path.exists() or path.is_symlink())

    def test_withdrawal_removes_own_sidecar_and_preserves_a_replaced_sidecar(self):
        for replaced in (False, True):
            with self.subTest(replaced=replaced):
                self.path.write_bytes(b'our bytes')
                self.record.write_text(json.dumps({'identity': IMAGE.identity(self.path.stat()),
                    'sha256': hashlib.sha256(self.path.read_bytes()).hexdigest()}))
                sidecar = self.root / f'sidecar-{replaced}'
                IMAGE.checksum(self.path, self.record, sidecar)
                with sidecar.open('rb'):
                    if replaced:
                        sidecar.unlink()
                        sidecar.write_bytes(b'foreign sidecar')
                        with self.assertRaisesRegex(RuntimeError, 'identity changed'):
                            IMAGE.withdraw(self.path, self.record)
                        self.assertEqual(sidecar.read_bytes(), b'foreign sidecar')
                        self.assertTrue(self.path.exists())
                    else:
                        IMAGE.withdraw(self.path, self.record)
                        self.assertFalse(sidecar.exists())
                        self.assertFalse(self.path.exists())

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
        for output, expected in ((b'p123\0\nf3\0tREG\0D0x10\0i42\0n/x/image.dmg\0\n', True),
                                 (b'p123\0\nf3\0tREG\0D0x10\0i43\0n/x/image.dmg\0\n', False),
                                 (b'p123\0\nf3\0tREG\0D0x11\0i42\0n/x/image.dmg\0\n', False),
                                 (b'p123\0\nftxt\0D0x10\0i42\0\n', None)):
            with self.subTest(output=output), patch.object(IMAGE.subprocess, 'run',
                    return_value=subprocess.CompletedProcess([], 0, output)):
                self.assertEqual(IMAGE.backing_identity({'hdid-pid': 123, 'image-path': '/x/image.dmg'}, [16, 42]), expected)
        self.assertFalse(IMAGE.backing_identity({}, [16, 42]))

    def test_backing_identity_judges_each_file_set_on_its_own(self):
        # Shape of a real diskimages-helper listing (-F0fDtin): cwd, txt and
        # character-device sets carry no backing authority and are skipped.
        head = (b'p973\0\nfcwd\0tDIR\0D0x10\0i2\0n/\0\n'
                b'ftxt\0tREG\0D0x10\0i9\0n/System/diskimages-helper\0\nftxt\0tREG\0n/lib\0\n'
                b'f0\0tCHR\0D0xdd\0i336\0n/dev/null\0\nf4\0tPIPE\0n->0x1\0\nf5\0tunix\0\n')
        image = {'hdid-pid': 973, 'image-path': '/x/image.dmg'}
        def query(output):
            with patch.object(IMAGE.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0, output, b'')):
                return IMAGE.backing_identity(image, [16, 42])
        own = b'f3\0tREG\0D0x10\0i42\0n/x/image.dmg\0\n'
        other = b'f6\0tREG\0D0x10\0i7\0n/x/other\0\n'
        self.assertTrue(query(head + own))
        self.assertTrue(query(head + other + own))
        self.assertTrue(query(head + own[:-1]))
        self.assertTrue(query(head + b'f3\0tREG\0D0x10\0i42\0\n'))
        self.assertIsNone(query(head + other))
        self.assertFalse(query(head + b'f3\0tREG\0D0x10\0i43\0n/x/image.dmg\0\n'))
        # D and i of different records can never pair, whichever fields were lost.
        uncertain = {
            'second set lost f and D': other + b'tREG\0i42\0n/y/lib\0\n',
            'first lost i, second lost f and D': b'f6\0tREG\0D0x10\0n/x/other\0\ntREG\0i42\0n/y/lib\0\n',
            'second set lost f, t and D': b'f6\0tREG\0D0x10\0n/x/other\0\ni42\0n/y/lib\0\n',
            'regular file lost D': b'f6\0tREG\0i42\0n/x/image.dmg\0\n',
            'regular file lost i': b'f6\0tREG\0D0x10\0n/x/image.dmg\0\n',
            'regular file lost D and i': b'f6\0tREG\0n/x/image.dmg\0\n',
            'numeric descriptor lost t': b'f6\0D0x10\0i42\0n/x/image.dmg\0\n',
            'invalid D': b'f6\0tREG\0D0xg\0i42\0\n',
            'invalid i': b'f6\0tREG\0D0x10\0i4x\0\n',
            'lost set delimiter': other[:-1] + own,
            'duplicate field': b'f3\0tREG\0D0x10\0i42\0i42\0\n',
            'empty field': b'f3\0tREG\0\0D0x10\0i42\0\n',
            'unterminated field': own + b'f7\0tREG\0D0x10\0i4',
            'unterminated trailing junk': own + b'nignored',
            'process set with file fields': b'p974\0f3\0tREG\0D0x10\0i42\0\n',
        }
        for name, tail in uncertain.items():
            with self.subTest(name=name):
                self.assertIsNone(query(head + tail))
                self.assertIsNone(query(head + own + tail))

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
