"""Portable mutation tests for the macOS final-package gate."""
import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from scripts.tests.posix_mode_fixture import model_modes

MODULE = Path(__file__).resolve().parents[2] / 'packaging/installers/macos-verify.py'
spec = importlib.util.spec_from_file_location('macos_verify', MODULE)
macos = importlib.util.module_from_spec(spec)
spec.loader.exec_module(macos)


class MacOSPackageGateTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name)
        self.source = self.base / 'source'
        self.expanded = self.base / 'expanded'
        self.source.mkdir()
        self.root = self.expanded / 'Payload'
        (self.root / 'usr/local/bin').mkdir(parents=True)
        (self.root / 'usr/local/share/haider').mkdir(parents=True)
        data = b'fixture binary\n'
        self.manifest = {'version': '0.0.970', 'target': 'aarch64-apple-darwin',
                         'members': {'haider': hashlib.sha256(data).hexdigest()}}
        (self.source / 'manifest.json').write_text(json.dumps(self.manifest))
        (self.source / 'uninstall-haider.sh').write_bytes(b'#!/bin/sh\n')
        (self.root / 'usr/local/bin/haider').write_bytes(data)
        (self.root / 'usr/local/bin/haider').chmod(0o755)
        (self.root / 'usr/local/bin/uninstall-haider.sh').write_bytes(b'#!/bin/sh\n')
        (self.root / 'usr/local/bin/uninstall-haider.sh').chmod(0o755)
        (self.root / 'usr/local/share/haider/installer-manifest.json').write_bytes(
            (self.source / 'manifest.json').read_bytes())
        self.modes = {self.root / 'usr/local/bin/haider': 0o755,
                      self.root / 'usr/local/bin/uninstall-haider.sh': 0o755}
        model_modes(self, self.modes)
        (self.expanded / 'PackageInfo').write_text(
            '<pkg-info identifier="ai.haidercode.haider" version="0.0.970" install-location="/"/>')

    def check(self):
        macos.verify(self.expanded, self.source, '0.0.970', 'aarch64-apple-darwin')

    def test_valid_payload(self):
        self.check()

    def test_changed_binary_rejected(self):
        (self.root / 'usr/local/bin/haider').write_bytes(b'changed')
        with self.assertRaisesRegex(ValueError, 'hash mismatch'):
            self.check()

    def test_unexpected_member_rejected(self):
        (self.root / 'usr/local/bin/unexpected').touch()
        with self.assertRaisesRegex(ValueError, 'members differ'):
            self.check()

    def test_stale_package_version_rejected(self):
        p = self.expanded / 'PackageInfo'
        p.write_text(p.read_text().replace('0.0.970', '0.0.969'))
        with self.assertRaisesRegex(ValueError, 'identifier/version'):
            self.check()

    def test_missing_executable_mode_rejected(self):
        for mode in (0o644, 0o744):
            self.modes[self.root / 'usr/local/bin/haider'] = mode
            with self.subTest(mode=mode), self.assertRaisesRegex(ValueError, 'executable bit'):
                self.check()

    def test_package_scripts_rejected(self):
        (self.expanded / 'Scripts').mkdir()
        with self.assertRaisesRegex(ValueError, 'unexpected package scripts'):
            self.check()


if __name__ == '__main__':
    unittest.main()
