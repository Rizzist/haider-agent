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
    return f"haider-v{version}-{WINDOWS_TARGET}.zip"


def _windows_url(version: str) -> str:
    artifact = _windows_artifact(version)
    return f"{REPOSITORY}/releases/download/v{version}/{artifact}"


def _icon_url(version: str) -> str:
    return (
        "https://cdn.jsdelivr.net/gh/Rizzist/haider-agent"
        f"@v{version}/packaging/chocolatey/icon.png"
    )


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
            output / "haider.nuspec",
            "__HAIDER_ICON_VERSION__",
            version,
            "nuspec iconUrl version",
        ),
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
    artifact = f"haider-v{version}-{target}.tar.xz"
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
                rf'(?P<indent>\s*)url "([^"]*haider-v[^"]*-{re.escape(target)}\.tar\.xz)"\s*'
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
        f"haider-v{version}-{WINDOWS_TARGET}",
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
                rf'(?P<indent>\s*)url "[^"]*haider-v[^"]*-{re.escape(target)}\.tar\.xz"\s*'
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
            "extract_dir": f"haider-v{version}-{WINDOWS_TARGET}",
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


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)

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

    npm = commands.add_parser("verify-npm")
    npm.add_argument("--package", type=Path, required=True)
    npm.add_argument("--version", required=True)
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    try:
        if args.command == "render-chocolatey":
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
        elif args.command == "verify-npm":
            verify_npm_archive(args.package, args.version)
        else:  # pragma: no cover - argparse makes this unreachable.
            raise PackagingError(f"unknown command: {args.command}")
    except (OSError, PackagingError) as error:
        print(f"release-packaging: FAIL: {error}", file=sys.stderr)
        return 1
    print(f"release-packaging: {args.command} passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
