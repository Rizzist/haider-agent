#!/usr/bin/env python3
"""Render verified Inno inputs for release packaging and disposable compile checks."""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import re
import shutil
import zipfile

from installer_payload import digest, prepare


TARGET = "x86_64-pc-windows-msvc"
TEMPLATE = Path(__file__).resolve().parents[1] / "packaging/installers/windows.iss"


def inno_path(path: Path) -> str:
    value = str(path.resolve())
    if any(char in value for char in ('"', '\r', '\n', '\0')):
        raise ValueError("Invalid Inno input path")
    return value


def render(payload: Path, manifest: Path, version: str, target: str,
           output: Path, work: Path) -> None:
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version) or target != TARGET:
        raise ValueError("Invalid Windows release coordinates")
    metadata = json.loads(manifest.read_text(encoding="utf-8-sig"))
    if metadata["version"] != version or metadata["target"] != target:
        raise ValueError("Manifest release mismatch")
    members = metadata["members"]
    if not isinstance(members, dict) or not {"haider.exe", "haiderd.exe"} <= members.keys():
        raise ValueError("Missing required binaries")
    files = []
    for name, expected_hash in sorted(members.items()):
        if not re.fullmatch(r"haider[a-zA-Z0-9_-]*\.exe", name):
            raise ValueError(f"Invalid binary member: {name}")
        source = payload / name
        if digest(source.read_bytes()) != expected_hash:
            raise ValueError(f"Payload hash mismatch: {source}")
        files.append(f'Source: "{inno_path(source)}"; DestDir: "{{app}}"; Flags: ignoreversion')
    files.append(f'Source: "{inno_path(manifest)}"; DestDir: "{{app}}"; '
                 'DestName: "installer-manifest.json"; Flags: ignoreversion')
    definitions = (f'#define ReleaseVersion "{version}"\n'
                   f'#define ReleaseTarget "{target}"\n'
                   f'#define OutputPath "{inno_path(output)}"\n')
    # Validate every member before writing any compiler input.
    work.mkdir(parents=True, exist_ok=True)
    output.mkdir(parents=True, exist_ok=True)
    (work / "members.iss").write_text("\n".join(files) + "\n", encoding="utf-8-sig")
    (work / "generated.iss").write_text(definitions, encoding="utf-8-sig")
    shutil.copyfile(TEMPLATE, work / "windows.iss")


def compile_fixture(work: Path) -> None:
    """Use the release archive/manifest pipeline, with bytes never meant to execute."""
    version = "0.0.970"
    work.mkdir(parents=True, exist_ok=True)
    archive = work / f"haider-v{version}-{TARGET}.zip"
    with zipfile.ZipFile(archive, "w") as bundle:
        for name in ("haider.exe", "haiderd.exe", "haider-tui.exe"):
            bundle.writestr(f"bundle/{name}", f"Inno compile-only stub: {name}\n".encode())
    Path(str(archive) + ".sha256").write_text(
        f"{digest(archive.read_bytes())}  {archive.name}\n", encoding="ascii")
    payload = work / "payload"
    prepare(archive, payload, version, TARGET)
    render(payload, payload / "manifest.json", version, TARGET, work / "output", work)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    fixture = commands.add_parser("fixture", help="render disposable compile-only inputs")
    fixture.add_argument("--work", type=Path, required=True)
    release = commands.add_parser("render", help="verify payload and render Inno inputs")
    for name in ("payload", "manifest", "output", "work"):
        release.add_argument(f"--{name}", type=Path, required=True)
    release.add_argument("--version", required=True)
    release.add_argument("--target", required=True)
    args = parser.parse_args()
    if args.command == "fixture":
        compile_fixture(args.work)
    else:
        render(args.payload, args.manifest, args.version, args.target, args.output, args.work)


if __name__ == "__main__":
    main()
