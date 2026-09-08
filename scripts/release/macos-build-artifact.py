#!/usr/bin/env python3
"""Pack/verify unsigned macOS release bytes, symbols and build identity."""

import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import tomllib


TARGET = "aarch64-apple-darwin"
BINARIES = ("haider", "haider-tui", "haiderd", "haider-symbol-archive")
RUNTIME = BINARIES[:3]


def digest(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def identity(repo, sha):
    if not re.fullmatch(r"[0-9a-f]{40}", sha):
        raise ValueError("expected a full commit SHA")
    with (repo / "Cargo.toml").open("rb") as source:
        release = tomllib.load(source)["profile"]["release"]
    profile = {
        "release": release,
        "env": {key: value for key, value in sorted(os.environ.items())
                if key.startswith("CARGO_PROFILE_RELEASE_") or key in (
                    "CARGO_INCREMENTAL", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS",
                    "CARGO_BUILD_RUSTFLAGS", "CARGO_TARGET_AARCH64_APPLE_DARWIN_RUSTFLAGS")},
        "config": {str(path.relative_to(repo)): digest(path)
                   for path in sorted((repo / ".cargo").glob("config*")) if path.is_file()},
        "lock_sha256": digest(repo / "Cargo.lock"),
    }
    if profile["env"].get("CARGO_INCREMENTAL") != "0":
        raise ValueError("CARGO_INCREMENTAL must be 0")
    rustc = subprocess.check_output(["rustc", "--version", "--verbose"], text=True).strip()
    if not rustc.startswith("rustc 1.95.0 ") or f"host: {TARGET}" not in rustc.splitlines():
        raise ValueError("expected native aarch64 Rust 1.95.0")
    return {"schema": 1, "sha": sha, "target": TARGET, "rustc": rustc,
            "profile": profile,
            "profile_sha256": hashlib.sha256(json.dumps(profile, sort_keys=True).encode()).hexdigest()}


def required_members(names):
    names = set(names)
    required = set(BINARIES)
    for binary in RUNTIME:
        required.add(f"{binary}.dSYM/Contents/Info.plist")
    if not required <= names:
        raise ValueError(f"missing release members: {sorted(required - names)}")
    for binary in RUNTIME:
        # Cargo copies the bundle out of deps/ but retains rustc's hashed
        # DWARF basename (e.g. haider_tui-<hash>), under both thin and fat LTO.
        dwarf = PurePosixPath(f"{binary}.dSYM/Contents/Resources/DWARF")
        if not any(PurePosixPath(name).parent == dwarf for name in names):
            raise ValueError(f"missing DWARF member: {binary}.dSYM")


def macho_uuids(path):
    output = subprocess.check_output(["xcrun", "dwarfdump", "--uuid", str(path)], text=True)
    identities = set(re.findall(
        r"^UUID: ([0-9A-Fa-f]{8}(?:-[0-9A-Fa-f]{4}){3}-[0-9A-Fa-f]{12}) ",
        output, re.MULTILINE))
    if not identities:
        raise ValueError(f"dwarfdump did not report a Mach-O UUID: {path}")
    return {identity.upper() for identity in identities}


def verify_symbols(release_dir):
    # Same mechanism as release.yml's haider-symbol-archive: compare the
    # executable and whole bundle UUID sets, without guessing DWARF filenames.
    for binary in RUNTIME:
        if macho_uuids(release_dir / binary) != macho_uuids(release_dir / f"{binary}.dSYM"):
            raise ValueError(f"Mach-O UUID mismatch: {binary}")


def allowed_member(name):
    path = PurePosixPath(name)
    return (str(path) == name and not path.is_absolute() and ".." not in path.parts
            and (name in BINARIES or (len(path.parts) > 1
                 and path.parts[0] in {f"{binary}.dSYM" for binary in BINARIES})))


def pack(release_dir, bundle, expected):
    files = []
    for binary in BINARIES:
        files.append(release_dir / binary)
        files.extend(path for path in (release_dir / f"{binary}.dSYM").rglob("*")
                     if not path.is_dir())
    names = [path.relative_to(release_dir).as_posix() for path in files]
    required_members(names)
    manifest = dict(expected, files={})
    for path, name in zip(files, names):
        if path.is_symlink() or not path.is_file() or not allowed_member(name):
            raise ValueError(f"invalid release member: {name}")
        mode = 0o755 if name in BINARIES else 0o644
        if name in BINARIES and not path.stat().st_mode & 0o111:
            raise ValueError(f"non-executable release binary: {name}")
        manifest["files"][name] = {"sha256": digest(path), "size": path.stat().st_size, "mode": mode}
    verify_symbols(release_dir)
    bundle.mkdir(parents=True, exist_ok=True)
    with tarfile.open(bundle / "release-build.tar.gz", "w:gz") as archive:
        for path, name in zip(files, names):
            archive.add(path, arcname=name, recursive=False)
    (bundle / "manifest.json").write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
    print(json.dumps(manifest, indent=2, sort_keys=True))


def restore(bundle, release_dir, expected):
    manifest = json.loads((bundle / "manifest.json").read_text())
    if {key: manifest.get(key) for key in expected} != expected:
        raise ValueError("build identity mismatch (SHA/rustc/profile)")
    files = manifest.get("files")
    if not isinstance(files, dict):
        raise ValueError("invalid file manifest")
    required_members(files)
    # Validate every byte in a private staging directory before touching target/.
    with tempfile.TemporaryDirectory() as directory:
        stage = Path(directory)
        with tarfile.open(bundle / "release-build.tar.gz", "r:gz") as archive:
            members = archive.getmembers()
            names = [member.name for member in members]
            if len(names) != len(set(names)) or set(names) != set(files):
                raise ValueError("archive/manifest inventory mismatch")
            for member in members:
                name = member.name
                if not member.isfile() or not allowed_member(name):
                    raise ValueError(f"unsafe archive member: {name}")
                record = files[name]
                mode = 0o755 if name in BINARIES else 0o644
                if record.get("size") != member.size or record.get("mode") != mode:
                    raise ValueError(f"size/mode mismatch: {name}")
                destination = stage / name
                destination.parent.mkdir(parents=True, exist_ok=True)
                with archive.extractfile(member) as source, destination.open("wb") as output:
                    shutil.copyfileobj(source, output)
                if digest(destination) != record.get("sha256"):
                    raise ValueError(f"checksum mismatch: {name}")
                destination.chmod(mode)
        verify_symbols(stage)
        release_dir.mkdir(parents=True, exist_ok=True)
        for name in (*BINARIES, *(f"{binary}.dSYM" for binary in BINARIES)):
            source = stage / name
            if not source.exists():
                continue
            destination = release_dir / name
            if destination.is_symlink() or destination.is_file():
                destination.unlink()
            elif destination.exists():
                shutil.rmtree(destination)
            shutil.move(str(source), destination)
    print(f"verified {expected['sha']} {TARGET}: {len(files)} files; profile={expected['profile_sha256']}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("pack", "restore"))
    parser.add_argument("--sha", required=True)
    parser.add_argument("--repo", type=Path, default=Path.cwd())
    parser.add_argument("--release-dir", type=Path, default=Path(f"target/{TARGET}/release"))
    parser.add_argument("--bundle", type=Path, required=True)
    args = parser.parse_args()
    try:
        expected = identity(args.repo, args.sha)
        if args.command == "pack":
            pack(args.release_dir, args.bundle, expected)
        else:
            restore(args.bundle, args.release_dir, expected)
    except (ValueError, OSError, KeyError, TypeError, tarfile.TarError, subprocess.CalledProcessError) as error:
        print(f"release artifact rejected: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
