import importlib.util
import json
from pathlib import Path
import struct
import tempfile
import unittest
from unittest.mock import patch
import zipfile

spec = importlib.util.spec_from_file_location('verify_native', Path(__file__).with_name('verify-native.py'))
verify = importlib.util.module_from_spec(spec)
spec.loader.exec_module(verify)


def fixture(machine=183, alignment=16384, relro=True, version='0.0.971'):
    metadata = json.dumps(dict(format=1, daemon_version=version, abi='arm64-v8a', api=26,
                              jni_version=1, wire_protocol=1, build_id='a' * 64)).encode()
    names = b'\0.shstrtab\0.haider.build\0'
    data = bytearray(512 + len(metadata))
    data[:7] = b'\x7fELF\x02\x01\x01'
    struct.pack_into('<HH', data, 16, 3, machine)
    struct.pack_into('<QQ', data, 32, 64, 192)
    struct.pack_into('<HHHHH', data, 54, 56, 2, 64, 3, 1)
    struct.pack_into('<IIQQQQQQ', data, 64, 1, 5, 0, 0, 0, len(data), len(data), alignment)
    struct.pack_into('<IIQQQQQQ', data, 120, 0x6474e552 if relro else 0, 4, 0, 0, 0, 1, 1, 1)
    struct.pack_into('<IIQQQQIIQQ', data, 256, 1, 3, 0, 0, 400, len(names), 0, 0, 1, 0)
    struct.pack_into('<IIQQQQIIQQ', data, 320, 11, 1, 0, 0, 512, len(metadata), 0, 0, 1, 0)
    data[400:400 + len(names)] = names
    data[512:] = metadata
    return bytes(data)


class NativeGateTest(unittest.TestCase):
    def test_valid_static_elf(self):
        self.assertEqual(verify.inspect_elf(fixture(), 'arm64-v8a', '0.0.971')['api'], 26)

    def test_mutations_fail_closed(self):
        for data in (fixture(machine=62), fixture(alignment=4096), fixture(relro=False),
                     fixture(version='wrong'), fixture()[:80], b'not an ELF'):
            with self.subTest(data=data[:20]), self.assertRaises(verify.InvalidNative):
                verify.inspect_elf(data, 'arm64-v8a', '0.0.971')

    def test_et_dyn_required(self):
        data = bytearray(fixture())
        struct.pack_into('<H', data, 16, 2)
        with self.assertRaisesRegex(verify.InvalidNative, 'ET_DYN'):
            verify.inspect_elf(data, 'arm64-v8a', '0.0.971')


class ApkGateTest(unittest.TestCase):
    def verify_archive(self, changes=None):
        changes = changes or {}
        with tempfile.TemporaryDirectory() as directory:
            apk = Path(directory) / 'app.apk'
            names = changes.get('names', ['lib/arm64-v8a/' + name for name in sorted(verify.PACKAGED_LIBRARIES)])
            with zipfile.ZipFile(apk, 'w') as archive:
                for name in names:
                    info = zipfile.ZipInfo(name)
                    info.compress_type = changes.get('compression', zipfile.ZIP_STORED)
                    # A real local ZIP header, with valid custom padding extra field.
                    padding = (-(archive.fp.tell() + 30 + len(name) + 4)) % 16384
                    info.extra = struct.pack('<HH', 0xcafe, padding) + bytes(padding)
                    if changes.get('unaligned'):
                        info.extra = b''
                    archive.writestr(info, fixture(alignment=4096 if changes.get('bad_dependency') and 'graphics' in name else 16384))
            checked = []
            def so(path, abi, version, readelf):
                checked.append((path.name, version))
                return verify.inspect_elf(path.read_bytes(), abi, version)
            with patch.object(verify.subprocess, 'check_output', return_value="package: versionName='0.0.971'"), \
                    patch.object(verify.subprocess, 'run') as align, patch.object(verify, 'verify_so', side_effect=so):
                result = verify.verify_apk(apk, 'arm64-v8a', '0.0.971', 'readelf', 'zipalign', 'aapt2')
                align.assert_called_once()
            return result, checked

    def test_checks_compose_and_haider_libraries(self):
        result, checked = self.verify_archive()
        self.assertEqual(set(result['libraries']), verify.PACKAGED_LIBRARIES)
        self.assertEqual(set(checked), {('libhaider.so', '0.0.971'), ('libandroidx.graphics.path.so', None)})

    def test_rejects_missing_extra_wrong_abi_duplicate_and_bad_alignment(self):
        correct = ['lib/arm64-v8a/' + name for name in sorted(verify.PACKAGED_LIBRARIES)]
        for change in ({'names': correct[:1]}, {'names': correct + ['lib/arm64-v8a/libunexpected.so']},
                       {'names': [name.replace('arm64-v8a', 'x86_64') for name in correct]},
                       {'names': [correct[0], correct[0]]}, {'compression': zipfile.ZIP_DEFLATED},
                       {'unaligned': True}, {'bad_dependency': True}):
            with self.subTest(change=change), self.assertRaises(verify.InvalidNative):
                self.verify_archive(change)

    def test_writable_executable_rejected(self):
        data = bytearray(fixture())
        struct.pack_into('<I', data, 68, 7)
        with self.assertRaisesRegex(verify.InvalidNative, 'writable executable'):
            verify.inspect_elf(data, 'arm64-v8a', '0.0.971')


if __name__ == '__main__':
    unittest.main()
