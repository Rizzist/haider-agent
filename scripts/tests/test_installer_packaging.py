"""Archive-to-installer regressions, including fail-closed mutation checks."""
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest
import zipfile
from scripts.tests.posix_mode_fixture import model_modes

ROOT = Path(__file__).resolve().parents[2]


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, ROOT / path)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


payload = module('installer_payload', 'scripts/installer_payload.py')
linux = module('linux_build', 'packaging/installers/linux-build.py')


class InstallerPackagingTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def archive(self, members=None, windows=False):
        members = members or {'bundle/haider': b'control', 'bundle/haiderd': b'daemon',
                              'bundle/haider-interactive': b'future payload'}
        archive = self.root / ('haider-v0.0.970-x86_64-pc-windows-msvc.zip' if windows else 'haider-v0.0.970-x86_64-unknown-linux-gnu.tar.xz')
        if windows:
            with zipfile.ZipFile(archive, 'w') as out:
                for name, data in members.items():
                    out.writestr(name, data)
        else:
            with tarfile.open(archive, 'w:xz') as out:
                for name, data in members.items():
                    info = tarfile.TarInfo(name)
                    info.size = len(data)
                    info.mode = 0o755
                    out.addfile(info, io.BytesIO(data))
        Path(str(archive) + '.sha256').write_text(f'{payload.digest(archive.read_bytes())}  {archive.name}\n')
        return archive

    def prepare(self, archive=None, **kwargs):
        return payload.prepare(archive or self.archive(), self.root / 'payload', '0.0.970',
                               kwargs.pop('target', 'x86_64-unknown-linux-gnu'), **kwargs)

    def test_dynamic_members_preserve_exact_bytes_and_future_payload(self):
        manifest = self.prepare()
        self.assertEqual(set(manifest['members']), {'haider', 'haiderd', 'haider-interactive'})
        self.assertEqual((self.root / 'payload/haider-interactive').read_bytes(), b'future payload')
        self.assertIn('haider-interactive', (self.root / 'payload/uninstall-haider.sh').read_text())
        self.assertEqual(manifest['members']['haider'], payload.digest(b'control'))

    def test_complete_split_archive_preferred_and_ambiguity_rejected(self):
        canonical = self.archive()
        self.assertEqual(payload.select_archive(self.root, 'x86_64-unknown-linux-gnu'), canonical)
        split = canonical.with_name(canonical.name.replace('.tar.xz', '-split.tar.xz'))
        split.write_bytes(canonical.read_bytes())
        self.assertEqual(payload.select_archive(self.root, 'x86_64-unknown-linux-gnu'), split)
        split.with_name(split.name.replace('0.0.970', '0.0.969')).write_bytes(b'old')
        with self.assertRaisesRegex(ValueError, 'ambiguous'):
            payload.select_archive(self.root, 'x86_64-unknown-linux-gnu')

    def test_windows_discovers_exes_but_excludes_launcher(self):
        archive = self.archive({'bundle/haider.exe': b'cli', 'bundle/haiderd.exe': b'd',
                                'bundle/haider-interactive.exe': b'i', 'bundle/haider.cmd': b'cmd'}, True)
        manifest = self.prepare(archive, target='x86_64-pc-windows-msvc')
        self.assertEqual(len(manifest['members']), 3)
        self.assertNotIn('haider.cmd', manifest['members'])

    def test_checksum_and_sidecar_filename_mutations_fail(self):
        archive = self.archive()
        sidecar = Path(str(archive) + '.sha256')
        for content in ('0'*64 + '  ' + archive.name, payload.digest(archive.read_bytes()) + '  wrong.tar.xz'):
            sidecar.write_text(content)
            with self.assertRaisesRegex(ValueError, 'checksum/filename'):
                self.prepare(archive)

    def test_traversal_missing_sibling_and_duplicate_members_fail(self):
        for members in ({'bundle/haider': b'x'},
                        {'../haider': b'x', 'bundle/haiderd': b'd'},
                        {'one/haider': b'x', 'two/haiderd': b'd'},
                        {'one/haider': b'x', 'two/haider': b'y', 'one/haiderd': b'd'}):
            with self.subTest(members=members), self.assertRaises(ValueError):
                self.prepare(self.archive(members))

    def test_symlink_archive_is_rejected(self):
        archive = self.archive()
        with tarfile.open(archive, 'w:xz') as out:
            entry = tarfile.TarInfo('bundle/haider')
            entry.type = tarfile.SYMTYPE
            entry.linkname = '/bin/sh'
            out.addfile(entry)
        Path(str(archive)+'.sha256').write_text(f'{payload.digest(archive.read_bytes())}  {archive.name}\n')
        with self.assertRaisesRegex(ValueError, 'links/special'):
            self.prepare(archive)

    def test_explicit_member_list_and_stale_destination_fail(self):
        with self.assertRaisesRegex(ValueError, 'member list'):
            self.prepare(members='haider,haiderd')
        self.prepare()
        with self.assertRaises(FileExistsError):
            self.prepare()

    def test_valid_but_wrong_target_archive_is_rejected(self):
        with self.assertRaisesRegex(ValueError, 'archive target/format mismatch'):
            self.prepare(self.archive(), target='aarch64-unknown-linux-gnu')

    def test_version_and_target_inputs_fail_closed(self):
        archive = self.archive()
        for version, target in [('0.0.970;whoami', 'x86_64-unknown-linux-gnu'), ('0.0.970', '../bad')]:
            with self.assertRaises(ValueError):
                payload.prepare(archive, self.root/'payload', version, target)

    def test_linux_postpack_rejects_version_hash_extra_and_missing(self):
        manifest = self.prepare()
        source = self.root / 'payload'
        with self.assertRaisesRegex(ValueError, 'version/target'):
            linux.read_manifest(source, '0.0.969', manifest['target'])
        tree = self.root / 'tree'
        linux.stage(tree, source, manifest)
        model_modes(self, {tree / 'usr/bin' / name: 0o755 for name in manifest['members']})
        linux.verify_tree(tree, source, manifest)
        member = tree / 'usr/bin/haider'
        member.write_bytes(b'changed')
        with self.assertRaisesRegex(ValueError, 'mismatch'):
            linux.verify_tree(tree, source, manifest)
        member.write_bytes(b'control')
        extra = tree / 'usr/bin/unexpected'
        extra.write_bytes(b'x')
        with self.assertRaisesRegex(ValueError, 'members differ'):
            linux.verify_tree(tree, source, manifest)
        extra.unlink()
        member.unlink()
        with self.assertRaisesRegex(ValueError, 'members differ'):
            linux.verify_tree(tree, source, manifest)

    def test_windows_path_cleanup_precedes_native_environment_broadcast(self):
        source = (ROOT / 'packaging/installers/windows.iss').read_text()
        callback = source.split('procedure CurUninstallStepChanged', 1)[1]
        before_cleanup, cleanup = callback.split('if Step <> usUninstall then exit;', 1)
        self.assertIn('if Step = usPostUninstall then begin', before_cleanup)
        self.assertIn('if not UninstallSilent then', before_cleanup)
        self.assertIn("RegDeleteValue(HKCU, 'Environment', 'Path')", cleanup)
        self.assertIn('RegDeleteKeyIncludingSubkeys(HKCU, OwnerKey)', cleanup)
        self.assertNotIn('usPostUninstall', cleanup)

    @unittest.skipIf(os.name == 'nt', 'POSIX shell uninstaller runs on macOS/Linux')
    def test_tarball_uninstall_refuses_changed_binary_before_any_removal(self):
        self.prepare()
        prefix = self.root / 'payload'
        (prefix / 'haiderd').write_bytes(b'replacement from a different installer')
        env = dict(os.environ, HOME=str(self.root))
        env.pop('SUDO_USER', None)
        result = subprocess.run(['sh', str(prefix/'uninstall-haider.sh'), '--keep-state'],
                                env=env, capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('Refusing changed binary', result.stderr)
        self.assertTrue((prefix/'haider').exists())
        self.assertTrue((prefix/'haiderd').exists())

    @unittest.skipIf(os.name == 'nt', 'POSIX shell uninstaller runs on macOS/Linux')
    def test_tarball_uninstall_preserves_state_and_unrelated_files(self):
        self.prepare()
        prefix = self.root / 'payload'
        home = self.root / 'home'
        (home / '.haider').mkdir(parents=True)
        (home / '.haider/sentinel').write_text('retain')
        (prefix / 'unrelated').write_text('retain')
        env = dict(os.environ, HOME=str(home))
        env.pop('SUDO_USER', None)
        result = subprocess.run(['sh', str(prefix/'uninstall-haider.sh'), '--prefix', str(prefix)],
                                env=env, stdin=subprocess.DEVNULL, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((prefix/'haider').exists())
        self.assertFalse((prefix/'haiderd').exists())
        self.assertFalse((prefix/'haider-interactive').exists())
        self.assertEqual((home/'.haider/sentinel').read_text(), 'retain')
        self.assertEqual((prefix/'unrelated').read_text(), 'retain')


if __name__ == '__main__':
    unittest.main()
