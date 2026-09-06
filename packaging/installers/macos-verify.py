#!/usr/bin/env python3
"""Fail closed on the expanded final pkg's metadata, exact payload, and bytes."""
import hashlib
import json
from pathlib import Path
import sys
import xml.etree.ElementTree as ET


def verify(expanded, payload, version, target):
    expanded, payload = Path(expanded), Path(payload)
    manifest = json.loads((payload / 'manifest.json').read_text())
    if manifest['version'] != version or manifest['target'] != target:
        raise ValueError('source manifest version/target mismatch')
    info = ET.parse(expanded / 'PackageInfo').getroot()
    if (info.get('identifier'), info.get('version'), info.get('install-location')) != (
            'ai.haidercode.haider', version, '/'):
        raise ValueError('built pkg identifier/version/install-location mismatch')
    root = expanded / 'Payload'
    expected = {'usr/local/bin/' + name: digest for name, digest in manifest['members'].items()}
    for name in manifest['members']:
        if not name or Path(name).name != name or name in ('.', '..'):
            raise ValueError('invalid payload member')
    expected['usr/local/bin/uninstall-haider.sh'] = hashlib.sha256((payload / 'uninstall-haider.sh').read_bytes()).hexdigest()
    expected['usr/local/share/haider/installer-manifest.json'] = hashlib.sha256((payload / 'manifest.json').read_bytes()).hexdigest()
    actual = {p.relative_to(root).as_posix(): p for p in root.rglob('*') if not p.is_dir() or p.is_symlink()}
    if set(actual) != set(expected):
        raise ValueError(f'pkg payload members differ: {set(actual) ^ set(expected)}')
    for name, digest in expected.items():
        path = actual[name]
        if path.is_symlink() or hashlib.sha256(path.read_bytes()).hexdigest() != digest:
            raise ValueError('pkg payload hash mismatch: ' + name)
        if name.startswith('usr/local/bin/') and path.stat().st_mode & 0o111 != 0o111:
            raise ValueError('pkg executable bit missing: ' + name)
    if (expanded / 'Scripts').exists():
        raise ValueError('unexpected package scripts')


if __name__ == '__main__':
    verify(*sys.argv[1:])
    print('macOS post-pack gate: metadata, exact members and SHA-256 verified')
