#!/usr/bin/env python3
"""Freeze installer input from a checksummed release archive, never rebuild binaries."""
from __future__ import annotations
import argparse
import hashlib
import json
import re
import tarfile
import zipfile
from pathlib import Path, PurePosixPath


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def select_archive(directory: Path, target: str) -> Path:
    suffix = ".zip" if target.endswith("windows-msvc") else ".tar.xz"
    # A future thin-executable wave may retain compatibility archives beside its
    # native split bundle. New installers must select the complete split bundle.
    for ending in (f"-{target}-split{suffix}", f"-{target}{suffix}"):
        matches = [p for p in directory.iterdir() if p.is_file() and p.name.endswith(ending)]
        if len(matches) > 1:
            raise ValueError("ambiguous release archives")
        if matches:
            return matches[0]
    raise ValueError("no release archive for target")


def prepare(archive: Path, output: Path, version: str, target: str,
            members: str | None = None) -> dict:
    if not re.fullmatch(r"\d+\.\d+\.\d+", version):
        raise ValueError("installer version must be numeric X.Y.Z")
    if target not in {"aarch64-apple-darwin", "x86_64-apple-darwin",
                      "aarch64-unknown-linux-gnu", "x86_64-unknown-linux-gnu",
                      "x86_64-pc-windows-msvc"}:
        raise ValueError("unsupported target")
    suffix = ".zip" if target.endswith("windows-msvc") else ".tar.xz"
    if not archive.name.endswith((f"-{target}{suffix}", f"-{target}-split{suffix}")):
        raise ValueError("archive target/format mismatch")
    sidecar = Path(str(archive) + ".sha256").read_text().strip().split()
    if len(sidecar) != 2 or sidecar[1].lstrip("*") != archive.name or sidecar[0].lower() != digest(archive.read_bytes()):
        raise ValueError("archive checksum/filename mismatch")
    windows = target.endswith("windows-msvc")
    pattern = r"haider(?:d|-[a-z0-9]+(?:-[a-z0-9]+)*)?" + (r"\.exe" if windows else "")
    found: dict[str, bytes] = {}
    roots = set()
    seen = set()

    def add(name: str, data: bytes | None, regular: bool = True):
        path = PurePosixPath(name)
        if "\\" in name or path.is_absolute() or ".." in path.parts or name in seen:
            raise ValueError("unsafe or duplicate archive member")
        seen.add(name)
        if not regular:
            raise ValueError("archive links/special members forbidden")
        if data is None:  # directory
            return
        if len(path.parts) != 2:
            raise ValueError("release files must have one bundle root")
        roots.add(path.parts[0])
        if re.fullmatch(pattern, path.name):
            if path.name in found or not data:
                raise ValueError("duplicate or empty binary")
            found[path.name] = data

    if windows:
        with zipfile.ZipFile(archive) as package:
            for entry in package.infolist():
                mode = (entry.external_attr >> 16) & 0o170000
                add(entry.filename, None if entry.is_dir() else package.read(entry), mode in (0, 0o100000, 0o040000))
    else:
        with tarfile.open(archive) as package:
            for entry in package:
                data = package.extractfile(entry).read() if entry.isfile() else None
                add(entry.name, data, entry.isfile() or entry.isdir())
    required = {"haider.exe", "haiderd.exe"} if windows else {"haider", "haiderd"}
    if len(roots) != 1 or not required <= found.keys():
        raise ValueError("missing required binaries or ambiguous bundle root")
    if members is not None and set(members.split(",")) != found.keys():
        raise ValueError("expected member list differs from archive")
    # A new destination prevents stale members leaking into a subsequent build.
    output.mkdir(parents=True, exist_ok=False)
    for name, data in found.items():
        (output / name).write_bytes(data)
        (output / name).chmod(0o755)
    manifest = {"version": version, "target": target,
                "members": {name: digest(data) for name, data in sorted(found.items())}}
    (output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    if not windows:
        template = Path(__file__).resolve().parents[1] / "packaging/installers/uninstall-haider.sh.in"
        source = template.read_text().replace("@MEMBERS@", " ".join(sorted(found)))
        source = source.replace("@HASHES@", "\n".join(f"{digest(data)}  {name}" for name, data in sorted(found.items())))
        if re.search(r"@[A-Z_]+@", source):
            raise ValueError("unresolved uninstaller token")
        (output / "uninstall-haider.sh").write_text(source)
        (output / "uninstall-haider.sh").chmod(0o755)
    return manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--archive", type=Path)
    source.add_argument("--archives", type=Path, help="release directory; prefers complete split bundle")
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--version", required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument("--members", help="optional comma-separated exact expected binary set")
    args = parser.parse_args()
    print(json.dumps(prepare(args.archive or select_archive(args.archives, args.target), args.output, args.version, args.target, args.members)))


if __name__ == "__main__":
    main()
