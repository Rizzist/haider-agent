#!/usr/bin/env python3
"""Fail-closed ELF/APK gate. Static provenance is not a JNI runtime result."""
import argparse
import json
from pathlib import Path
import re
import struct
import subprocess
import sys
import tempfile
import zipfile

MACHINES = {'arm64-v8a': 183, 'x86_64': 62}
JNI = {'Java_ai_diffforge_haider_daemon_NativeDaemon_' + name for name in
       ('nativeVersion', 'nativeInit', 'nativeStart', 'nativeObserve', 'nativeShutdown', 'nativeRelease')}
NEEDED = {'libc.so', 'libm.so', 'libdl.so', 'liblog.so', 'libandroid.so'}
# Compose's graphics-path dependency ships this library for each filtered ABI.
PACKAGED_LIBRARIES = {'libhaider.so', 'libandroidx.graphics.path.so'}


class InvalidNative(ValueError):
    pass


def require(condition, message):
    if not condition:
        raise InvalidNative(message)


def cstring(data, offset):
    end = data.find(b'\0', offset)
    require(0 <= offset < len(data) and end >= 0, 'invalid ELF string')
    return data[offset:end].decode('utf-8')


def inspect_elf(data, abi, version=None):
    require(len(data) >= 64 and data[:7] == b'\x7fELF\x02\x01\x01', 'expected ELF64 little endian')
    kind, machine = struct.unpack_from('<HH', data, 16)
    require(kind == 3, 'expected ET_DYN')
    require(machine == MACHINES[abi], 'wrong ELF machine')
    phoff, shoff = struct.unpack_from('<QQ', data, 32)
    phsize, phnum, shsize, shnum, shstr = struct.unpack_from('<HHHHH', data, 54)
    require(phsize == 56 and shsize == 64 and 0 < shstr < shnum, 'invalid ELF tables')
    require(phoff + phsize * phnum <= len(data) and shoff + shsize * shnum <= len(data), 'truncated ELF tables')
    loads = 0
    relro = False
    for index in range(phnum):
        ptype, flags, offset, address, _, filesz, memsz, alignment = struct.unpack_from('<IIQQQQQQ', data, phoff + index * phsize)
        require(offset + filesz <= len(data), 'truncated ELF segment')
        if ptype == 1:
            loads += 1
            require(alignment >= 16384 and alignment & (alignment - 1) == 0, 'LOAD alignment below 16 KiB')
            require(offset % 16384 == address % 16384, 'LOAD offset/address misalignment')
            require(not flags & 1 or not flags & 2, 'writable executable LOAD')
        if ptype == 0x6474e552:
            relro = memsz > 0
    require(loads > 0 and relro, 'missing LOAD/GNU RELRO')
    headers = [struct.unpack_from('<IIQQQQIIQQ', data, shoff + index * shsize) for index in range(shnum)]
    strings = headers[shstr]
    names = data[strings[4]:strings[4] + strings[5]]
    sections = {}
    for header in headers:
        name = cstring(names, header[0])
        if header[1] != 8:  # NOBITS has no file bytes.
            require(header[4] + header[5] <= len(data), 'truncated ELF section')
            sections[name] = data[header[4]:header[4] + header[5]]
    require('.debug_info' not in sections, 'unstripped debug data in APK library')
    if version is None:
        return {}
    require('.haider.build' in sections, 'missing same-build provenance section')
    metadata = json.loads(sections['.haider.build'])
    require(metadata.get('format') == 1 and metadata.get('daemon_version') == version, 'embedded version mismatch')
    require(metadata.get('abi') == abi and metadata.get('api') == 26, 'embedded ABI/API mismatch')
    require(metadata.get('jni_version') == 1 and metadata.get('wire_protocol') == 1, 'embedded contract mismatch')
    require(re.fullmatch('[0-9a-f]{64}', metadata.get('build_id', '')) is not None, 'missing source build ID')
    return metadata


