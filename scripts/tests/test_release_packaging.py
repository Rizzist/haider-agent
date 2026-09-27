#!/usr/bin/env python3
"""Regression tests for the release package substitution and final gates."""

from __future__ import annotations

import contextlib
import importlib.util
import hashlib
import io
import json
import os
import re
import shutil
import struct
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
            b"    <iconUrl>https://haidercode.ai/logo.svg</iconUrl>",
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


def _pe(imports=(), delay_imports=(), *, pe32=False, legacy_delay=False, zero_size=()) -> bytes:
    """A minimal x64/x86 PE image whose .idata holds the given import names."""
    image_base = 0x400000 if pe32 else 0x140000000
    section_rva, section_offset = 0x1000, 0x200
    import_size = 20 * (len(imports) + 1)
    delay_size = 32 * (len(delay_imports) + 1)
    names = b""
    name_rvas = []
    for name in (*imports, *delay_imports):
        name_rvas.append(section_rva + import_size + delay_size + len(names))
        names += name.encode("ascii") + b"\0"
    body = b"".join(
        struct.pack("<5I", 1, 0, 0, rva, 1) for rva in name_rvas[: len(imports)]
    ) + bytes(20)
    for rva in name_rvas[len(imports):]:
        body += struct.pack(
            "<8I", 0 if legacy_delay else 1,
            (image_base + rva) if legacy_delay else rva, 0, 0, 0, 0, 0, 0,
        )
    body += bytes(32) + names
    raw_size = (len(body) + 0x1FF) & ~0x1FF
    optional_size = 224 if pe32 else 240
    directories = [(0, 0)] * 16
    if imports:
        directories[1] = (section_rva, import_size)
    if delay_imports:
        directories[13] = (section_rva + import_size, delay_size)
    for index in zero_size:
        directories[index] = (directories[index][0] or section_rva, 0)
    optional = bytearray(optional_size)
    if pe32:
        struct.pack_into("<HI", optional, 0, 0x10B, 0)
        struct.pack_into("<I", optional, 28, image_base)
        struct.pack_into("<I", optional, 92, 16)
        base = 96
    else:
        struct.pack_into("<H", optional, 0, 0x20B)
        struct.pack_into("<Q", optional, 24, image_base)
        struct.pack_into("<I", optional, 108, 16)
        base = 112
    for index, (rva, size) in enumerate(directories):
        struct.pack_into("<II", optional, base + index * 8, rva, size)
    header = bytearray(section_offset)
    header[0:2] = b"MZ"
    struct.pack_into("<I", header, 0x3C, 0x40)
    header[0x40:0x44] = b"PE\0\0"
    struct.pack_into(
        "<HHIIIHH", header, 0x44, 0x14C if pe32 else 0x8664, 1, 0, 0, 0,
        optional_size, 0x22,
    )
    header[0x58 : 0x58 + optional_size] = optional
    struct.pack_into(
        "<8sIIII", header, 0x58 + optional_size, b".idata", len(body),
        section_rva, raw_size, section_offset,
    )
    return bytes(header) + body + bytes(raw_size - len(body))


SYSTEM_IMPORTS = ("KERNEL32.dll", "ntdll.dll", "bcryptprimitives.dll", "ws2_32.dll")


def _with_directory_count(data: bytes, count: int, *, shorten: bool) -> bytes:
    """Declare count directories, optionally moving the section table left."""
    image = bytearray(data)
    pe = struct.unpack_from("<I", image, 0x3C)[0]
    optional = pe + 24
    fixed_size = 96 if struct.unpack_from("<H", image, optional)[0] == 0x10B else 112
    struct.pack_into("<I", image, optional + fixed_size - 4, count)
    if shorten:
        old_size = struct.unpack_from("<H", image, pe + 20)[0]
        new_size = fixed_size + 8 * count
        section_table = bytes(image[optional + old_size:optional + old_size + 40])
        image[optional + new_size:0x200] = bytes(0x200 - optional - new_size)
        image[optional + new_size:optional + new_size + 40] = section_table
        struct.pack_into("<H", image, pe + 20, new_size)
    return bytes(image)


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


