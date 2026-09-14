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
import importlib.util

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


def source_digest():
    files = subprocess.check_output(['git', 'ls-files', '-co', '--exclude-standard', '-z'], cwd=ROOT).split(b'\0')
    digest = hashlib.sha256()
    for raw in sorted(set(files)):
        if not raw:
            continue
        name = raw.decode()
        if name.startswith(('crates/', '.cargo/', 'scripts/android/', 'android/buildSrc/')) or name in ('Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml', '.github/workflows/android-apk.yml', 'customprov.bundle'):
            path = ROOT / name
            if path.is_file():
                digest.update(raw + b'\0' + path.read_bytes() + b'\0')
    return digest.hexdigest()


def native_verifier():
    spec = importlib.util.spec_from_file_location('verify_native', ROOT / 'scripts/android/verify-native.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def output_files(abi, elf_id):
    return [f'jniLibs/{abi}/libhaider.so', f'symbols/{elf_id}/{abi}/libhaider.so',
            f'symbols/{elf_id}/{abi}/libhaider.so.dwp']


def sha256(path):
    with path.open('rb') as stream:
        digest = hashlib.sha256()
        for chunk in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(chunk)
        return digest.hexdigest()


def verify_output(output, abis, version, build_id, readelf):
    verifier = native_verifier()
    for abi in abis:
        result = verifier.verify_so(output / 'jniLibs' / abi / 'libhaider.so', abi, version, str(readelf))
        metadata = result['metadata']
        if metadata['build_id'] != build_id or metadata['ndk'] != NDK_VERSION:
            raise ValueError(f'Stale native output for {abi}: source/toolchain mismatch')
        manifest = json.loads((output / f'manifest-{abi}.json').read_text())
        expected = output_files(abi, metadata['elf_build_id'])
        if (manifest.get('format') != 1 or manifest.get('source_digest') != build_id
                or manifest.get('version') != version or manifest.get('abi') != abi
                or set(manifest.get('sha256', {})) != set(expected)):
            raise ValueError(f'Invalid native output manifest for {abi}')
        for name in expected:
            path = output / name
            if not path.is_file() or path.stat().st_size == 0 or sha256(path) != manifest['sha256'][name]:
                raise ValueError(f'Native output checksum mismatch: {name}')
        print(json.dumps(dict(result, verdict='PASS_STATIC', source_digest=build_id), sort_keys=True))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--output', type=Path)
    parser.add_argument('--version')
    parser.add_argument('--abi', choices=ABIS, action='append')
    parser.add_argument('--source-digest', action='store_true')
    parser.add_argument('--verify-only', action='store_true')
    args = parser.parse_args()
    if args.source_digest:
        print(source_digest())
        return 0
    if args.output is None or args.version is None:
        parser.error('--output and --version are required for build/verification')
    abis = args.abi or list(ABIS)
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
    build_id = source_digest()
    if args.verify_only:
        verify_output(args.output, abis, version, build_id, llvm / 'llvm-readelf')
        return 0
    if not run(['cargo', 'ndk', '--version']).strip().endswith('4.1.2'):
        raise ValueError('cargo-ndk 4.1.2 required')
    if not run(['rustc', '--version']).startswith('rustc 1.95.0 '):
        raise ValueError('Rust 1.95.0 required')
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
    command = ['cargo', 'ndk']
    for abi in abis:
        command.extend(['-t', abi])
    command.extend(['-P', '26', 'build', '-p', 'haider-android', '--release', '--locked'])
    print('+ ' + ' '.join(command), flush=True)
    subprocess.run(command, cwd=ROOT, env=env, check=True)
    target_root = Path(env.get('CARGO_TARGET_DIR', ROOT / 'target'))
    if not target_root.is_absolute():
        target_root = ROOT / target_root
    for abi in abis:
        target = ABIS[abi]
        (args.output / f'manifest-{abi}.json').unlink(missing_ok=True)
        shutil.rmtree(args.output / 'jniLibs' / abi, ignore_errors=True)
        for old in (args.output / 'symbols').glob(f'*/{abi}'):
            shutil.rmtree(old)
        source = target_root / target / 'release/libhaider.so'
        notes = run([llvm / 'llvm-readelf', '-n', source])
        match = re.search(r'Build ID: ([0-9a-f]+)', notes)
        if not match:
            raise ValueError('Native ELF has no build ID')
        elf_id = match[1]
        symbols = args.output / 'symbols' / elf_id / abi / 'libhaider.so'
        symbols.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, symbols)
        companion = source.with_suffix('.so.dwp')
        if not companion.is_file() or companion.stat().st_size == 0:
            raise ValueError(f'Missing packed debug companion for {abi}')
        shutil.copy2(companion, symbols.with_suffix('.so.dwp'))
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
        manifest = dict(format=1, source_digest=build_id, version=version, abi=abi,
                        sha256={name: sha256(args.output / name) for name in output_files(abi, elf_id)})
        (args.output / f'manifest-{abi}.json').write_text(json.dumps(manifest, sort_keys=True, indent=2) + '\n')
    verify_output(args.output, abis, version, build_id, llvm / 'llvm-readelf')
    return 0


if __name__ == '__main__':
    try:
        sys.exit(main())
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        print(str(error), file=sys.stderr)
        sys.exit(1)
