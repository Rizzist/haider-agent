#!/usr/bin/env python3
"""Render and verify release package metadata before it can be published."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import shutil
import sys
import tarfile
import xml.etree.ElementTree as ET
import zipfile
from pathlib import Path, PurePosixPath


REPOSITORY = "https://github.com/Rizzist/haider-agent"
WINDOWS_TARGET = "x86_64-pc-windows-msvc"
HOMEBREW_TARGETS = (
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-gnu",
)
VERSION_PATTERN = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?")
SHA256_PATTERN = re.compile(r"[a-fA-F0-9]{64}")


class PackagingError(RuntimeError):
    """A release package would contain missing, duplicate, or stale metadata."""


def _version(value: str) -> str:
    if not VERSION_PATTERN.fullmatch(value):
        raise PackagingError(f"invalid release version: {value!r}")
    return value


def _sha256(value: str, label: str) -> str:
    if not SHA256_PATTERN.fullmatch(value):
        raise PackagingError(f"{label}: expected a 64-digit SHA-256, got {value!r}")
    return value.lower()


def _windows_artifact(version: str) -> str:
    return f"haider-v{version}-{WINDOWS_TARGET}-split.zip"


def _windows_url(version: str) -> str:
    artifact = _windows_artifact(version)
    return f"{REPOSITORY}/releases/download/v{version}/{artifact}"


ICON_URL = "https://haidercode.ai/logo.svg"


def _icon_url(version: str) -> str:
    """The package icon is served statically by haidercode.ai; it does not depend on the tag."""
    del version
    return ICON_URL


def windows_artifact_sha256(artifact: Path, version: str) -> str:
    version = _version(version)
    if artifact.name != _windows_artifact(version):
        raise PackagingError(
            f"{artifact}: artifact filename mismatch: expected "
            f"{_windows_artifact(version)!r}"
        )
    if not artifact.is_file():
        raise PackagingError(f"{artifact}: release artifact does not exist")
    digest = hashlib.sha256()
    with artifact.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _replace_token(path: Path, token: str, replacement: str, field: str) -> None:
    data = path.read_bytes()
    needle = token.encode("ascii")
    count = data.count(needle)
    if count != 1:
        raise PackagingError(
            f"{path}: required substitution {field!r} token {token!r} "
            f"matched {count} times; expected exactly 1"
        )
    path.write_bytes(data.replace(needle, replacement.encode("ascii")))


def render_chocolatey_tree(
    source: Path, output: Path, version: str, sha256: str
) -> None:
    """Copy and render the Chocolatey template without interpreting line endings."""

    version = _version(version)
    sha256 = _sha256(sha256, "Chocolatey artifact")
    if output.exists():
        raise PackagingError(f"{output}: output directory already exists")
    shutil.copytree(source, output)

    substitutions = (
        (output / "haider.nuspec", "__HAIDER_VERSION__", version, "nuspec version"),
        (
            output / "tools" / "chocolateyinstall.ps1",
            "__HAIDER_VERSION__",
            version,
            "install version",
        ),
        (
            output / "tools" / "chocolateyinstall.ps1",
            "__HAIDER_WINDOWS_X64_URL__",
            _windows_url(version),
            "install URL",
        ),
        (
            output / "tools" / "chocolateyinstall.ps1",
            "__HAIDER_WINDOWS_X64_SHA256__",
            sha256,
            "install SHA-256",
        ),
        (
            output / "tools" / "VERIFICATION.txt",
            "__HAIDER_VERSION__",
            version,
            "VERIFICATION release-tag version",
        ),
        (
            output / "tools" / "VERIFICATION.txt",
            "__HAIDER_WINDOWS_X64_SHA256__",
            sha256,
            "VERIFICATION SHA-256",
        ),
    )
    for path, token, replacement, field in substitutions:
        _replace_token(path, token, replacement, field)

    for relative in (
        Path("haider.nuspec"),
        Path("tools/chocolateyinstall.ps1"),
        Path("tools/VERIFICATION.txt"),
    ):
        path = output / relative
        leftovers = sorted(set(re.findall(rb"__HAIDER_[A-Z0-9_]+__", path.read_bytes())))
        if leftovers:
            rendered = ", ".join(item.decode("ascii") for item in leftovers)
            raise PackagingError(f"{path}: unresolved release placeholders: {rendered}")


def render_chocolatey_from_artifact(
    source: Path, output: Path, version: str, artifact: Path
) -> None:
    render_chocolatey_tree(
        source, output, version, windows_artifact_sha256(artifact, version)
    )


def _single_xml_text(root: ET.Element, name: str, source: str) -> str:
    elements = root.findall(f".//{{*}}{name}")
    if len(elements) != 1 or elements[0].text is None:
        raise PackagingError(
            f"{source}: required nuspec field {name!r} matched {len(elements)} times; "
            "expected exactly 1"
        )
    return elements[0].text.strip()


def _assert_equal(actual: str, expected: str, source: str, field: str) -> None:
    if actual != expected:
        raise PackagingError(
            f"{source}: {field} mismatch: expected {expected!r}, got {actual!r}"
        )


def _install_assignment(text: str, variable: str, source: str) -> str:
    assignment = re.compile(rf"\s*\${re.escape(variable)}\s*=\s*'([^']*)'\s*")
    candidates = [
        line
        for line in text.splitlines()
        if re.match(rf"\s*\${re.escape(variable)}\s*=", line)
    ]
    if len(candidates) != 1:
        raise PackagingError(
            f"{source}: ${variable} assignment matched {len(candidates)} times; "
            "expected exactly 1"
        )
    match = assignment.fullmatch(candidates[0])
    if match is None:
        raise PackagingError(f"{source}: ${variable} assignment has an unexpected shape")
    return match.group(1)


def verify_chocolatey_contents(
    nuspec: bytes,
    install: bytes,
    verification: bytes,
    version: str,
    sha256: str,
    source: str,
) -> None:
    """Verify every release pin in the material that will be published."""

    version = _version(version)
    sha256 = _sha256(sha256, "Chocolatey artifact")
    try:
        nuspec_root = ET.fromstring(nuspec)
    except ET.ParseError as error:
        raise PackagingError(f"{source}: invalid nuspec XML: {error}") from error

    _assert_equal(
        _single_xml_text(nuspec_root, "version", source),
        version,
        source,
        "nuspec version",
    )
    _assert_equal(
        _single_xml_text(nuspec_root, "iconUrl", source),
        _icon_url(version),
        source,
        "nuspec iconUrl",
    )

    install_text = install.decode("utf-8-sig")
    _assert_equal(
        _install_assignment(install_text, "version", source),
        version,
        source,
        "install version",
    )
    install_url = _install_assignment(install_text, "url64", source)
    _assert_equal(install_url, _windows_url(version), source, "install URL")
    _assert_equal(
        PurePosixPath(install_url).name,
        _windows_artifact(version),
        source,
        "install URL filename",
    )
    if f"/releases/download/v{version}/" not in install_url:
        raise PackagingError(f"{source}: install URL tag segment is not v{version}")
    _assert_equal(
        _install_assignment(install_text, "checksum64", source).lower(),
        sha256,
        source,
        "install SHA-256",
    )

    verification_text = verification.decode("utf-8-sig")
    release_urls = re.findall(
        rf"{re.escape(REPOSITORY)}/releases/tag/v[^\s]+", verification_text
    )
    if len(release_urls) != 1:
        raise PackagingError(
            f"{source}: VERIFICATION release-tag URL matched {len(release_urls)} "
            "times; expected exactly 1"
        )
    _assert_equal(
        release_urls[0],
        f"{REPOSITORY}/releases/tag/v{version}",
        source,
        "VERIFICATION release-tag URL",
    )
    verification_shas = [
        line.strip().lower()
        for line in verification_text.splitlines()
        if SHA256_PATTERN.fullmatch(line.strip())
    ]
    if len(verification_shas) != 1:
        raise PackagingError(
            f"{source}: VERIFICATION SHA-256 line matched {len(verification_shas)} "
            "times; expected exactly 1"
        )
    _assert_equal(
        verification_shas[0], sha256, source, "VERIFICATION SHA-256"
    )


def _zip_member(archive: zipfile.ZipFile, suffix: str, label: str) -> bytes:
    normalized = {
        name: "/" + name.replace("\\", "/").lstrip("/").lower()
        for name in archive.namelist()
    }
    matches = [name for name, value in normalized.items() if value.endswith(suffix.lower())]
    if len(matches) != 1:
        raise PackagingError(
            f"{archive.filename}: {label} matched {len(matches)} archive entries; "
            "expected exactly 1"
        )
    return archive.read(matches[0])


def verify_chocolatey_nupkg(nupkg: Path, version: str, sha256: str) -> None:
    if not nupkg.is_file():
        raise PackagingError(f"{nupkg}: Chocolatey package does not exist")
    try:
        with zipfile.ZipFile(nupkg) as archive:
            verify_chocolatey_contents(
                _zip_member(archive, ".nuspec", "nuspec"),
                _zip_member(
                    archive,
                    "/tools/chocolateyinstall.ps1",
                    "tools/chocolateyinstall.ps1",
                ),
                _zip_member(
                    archive, "/tools/verification.txt", "tools/VERIFICATION.txt"
                ),
                version,
                sha256,
                str(nupkg),
            )
    except zipfile.BadZipFile as error:
        raise PackagingError(f"{nupkg}: invalid nupkg ZIP: {error}") from error


def verify_chocolatey_against_artifact(
    nupkg: Path, version: str, artifact: Path
) -> None:
    version = _version(version)
    expected_package = f"haider.{version}.nupkg"
    if nupkg.name != expected_package:
        raise PackagingError(
            f"{nupkg}: nupkg filename mismatch: expected {expected_package!r}"
        )
    verify_chocolatey_nupkg(
        nupkg, version, windows_artifact_sha256(artifact, version)
    )


def _line_ending(line: str) -> str:
    if line.endswith("\r\n"):
        return "\r\n"
    if line.endswith("\n"):
        return "\n"
    if line.endswith("\r"):
        return "\r"
    return ""


def _line_body(line: str) -> str:
    return line[: len(line) - len(_line_ending(line))] if _line_ending(line) else line


def _read_text_raw(path: Path) -> str:
    with path.open("r", encoding="utf-8", newline="") as handle:
        return handle.read()


def _unique_line(
    lines: list[str], pattern: re.Pattern[str], source: Path, field: str
) -> tuple[int, re.Match[str]]:
    matches = [
        (index, match)
        for index, line in enumerate(lines)
        if (match := pattern.fullmatch(_line_body(line))) is not None
    ]
    if len(matches) != 1:
        raise PackagingError(
            f"{source}: required substitution {field!r} pattern {pattern.pattern!r} "
            f"matched {len(matches)} times; expected exactly 1"
        )
    return matches[0]


def _homebrew_url(version: str, target: str) -> str:
    artifact = f"haider-v{version}-{target}-split.tar.xz"
    return f"{REPOSITORY}/releases/download/v{version}/{artifact}"


def _homebrew_fields(formula: str, source: Path) -> tuple[str, dict[str, tuple[str, str]]]:
    lines = formula.splitlines(keepends=True)
    _, version_match = _unique_line(
        lines, re.compile(r'\s*version "([^"]+)"\s*'), source, "Homebrew version"
    )
    fields: dict[str, tuple[str, str]] = {}
    for target in HOMEBREW_TARGETS:
        index, url_match = _unique_line(
            lines,
            re.compile(
                rf'(?P<indent>\s*)url "([^"]*haider-v[^"]*-{re.escape(target)}(?:-split)?\.tar\.xz)"\s*'
            ),
            source,
            f"Homebrew URL for {target}",
        )
        if index + 1 >= len(lines):
            raise PackagingError(f"{source}: Homebrew SHA-256 missing after {target}")
        sha_match = re.fullmatch(
            rf'{re.escape(url_match.group("indent"))}sha256 "([^"]+)"\s*',
            _line_body(lines[index + 1]),
        )
        if sha_match is None:
            raise PackagingError(
                f"{source}: Homebrew SHA-256 for {target} must immediately follow its URL"
            )
        fields[target] = (url_match.group(2), sha_match.group(1))
    return version_match.group(1), fields


def verify_homebrew_scoop(
    packaging_root: Path, version: str, shas: dict[str, str]
) -> None:
    version = _version(version)
    formula_path = packaging_root / "homebrew" / "haider.rb"
    formula_version, formula_fields = _homebrew_fields(
        _read_text_raw(formula_path), formula_path
    )
    _assert_equal(formula_version, version, str(formula_path), "Homebrew version")
    for target in HOMEBREW_TARGETS:
        url, digest = formula_fields[target]
        _assert_equal(
            url,
            _homebrew_url(version, target),
            str(formula_path),
            f"Homebrew URL for {target}",
        )
        _assert_equal(
            digest.lower(),
            shas[target],
            str(formula_path),
            f"Homebrew SHA-256 for {target}",
        )

    scoop_path = packaging_root / "scoop" / "haider.json"
    try:
        scoop = json.loads(scoop_path.read_text(encoding="utf-8"))
        windows = scoop["architecture"]["64bit"]
    except (json.JSONDecodeError, KeyError, TypeError) as error:
        raise PackagingError(f"{scoop_path}: invalid Scoop manifest: {error}") from error
    _assert_equal(str(scoop.get("version")), version, str(scoop_path), "Scoop version")
    _assert_equal(
        str(windows.get("url")),
        _windows_url(version),
        str(scoop_path),
        "Scoop URL",
    )
    _assert_equal(
        str(windows.get("hash")).lower(),
        shas[WINDOWS_TARGET],
        str(scoop_path),
        "Scoop SHA-256",
    )
    _assert_equal(
        str(windows.get("extract_dir")),
        f"haider-v{version}-{WINDOWS_TARGET}-split",
        str(scoop_path),
        "Scoop extract directory",
    )


def repin_homebrew_scoop(
    packaging_root: Path, version: str, shas: dict[str, str]
) -> None:
    version = _version(version)
    formula_path = packaging_root / "homebrew" / "haider.rb"
    formula = _read_text_raw(formula_path)
    lines = formula.splitlines(keepends=True)
    version_index, version_match = _unique_line(
        lines, re.compile(r'(?P<indent>\s*)version "[^"]+"\s*'), formula_path, "Homebrew version"
    )
    lines[version_index] = (
        f'{version_match.group("indent")}version "{version}"'
        f"{_line_ending(lines[version_index])}"
    )
    for target in HOMEBREW_TARGETS:
        index, url_match = _unique_line(
            lines,
            re.compile(
                rf'(?P<indent>\s*)url "[^"]*haider-v[^"]*-{re.escape(target)}(?:-split)?\.tar\.xz"\s*'
            ),
            formula_path,
            f"Homebrew URL for {target}",
        )
        if index + 1 >= len(lines):
            raise PackagingError(f"{formula_path}: SHA-256 missing after {target}")
        sha_pattern = re.compile(
            rf'{re.escape(url_match.group("indent"))}sha256 "[^"]+"\s*'
        )
        if sha_pattern.fullmatch(_line_body(lines[index + 1])) is None:
            raise PackagingError(
                f"{formula_path}: required substitution 'Homebrew SHA-256 for "
                f"{target}' pattern {sha_pattern.pattern!r} matched 0 times; "
                "expected exactly 1 immediately after its URL"
            )
        lines[index] = (
            f'{url_match.group("indent")}url "{_homebrew_url(version, target)}"'
            f"{_line_ending(lines[index])}"
        )
        lines[index + 1] = (
            f'{url_match.group("indent")}sha256 "{shas[target]}"'
            f"{_line_ending(lines[index + 1])}"
        )
    formula_path.write_bytes("".join(lines).encode("utf-8"))

    scoop_path = packaging_root / "scoop" / "haider.json"
    try:
        scoop = json.loads(scoop_path.read_text(encoding="utf-8"))
        windows = scoop["architecture"]["64bit"]
        for field in ("version",):
            if field not in scoop:
                raise KeyError(field)
        for field in ("url", "hash", "extract_dir"):
            if field not in windows:
                raise KeyError(f"architecture.64bit.{field}")
    except (json.JSONDecodeError, KeyError, TypeError) as error:
        raise PackagingError(f"{scoop_path}: invalid Scoop manifest: {error}") from error
    scoop["version"] = version
    windows.update(
        {
            "url": _windows_url(version),
            "hash": shas[WINDOWS_TARGET],
            "extract_dir": f"haider-v{version}-{WINDOWS_TARGET}-split",
        }
    )
    scoop_path.write_text(json.dumps(scoop, indent=2) + "\n", encoding="utf-8", newline="\n")
    verify_homebrew_scoop(packaging_root, version, shas)


def verify_npm_archive(package: Path, version: str) -> None:
    version = _version(version)
    if not package.is_file():
        raise PackagingError(f"{package}: npm package does not exist")
    try:
        with tarfile.open(package, mode="r:gz") as archive:
            package_json = archive.extractfile("package/package.json")
            install_js = archive.extractfile("package/install.js")
            if package_json is None or install_js is None:
                raise PackagingError(
                    f"{package}: npm archive must contain package.json and install.js"
                )
            metadata = json.loads(package_json.read().decode("utf-8"))
            installer = install_js.read().decode("utf-8")
    except (tarfile.TarError, json.JSONDecodeError, KeyError) as error:
        raise PackagingError(f"{package}: invalid npm archive: {error}") from error
    _assert_equal(str(metadata.get("version")), version, str(package), "npm version")
    required = (
        'const VERSION = pkg.version.replace(/^v/, "");',
        "releases/download/v${VERSION}",
        "haider-v${VERSION}-",
    )
    for pattern in required:
        if pattern not in installer:
            raise PackagingError(
                f"{package}: npm install.js required dynamic pattern {pattern!r} "
                "matched 0 times"
            )
    stale_patterns = (
        re.compile(r"releases/download/v[0-9]+\.[0-9]+\.[0-9]+"),
        re.compile(r"haider-v[0-9]+\.[0-9]+\.[0-9]+-"),
    )
    for pattern in stale_patterns:
        if pattern.search(installer):
            raise PackagingError(
                f"{package}: npm install.js contains a static release pin matching "
                f"{pattern.pattern!r}"
            )


def _release_shas(args: argparse.Namespace) -> dict[str, str]:
    return {
        "aarch64-apple-darwin": _sha256(args.sha_darwin_arm64, "macOS arm64"),
        "x86_64-apple-darwin": _sha256(args.sha_darwin_x64, "macOS x64"),
        "aarch64-unknown-linux-gnu": _sha256(args.sha_linux_arm64, "Linux arm64"),
        "x86_64-unknown-linux-gnu": _sha256(args.sha_linux_x64, "Linux x64"),
        WINDOWS_TARGET: _sha256(args.sha_windows_x64, "Windows x64"),
    }


def verify_split_bundle(artifact: Path, target: str) -> None:
    """Check the actual release payload and sidecar before any manager sees it."""
    _verify_release_bundle(artifact, target, legacy=False)


def verify_legacy_bundle(artifact: Path, target: str) -> None:
    """Keep canonical macOS compatibility archives readable by old updaters."""
    if target not in ("aarch64-apple-darwin", "x86_64-apple-darwin"):
        raise PackagingError(f"legacy compatibility bundle is macOS-only: {target}")
    _verify_release_bundle(artifact, target, legacy=True)


def _verify_release_bundle(artifact: Path, target: str, *, legacy: bool) -> None:
    windows = target == WINDOWS_TARGET
    extension = ".zip" if windows else ".tar.xz"
    flavor = "" if legacy else "-split"
    if not artifact.name.endswith(f"-{target}{flavor}{extension}"):
        raise PackagingError(f"{artifact}: wrong bundle format for {target}")
    top = artifact.name.removesuffix(extension)
    suffix = ".exe" if windows else ""
    members = ("haider", "haiderd") if legacy else ("haider", "haider-tui", "haiderd")
    required = {f"{top}/{name}{suffix}" for name in members}
    if "linux" in target:
        required.add(f"{top}/haider-wayland-portal")
    if windows:
        required.update((f"{top}/haider.cmd", f"{top}/README.txt"))
        with zipfile.ZipFile(artifact) as archive:
            entries = [(item.filename.rstrip("/"), item.is_dir(), item.file_size) for item in archive.infolist()]
    else:
        with tarfile.open(artifact, "r:xz") as archive:
            entries = []
            for item in archive.getmembers():
                if not (item.isdir() or item.isfile()):
                    raise PackagingError(f"{artifact}: non-regular archive member {item.name}")
                if item.isfile() and item.mode & 0o100 == 0:
                    raise PackagingError(f"{artifact}: non-executable sibling {item.name}")
                entries.append((item.name.rstrip("/"), item.isdir(), item.size))
    seen = set()
    for name, directory, size in entries:
        if name in seen:
            raise PackagingError(f"{artifact}: duplicate archive member {name}")
        seen.add(name)
        if directory:
            if name != top:
                raise PackagingError(f"{artifact}: unexpected archive directory {name}")
        elif name not in required or size == 0:
            raise PackagingError(f"{artifact}: unexpected or empty archive member {name}")
    if (not windows and top not in seen) or seen - {top} != required:
        raise PackagingError(f"{artifact}: missing sibling or launcher: {sorted(required - seen)}")
    sidecar = artifact.with_name(artifact.name + ".sha256")
    fields = sidecar.read_text().strip().split()
    if len(fields) != 2 or Path(fields[1].lstrip("*")).name != artifact.name:
        raise PackagingError(f"{sidecar}: expected exact archive checksum record")
    expected = _sha256(fields[0], str(sidecar))
    digest = hashlib.sha256()
    with artifact.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    actual = digest.hexdigest()
    if expected != actual:
        raise PackagingError(f"{sidecar}: archive checksum mismatch")


# Redistributable Microsoft C/C++ runtime DLLs. They are absent from clean
# Windows installs without the VC++ redistributable, so a shipped PE that
# imports one dies with 0xC0000135 (STATUS_DLL_NOT_FOUND) before main
# (registry #176: haider 0.0.972 on Chocolatey's clean Server 2019 verifier).
# The api-ms-win-crt-* API sets and ucrtbase.dll are OS components on Windows
# 10+, but this gate also uses them to detect /MD (registry #176). The entries
# below describe runtime *families*, not arbitrary version-numbered DLLs.
# Sources: https://learn.microsoft.com/en-us/cpp/windows/determining-which-dlls-to-redistribute
# and https://learn.microsoft.com/en-us/cpp/c-runtime-library/crt-library-features
FORBIDDEN_WINDOWS_IMPORT_FAMILIES = (
    # VC 1.x and VC 4/5 debug CRT: Microsoft's archived KB154753/KB165685.
    (r"msvcrt10\.dll", "VC 1.x CRT", "https://ftp.zx.net.nz/pub/archive/ftp.microsoft.com/MISC/KB/en-us/115/082.HTM"),
    (r"msvcrt21\.dll", "VC 2.x Win32s CRT", "https://ftp.zx.net.nz/pub/mirror/ftp.microsoft.com/MISC/KB/en-us/130/384.HTM"),
    (r"msvcrtd\.dll", "VC 4/5 debug CRT", "https://ftp.zx.net.nz/pub/archive/ftp.microsoft.com/MISC/KB/en-us/154/753.HTM"),
    (r"msvcirtd\.dll", "VC 4/5 debug iostreams", "https://ftp.zx.net.nz/pub/archive/ftp.microsoft.com/MISC/KB/en-us/165/685.HTM"),
    (r"msvcr(?:40|70|71|80|90|100|110|120)d?(?:_[a-z0-9_]+)?\.dll", "versioned VC CRT", "https://learn.microsoft.com/en-us/cpp/c-runtime-library/crt-library-features"),
    (r"msvcp(?:50|60|70|71|80|90|100|110|120|140)d?(?:_[a-z0-9_]+)?\.dll", "VC C++ library and satellites", "https://learn.microsoft.com/en-us/cpp/c-runtime-library/crt-library-features"),
    (r"msvci(?:70|71)d?\.dll", "VC iostreams", "https://learn.microsoft.com/en-us/cpp/windows/determining-which-dlls-to-redistribute"),
    (r"msvcm(?:80|90)d?\.dll", "mixed-mode C++/CLI CRT", "https://learn.microsoft.com/en-us/cpp/c-runtime-library/crt-library-features"),
    (r"vcruntime140d?(?:_[a-z0-9_]+)?\.dll", "VC14 runtime and satellites", "https://learn.microsoft.com/en-us/cpp/c-runtime-library/crt-library-features"),
    (r"(?:appcrt|desktopcrt)140d?\.dll", "VS14 preview split CRT", "https://devblogs.microsoft.com/cppblog/the-great-c-runtime-crt-refactoring/"),
    (r"concrt(?:100|110|120|140)d?(?:_[a-z0-9_]+)?\.dll", "Concurrency Runtime", "https://learn.microsoft.com/en-us/cpp/windows/determining-which-dlls-to-redistribute"),
    (r"vccorlib(?:110|120|140)d?(?:_[a-z0-9_]+)?\.dll", "C++/CX runtime", "https://learn.microsoft.com/en-us/cpp/windows/determining-which-dlls-to-redistribute"),
    (r"vcamp(?:110|120|140)d?(?:_[a-z0-9_]+)?\.dll", "C++ AMP runtime", "https://learn.microsoft.com/en-us/cpp/windows/determining-which-dlls-to-redistribute"),
    (r"vcomp(?:d|(?:90|100|110|120|140)d?(?:_[a-z0-9_]+)?)?\.dll", "VC OpenMP, including VC8 unversioned", "https://jacobfilipp.com/MSDN/2005_10/OpenMP/chm.htm"),
    (r"vcomp100ui\.dll", "VC10 OpenMP UI resources", "https://support.microsoft.com/en-us/topic/visual-studio-fix-module-state-is-corrupted-in-a-visual-c-2010-mfc-application-that-is-running-in-windows-8-774d1687-eb29-3468-b68a-df5d8517300e"),
    (r"libomp[a-z0-9_.-]*\.dll", "LLVM OpenMP", "https://devblogs.microsoft.com/cppblog/openmp-updates-and-fixes-for-cpp-in-visual-studio-2019-16-10/"),
    (r"mfc(?:30|40|42|70|71|80|90|100|110|120|140)(?:u|d|ud|[a-z]{3})?\.dll", "MFC, Unicode, debug, and localized resources", "https://learn.microsoft.com/en-us/cpp/windows/redistributing-the-mfc-library"),
    (r"mfcm(?:80|90|100|110|120|140)(?:u|d|ud)?\.dll", "managed MFC", "https://learn.microsoft.com/en-us/cpp/windows/determining-which-dlls-to-redistribute"),
    (r"mfc[don](?:30|40|42)(?:u?d?)\.dll", "split VC4/5 MFC", "https://ftp.zx.net.nz/pub/archive/ftp.microsoft.com/MISC/KB/en-us/165/685.HTM"),
    (r"mfcmifc80\.dll", "managed MFC interface assembly", "https://learn.microsoft.com/en-us/cpp/windows/redistributing-the-mfc-library"),
    (r"atl(?:70|71|80|90|100|110|120|140)d?\.dll", "versioned ATL", "https://learn.microsoft.com/en-us/cpp/windows/determining-which-dlls-to-redistribute"),
    (r"clang_rt\.asan_(?:dbg_)?dynamic-[a-z0-9_]+\.dll", "VS AddressSanitizer runtime", "https://learn.microsoft.com/en-us/cpp/sanitizers/asan-runtime"),
    (r"ucrtbased?\.dll", "UCRT and debug UCRT; /MD policy", "https://learn.microsoft.com/en-us/cpp/c-runtime-library/crt-library-features"),
    (r"api-ms-win-crt-[a-z0-9-]+\.dll", "UCRT API sets; /MD policy", "https://learn.microsoft.com/en-us/cpp/porting/upgrade-your-code-to-the-universal-crt"),
)
FORBIDDEN_WINDOWS_IMPORT = re.compile(
    "|".join(f"(?:{pattern})" for pattern, _, _ in FORBIDDEN_WINDOWS_IMPORT_FAMILIES),
    re.IGNORECASE,
)
# Exact Windows/.NET component names. The CLR entries depend on the installed
# Framework update; they do not claim availability on every stock Windows SKU.
WINDOWS_SYSTEM_RUNTIME_IMPORTS = {
    "msvcp60.dll",  # Windows VC6 compatibility; https://learn.microsoft.com/en-us/security-updates/securitybulletins/2007/ms07-012
    "mfc40.dll",  # Windows MFC 4.0; https://support.microsoft.com/en-gb/topic/ms10-074-vulnerability-in-microsoft-foundation-classes-could-allow-remote-code-execution-591ee7a4-48a6-5f55-4822-42419f51ef49
    "mfc40u.dll",  # Windows Unicode MFC 4.0; https://learn.microsoft.com/en-us/security-updates/securitybulletins/2007/ms07-012
    "mfc42.dll",  # Windows MFC 4.2; https://learn.microsoft.com/en-us/security-updates/securitybulletins/2007/ms07-012
    "mfc42u.dll",  # Windows Unicode MFC 4.2; https://learn.microsoft.com/en-us/security-updates/securitybulletins/2007/ms07-012
    "mfc42loc.dll",  # Windows XP MFC locale resources; https://learn.microsoft.com/en-us/archive/msdn-magazine/2001/september/under-the-hood-new-vectored-exception-handling-in-windows-xp
    "msvcp110_win.dll",  # Windows VC11 compatibility; https://learn.microsoft.com/en-us/answers/questions/950582/is-msvcp110-win-specific-to-a-visual-studio-versio
    "msvcp110_clr0400.dll",  # Windows 8/.NET 4.5 CLR; https://www.nirsoft.net/dll_information/windows8/m.html
    "msvcr100_clr0400.dll",  # Windows 8/.NET 4.x CLR; https://www.nirsoft.net/dll_information/windows8/m.html
    "msvcr110_clr0400.dll",  # Windows 8/.NET 4.x CLR; https://www.nirsoft.net/dll_information/windows8/m.html
    "msvcr120_clr0400.dll",  # .NET 4.x CLR; https://support.microsoft.com/en-us/topic/security-only-update-for-net-framework-4-5-2-for-windows-8-1-and-windows-server-2012-r2-kb4565581-6389d650-54a8-56dd-96cb-33298264d11f
    "msvcp120_clr0400.dll",  # .NET 4.x CLR; https://support.microsoft.com/tr-tr/servicing/dotnetframework/windows-10/1809/2019/01/december-5-2018-kb4469041-preview-of-cumulative-update-for-net-framework-3-5-and-4-7-2-for-windows-1
    "msvcp140_clr0400.dll",  # .NET 4.8 WPF; https://github.com/microsoft/microsoft-ui-xaml/issues/9158
    "vcruntime140_clr0400.dll",  # .NET 4.8 CLR; https://github.com/microsoft/microsoft-ui-xaml/issues/9158
    "vcruntime140_1_clr0400.dll",  # .NET 4.8 CLR; https://github.com/microsoft/microsoft-ui-xaml/issues/9158
}
_PE_IMPORT_DIRECTORY = 1
_PE_DELAY_IMPORT_DIRECTORY = 13
_PE_IMPORT_NAME_WINDOW = 512


class _PeImage:
    """Just enough of the PE/COFF format to read import and delay-import names."""

    def __init__(self, data: bytes, source: str) -> None:
        self.data = data
        self.source = source
        if len(data) < 0x40 or data[:2] != b"MZ":
            raise PackagingError(f"{source}: not a PE image (missing MZ header)")
        pe = self._u32(0x3C)
        if data[pe : pe + 4] != b"PE\0\0":
            raise PackagingError(f"{source}: not a PE image (missing PE signature)")
        coff = pe + 4
        sections = self._u16(coff + 2)
        optional_size = self._u16(coff + 16)
        optional = coff + 20
        magic = self._u16(optional)
        if magic == 0x10B:
            directories = optional + 96
        elif magic == 0x20B:
            directories = optional + 112
        else:
            raise PackagingError(f"{source}: unknown PE optional-header magic {magic:#x}")
        fixed_size = directories - optional
        if optional_size < fixed_size or optional + optional_size > len(data):
            raise PackagingError(f"{source}: truncated PE optional header")
        self.image_base = (
            self._u32(optional + 28) if magic == 0x10B else self._u64(optional + 24)
        )
        self.directory_count = self._u32(directories - 4)
        if self.directory_count > 16 or fixed_size + self.directory_count * 8 > optional_size:
            raise PackagingError(
                f"{source}: PE optional header cannot hold {self.directory_count} data directories"
            )
        self.directories = directories
        table = optional + optional_size
        self.sections = []
        for index in range(sections):
            entry = table + index * 40
            virtual_size, virtual_address, raw_size, raw_pointer = (
                self._u32(entry + 8),
                self._u32(entry + 12),
                self._u32(entry + 16),
                self._u32(entry + 20),
            )
            self.sections.append(
                (virtual_address, max(virtual_size, raw_size), raw_pointer, raw_size)
            )

    def _read(self, offset: int, size: int) -> bytes:
        if offset < 0 or offset + size > len(self.data):
            raise PackagingError(f"{self.source}: truncated PE structure at {offset:#x}")
        return self.data[offset : offset + size]

    def _u16(self, offset: int) -> int:
        return int.from_bytes(self._read(offset, 2), "little")

    def _u32(self, offset: int) -> int:
        return int.from_bytes(self._read(offset, 4), "little")

    def _u64(self, offset: int) -> int:
        return int.from_bytes(self._read(offset, 8), "little")

    def directory(self, index: int) -> tuple[int, int]:
        if index >= self.directory_count:
            # An undeclared directory is absent to the loader too:
            # RtlImageDirectoryEntryToData returns NULL for that index.
            return 0, 0
        entry = self.directories + index * 8
        rva, size = self._u32(entry), self._u32(entry + 4)
        if rva and not size:
            # The loader walks descriptors from the address until a null entry
            # regardless of size; never treat a sized-zero table as absent.
            raise PackagingError(
                f"{self.source}: data directory {index} has address {rva:#x} but size 0"
            )
        return rva, size

    def offset(self, rva: int) -> int:
        for virtual_address, size, raw_pointer, raw_size in self.sections:
            if virtual_address <= rva < virtual_address + size:
                delta = rva - virtual_address
                if delta >= raw_size:
                    break
                return raw_pointer + delta
        raise PackagingError(f"{self.source}: RVA {rva:#x} is outside every PE section")

    def string(self, rva: int) -> str:
        start = self.offset(rva)
        for virtual_address, size, raw_pointer, raw_size in self.sections:
            delta = rva - virtual_address
            if 0 <= delta < size and delta < raw_size:
                limit = min(
                    start + _PE_IMPORT_NAME_WINDOW,
                    raw_pointer + raw_size,
                    len(self.data),
                )
                break
        end = self.data.find(b"\0", start, limit)
        if end < 0:
            raise PackagingError(f"{self.source}: unterminated PE import name at RVA {rva:#x}")
        name = self.data[start:end]
        if not name:
            raise PackagingError(f"{self.source}: empty PE import name at RVA {rva:#x}")
        if any(byte < 0x20 or byte > 0x7E for byte in name):
            raise PackagingError(f"{self.source}: non-printable ASCII PE import name at RVA {rva:#x}")
        return name.decode("ascii")

    def imports(self) -> tuple[list[str], list[str]]:
        """Return (imported DLLs, delay-loaded DLLs) in table order."""
        normal: list[str] = []
        rva, size = self.directory(_PE_IMPORT_DIRECTORY)
        if rva:
            cursor = self.offset(rva)
            while True:
                descriptor = self._read(cursor, 20)
                if descriptor == bytes(20):
                    break
                normal.append(self.string(int.from_bytes(descriptor[12:16], "little")))
                cursor += 20
        delayed: list[str] = []
        rva, size = self.directory(_PE_DELAY_IMPORT_DIRECTORY)
        if rva:
            cursor = self.offset(rva)
            while True:
                descriptor = self._read(cursor, 32)
                if descriptor == bytes(32):
                    break
                attributes = int.from_bytes(descriptor[0:4], "little")
                name = int.from_bytes(descriptor[4:8], "little")
                if not attributes & 1:
                    # Pre-VC7 delay descriptors hold virtual addresses.
                    name -= self.image_base
                delayed.append(self.string(name))
                cursor += 32
        return normal, delayed


def windows_pe_imports(data: bytes, source: str) -> dict[str, list[str]]:
    normal, delayed = _PeImage(data, source).imports()
    return {"imports": normal, "delay_imports": delayed}


def forbidden_windows_imports(imports: dict[str, list[str]]) -> list[str]:
    def is_forbidden(name: str) -> bool:
        # Wine's build_import_name strips trailing spaces before appending .dll;
        # Win32 path normalization (collapse_path / RtlDosPathNameToNtPathName)
        # trims spaces and dots and collapses path segments. Check every segment
        # conservatively so a path alias cannot hide a loadable CRT import.
        for segment in re.split(r"[/\\:]", name.rstrip(" ")):
            component = segment.rstrip(" .")
            if not component or component in (".", ".."):
                continue
            component = component.casefold()
            if "." not in component:
                component += ".dll"
            if component in WINDOWS_SYSTEM_RUNTIME_IMPORTS:
                continue
            # Wine's get_apiset_entry ignores the suffix after the last hyphen
            # and stops at the first dot; only the CRT API-set family is banned.
            if component.startswith("api-ms-win-crt-") or FORBIDDEN_WINDOWS_IMPORT.fullmatch(component):
                return True
        return False

    return sorted(
        {
            name
            for names in imports.values()
            for name in names
            if is_forbidden(name)
        },
        key=str.lower,
    )


def _windows_pe_inputs(path: Path) -> list[tuple[str, bytes]]:
    suffixes = (".exe", ".dll")
    if path.is_dir():
        return [
            (str(item), item.read_bytes())
            for item in sorted(path.rglob("*"))
            if item.is_file() and item.suffix.lower() in suffixes
        ]
    if path.suffix.lower() == ".zip":
        try:
            with zipfile.ZipFile(path) as archive:
                return [
                    (f"{path}!{item.filename}", archive.read(item))
                    for item in archive.infolist()
                    if not item.is_dir() and item.filename.lower().endswith(suffixes)
                ]
        except zipfile.BadZipFile as error:
            raise PackagingError(f"{path}: invalid ZIP: {error}") from error
    if not path.is_file():
        raise PackagingError(f"{path}: Windows PE input does not exist")
    return [(str(path), path.read_bytes())]


def verify_windows_imports(paths: list[Path]) -> list[str]:
    """Fail unless every shipped PE avoids the redistributable CRT DLLs.

    Accepts PE files, directories (every nested .exe/.dll) and ZIP bundles
    (every .exe/.dll member). Returns one report line per inspected PE.
    """
    report: list[str] = []
    failures: list[str] = []
    for path in paths:
        inputs = _windows_pe_inputs(path)
        if not inputs:
            raise PackagingError(f"{path}: contains no .exe or .dll to inspect")
        for source, data in inputs:
            imports = windows_pe_imports(data, source)
            forbidden = forbidden_windows_imports(imports)
            report.append(
                f"{source}: imports={','.join(imports['imports']) or '-'} "
                f"delay={','.join(imports['delay_imports']) or '-'}"
            )
            if forbidden:
                failures.append(f"{source}: imports {', '.join(map(repr, forbidden))}")
    if failures:
        raise PackagingError(
            "Windows PE imports the dynamic MSVC C runtime (link with "
            "-C target-feature=+crt-static; clean Windows lacks the VC++ "
            "redistributable):\n  " + "\n  ".join(failures)
        )
    return report


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)

    for name in ("verify-bundle", "verify-legacy-bundle"):
        bundle = commands.add_parser(name)
        bundle.add_argument("--artifact", type=Path, required=True)
        bundle.add_argument("--target", required=True)

    render = commands.add_parser("render-chocolatey")
    render.add_argument("--source", type=Path, required=True)
    render.add_argument("--output", type=Path, required=True)
    render.add_argument("--version", required=True)
    render.add_argument("--artifact", type=Path, required=True)

    verify = commands.add_parser("verify-chocolatey")
    verify.add_argument("--nupkg", type=Path, required=True)
    verify.add_argument("--version", required=True)
    verify.add_argument("--artifact", type=Path, required=True)

    repin = commands.add_parser("repin-manifests")
    repin.add_argument("--packaging-root", type=Path, required=True)
    repin.add_argument("--version", required=True)
    repin.add_argument("--sha-darwin-arm64", required=True)
    repin.add_argument("--sha-darwin-x64", required=True)
    repin.add_argument("--sha-linux-arm64", required=True)
    repin.add_argument("--sha-linux-x64", required=True)
    repin.add_argument("--sha-windows-x64", required=True)

    imports = commands.add_parser(
        "verify-windows-imports",
        help="fail if a Windows PE imports VCRUNTIME/MSVCP/api-ms-win-crt DLLs",
    )
    imports.add_argument(
        "paths", type=Path, nargs="+", help="PE files, directories or ZIP bundles"
    )

    npm = commands.add_parser("verify-npm")
    npm.add_argument("--package", type=Path, required=True)
    npm.add_argument("--version", required=True)
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    try:
        if args.command == "verify-bundle":
            verify_split_bundle(args.artifact, args.target)
        elif args.command == "verify-legacy-bundle":
            verify_legacy_bundle(args.artifact, args.target)
        elif args.command == "render-chocolatey":
            render_chocolatey_from_artifact(
                args.source, args.output, args.version, args.artifact
            )
        elif args.command == "verify-chocolatey":
            verify_chocolatey_against_artifact(
                args.nupkg, args.version, args.artifact
            )
        elif args.command == "repin-manifests":
            repin_homebrew_scoop(
                args.packaging_root, args.version, _release_shas(args)
            )
        elif args.command == "verify-windows-imports":
            for line in verify_windows_imports(args.paths):
                print(line)
        elif args.command == "verify-npm":
            verify_npm_archive(args.package, args.version)
        else:  # pragma: no cover - argparse makes this unreachable.
            raise PackagingError(f"unknown command: {args.command}")
    except (OSError, PackagingError, tarfile.TarError, zipfile.BadZipFile) as error:
        print(f"release-packaging: FAIL: {error}", file=sys.stderr)
        return 1
    print(f"release-packaging: {args.command} passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