class SplitArchiveTests(unittest.TestCase):
    def archive(self, root, *, windows, missing="", bad_checksum=False, legacy=False):
        target = release_packaging.WINDOWS_TARGET if windows else "aarch64-apple-darwin"
        top = f"haider-v{VERSION}-{target}" + ("" if legacy else "-split")
        artifact = root / (top + (".zip" if windows else ".tar.xz"))
        names = [name for name in ("haider", "haider-tui", "haiderd") if name != missing]
        if windows:
            with zipfile.ZipFile(artifact, "w") as archive:
                archive.writestr(top + "/", b"")
                for name in names:
                    archive.writestr(f"{top}/{name}.exe", b"binary")
                archive.writestr(f"{top}/haider.cmd", b"launcher")
                archive.writestr(f"{top}/README.txt", b"readme")
        else:
            with tarfile.open(artifact, "w:xz") as archive:
                info = tarfile.TarInfo(top)
                info.type = tarfile.DIRTYPE
                archive.addfile(info)
                for name in names:
                    info = tarfile.TarInfo(f"{top}/{name}")
                    info.mode = 0o755
                    info.size = 6
                    archive.addfile(info, io.BytesIO(b"binary"))
        digest = "00" * 32 if bad_checksum else hashlib.sha256(artifact.read_bytes()).hexdigest()
        artifact.with_name(artifact.name + ".sha256").write_text(f"{digest}  {artifact.name}\n")
        return artifact, target

    def test_actual_tar_and_zip_payloads_require_all_siblings_and_checksum(self):
        for windows in (False, True):
            for missing, bad_checksum in (("", False), ("haider-tui", False), ("", True)):
                with self.subTest(windows=windows, missing=missing, bad_checksum=bad_checksum), tempfile.TemporaryDirectory() as temporary:
                    artifact, target = self.archive(Path(temporary), windows=windows, missing=missing, bad_checksum=bad_checksum)
                    if missing or bad_checksum:
                        with self.assertRaises(release_packaging.PackagingError):
                            release_packaging.verify_split_bundle(artifact, target)
                    else:
                        release_packaging.verify_split_bundle(artifact, target)

    def test_legacy_canonical_mac_archive_accepts_only_original_pair(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            artifact, target = self.archive(root, windows=False, missing="haider-tui", legacy=True)
            release_packaging.verify_legacy_bundle(artifact, target)
            with self.assertRaises(release_packaging.PackagingError):
                release_packaging.verify_legacy_bundle(artifact, release_packaging.WINDOWS_TARGET)
            artifact, target = self.archive(root, windows=False, legacy=True)
            with self.assertRaisesRegex(release_packaging.PackagingError, "unexpected or empty"):
                release_packaging.verify_legacy_bundle(artifact, target)

    def test_release_and_manager_templates_preserve_split_format_and_public_entrypoint(self):
        workflow = (ROOT / ".github/workflows/release.yml").read_text()
        for fragment in ("-p haider-tui-exe", 'cp "$BIN" "$TBIN" "$DBIN"', "haider-tui --version", "verify-bundle"):
            if fragment == "haider-tui --version":
                self.assertIn('"$TBIN" --version', workflow)
            else:
                self.assertIn(fragment, workflow)
        formula = (ROOT / "packaging/homebrew/haider.rb").read_text()
        self.assertIn('"#{source}/haider-tui"', formula)
        scoop = json.loads((ROOT / "packaging/scoop/haider.json").read_text())
        self.assertEqual(scoop["bin"], ["haider.exe"])
        self.assertIn("haider-tui.exe", "\n".join(scoop["pre_install"]))
        npm = json.loads((ROOT / "packaging/npm/package.json").read_text())
        self.assertEqual(npm["bin"], {"haider": "bin/haider.js"})
        choco = (ROOT / "packaging/chocolatey/tools/chocolateyinstall.ps1").read_text()
        self.assertIn("haider-tui.exe", choco)
        self.assertIn("$binary.ignore", choco)
        # Upgrade/uninstall must not fail on a still-running sibling image.
        before = (ROOT / "packaging/chocolatey/tools/chocolateybeforemodify.ps1").read_text()
        for name in ("'haider'", "'haider-tui'", "'haiderd'", "StartsWith($prefix", "Stop-Process"):
            self.assertIn(name, before)


class WindowsCrtImportTests(unittest.TestCase):
    """Registry #176: a shipped PE must not need the VC++ redistributable."""

    def write(self, root: Path, name: str, data: bytes) -> Path:
        path = root / name
        path.write_bytes(data)
        return path

    def test_parser_reads_import_and_delay_tables_for_pe32_and_pe32_plus(self):
        # Pre-VC7 (VA-based) delay descriptors only exist in 32-bit images.
        for pe32, legacy_delay in ((False, False), (True, False), (True, True)):
            with self.subTest(pe32=pe32, legacy_delay=legacy_delay):
                image = _pe(SYSTEM_IMPORTS, ("user32.dll",), pe32=pe32, legacy_delay=legacy_delay)
                self.assertEqual(
                    release_packaging.windows_pe_imports(image, "fixture"),
                    {"imports": list(SYSTEM_IMPORTS), "delay_imports": ["user32.dll"]},
                )

    def test_static_crt_image_passes(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = self.write(Path(temporary), "haider.exe", _pe(SYSTEM_IMPORTS))
            report = release_packaging.verify_windows_imports([path])
            self.assertEqual(len(report), 1)
            self.assertIn("KERNEL32.dll", report[0])

    def test_malformed_import_names_fail_closed(self):
        name = b"VCRUNTIME140.dll\0"
        for delayed in (False, True):
            base = _pe(SYSTEM_IMPORTS, ("VCRUNTIME140.dll",)) if delayed else _pe(("VCRUNTIME140.dll",))
            start = base.index(name)
            cases = {}
            for label, replacement in (
                ("high-bit", b"\xffCRUNTIME140.dll\0"),
                ("empty", b"\0"),
                ("control", b"\x01CRUNTIME140.dll\0"),
            ):
                image = bytearray(base)
                image[start:start + len(name)] = replacement.ljust(len(name), b"\0")
                cases[label] = bytes(image)
            cases["overlong"] = (
                _pe(SYSTEM_IMPORTS, ("A" * 512,)) if delayed else _pe(("A" * 512,))
            )
            unterminated = bytearray(base)
            unterminated[start:] = b"A" * (len(base) - start)
            cases["unterminated at end of data"] = bytes(unterminated)
            cases["terminator outside section"] = bytes(unterminated) + b"\0"
            for label, image in cases.items():
                with self.subTest(delayed=delayed, name=label):
                    with self.assertRaisesRegex(
                        release_packaging.PackagingError,
                        r"malformed-fixture: .* at RVA 0x[0-9a-f]+",
                    ):
                        release_packaging.windows_pe_imports(image, "malformed-fixture")

    def test_short_directory_tables_and_header_bounds_through_cli(self):
        for pe32 in (False, True):
            for count in (0, 1, 2, 8, 13):
                for shorten in (False, True):
                    with self.subTest(pe32=pe32, count=count, shorten=shorten), tempfile.TemporaryDirectory() as temporary:
                        # Counts 0 and 1 leave the OS import bytes undeclared;
                        # the loader and verifier must both ignore that slot.
                        image = _with_directory_count(_pe(SYSTEM_IMPORTS, pe32=pe32), count, shorten=shorten)
                        path = self.write(Path(temporary), "os.exe", image)
                        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                            self.assertEqual(release_packaging.main(["verify-windows-imports", str(path)]), 0)
            for count, delay in ((2, False), (13, False), (14, True)):
                for shorten in (False, True):
                    with self.subTest(pe32=pe32, count=count, delay=delay, shorten=shorten), tempfile.TemporaryDirectory() as temporary:
                        image = _pe(SYSTEM_IMPORTS if delay else ("VCRUNTIME140.dll",),
                                    ("VCRUNTIME140.dll",) if delay else (), pe32=pe32)
                        path = self.write(Path(temporary), "crt.exe", _with_directory_count(image, count, shorten=shorten))
                        output, errors = io.StringIO(), io.StringIO()
                        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
                            self.assertEqual(release_packaging.main(["verify-windows-imports", str(path)]), 1)
                        self.assertIn("VCRUNTIME140.dll", errors.getvalue())
            for count, allocated in ((16, 2), (17, 16)):
                with self.subTest(pe32=pe32, count=count, allocated=allocated), tempfile.TemporaryDirectory() as temporary:
                    image = bytearray(_with_directory_count(_pe(SYSTEM_IMPORTS, pe32=pe32), allocated, shorten=allocated == 2))
                    pe = struct.unpack_from("<I", image, 0x3C)[0]
                    fixed_size = 96 if pe32 else 112
                    struct.pack_into("<I", image, pe + 24 + fixed_size - 4, count)
                    path = self.write(Path(temporary), "invalid.exe", bytes(image))
                    with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                        self.assertEqual(release_packaging.main(["verify-windows-imports", str(path)]), 1)
                    with self.assertRaisesRegex(release_packaging.PackagingError, "cannot hold"):
                        release_packaging.windows_pe_imports(bytes(image), "invalid")
            with self.subTest(pe32=pe32, truncated_optional=True):
                image = bytearray(_pe(SYSTEM_IMPORTS, pe32=pe32))
                pe = struct.unpack_from("<I", image, 0x3C)[0]
                struct.pack_into("<H", image, pe + 20, 0xFFFF)
                with self.assertRaisesRegex(release_packaging.PackagingError, "truncated PE optional header"):
                    release_packaging.windows_pe_imports(bytes(image), "invalid")

    def test_crt_aliases_and_os_names_in_each_import_mode(self):
        modes = ((False, False, False), (True, False, False),
                 (False, True, False), (True, True, False), (True, True, True))
        B = '\\'
        cases = [
            # drive-relative (Wine contains_path: name[1] == ':' is a path; RtlDosPathNameToNtPathName resolves
            # it against the drive's current directory)
            ('C:VCRUNTIME140.dll', 1), ('c:vcruntime140', 1), ('C:MSVCP140.dll', 1),
            ('C:api-ms-win-crt-runtime-l1-1-0.dll', 1), ('C:VCRUNTIME140.dll ', 1),
            ('D:ucrtbase.dll', 1), ('C:..' + B + 'VCRUNTIME140.dll', 1),
            # absolute / UNC / device namespace paths
            (B*2 + 'server' + B + 'share' + B + 'VCRUNTIME140.dll', 1),
            (B*2 + '?' + B + 'C:' + B + 'x' + B + 'VCRUNTIME140.dll', 1),
            (B*2 + '.' + B + 'C:' + B + 'x' + B + 'VCRUNTIME140.dll', 1),
            (B + '??' + B + 'C:' + B + 'Windows' + B + 'System32' + B + 'vcruntime140.dll', 1),
            (B*2 + '?' + B + 'GLOBALROOT' + B + 'Device' + B + 'HarddiskVolume3' + B + 'Windows' + B + 'System32' + B + 'vcruntime140.dll', 1),
            (B*2 + '?' + B + 'UNC' + B + 'server' + B + 'share' + B + 'msvcp140.dll', 1),
            ('C:' + B + 'VCRUNTIME140.dll', 1), (B + 'VCRUNTIME140.dll', 1),
            # case
            ('VcRuNtImE140.DlL', 1), ('VCRUNTIME140.DLL', 1), ('UCRTBASE.DLL', 1),
            # streams
            ('VCRUNTIME140.dll:stream', 1), ('VCRUNTIME140:$DATA', 1),
            # extension spellings that stay the CRT via API-set resolution
            ('api-ms-win-crt-runtime-l1-1-0.ocx', 1), ('api-ms-win-crt-stdio-l1-1-0.drv', 1),
            ('API-MS-WIN-CRT-MATH-L1-1-0', 1),
            # known alias vocabulary
            *[(n, 1) for n in (
                'vcruntime140.dll', 'vcruntime140_1.dll', 'vcruntime140_threads.dll', 'vcruntime140d.dll', 'vcruntime140_1d.dll',
                'vcruntime140_threadsd.dll', 'vcruntime140_app.dll', 'vcruntime140_1_app.dll', 'vcruntime140d_app.dll',
                'msvcp140.dll', 'msvcp140_1.dll', 'msvcp140_2.dll', 'msvcp140_atomic_wait.dll', 'msvcp140_codecvt_ids.dll',
                'msvcp140d.dll', 'msvcp140d_atomic_wait.dll', 'msvcp140_app.dll', 'msvcp120_app.dll', 'msvcp120.dll', 'msvcp100.dll',
                'msvcr120.dll', 'msvcr110.dll', 'msvcr100.dll', 'msvcr90.dll', 'msvcr80.dll', 'msvcr71.dll', 'msvcr120d.dll',
                'ucrtbase.dll', 'ucrtbased.dll', 'concrt140.dll', 'concrt140d.dll', 'vcomp140.dll', 'vcomp140d.dll', 'vcomp120.dll',
                'vcomp90.dll', 'vccorlib140.dll', 'vccorlib140d.dll', 'vccorlib120.dll', 'mfc140.dll', 'mfc140u.dll', 'mfc140d.dll',
                'mfcm140u.dll', 'mfc140chs.dll', 'mfc140enu.dll', 'mfc120u.dll', 'atl110.dll', 'atl100.dll', 'atl90.dll', 'vcamp140.dll',
                'vcamp140d.dll', 'libomp140.x86_64.dll', 'libomp140d.x86_64.dll', 'libomp140.aarch64.dll',
                'api-ms-win-crt-runtime-l1-1-0.dll', 'api-ms-win-crt-private-l1-1-0.dll', 'api-ms-win-crt-conio-l1-1-0.dll')],
            # candidate vocabulary gaps (source-derived: Microsoft.VCLibs appx framework DLLs, C++/CLI msvcm)
            *[(n, 1) for n in (
                'concrt140_app.dll', 'vccorlib140_app.dll', 'vcomp140_app.dll', 'vcamp140_app.dll', 'concrt140d_app.dll',
                'msvcr120_app.dll', 'msvcr110_app.dll', 'vccorlib120_app.dll', 'msvcm90.dll', 'msvcm80.dll', 'msvcm90d.dll',
                'msvci70.dll')],
            # No unknown filename or 8.3 guess receives an OS exemption.
            ('VCRUNT~1.DLL', 1), ('VCRUNT~2.DLL', 1), ('MSVCP1~1.DLL', 1), ('API-MS~1.DLL', 1),
            ('UCRTBA~1.DLL', 1), ('VCRUNT~1', 1),
            ('VCRUNTIME140.ocx', 1), ('VCRUNTIME140.drv', 1), ('VCRUNTIME140.dll.dll', 1),
            ('VCRUNTIME140.exe', 1), (' VCRUNTIME140.dll', 1), ('VCRUNTIME140 .dll', 1),
            ('xVCRUNTIME140.dll', 1), ('foo.dll:VCRUNTIME140.dll', 1), ('NUL', 1),
            ('CON.dll', 1), (B*2 + '.' + B + 'NUL', 1), ('libomp140.x86_64', 1),
            ('api-ms-win-crt.dll', 1),
            # legitimate OS names
            *[(n, 0) for n in (
                'KERNEL32.dll', 'api-ms-win-core-crt-l1-1-0.dll', 'api-ms-win-core-crt-l2-1-0.dll', 'API-MS-WIN-CORE-CRT-L1-1-0',
                'ext-ms-win-ntuser-window-l1-1-0.dll', 'ext-ms-win-kernel32-package-current-l1-1-0.dll',
                'msvcrt.dll', 'msvcp_win.dll',
                'C:' + B + 'Windows' + B + 'System32' + B + 'bcrypt.dll',
                B*2 + '?' + B + 'C:' + B + 'Windows' + B + 'System32' + B + 'ntdll.dll', 'C:KERNEL32.dll',
            )],
            *[(n, 1) for n in ('vcruntime.dll', 'mfc.dll', 'msvcr.dll', 'concrt.dll',
                                'My_Odd Name~1+(x).dll', 'mfc42u.dll', 'msvcp110_win.dll',
                                'msvcirt.dll', 'atl.dll', 'atlthunk.dll', 'mfcsubs.dll',
                                'mfcans32.dll')],
            *[(n, 1) for n in ('msvcrt20.dll', 'msvcrt40.dll', 'msvcp60.dll',
                              'mfc40.dll', 'mfc40u.dll', 'mfc42.dll', 'mfc42loc.dll',
                              'msvcr120_clr0400.dll', 'vcruntime140_clr0400.dll',
                              'ucrtbase_clr0400.dll', 'ucrtbase_enclave.dll')],
        ]
        malformed = [('tab', 'VCRUNTIME140.dll', b'\t'), ('trailing CR', 'VCRUNTIME140.dll', b'\r'),
                     ('U+0130 dotted I', 'VCRUNTXXME140.dll', '\u0130'.encode()), ('U+212A Kelvin in KERNEL32', 'XXXERNEL32.dll', '\u212a'.encode()),
                     ('NBSP trailing', 'VCRUNTIME140.dllXX', '\u00a0'.encode()), ('DEL', 'VCRUNTIME140.dll', b'\x7f')]

        cases.extend([
            # Additional drive/stream separators and suffix forms for this repair.
            ('C:VCRUNTIME140', 1),
            ('c:vcruntime140.dll', 1),
            (r'C:x/..\VCRUNTIME140.dll', 1),
            ('C:x:y:MSVCP140.dll', 1),
            ('x:y:vcruntime140', 1),
            (r'\\?\C:VCRUNTIME140.dll', 1),
            ('C. :VCRUNTIME140.dll', 1),
            ('C:VCRUNTIME140.dll:stream:$DATA', 1),
            ('mfc42u.dll:vcruntime140.dll', 1),
            ('vcruntime140.dll' + B + '..' + B + 'msvcp60.dll', 1),
            ('vcomp140d_app.dll', 1),
            ('vccorlib140d_app.dll', 1),
            ('vcamp140d_app.dll', 1),
            ('msvcr120d_app.dll', 1),
            ('msvcm80d.dll', 1),
            ('msvci70d.dll', 1),
            ('VCRUNTIME140..', 1),
            (r'C:\runtime\VCRUNTIME140', 1),
            ('api-ms-win-crt-runtime-l1-1-0.xyz', 1),
            ('api-ms-win-crt-runtime-l1-1-9.dll', 1),
            ('API-MS-WIN-CRT-HEAP-L1-1-0.foo', 1),
            (r'C:\Windows\System32\mfc42.dll', 1),
            ('C:mfc42.dll', 1),
            (r'C:\Windows\System32\msvcp60.dll', 1),
            ('C:msvcp60.dll', 1),
            (r'C:\Windows\System32\vcruntime140_clr0400.dll', 1),
            ('C:vcruntime140_clr0400.dll', 1),
            (r'C:\Windows\System32\mfc40.dll', 1),
            ('C:mfc40.dll', 1),
            (r'C:\Windows\System32\mfc42u.dll', 1),
            ('C:mfc42u.dll', 1),
            (r'C:\Windows\System32\msvcr120_clr0400.dll', 1),
            ('C:msvcr120_clr0400.dll', 1),
        ])
        self.assertEqual(len(cases), len(set(cases)))
        for pe32, delayed, legacy in modes:
            for name, expected in cases:
                with self.subTest(pe32=pe32, delayed=delayed, legacy=legacy, name=name, expected=expected), tempfile.TemporaryDirectory() as temporary:
                    image = _pe(SYSTEM_IMPORTS if delayed else (name,),
                                (name,) if delayed else (), pe32=pe32, legacy_delay=legacy)
                    path = self.write(Path(temporary), 'alias.exe', image)
                    output, errors = io.StringIO(), io.StringIO()
                    with contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
                        self.assertEqual(release_packaging.main(['verify-windows-imports', str(path)]), expected)
                    if expected:
                        self.assertIn(repr(name), errors.getvalue())
        for label, placeholder, raw in malformed:
            for pe32, delayed, legacy in modes:
                with self.subTest(pe32=pe32, delayed=delayed, legacy=legacy, malformed=label), tempfile.TemporaryDirectory() as temporary:
                    base_name = placeholder if 'XX' in placeholder else placeholder + 'Q'
                    image = _pe(SYSTEM_IMPORTS if delayed else (base_name,),
                                (base_name,) if delayed else (), pe32=pe32, legacy_delay=legacy)
                    encoded = base_name.encode('ascii')
                    replacement = encoded.replace(b'X' * len(raw), raw, 1) if 'XX' in placeholder else encoded[:-1] + raw
                    self.assertEqual(len(replacement), len(encoded))
                    self.assertEqual(image.count(encoded + b'\0'), 1)
                    image = image.replace(encoded + b'\0', replacement + b'\0')
                    path = self.write(Path(temporary), 'malformed.exe', image)
                    with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                        self.assertEqual(release_packaging.main(['verify-windows-imports', str(path)]), 1)

    def test_inventory_vocabulary_in_each_import_mode(self):
        # Independent literal representatives: removing a production family row
        # cannot silently remove its coverage from this list.
        forbidden = (
            'msvcrt10.dll', 'msvcrt21.dll', 'msvcrtd.dll', 'msvcirtd.dll',
            'msvcr40d.dll', 'msvcr70.dll', 'msvcr71d.dll', 'msvcr80.dll',
            'msvcr90d.dll', 'msvcr100.dll', 'msvcr110_app.dll', 'msvcr120d_app.dll',
            'msvcp50.dll', 'msvcp60d.dll', 'msvcp70.dll', 'msvcp71d.dll',
            'msvcp80.dll', 'msvcp90d.dll', 'msvcp100.dll', 'msvcp110d.dll',
            'msvcp120_app.dll', 'msvcp140_1.dll', 'msvcp140_2.dll',
            'msvcp140_atomic_wait.dll', 'msvcp140d_codecvt_ids.dll',
            'msvci70.dll', 'msvci70d.dll', 'msvci71.dll', 'msvci71d.dll',
            'msvcm80.dll', 'msvcm90d.dll',
            'vcruntime140.dll', 'vcruntime140_1d.dll',
            'vcruntime140_threads.dll', 'vcruntime140d_app.dll',
            'appcrt140.dll', 'appcrt140d.dll', 'desktopcrt140.dll',
            'desktopcrt140d.dll',
            'concrt100.dll', 'concrt110d.dll', 'concrt120.dll', 'concrt140d_app.dll',
            'vccorlib110.dll', 'vccorlib120_app.dll', 'vccorlib140d.dll',
            'vcamp110.dll', 'vcamp120d.dll', 'vcamp140_app.dll',
            'vcomp.dll', 'vcompd.dll', 'vcomp90.dll', 'vcomp100d.dll',
            'vcomp110.dll', 'vcomp120d.dll', 'vcomp140_app.dll', 'vcomp100ui.dll',
            'libomp140.x86_64.dll', 'libomp140d.aarch64.dll',
            'mfc30.dll', 'mfc40ud.dll', 'mfc42d.dll', 'mfc70.dll', 'mfc71u.dll',
            'mfc80d.dll', 'mfc90ud.dll', 'mfc100chs.dll', 'mfc110jpn.dll',
            'mfc120enu.dll', 'mfc140rus.dll', 'mfcm80.dll', 'mfcm90ud.dll',
            'mfcm100.dll', 'mfcm110u.dll', 'mfcm120d.dll', 'mfcm140ud.dll',
            'mfcd40d.dll', 'mfcd40ud.dll', 'mfcn40d.dll', 'mfcn40ud.dll',
            'mfco40d.dll', 'mfco40ud.dll', 'mfcd42d.dll', 'mfcd42ud.dll',
            'mfcn42d.dll', 'mfcn42ud.dll', 'mfco42d.dll', 'mfco42ud.dll',
            'mfcmifc80.dll', 'mfcmifc90.dll', 'mfcmifc90d.dll',
            'mfcmifc150_app.dll', 'atl70.dll', 'atl71d.dll', 'atl80.dll',
            'atl90d.dll', 'atl100.dll', 'atl110d.dll', 'atl140.dll',
            'clang_rt.asan_dynamic-x86_64.dll',
            'clang_rt.asan_dbg_dynamic-x86_64.dll',
            'ucrtbase.dll', 'ucrtbased.dll', 'ucrtbase_clr0400.dll',
            'ucrtbase_app.dll', 'ucrtbase_future.dll', 'ucrtbase_150.dll',
            'ucrtbase_enclave.dll', 'api-ms-win-crt-heap-l1-1-0.dll',
            'pgort100.dll', 'pgort140.dll', 'pgort140d.dll',
            'pgort150.dll', 'pgort150_app.dll', 'PGORT140',
            'msvcr100d_clr0400.dll', 'msvcr110d_clr0400.dll',
            'msvcr120d_clr0400.dll', 'msvcp100_clr0400.dll',
            'msvcp140_1_clr0400.dll',
        )
        self.assertEqual(len(forbidden), len(set(forbidden)))
        modes = ((False, False, False), (True, False, False),
                 (False, True, False), (True, True, False), (True, True, True))
        with tempfile.TemporaryDirectory() as temporary:
            for pe32, delayed, legacy in modes:
                for name in forbidden:
                    with self.subTest(name=name, pe32=pe32, delayed=delayed, legacy=legacy):
                        image = _pe(SYSTEM_IMPORTS if delayed else (name,),
                                    (name,) if delayed else (), pe32=pe32, legacy_delay=legacy)
                        path = self.write(Path(temporary), 'inventory.exe', image)
                        output, errors = io.StringIO(), io.StringIO()
                        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
                            self.assertEqual(release_packaging.main(['verify-windows-imports', str(path)]), 1)
                        self.assertIn(repr(name), errors.getvalue())

    def test_future_runtime_versions_in_each_import_mode(self):
        # Literal representatives independent of the production family table.
        # Cover each versioned family and suffix axis, including OS-name siblings.
        forbidden = (
            'vcruntime141.dll', 'vcruntime150.dll', 'vcruntime150_1.dll',
            'vcruntime150d_app.dll', 'vcruntime150_threads.dll',
            'msvcr140.dll', 'msvcr200d.dll', 'msvcr200_app.dll',
            'msvcp150.dll', 'msvcp150_atomic_wait.dll',
            'msvcp150d_codecvt_ids.dll', 'msvcp150_2.dll',
            'concrt150.dll', 'concrt150d_app.dll',
            'vccorlib150.dll', 'vccorlib150_app.dll',
            'vcamp150.dll', 'vcamp150d.dll',
            'vcomp150.dll', 'vcomp150d.dll', 'vcomp150ui.dll',
            'vcomp150ui_app.dll',
            'msvcm100.dll', 'msvcm100d_app.dll',
            'msvci80.dll', 'msvci80d.dll',
            'mfc150.dll', 'mfc150u.dll', 'mfc150ud.dll',
            'mfc150enu.dll', 'mfc150d_app.dll', 'mfcm150u.dll',
            'mfcm150ud_app.dll', 'mfcd50d.dll', 'mfcd50ud_app.dll',
            'mfcn50ud.dll', 'mfco50d.dll',
            'atl150.dll', 'atl150d.dll', 'atl150d_app.dll',
            'msvcrt30.dll', 'msvcrt30d_app.dll',
            'msvcrt20d.dll', 'msvcrt40d.dll',
            'appcrt150.dll', 'desktopcrt150d.dll',
            'clang_rt.asan_dynamic-arm64ec.dll',
            'ucrtbased_enclave.dll',
            'VCRUNTIME150', 'VCRUNTIME150.DLL',
            'C:MSVCP150_ATOMIC_WAIT.DLL',
            r'C:\Windows\System32\mfc150enu.dll .',
            'msvcr200d.dll::$DATA',
        )
        self.assertEqual(len(forbidden), len(set(forbidden)))
        modes = ((False, False, False), (True, False, False),
                 (False, True, False), (True, True, False), (True, True, True))
        with tempfile.TemporaryDirectory() as temporary:
            for pe32, delayed, legacy in modes:
                for name in forbidden:
                    with self.subTest(name=name, pe32=pe32, delayed=delayed, legacy=legacy):
                        image = _pe(SYSTEM_IMPORTS if delayed else (name,),
                                    (name,) if delayed else (), pe32=pe32, legacy_delay=legacy)
                        path = self.write(Path(temporary), 'future.exe', image)
                        output, errors = io.StringIO(), io.StringIO()
                        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
                            self.assertEqual(release_packaging.main(['verify-windows-imports', str(path)]), 1)
                        self.assertIn(repr(name), errors.getvalue())

    def test_removed_os_exemptions_and_siblings_in_each_import_mode(self):
        removed = (
            'mfc42u.dll', 'msvcp110_win.dll',
            'msvcrt20.dll', 'msvcrt40.dll', 'msvcp60.dll',
            'mfc40.dll', 'mfc40u.dll', 'mfc42.dll', 'mfc42loc.dll',
            'msvcp110_clr0400.dll',
            'msvcr100_clr0400.dll', 'msvcr110_clr0400.dll',
            'msvcr120_clr0400.dll', 'msvcp120_clr0400.dll',
            'msvcp140_clr0400.dll', 'vcruntime140_clr0400.dll',
            'vcruntime140_1_clr0400.dll',
        )
        siblings = (
            'msvcp60d.dll', 'mfc40ud.dll', 'mfc42d.dll', 'msvcp110.dll',
            'msvcp110d.dll', 'msvcp120.dll', 'msvcp140.dll',
            'msvcr120d_clr0400.dll', 'vcruntime140d_clr0400.dll',
        )
        names = [(name, 1) for base in removed for name in (
            base, base[:-4], base.upper(), 'C:' + base.upper(),
            'C:\\Windows\\System32\\' + base + ' .', base + '::$DATA')]
        names += [(name, 1) for name in siblings]
        modes = ((False, False, False), (True, False, False),
                 (False, True, False), (True, True, False), (True, True, True))
        with tempfile.TemporaryDirectory() as temporary:
            for pe32, delayed, legacy in modes:
                for name, expected in names:
                    with self.subTest(name=name, pe32=pe32, delayed=delayed, legacy=legacy):
                        image = _pe(SYSTEM_IMPORTS if delayed else (name,),
                                    (name,) if delayed else (), pe32=pe32, legacy_delay=legacy)
                        path = self.write(Path(temporary), 'exempt.exe', image)
                        output, errors = io.StringIO(), io.StringIO()
                        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
                            self.assertEqual(release_packaging.main(['verify-windows-imports', str(path)]), expected)
                        if expected:
                            self.assertIn(repr(name), errors.getvalue())
        self.assertTrue(set(removed).isdisjoint(release_packaging.WINDOWS_OS_IMPORTS))

    def test_every_redistributable_crt_import_fails(self):
        for forbidden in (
            "VCRUNTIME140.dll", "vcruntime140_1.dll", "VCRUNTIME140D.dll",
            "MSVCP140.dll", "msvcp140_atomic_wait.dll", "MSVCR120.dll",
            "api-ms-win-crt-runtime-l1-1-0.dll", "api-ms-win-crt-heap-l1-1-0.dll",
            "ucrtbase.dll", "ucrtbased.dll", "CONCRT140.dll", "VCOMP140.dll",
            "vcruntime140_threads.dll", "vcruntime140_threadsd.dll", "vcruntime140_1d.dll",
            "MSVCP140D.dll", "msvcp140d_atomic_wait.dll", "msvcp140_codecvt_ids.dll",
            "mfc140u.dll", "mfcm140.dll", "mfcm140ud.dll", "atl140.dll", "ATL110.dll",
            "vcamp140.dll", "vcamp140d.dll", "libomp140.x86_64.dll", "libomp.dll",
            "vccorlib140d.dll", "concrt140d.dll",
        ):
            for delayed in (False, True):
                with self.subTest(forbidden=forbidden, delayed=delayed), tempfile.TemporaryDirectory() as temporary:
                    image = (
                        _pe(SYSTEM_IMPORTS, (forbidden,)) if delayed
                        else _pe((*SYSTEM_IMPORTS, forbidden))
                    )
                    path = self.write(Path(temporary), "haiderd.exe", image)
                    with self.assertRaisesRegex(release_packaging.PackagingError, re.escape(forbidden)):
                        release_packaging.verify_windows_imports([path])

    def test_any_non_os_import_fails_in_all_five_modes(self):
        # The allowlist, not the diagnostic family vocabulary, makes the
        # decision. Include the OpenMP and ASan gaps from the independent r7
        # review, future variants, and arbitrary third-party names.
        names = (
            'libiomp5md.dll', 'libiompstubs5md.dll', 'libiompprof5md.dll',
            'libguide40.dll', 'libguide40_stats.dll', 'libgomp-1.dll',
            'cyggomp-1.dll', 'pgmp.dll', 'libiomp6md.dll',
            'libiompstubs6md.dll', 'libiompprof6md.dll', 'libguide50.dll',
            'libguide50_stats.dll', 'libgomp-2.dll', 'cyggomp-2.dll',
            'libclang_rt.asan_dynamic-x86_64.dll',
            'libclang_rt.asan_dynamic-i386.dll',
            'clang_rt.asan_dynamic.dll', 'libclang_rt.asan_dynamic.dll',
            'clang_rt.asan_dbg_dynamic.dll',
            'libclang_rt.asan_dbg_dynamic.dll',
            'libclang_rt.asan_dbg_dynamic-x86_64.dll',
            'libclang_rt.asan_dynamic-aarch64.dll',
            'sqlite3.dll', 'libssl-3-x64.dll', 'thirdparty.dll',
            'mfc42u.dll', 'msvcp110_win.dll',
        )
        modes = ((False, False, False), (True, False, False),
                 (False, True, False), (True, True, False), (True, True, True))
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / 'runtime.exe'
            for pe32, delayed, legacy in modes:
                for base in names:
                    for name in (
                        base, base.upper(), 'C:' + base,
                        'C:\\Windows\\System32\\' + base,
                        '\\\\?\\C:\\runtime\\' + base,
                        '//server/share/' + base,
                        '.\\x\\..\\' + base,
                        base + ' .', base + '::$DATA',
                        *(() if base.count('.') > 1 else (base[:-4],)),
                    ):
                        with self.subTest(name=name, pe32=pe32, delayed=delayed, legacy=legacy):
                            path.write_bytes(_pe(SYSTEM_IMPORTS if delayed else (name,),
                                                 (name,) if delayed else (), pe32=pe32,
                                                 legacy_delay=legacy))
                            output, errors = io.StringIO(), io.StringIO()
                            with contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
                                self.assertEqual(release_packaging.main(['verify-windows-imports', str(path)]), 1)
                            self.assertIn(repr(name), errors.getvalue())

    def test_allowlist_and_diagnostic_are_separate(self):
        self.assertEqual(
            release_packaging.rejected_windows_imports({'imports': ['vcruntime140.dll', 'thirdparty.dll']}),
            [('thirdparty.dll', 'rejected: not an allowlisted OS DLL'),
             ('vcruntime140.dll', 'rejected: VC runtime family VC runtime and satellites')],
        )
        self.assertEqual(
            release_packaging.forbidden_windows_imports({'imports': [
                'KERNEL32.dll', r'C:\Windows\System32\bcrypt.dll',
                'api-ms-win-core-synch-l1-2-0.dll',
                'ext-ms-win-ntuser-window-l1-1-0.dll',
                'kernel32.dll::$DATA',
            ]}),
            [],
        )
        self.assertEqual(
            release_packaging.forbidden_windows_imports({'imports': [
                'api-ms-win-crt-runtime-l1-1-0.dll', 'api-ms-win-core-fake.dllx',
                'ext-ms-win-other.ocx', 'api-ms-win-crt-math-l1-1-0.xyz',
            ]}),
            ['api-ms-win-core-fake.dllx', 'api-ms-win-crt-math-l1-1-0.xyz',
             'api-ms-win-crt-runtime-l1-1-0.dll', 'ext-ms-win-other.ocx'],
        )

    def test_reviewed_os_imports_pass(self):
        # Independent release-policy mirror: a removed allowlist row breaks
        # this test, and an added row must be reviewed here and in the docs.
        names = (
            'advapi32.dll', 'bcrypt.dll', 'bcryptprimitives.dll', 'combase.dll',
            'crypt32.dll', 'gdi32.dll', 'kernel32.dll', 'ntdll.dll', 'ole32.dll',
            'oleaut32.dll', 'shell32.dll', 'user32.dll', 'userenv.dll', 'ws2_32.dll',
            'kernelbase.dll', 'secur32.dll', 'ncrypt.dll', 'iphlpapi.dll',
            'dwmapi.dll', 'uxtheme.dll', 'shlwapi.dll', 'winhttp.dll', 'dnsapi.dll',
            'powrprof.dll', 'psapi.dll', 'version.dll', 'setupapi.dll',
            'cfgmgr32.dll', 'rpcrt4.dll', 'shcore.dll', 'profapi.dll',
            'netapi32.dll', 'wtsapi32.dll', 'win32u.dll', 'msvcrt.dll',
            'msvcp_win.dll', 'mf.dll', 'mfplat.dll',
            'api-ms-win-core-synch-l1-2-0.dll',
            'ext-ms-win-ntuser-window-l1-1-0.dll',
        )
        self.assertEqual(set(names[:-2]), release_packaging.WINDOWS_OS_IMPORTS)
        modes = ((False, False, False), (True, False, False),
                 (False, True, False), (True, True, False), (True, True, True))
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / 'system.exe'
            for name in names:
                for pe32, delayed, legacy in modes:
                    with self.subTest(name=name, pe32=pe32, delayed=delayed, legacy=legacy):
                        path.write_bytes(_pe(SYSTEM_IMPORTS if delayed else (name,),
                                             (name,) if delayed else (), pe32=pe32,
                                             legacy_delay=legacy))
                        self.assertIn(name, release_packaging.verify_windows_imports([path])[0])

    def test_zip_and_directory_inputs_check_every_pe_member(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            bundle = root / "haider-v9.8.7-x86_64-pc-windows-msvc-split.zip"
            with zipfile.ZipFile(bundle, "w") as archive:
                archive.writestr("top/", b"")
                archive.writestr("top/haider.exe", _pe(SYSTEM_IMPORTS))
                archive.writestr("top/haider-tui.exe", _pe(SYSTEM_IMPORTS))
                archive.writestr("top/haiderd.EXE", _pe((*SYSTEM_IMPORTS, "VCRUNTIME140.dll")))
                archive.writestr("top/README.txt", b"readme")
            with self.assertRaisesRegex(release_packaging.PackagingError, r"haiderd\.EXE: imports 'VCRUNTIME140\.dll'"):
                release_packaging.verify_windows_imports([bundle])
            directory = root / "bin"
            (directory / "nested").mkdir(parents=True)
            self.write(directory, "haider.exe", _pe(SYSTEM_IMPORTS))
            self.write(directory / "nested", "helper.dll", _pe(("MSVCP140.dll",)))
            with self.assertRaisesRegex(release_packaging.PackagingError, r"helper\.dll: imports 'MSVCP140\.dll'"):
                release_packaging.verify_windows_imports([directory])

    def test_inputs_without_a_real_pe_fail_closed(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            empty = root / "empty.zip"
            with zipfile.ZipFile(empty, "w") as archive:
                archive.writestr("top/README.txt", b"readme")
            with self.assertRaisesRegex(release_packaging.PackagingError, "no .exe or .dll"):
                release_packaging.verify_windows_imports([empty])
            with self.assertRaisesRegex(release_packaging.PackagingError, "not a PE image"):
                release_packaging.verify_windows_imports([self.write(root, "fake.exe", b"binary")])
            with self.assertRaisesRegex(release_packaging.PackagingError, "does not exist"):
                release_packaging.verify_windows_imports([root / "missing.exe"])
            truncated = _pe(SYSTEM_IMPORTS)[:0x210]
            with self.assertRaisesRegex(release_packaging.PackagingError, "truncated|outside"):
                release_packaging.verify_windows_imports([self.write(root, "cut.exe", truncated)])

    def test_directory_with_address_but_zero_size_fails_closed(self):
        # The loader walks a table from its address regardless of the size field.
        for index, image in (
            (1, _pe(("VCRUNTIME140.dll",), zero_size=(1,))),
            (1, _pe(SYSTEM_IMPORTS, zero_size=(1,))),
            (13, _pe(SYSTEM_IMPORTS, ("VCRUNTIME140.dll",), zero_size=(13,))),
        ):
            with self.subTest(index=index), tempfile.TemporaryDirectory() as temporary:
                path = self.write(Path(temporary), "haider.exe", image)
                with self.assertRaisesRegex(release_packaging.PackagingError, f"data directory {index} has address"):
                    release_packaging.verify_windows_imports([path])

    def test_cli_exit_codes(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            good = self.write(root, "good.exe", _pe(SYSTEM_IMPORTS))
            bad = self.write(root, "bad.exe", _pe(("VCRUNTIME140.dll",)))
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(release_packaging.main(["verify-windows-imports", str(good)]), 0)
                self.assertEqual(release_packaging.main(["verify-windows-imports", str(good), str(bad)]), 1)

    def test_cli_rejects_malformed_import_name(self):
        image = bytearray(_pe(("VCRUNTIME140.dll",)))
        image[image.index(b"VCRUNTIME140.dll\0")] = 0xFF
        with tempfile.TemporaryDirectory() as temporary:
            path = self.write(Path(temporary), "malformed.exe", bytes(image))
            output, errors = io.StringIO(), io.StringIO()
            with contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
                exit_code = release_packaging.main(["verify-windows-imports", str(path)])
            self.assertEqual(exit_code, 1)
            self.assertEqual(output.getvalue(), "")
            self.assertIn(str(path), errors.getvalue())
            self.assertIn("RVA", errors.getvalue())

    @unittest.skipUnless(os.environ.get("HAIDER_DYNAMIC_CRT_WINDOWS_ZIP"), "set HAIDER_DYNAMIC_CRT_WINDOWS_ZIP to a known-bad (e.g. 0.0.972) Windows zip")
    def test_published_dynamic_crt_bundle_fails(self):
        with self.assertRaisesRegex(release_packaging.PackagingError, "VCRUNTIME140.dll"):
            release_packaging.verify_windows_imports([Path(os.environ["HAIDER_DYNAMIC_CRT_WINDOWS_ZIP"])])

    @unittest.skipUnless(os.environ.get("HAIDER_DYNAMIC_CRT_WINDOWS_ZIP"), "set HAIDER_DYNAMIC_CRT_WINDOWS_ZIP")
    def test_published_crt_names_without_dll_suffix_fail(self):
        with zipfile.ZipFile(os.environ["HAIDER_DYNAMIC_CRT_WINDOWS_ZIP"]) as archive:
            member = next(name for name in archive.namelist() if name.endswith("/haider.exe"))
            original = archive.read(member)
        image = release_packaging._PeImage(original, "972 haider.exe")
        rva, _ = image.directory(1)
        cursor = image.offset(rva)
        mutated = bytearray(original)
        changed = 0
        while True:
            descriptor = image._read(cursor, 20)
            if descriptor == bytes(20):
                break
            name_rva = int.from_bytes(descriptor[12:16], "little")
            name = image.string(name_rva)
            if release_packaging.FORBIDDEN_WINDOWS_IMPORT.fullmatch(name):
                dot = image.offset(name_rva) + len(name) - 4
                self.assertEqual(mutated[dot:dot + 4].lower(), b".dll")
                mutated[dot] = 0
                changed += 1
            cursor += 20
        self.assertEqual(changed, 9)
        with tempfile.TemporaryDirectory() as temporary:
            path = self.write(Path(temporary), "mutated-972.exe", bytes(mutated))
            output, errors = io.StringIO(), io.StringIO()
            with contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
                self.assertEqual(release_packaging.main(["verify-windows-imports", str(path)]), 1)
            self.assertIn("VCRUNTIME140", errors.getvalue())

    def test_windows_msvc_targets_link_the_crt_statically(self):
        # Plain-text parse: tomllib is 3.11+, and these tests also run on 3.9.
        config = (ROOT / ".cargo/config.toml").read_text()
        for target in ("x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc"):
            table = re.search(rf"(?ms)^\[target\.{re.escape(target)}\]\n(.*?)(?=^\[|\Z)", config)
            self.assertIsNotNone(table, target)
            self.assertRegex(
                table.group(1),
                r'(?m)^rustflags = \["-C", "target-feature=\+crt-static"\]$',
                target,
            )

    def test_workflows_gate_windows_imports_before_publication(self):
        release = (ROOT / ".github/workflows/release.yml").read_text()
        for job, before in (
            ("\n  build:", "upload distribution archives"),
            ("\n  installers:", "Build sign and inspect Windows installer"),
            ("\n  publish:", "create or update release"),
            ("\n  chocolatey:", "choco pack"),
        ):
            section = release[release.index(job):]
            self.assertLess(section.index("verify-windows-imports"), section.index(before), job)
        installer_check = (ROOT / ".github/workflows/windows-installer-check.yml").read_text()
        self.assertLess(installer_check.index("verify-windows-imports"), installer_check.index("upload distribution archives"))
        xplat = (ROOT / ".github/workflows/xplat.yml").read_text()
        self.assertIn("verify-windows-imports target/debug/haider.exe target/debug/haider-tui.exe target/debug/haiderd.exe", xplat)
        # Behavioral proof on every candidate: the siblings run on Server Core without the redist.
        clean = xplat[xplat.index("run the executables in a clean Server Core container"):]
        clean = clean[: clean.index("\n      - name:")]
        self.assertIn("matrix.phase == 'test' && runner.os == 'Windows' && matrix.shard == 1", clean)
        for fragment in ("mcr.microsoft.com/windows/servercore:", "vcruntime140.dll", "--isolation process",
                         "'haider.exe', 'haider-tui.exe', 'haiderd.exe'", "--version", "0xC0000135"):
            self.assertIn(fragment, clean)
        self.assertNotIn("continue-on-error", clean)
        self.assertLess(xplat.index("verify-windows-imports target/debug"), xplat.index("run the executables in a clean Server Core container"))
        for text in (release, installer_check, xplat):
            self.assertNotRegex(text, r"(?m)^\s*(?:RUSTFLAGS|CARGO_ENCODED_RUSTFLAGS|CARGO_BUILD_RUSTFLAGS|CARGO_TARGET_\w*WINDOWS\w*_RUSTFLAGS)\s*:")


if __name__ == "__main__":
    unittest.main()
