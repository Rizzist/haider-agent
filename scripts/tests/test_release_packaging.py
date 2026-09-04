#!/usr/bin/env python3
"""Regression tests for the release package substitution and final gates."""

from __future__ import annotations

import importlib.util
import hashlib
import io
import json
import shutil
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
MODULE_PATH = ROOT / "scripts" / "release_packaging.py"
SPEC = importlib.util.spec_from_file_location("release_packaging", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
release_packaging = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(release_packaging)

VERSION = "9.8.7"
STALE_VERSION = "9.8.6"
SHA256 = "ab" * 32
SHAS = {
    target: f"{index:02x}" * 32
    for index, target in enumerate(
        (*release_packaging.HOMEBREW_TARGETS, release_packaging.WINDOWS_TARGET),
        start=1,
    )
}


def _template(root: Path, newline: bytes) -> Path:
    source = root / "template"
    tools = source / "tools"
    tools.mkdir(parents=True)
    nuspec = newline.join(
        (
            b'<?xml version="1.0" encoding="utf-8"?>',
            b'<package xmlns="http://schemas.microsoft.com/packaging/2015/06/nuspec.xsd">',
            b"  <metadata>",
            b"    <version>__HAIDER_VERSION__</version>",
            b"    <iconUrl>https://cdn.jsdelivr.net/gh/Rizzist/haider-agent@v__HAIDER_ICON_VERSION__/packaging/chocolatey/icon.png</iconUrl>",
            b"  </metadata>",
            b"</package>",
            b"",
        )
    )
    install = newline.join(
        (
            b"$version = '__HAIDER_VERSION__'",
            b"$url64 = '__HAIDER_WINDOWS_X64_URL__'",
            b"$checksum64 = '__HAIDER_WINDOWS_X64_SHA256__'",
            b"",
        )
    )
    verification = newline.join(
        (
            b"VERIFICATION",
            b"https://github.com/Rizzist/haider-agent/releases/tag/v__HAIDER_VERSION__",
            b"__HAIDER_WINDOWS_X64_SHA256__",
            b"",
        )
    )
    (source / "haider.nuspec").write_bytes(nuspec)
    (tools / "chocolateyinstall.ps1").write_bytes(install)
    (tools / "VERIFICATION.txt").write_bytes(verification)
    return source


def _nupkg(tree: Path, destination: Path) -> None:
    with zipfile.ZipFile(destination, "w", zipfile.ZIP_DEFLATED) as archive:
        for path in tree.rglob("*"):
            if path.is_file():
                archive.write(path, path.relative_to(tree).as_posix())


class ChocolateyReleaseTests(unittest.TestCase):
    def test_repository_template_has_every_required_placeholder_once(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "rendered"
            release_packaging.render_chocolatey_tree(
                ROOT / "packaging" / "chocolatey", output, VERSION, SHA256
            )

    def test_render_rewrites_every_field_for_lf_and_crlf(self) -> None:
        for name, newline in (("LF", b"\n"), ("CRLF", b"\r\n")):
            with self.subTest(newline=name), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                source = _template(root, newline)
                output = root / "rendered"
                artifact = root / release_packaging._windows_artifact(VERSION)
                artifact.write_bytes(b"real release artifact")
                artifact_sha = hashlib.sha256(artifact.read_bytes()).hexdigest()
                release_packaging.render_chocolatey_tree(
                    source, output, VERSION, artifact_sha
                )
                package = root / f"haider.{VERSION}.nupkg"
                _nupkg(output, package)
                release_packaging.verify_chocolatey_against_artifact(
                    package, VERSION, artifact
                )

                rendered = b"\n".join(
                    path.read_bytes()
                    for path in (
                        output / "haider.nuspec",
                        output / "tools" / "chocolateyinstall.ps1",
                        output / "tools" / "VERIFICATION.txt",
                    )
                )
                self.assertNotIn(b"__HAIDER_", rendered)
                self.assertIn(VERSION.encode(), rendered)
                self.assertIn(release_packaging._windows_url(VERSION).encode(), rendered)
                self.assertIn(artifact_sha.encode(), rendered)
                self.assertIn(release_packaging._icon_url(VERSION).encode(), rendered)
                self.assertIn(newline, (output / "haider.nuspec").read_bytes())

    def test_render_fails_loudly_when_a_required_token_is_missing(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = _template(root, b"\r\n")
            install = source / "tools" / "chocolateyinstall.ps1"
            install.write_bytes(
                install.read_bytes().replace(b"__HAIDER_WINDOWS_X64_URL__", b"stale")
            )
            with self.assertRaisesRegex(
                release_packaging.PackagingError,
                r"chocolateyinstall\.ps1.*install URL.*matched 0 times",
            ):
                release_packaging.render_chocolatey_tree(
                    source, root / "rendered", VERSION, SHA256
                )

    def test_post_pack_gate_rejects_a_deliberately_stale_pin(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output = root / "rendered"
            artifact = root / release_packaging._windows_artifact(VERSION)
            artifact.write_bytes(b"real release artifact")
            release_packaging.render_chocolatey_from_artifact(
                _template(root, b"\n"), output, VERSION, artifact
            )
            install = output / "tools" / "chocolateyinstall.ps1"
            install.write_text(
                install.read_text().replace(
                    release_packaging._windows_url(VERSION),
                    release_packaging._windows_url(STALE_VERSION),
                ),
                encoding="utf-8",
            )
            package = root / f"haider.{VERSION}.nupkg"
            _nupkg(output, package)
            with self.assertRaisesRegex(
                release_packaging.PackagingError, "install URL mismatch"
            ):
                release_packaging.verify_chocolatey_against_artifact(
                    package, VERSION, artifact
                )


class SiblingPackagerTests(unittest.TestCase):
    def test_homebrew_and_scoop_repin_and_gate_all_pins(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            packaging = Path(temporary) / "packaging"
            (packaging / "homebrew").mkdir(parents=True)
            (packaging / "scoop").mkdir(parents=True)
            shutil.copy2(
                ROOT / "packaging" / "homebrew" / "haider.rb",
                packaging / "homebrew" / "haider.rb",
            )
            shutil.copy2(
                ROOT / "packaging" / "scoop" / "haider.json",
                packaging / "scoop" / "haider.json",
            )
            release_packaging.repin_homebrew_scoop(packaging, VERSION, SHAS)
            release_packaging.verify_homebrew_scoop(packaging, VERSION, SHAS)

            scoop_path = packaging / "scoop" / "haider.json"
            scoop = json.loads(scoop_path.read_text())
            scoop["architecture"]["64bit"]["url"] = release_packaging._windows_url(
                STALE_VERSION
            )
            scoop_path.write_text(json.dumps(scoop), encoding="utf-8")
            with self.assertRaisesRegex(
                release_packaging.PackagingError, "Scoop URL mismatch"
            ):
                release_packaging.verify_homebrew_scoop(packaging, VERSION, SHAS)

    def test_npm_archive_gate_uses_the_packed_version_and_dynamic_urls(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            package = Path(temporary) / "haider-agent.tgz"
            metadata = json.loads(
                (ROOT / "packaging" / "npm" / "package.json").read_text()
            )
            metadata["version"] = VERSION
            installer = (ROOT / "packaging" / "npm" / "install.js").read_bytes()
            with tarfile.open(package, "w:gz") as archive:
                for name, data in (
                    ("package/package.json", json.dumps(metadata).encode()),
                    ("package/install.js", installer),
                ):
                    info = tarfile.TarInfo(name)
                    info.size = len(data)
                    archive.addfile(info, io.BytesIO(data))
            release_packaging.verify_npm_archive(package, VERSION)
            with self.assertRaisesRegex(
                release_packaging.PackagingError, "npm version mismatch"
            ):
                release_packaging.verify_npm_archive(package, STALE_VERSION)


if __name__ == "__main__":
    unittest.main()