def verify_so(path, abi, version, readelf):
    metadata = inspect_elf(path.read_bytes(), abi, version)
    def read(*args):
        return subprocess.check_output([readelf, *args, str(path)], text=True)
    needed = set(re.findall(r'\(NEEDED\).*?\[([^]]+)\]', read('-dW')))
    require('libc.so' in needed and needed <= NEEDED, f'unexpected DT_NEEDED: {sorted(needed - NEEDED)}')
    if version is None:
        return dict(abi=abi, needed=sorted(needed))
    exported = set()
    for line in read('--dyn-syms', '--wide').splitlines():
        fields = line.split()
        if len(fields) >= 8 and fields[4] in ('GLOBAL', 'WEAK') and fields[5] in ('DEFAULT', 'PROTECTED') and fields[6] != 'UND':
            exported.add(fields[7].split('@')[0])
    require(JNI <= exported, f'missing JNI exports: {sorted(JNI - exported)}')
    require(not {name for name in exported if name.startswith('Java_')} - JNI, 'unexpected JNI export')
    build_ids = re.findall(r'Build ID: ([0-9a-f]+)', read('-n'))
    require(metadata.get('elf_build_id') in build_ids, 'ELF build ID mismatch')
    return dict(abi=abi, version=version, needed=sorted(needed), metadata=metadata)


def verify_apk(path, abi, version, readelf, zipalign, aapt2):
    badging = subprocess.check_output([aapt2, 'dump', 'badging', str(path)], text=True)
    apk_version = re.search(r"versionName='([^']+)'", badging)
    require(apk_version is not None and apk_version[1] == version, 'APK version mismatch')
    with zipfile.ZipFile(path) as archive:
        names = [info.filename for info in archive.infolist() if info.filename.startswith('lib/') and not info.is_dir()]
        expected = {f'lib/{abi}/{name}' for name in PACKAGED_LIBRARIES}
        require(len(names) == len(expected) and set(names) == expected, f'wrong APK native contents: {names}')
        libraries = {}
        with tempfile.TemporaryDirectory() as temp:
            for name in names:
                info = archive.getinfo(name)
                require(info.compress_type == zipfile.ZIP_STORED, f'native library is compressed: {name}')
                with path.open('rb') as raw:
                    raw.seek(info.header_offset)
                    header = raw.read(30)
                    name_length, extra_length = struct.unpack_from('<HH', header, 26)
                    require((info.header_offset + 30 + name_length + extra_length) % 16384 == 0, f'APK native entry not 16 KiB aligned: {name}')
                so = Path(temp) / Path(name).name
                so.write_bytes(archive.read(info))
                libraries[so.name] = verify_so(so, abi, version if so.name == 'libhaider.so' else None, readelf)
    subprocess.run([zipalign, '-c', '-P', '16', '-v', '4', str(path)], check=True, stdout=subprocess.DEVNULL)
    return dict(abi=abi, version=version, libraries=libraries)


def main():
    parser = argparse.ArgumentParser()
    artifact = parser.add_mutually_exclusive_group(required=True)
    artifact.add_argument('--so', type=Path)
    artifact.add_argument('--apk', type=Path)
    parser.add_argument('--abi', choices=MACHINES, required=True)
    parser.add_argument('--version', required=True)
    parser.add_argument('--readelf', default='llvm-readelf')
    parser.add_argument('--zipalign', default='zipalign')
    parser.add_argument('--aapt2', default='aapt2')
    args = parser.parse_args()
    result = verify_so(args.so, args.abi, args.version, args.readelf) if args.so else verify_apk(
        args.apk, args.abi, args.version, args.readelf, args.zipalign, args.aapt2)
    print(json.dumps(dict(result, verdict='PASS_STATIC', runtime_native_version='requires instrumented JNI query'), indent=2))


if __name__ == '__main__':
    try:
        main()
    except (ValueError, OSError, struct.error, subprocess.CalledProcessError, zipfile.BadZipFile, AttributeError) as error:
        print(f'native verification failed: {error}', file=sys.stderr)
        sys.exit(1)
