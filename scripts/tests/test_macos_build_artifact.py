"""Round-trip the actual artifact CLI and reject altered bytes/build identity."""

import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / 'release/macos-build-artifact.py'
SPEC = importlib.util.spec_from_file_location('macos_build_artifact', SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)
SHA = 'a' * 40
RUSTC = 'rustc 1.95.0 (fixture 2026-04-14)\nhost: aarch64-apple-darwin\nrelease: 1.95.0\n'
UUID = '01234567-89AB-CDEF-0123-456789ABCDEF'


class ArtifactTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.release = self.root / 'release'
        self.release.mkdir()
        self.bundle = self.root / 'bundle'
        self.restored = self.root / 'restored'
        (self.root / 'Cargo.toml').write_text('[profile.release]\nlto="fat"\ncodegen-units=1\n')
        (self.root / 'Cargo.lock').write_text('version = 4\n')
        (self.root / 'sitecustomize.py').write_text(
            'import os, subprocess\n'
            'original = subprocess.check_output\n'
            'def check_output(command, **kwargs):\n'
            '    if command[0] == "rustc": return os.environ["RUSTC_FIXTURE"]\n'
            '    if command[:3] == ["xcrun", "dwarfdump", "--uuid"]:\n'
            '        code = int(os.environ.get("DWARFDUMP_EXIT", "0"))\n'
            '        if code: raise subprocess.CalledProcessError(code, command)\n'
            '        key = "SYMBOL_UUID" if command[3].endswith(".dSYM") else "BINARY_UUID"\n'
            '        value = os.environ[key]\n'
            '        return "UUID: " + value + " (arm64) " + command[3] + "\\n" if value else ""\n'
            '    return original(command, **kwargs)\n'
            'subprocess.check_output = check_output\n'
            # Like posix_mode_fixture.py: Windows cannot set POSIX execute bits.
            # Only these generated macOS fixture paths receive synthetic modes.
            'if os.name == "nt":\n'
            '    from pathlib import Path\n'
            '    native_stat = Path.stat\n'
            '    def fixture_stat(path, *args, **kwargs):\n'
            '        result = native_stat(path, *args, **kwargs)\n'
            f'        if path.parent.name == "release" and path.name in {MODULE.BINARIES!r}:\n'
            '            values = list(result); values[0] |= 0o111\n'
            '            return os.stat_result(values)\n'
            '        return result\n'
            '    Path.stat = fixture_stat\n')
        self.env = dict(os.environ, PYTHONPATH=str(self.root),
                        CARGO_INCREMENTAL='0', RUSTC_FIXTURE=RUSTC,
                        BINARY_UUID=UUID, SYMBOL_UUID=UUID, DWARFDUMP_EXIT='0')
        for binary in MODULE.BINARIES:
            path = self.release / binary
            path.write_bytes(b'unsigned fixture bytes: ' + binary.encode())
            path.chmod(0o755)
        for binary in MODULE.RUNTIME:
            symbols = self.release / f'{binary}.dSYM/Contents'
            dwarf = symbols / 'Resources/DWARF'
            dwarf.mkdir(parents=True)
            # Observed with native Rust 1.95.0, --target aarch64-apple-darwin,
            # packed line tables + strip=symbols, both thin/16 and fat/1 LTO.
            # Cargo renames the outer bundle, not the hashed rustc members.
            member = binary.replace('-', '_') + '-0123456789abcdef'
            (dwarf / member).write_bytes(b'line tables')
            relocations = symbols / 'Resources/Relocations/aarch64'
            relocations.mkdir(parents=True)
            (relocations / (member + '.yml')).write_bytes(b'fixture relocations')
            (symbols / 'Info.plist').write_bytes(b'fixture plist')

    def cli(self, command, *, sha=SHA, env=None):
        return subprocess.run([sys.executable, str(SCRIPT), command, '--repo', str(self.root),
                               '--sha', sha, '--bundle', str(self.bundle), '--release-dir',
                               str(self.release if command == 'pack' else self.restored)],
                              env=env or self.env, capture_output=True, text=True)

    def pack(self):
        result = self.cli('pack')
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_round_trip_preserves_all_bytes_and_execute_permission(self):
        self.pack()
        result = self.cli('restore')
        self.assertEqual(result.returncode, 0, result.stderr)
        for original in self.release.rglob('*'):
            if original.is_file():
                restored = self.restored / original.relative_to(self.release)
                self.assertEqual(original.read_bytes(), restored.read_bytes())
                self.assertEqual(bool(original.stat().st_mode & 0o111), bool(restored.stat().st_mode & 0o111))

    def test_sha_rustc_profile_lock_and_incremental_mismatch(self):
        self.pack()
        attempts = [dict(sha='b' * 40), dict(env=dict(self.env, RUSTC_FIXTURE=RUSTC + 'different')),
                    dict(env=dict(self.env, CARGO_PROFILE_RELEASE_LTO='thin')),
                    dict(env=dict(self.env, CARGO_INCREMENTAL='1'))]
        for kwargs in attempts:
            with self.subTest(kwargs=kwargs):
                self.assertNotEqual(self.cli('restore', **kwargs).returncode, 0)
                self.assertFalse(self.restored.exists())
        (self.root / 'Cargo.lock').write_text('changed')
        self.assertNotEqual(self.cli('restore').returncode, 0)

    def rewrite_archive(self, mutation):
        archive_path = self.bundle / 'release-build.tar.gz'
        with tarfile.open(archive_path, 'r:gz') as archive:
            entries = [(member, archive.extractfile(member).read()) for member in archive.getmembers()]
        entries = mutation(entries)
        with tarfile.open(archive_path, 'w:gz') as archive:
            for member, data in entries:
                archive.addfile(member, io.BytesIO(data))

    def test_corrupt_missing_extra_duplicate_and_unsafe_members(self):
        def corrupt(entries):
            member, data = entries[0]
            return [(member, bytes([data[0] ^ 1]) + data[1:]), *entries[1:]]
        def unsafe(entries):
            entries[0][0].name = '../outside'
            return entries
        def symlink(entries):
            entries[0][0].type = tarfile.SYMTYPE
            entries[0][0].linkname = '/tmp/outside'
            return entries
        for mutation in [corrupt, lambda e: e[1:], lambda e: e + e[:1], unsafe, symlink]:
            with self.subTest(mutation=mutation):
                self.pack()
                self.rewrite_archive(mutation)
                result = self.cli('restore')
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertFalse(self.restored.exists())

    def test_missing_symbols_rejected(self):
        (self.release / 'haiderd.dSYM/Contents/Info.plist').unlink()
        self.assertNotEqual(self.cli('pack').returncode, 0)
        (self.release / 'haiderd.dSYM/Contents/Info.plist').write_text('restored')

    def test_missing_dwarf_rejected_by_pack_and_restore(self):
        self.pack()
        name = 'haiderd.dSYM/Contents/Resources/DWARF/haiderd-0123456789abcdef'
        (self.release / name).unlink()
        self.assertIn('missing DWARF member', self.cli('pack').stderr)
        manifest_path = self.bundle / 'manifest.json'
        manifest = json.loads(manifest_path.read_text())
        del manifest['files'][name]
        manifest_path.write_text(json.dumps(manifest))
        self.rewrite_archive(lambda entries: [(m, d) for m, d in entries if m.name != name])
        self.assertIn('missing DWARF member', self.cli('restore').stderr)
        self.assertFalse(self.restored.exists())

    def test_symbol_uuid_mismatch_missing_uuid_and_tool_failure_rejected(self):
        self.pack()
        for changes in [dict(SYMBOL_UUID='FFFFFFFF-FFFF-FFFF-FFFF-FFFFFFFFFFFF'),
                        dict(SYMBOL_UUID=''), dict(BINARY_UUID=''), dict(DWARFDUMP_EXIT='1')]:
            for command in ['pack', 'restore']:
                with self.subTest(changes=changes, command=command):
                    result = self.cli(command, env=dict(self.env, **changes))
                    self.assertNotEqual(result.returncode, 0, result.stdout)
                    self.assertFalse(self.restored.exists())

    @unittest.skipUnless(os.name == 'posix',
                         'POSIX execute bits are not writable on Windows; covered by '
                         "xplat-check Linux check leg's pipeline regression tests")
    def test_nonexecutable_payload_rejected(self):
        (self.release / 'haider').chmod(0o644)
        self.assertNotEqual(self.cli('pack').returncode, 0)

    def test_manifest_checksum_and_mode_cannot_be_ignored(self):
        for field, value in [('sha256', '0' * 64), ('mode', 0o644), ('size', 0)]:
            self.pack()
            manifest_path = self.bundle / 'manifest.json'
            manifest = json.loads(manifest_path.read_text())
            manifest['files']['haider'][field] = value
            manifest_path.write_text(json.dumps(manifest))
            self.assertNotEqual(self.cli('restore').returncode, 0)
            self.assertFalse(self.restored.exists())


if __name__ == '__main__':
    unittest.main()
