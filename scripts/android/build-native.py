#!/usr/bin/env python3
"""Single-checkout native build, unstripped symbols, stripped APK inputs and provenance."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import re

ROOT = Path(__file__).resolve().parents[2]
ABIS = {'arm64-v8a': 'aarch64-linux-android', 'x86_64': 'x86_64-linux-android'}
NDK_VERSION = '28.2.13676358'


def run(command, **kwargs):
    print('+ ' + ' '.join(map(str, command)), flush=True)
    return subprocess.check_output(list(map(str, command)), text=True, **kwargs)


def workspace_version(manifest):
    # Match the same single workspace.package version used by HaiderVersion.kt;
    # macOS' system Python 3.9 does not include tomllib.
    in_package = False
    for line in manifest.splitlines():
        line = line.split('#', 1)[0].strip()
        if line.startswith('['):
            in_package = line == '[workspace.package]'
        elif in_package:
            match = re.fullmatch(r'version\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"', line)
            if match:
                return match[1]
    raise ValueError('No numeric workspace.package version in Cargo.toml')


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--version', required=True)
    args = parser.parse_args()
    version = workspace_version((ROOT / 'Cargo.toml').read_text())
    if version != args.version:
        raise ValueError('APK override must match workspace/native version')
    if not (ROOT / 'crates/haider-android/Cargo.toml').exists():
        raise ValueError('haider-android is missing: integrate lane 971-1 before packaging')
    sdk = Path(os.environ.get('ANDROID_HOME', os.environ.get('ANDROID_SDK_ROOT', '')))
    ndk = sdk / 'ndk' / NDK_VERSION
    host = 'darwin-x86_64' if sys.platform == 'darwin' else 'linux-x86_64'
    llvm = ndk / 'toolchains/llvm/prebuilt' / host / 'bin'
    if not (llvm / 'llvm-readelf').exists():
        raise ValueError(f'Pinned NDK {NDK_VERSION} is unavailable')
    if not run(['cargo', 'ndk', '--version']).strip().endswith('4.1.2'):
        raise ValueError('cargo-ndk 4.1.2 required')
    if not run(['rustc', '--version']).startswith('rustc 1.95.0 '):
        raise ValueError('Rust 1.95.0 required')
    files = subprocess.check_output(['git', 'ls-files', '-co', '--exclude-standard', '-z'], cwd=ROOT).split(b'\0')
    digest = hashlib.sha256()
    for raw in sorted(set(files)):
        if not raw:
            continue
        name = raw.decode()
        if name.startswith(('crates/', '.cargo/', 'scripts/android/')) or name in ('Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml'):
            path = ROOT / name
            if path.is_file():
                digest.update(raw + b'\0' + path.read_bytes() + b'\0')
    build_id = digest.hexdigest()
    env = os.environ.copy()
    env.update(ANDROID_NDK_HOME=str(ndk), ANDROID_NDK_ROOT=str(ndk), ANDROID_NDK=str(ndk),
               CARGO_BUILD_JOBS='2', CARGO_INCREMENTAL='0', HAIDER_ANDROID_BUILD_ID=build_id,
               CARGO_PROFILE_RELEASE_DEBUG='2', CARGO_PROFILE_RELEASE_STRIP='none')
    # cargo-ndk supplies target linkers and API-26 C/C++ tools; no ambient API-21 overrides win.
    for key in list(env):
        if key.startswith(('CC_', 'CXX_', 'AR_', 'RANLIB_', 'CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER', 'CARGO_TARGET_X86_64_LINUX_ANDROID_LINKER')):
            env.pop(key)
    args.output.mkdir(parents=True, exist_ok=True)
    # The caller owns scheduling for the whole Gradle/native build. Nested
    # admission can deadlock a shared slot or starve an already admitted build.
    command = ['cargo', 'ndk', '-t', 'arm64-v8a', '-t', 'x86_64', '-P', '26', 'build', '-p', 'haider-android', '--release', '--locked']
    print('+ ' + ' '.join(command), flush=True)
    subprocess.run(command, cwd=ROOT, env=env, check=True)
    target_root = Path(env.get('CARGO_TARGET_DIR', ROOT / 'target'))
    if not target_root.is_absolute():
        target_root = ROOT / target_root
    for generated in ('symbols', 'jniLibs'):
        shutil.rmtree(args.output / generated, ignore_errors=True)
    for abi, target in ABIS.items():
        source = target_root / target / 'release/libhaider.so'
        notes = run([llvm / 'llvm-readelf', '-n', source])
        match = re.search(r'Build ID: ([0-9a-f]+)', notes)
        if not match:
            raise ValueError('Native ELF has no build ID')
        elf_id = match[1]
        symbols = args.output / 'symbols' / elf_id / abi / 'libhaider.so'
        symbols.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, symbols)
        packaged = args.output / 'jniLibs' / abi / 'libhaider.so'
        packaged.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, packaged)
        subprocess.run([str(llvm / 'llvm-strip'), '--strip-unneeded', str(packaged)], check=True)
        metadata = dict(format=1, daemon_version=version, abi=abi, jni_version=1, wire_protocol=1,
                        build_id=build_id, elf_build_id=elf_id, ndk=NDK_VERSION, api=26)
        # Build provenance section is distinct from runtime nativeVersion. Device tests check the latter.
        with tempfile.TemporaryDirectory() as temp:
            manifest = Path(temp) / 'metadata.json'
            manifest.write_text(json.dumps(metadata, sort_keys=True, separators=(',', ':')))
            subprocess.run([str(llvm / 'llvm-objcopy'), '--add-section', f'.haider.build={manifest}', str(packaged)], check=True)
        print(run([sys.executable, ROOT / 'scripts/android/verify-native.py', '--so', packaged,
                   '--abi', abi, '--version', version, '--readelf', llvm / 'llvm-readelf']))
    return 0


if __name__ == '__main__':
    try:
        sys.exit(main())
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        print(str(error), file=sys.stderr)
        sys.exit(1)
