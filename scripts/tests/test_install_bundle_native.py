#!/usr/bin/env python3
"""Run actual package installers against the native prebuilt runtime siblings.

Invoked after ci-test.sh has built executable siblings. A missing binary is a
failure, never a reason to skip; no build is launched by this suite.
"""
from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from scripts.tests.test_install_bundle import (
    BINARIES, ROOT, contents, fake_download_environment, make_archive, old_install,
)

sys.path.insert(0, str(ROOT / "scripts" / "qa-gate"))
from gate.contract import VERSION_QUERY

# Preserve this suite's original whole-fixture watchdog. It covers local
# archive extraction, copied/staged member verification, and publication.
# This is not a sum of enforced inner deadlines: staging bounds subprocess
# output bytes only. Even the six staged/installed version probes would get
# 6 * VERSION_QUERY.seconds = 180 s using the shared per-probe allowance,
# before hashing, copying, signatures, and offline self-test. A 120 s phase
# derivation is therefore unproven; do not invent allocations or relax the cap.
INSTALL_WATCHDOG_SECONDS = 120


class NativeBundleInstallationTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        configured = os.environ.get("HAIDER_INSTALL_TEST_BIN_DIR")
        if not configured:
            raise RuntimeError("HAIDER_INSTALL_TEST_BIN_DIR must name the prebuilt native sibling directory")
        cls.binaries = Path(configured).resolve()
        cls.windows = os.name == "nt"
        cls.extension = ".exe" if cls.windows else ""
        for name in BINARIES:
            if not (cls.binaries / (name + cls.extension)).is_file():
                raise RuntimeError(f"missing prebuilt runtime sibling: {name}")
        if (cls.binaries / ("haiderd" + cls.extension)).stat().st_size <= 10 * 1024 * 1024:
            raise RuntimeError("prebuilt haiderd must exceed the 10 MiB sibling-binary floor")
        result = subprocess.run([str(cls.binaries / ("haider" + cls.extension)), "--version"], capture_output=True, text=True, timeout=VERSION_QUERY.seconds, check=True)
        if not result.stdout.startswith("haider "):
            raise RuntimeError(f"unexpected native CLI version: {result.stdout!r}")
        cls.version = result.stdout.strip().removeprefix("haider ")
        cls.temporary = tempfile.TemporaryDirectory(prefix="haider-native-install-")
        cls.fixtures = Path(cls.temporary.name)
        cls.archives = {}
        for variant in ("complete", "missing", "mixed"):
            directory = cls.fixtures / variant
            directory.mkdir()
            cls.archives[variant] = make_archive(directory, windows=cls.windows, missing="haider-tui" if variant == "missing" else "", binary_dir=cls.binaries, version=cls.version, wrong_payload=variant == "mixed")

    @classmethod
    def tearDownClass(cls):
        cls.temporary.cleanup()

    def test_direct_installer_fresh_prefix_contains_matching_runtime_siblings(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            prefix = root / "fresh prefix with spaces" / "bin"
            result = self.direct_install(root, prefix, "complete")
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assert_versions(prefix)

    def test_direct_installer_missing_payload_preserves_existing_binaries(self):
        self.assert_direct_refusal("missing")

    def test_direct_installer_rejects_wrong_payload_identity_before_publication(self):
        self.assert_direct_refusal("mixed")

    def test_npm_installs_with_real_shared_helper(self):
        with tempfile.TemporaryDirectory() as temporary:
            prefix = Path(temporary) / "vendor with spaces"
            result = self.npm_install(prefix, "complete")
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assert_versions(prefix)

    def test_npm_refusal_keeps_existing_recovery_marker_and_binaries(self):
        with tempfile.TemporaryDirectory() as temporary:
            prefix = Path(temporary) / "vendor"
            before = old_install(prefix, self.windows)
            marker = prefix / ".haider-update-transaction.json"
            marker.write_bytes(b"existing recovery evidence must not be deleted")
            before[marker.name] = marker.read_bytes()
            result = self.npm_install(prefix, "complete")
            self.assertNotEqual(result.returncode, 0)
            # The shared transaction may create its persistent OS lock file;
            # every preexisting byte remains authoritative after refusal.
            after = contents(prefix)
            for name, data in before.items():
                self.assertEqual(after[name], data)

    def assert_direct_refusal(self, variant):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            prefix = root / "prefix with spaces" / "bin"
            before = old_install(prefix, self.windows)
            result = self.direct_install(root, prefix, variant)
            self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertEqual(contents(prefix), before)

    def assert_versions(self, prefix):
        for name in BINARIES:
            result = subprocess.run([str(prefix / (name + self.extension)), "--version"], capture_output=True, text=True, timeout=VERSION_QUERY.seconds)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout.strip(), f"{name} {self.version}")
        if not self.windows and os.uname().sysname == "Linux":
            self.assertTrue((prefix / "haider-wayland-portal").is_file())

    def npm_install(self, prefix, variant):
        code = r'''
const fs = require('fs');
const path = require('path');
const installer = require(process.argv[1]);
installer.installArchive(fs.readFileSync(process.argv[2]), path.basename(process.argv[2]), process.argv[3]);
'''
        return subprocess.run(["node", "-e", code, str(ROOT / "packaging/npm/install.js"), str(self.archives[variant]), str(prefix)], capture_output=True, text=True, timeout=INSTALL_WATCHDOG_SECONDS)

    def direct_install(self, root, prefix, variant):
        artifact = self.archives[variant]
        if not self.windows:
            env = fake_download_environment(root, prefix, self.version)
            env["INSTALL_FIXTURE"] = str(artifact.parent)
            return subprocess.run(["sh", str(ROOT / "scripts/install.sh")], env=env, capture_output=True, text=True, timeout=INSTALL_WATCHDOG_SECONDS)
        wrapper = root / "fixture.ps1"
        wrapper.write_text(r'''
$ErrorActionPreference = 'Stop'
function Invoke-WebRequest {
  param($Uri, $OutFile, $Headers)
  Copy-Item (Join-Path $env:INSTALL_FIXTURE ([IO.Path]::GetFileName($Uri))) $OutFile
}
$SavedPath = [Environment]::GetEnvironmentVariable('Path', 'User')
try {
  [Environment]::SetEnvironmentVariable('Path', ($SavedPath + ';' + $env:HAIDER_INSTALL_DIR), 'User')
  & $env:INSTALL_SCRIPT
} finally {
  [Environment]::SetEnvironmentVariable('Path', $SavedPath, 'User')
}
''')
        env = dict(os.environ, INSTALL_FIXTURE=str(artifact.parent), INSTALL_SCRIPT=str(ROOT / "scripts/install.ps1"), HAIDER_INSTALL_DIR=str(prefix), HAIDER_VERSION=self.version, PROCESSOR_ARCHITECTURE="AMD64")
        return subprocess.run(["pwsh", "-NoProfile", "-File", str(wrapper)], env=env, capture_output=True, text=True, timeout=INSTALL_WATCHDOG_SECONDS)


if __name__ == "__main__":
    unittest.main()
