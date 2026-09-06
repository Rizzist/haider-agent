#!/usr/bin/env python3
"""Build additional Linux packages from checksum-verified release payloads.

No maintainer script runs, and no package owns user state. --package-version is
only for an explicitly synthetic lower-version lifecycle fixture.
"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
import tempfile

ARCHES = {
    'x86_64-unknown-linux-gnu': ('amd64', 'x86_64'),
    'aarch64-unknown-linux-gnu': ('arm64', 'aarch64'),
}


def run(*args, **kwargs):
    return subprocess.run(args, check=True, text=True, **kwargs)


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def read_manifest(payload, version, target):
    manifest = json.loads((payload / 'manifest.json').read_text())
    if manifest.get('version') != version or manifest.get('target') != target:
        raise ValueError('manifest version/target differs from requested release')
    members = manifest.get('members')
    if not isinstance(members, dict) or not members:
        raise ValueError('manifest must contain a nonempty member mapping')
    for name, sha in members.items():
        if not re.fullmatch(r'haider[a-zA-Z0-9_-]*', name):
            raise ValueError(f'unsafe binary member: {name!r}')
        path = payload / name
        if path.is_symlink() or not path.is_file() or digest(path) != sha:
            raise ValueError(f'payload hash/type mismatch: {name}')
    return manifest


def verify_tree(root, payload, manifest):
    expected = {'usr/bin/' + name for name in manifest['members']}
    expected.add('usr/share/haider/manifest.json')
    actual = {p.relative_to(root).as_posix() for p in root.rglob('*') if not p.is_dir()}
    if actual != expected:
        raise ValueError(f'package members differ: missing={expected-actual}, extra={actual-expected}')
    if (root / 'usr/share/haider/manifest.json').read_bytes() != (payload / 'manifest.json').read_bytes():
        raise ValueError('packed manifest differs from release manifest')
    for name, sha in manifest['members'].items():
        path = root / 'usr/bin' / name
        if path.is_symlink() or digest(path) != sha or path.stat().st_mode & 0o111 != 0o111:
            raise ValueError(f'packed binary mismatch or not executable: {name}')


def stage(root, payload, manifest):
    (root / 'usr/bin').mkdir(parents=True)
    (root / 'usr/share/haider').mkdir(parents=True)
    for name in manifest['members']:
        dest = root / 'usr/bin' / name
        shutil.copyfile(payload / name, dest)
        dest.chmod(0o755)
    shutil.copyfile(payload / 'manifest.json', root / 'usr/share/haider/manifest.json')
    (root / 'usr/share/haider/manifest.json').chmod(0o644)


def build_deb(work, payload, output, manifest, version, arch):
    root = work / 'deb-root'
    stage(root, payload, manifest)
    # Resolve runtime dependencies from the exact ELF payload on a native runner.
    debian = work / 'debian'
    debian.mkdir()
    (debian / 'control').write_text('Source: haider\n\nPackage: haider\nArchitecture: any\nDescription: Haider coding harness\n')
    dependency_line = run(
        'dpkg-shlibdeps', '-O', *('-e' + str(root / 'usr/bin' / name) for name in manifest['members']),
        cwd=work, capture_output=True,
    ).stdout.strip()
    if not dependency_line.startswith('shlibs:Depends='):
        raise ValueError('dpkg-shlibdeps did not emit runtime dependencies')
    dependencies = dependency_line.removeprefix('shlibs:Depends=')
    control = root / 'DEBIAN'
    control.mkdir()
    (control / 'control').write_text(
        f'Package: haider\nVersion: {version}\nArchitecture: {arch}\n'
        'Maintainer: Haider <support@haidercode.ai>\nSection: devel\nPriority: optional\n'
        f'Depends: {dependencies}\nDescription: Haider coding harness\n')
    asset = output / f"haider-v{version}-{manifest['target']}.deb"
    run('dpkg-deb', '--root-owner-group', '--build', str(root), str(asset))
    metadata = run('dpkg-deb', '-f', str(asset), 'Package', 'Version', 'Architecture', capture_output=True).stdout
    fields = dict(line.split(': ', 1) for line in metadata.splitlines())
    if fields != {'Package': 'haider', 'Version': version, 'Architecture': arch}:
        raise ValueError(f'deb metadata mismatch: {fields}')
    extracted = work / 'deb-extracted'
    run('dpkg-deb', '-x', str(asset), str(extracted))
    verify_tree(extracted, payload, manifest)
    packed_control = work / 'deb-control'
    run('dpkg-deb', '-e', str(asset), str(packed_control))
    if {p.name for p in packed_control.iterdir()} != {'control'}:
        raise ValueError('unexpected Debian maintainer scripts or conffiles')
    return asset


def build_rpm(work, payload, output, manifest, version, arch):
    top = work / 'rpm'
    for directory in ('BUILD', 'BUILDROOT', 'RPMS', 'SOURCES', 'SPECS', 'SRPMS'):
        (top / directory).mkdir(parents=True)
    source = top / 'payload'
    stage(source, payload, manifest)
    # BuildRoot is populated without compilation. Disable rpm's ELF stripping so
    # the installed binaries remain byte-identical to the release tarball.
    files = '\n'.join('/usr/bin/' + name for name in manifest['members'])
    spec = top / 'SPECS/haider.spec'
    spec.write_text(f'''%global __os_install_post %{{nil}}
%global _build_id_links none
%global debug_package %{{nil}}
Name: haider
Version: {version}
Release: 1
Summary: Haider coding harness
License: LicenseRef-KOA-P-1.0
AutoReq: yes
AutoProv: no
%description
Haider coding harness, packaged from the release payload.
%install
mkdir -p "%{{buildroot}}"
cp -a "{source}/usr" "%{{buildroot}}/"
%files
%defattr(-,root,root,-)
{files}
%dir /usr/share/haider
/usr/share/haider/manifest.json
''')
    run('rpmbuild', '--define', f'_topdir {top}', '--target', arch, '-bb', str(spec))
    candidates = list((top / 'RPMS').rglob('*.rpm'))
    if len(candidates) != 1:
        raise ValueError(f'expected one rpm, found {len(candidates)}')
    asset = output / f"haider-v{version}-{manifest['target']}.rpm"
    shutil.copyfile(candidates[0], asset)
    metadata = run('rpm', '-qp', '--qf', '%{NAME} %{VERSION} %{RELEASE} %{ARCH}', str(asset), capture_output=True).stdout
    if metadata != f'haider {version} 1 {arch}':
        raise ValueError(f'rpm metadata mismatch: {metadata}')
    if run('rpm', '-qp', '--scripts', str(asset), capture_output=True).stdout.strip():
        raise ValueError('unexpected RPM lifecycle scripts')
    extracted = work / 'rpm-extracted'
    extracted.mkdir()
    archive = work / 'rpm.cpio'
    with archive.open('wb') as stream:
        subprocess.run(['rpm2cpio', str(asset)], stdout=stream, check=True)
    with archive.open('rb') as stream:
        subprocess.run(['cpio', '-idm', '--quiet', '--no-absolute-filenames'], stdin=stream, cwd=extracted, check=True)
    verify_tree(extracted, payload, manifest)
    return asset


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--payload', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--version', required=True)
    parser.add_argument('--target', required=True, choices=ARCHES)
    parser.add_argument('--format', choices=('deb', 'rpm', 'all'), default='all')
    parser.add_argument('--package-version', help='synthetic lower metadata version for QA only')
    args = parser.parse_args()
    version = args.package_version or args.version
    for value in (version, args.version):
        if not re.fullmatch(r'[0-9]+\.[0-9]+\.[0-9]+', value):
            parser.error('version must have numeric major.minor.patch form')
    payload, output = args.payload.resolve(), args.output.resolve()
    manifest = read_manifest(payload, args.version, args.target)
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='haider-linux-') as tmp:
        work = Path(tmp)
        for kind, builder, arch in zip(('deb', 'rpm'), (build_deb, build_rpm), ARCHES[args.target]):
            if args.format not in ('all', kind):
                continue
            asset = builder(work, payload, output, manifest, version, arch)
            asset.with_name(asset.name + '.sha256').write_text(f'{digest(asset)}  {asset.name}\n')
            print(f'POSTPACK PASS {asset.name}: version, exact members, hashes, executable bits, no state hooks')


if __name__ == '__main__':
    main()
