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
        stubdir = self.root / 'bin'
        stubdir.mkdir()
        stub = stubdir / 'rustc'
        stub.write_text(f'#!{sys.executable}\nimport os\nprint(os.environ["RUSTC_FIXTURE"])\n')
        stub.chmod(0o755)
        self.env = dict(os.environ, PATH=str(stubdir) + os.pathsep + os.environ['PATH'],
                        CARGO_INCREMENTAL='0', RUSTC_FIXTURE=RUSTC)
        for binary in MODULE.BINARIES:
            path = self.release / binary
            path.write_bytes(b'unsigned fixture bytes: ' + binary.encode())
            path.chmod(0o755)
        for binary in MODULE.RUNTIME:
            symbols = self.release / f'{binary}.dSYM/Contents'
            dwarf = symbols / 'Resources/DWARF'
            dwarf.mkdir(parents=True)
            (dwarf / binary).write_bytes(b'line tables')
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

    def test_missing_symbols_and_nonexecutable_payload_rejected(self):
        (self.release / 'haiderd.dSYM/Contents/Info.plist').unlink()
        self.assertNotEqual(self.cli('pack').returncode, 0)
        (self.release / 'haiderd.dSYM/Contents/Info.plist').write_text('restored')
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
