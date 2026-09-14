import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('build_native', Path(__file__).with_name('build-native.py'))
native = importlib.util.module_from_spec(spec)
spec.loader.exec_module(native)


class WorkspaceVersionTest(unittest.TestCase):
    def test_workspace_authority_not_dependency_version(self):
        manifest = '[package]\nversion = "1.2.3"\n[workspace.package]\nversion = "0.0.970" # build\n[dependencies]\nversion = "9.9.9"\n'
        self.assertEqual(native.workspace_version(manifest), '0.0.970')

    def test_missing_or_non_numeric_version_is_rejected(self):
        for text in ('[package]\nversion="0.0.970"', '[workspace.package]\nversion="next"'):
            with self.assertRaises(ValueError):
                native.workspace_version(text)


class NativeCheckpointTest(unittest.TestCase):
    def test_source_digest_invalidates_for_rust_recipe_and_lock_but_not_kotlin_ui(self):
        import subprocess
        import tempfile
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            subprocess.run(['git', 'init', '-q', str(root)], check=True)
            inputs = ['Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml', '.cargo/config.toml',
                      'crates/embed/src/lib.rs', 'crates/embed/build.rs', 'crates/embed/data.bin',
                      'scripts/android/build-native.py', 'android/buildSrc/src/main/kotlin/HaiderNative.kt',
                      '.github/workflows/android-apk.yml', 'customprov.bundle']
            for name in inputs + ['android/app/src/main/Screen.kt']:
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text('before')
            with patch.object(native, 'ROOT', root):
                original = native.source_digest()
                for name in inputs:
                    path = root / name
                    path.write_text('after')
                    self.assertNotEqual(native.source_digest(), original, name)
                    path.write_text('before')
                (root / 'android/app/src/main/Screen.kt').write_text('UI only')
                self.assertEqual(native.source_digest(), original)
                (root / inputs[4]).unlink()
                self.assertNotEqual(native.source_digest(), original)

    def test_checkpoint_rejects_stale_missing_modified_or_incomplete_outputs(self):
        import json
        import tempfile
        from unittest.mock import Mock, patch
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            abi, digest, version, elf_id = 'arm64-v8a', 'a' * 64, '0.0.971', 'b' * 40
            names = native.output_files(abi, elf_id)
            for name in names:
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(b'checkpoint fixture')
            manifest = dict(format=1, source_digest=digest, version=version, abi=abi,
                            sha256={name: native.sha256(root / name) for name in names})
            manifest_path = root / f'manifest-{abi}.json'
            manifest_path.write_text(json.dumps(manifest))
            # ELF/ABI checks themselves have independent real-readelf tests.
            verifier = Mock()
            verifier.verify_so.return_value = {'metadata': dict(build_id=digest, ndk=native.NDK_VERSION,
                                                                elf_build_id=elf_id)}
            with patch.object(native, 'native_verifier', return_value=verifier):
                native.verify_output(root, [abi], version, digest, 'readelf')
                with self.assertRaisesRegex(ValueError, 'Stale'):
                    native.verify_output(root, [abi], version, 'c' * 64, 'readelf')
                for name in names:
                    path = root / name
                    for bad in (b'', b'corrupted'):
                        path.write_bytes(bad)
                        with self.assertRaisesRegex(ValueError, 'checksum mismatch'):
                            native.verify_output(root, [abi], version, digest, 'readelf')
                    path.unlink()
                    with self.assertRaisesRegex(ValueError, 'checksum mismatch'):
                        native.verify_output(root, [abi], version, digest, 'readelf')
                    path.write_bytes(b'checkpoint fixture')
                del manifest['sha256'][names[-1]]
                manifest_path.write_text(json.dumps(manifest))
                with self.assertRaisesRegex(ValueError, 'manifest'):
                    native.verify_output(root, [abi], version, digest, 'readelf')


if __name__ == '__main__':
    unittest.main()
