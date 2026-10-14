"""Stable backing-file identities for the macOS installer and its test tracer."""
import hashlib
import json
import os
import plistlib
import re
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time


def identity(stat):
    return [stat.st_dev, stat.st_ino]


def backing_identity(image, expected):
    """Inspect the helper's open vnode, never stat its mutable backing pathname.

    Native attach plists have only system-entities on supported macOS versions.
    hdiutil info supplies the helper PID; lsof supplies the actual open inode.
    Missing helper/descriptor information grants no ownership.
    """
    pid = image.get('hdid-pid')
    if not isinstance(pid, int) or pid <= 0:
        return None
    result = subprocess.run(['/usr/sbin/lsof', '-a', '-p', str(pid), '-nP', '-F0fDtin'],
                            capture_output=True, timeout=60)
    if result.returncode:
        return None
    fields = result.stdout.split(b'\0') if b'\0' in result.stdout else result.stdout.splitlines()
    records = []
    current = {}
    for field in fields:
        field = field.lstrip(b'\n')
        if not field:
            continue
        key, value = chr(field[0]), os.fsdecode(field[1:])
        if key == 'f':
            records.append(current)
            current = {}
        current[key] = value
    records.append(current)
    confirmed_different = False
    for item in records:
        if not item.get('f', '')[:1].isdigit() or item.get('t') != 'REG':
            continue
        if 'D' not in item or 'i' not in item:
            continue
        if [int(item['D'], 16), int(item['i'])] == expected:
            return True
        # A missing descriptor is uncertainty, not evidence that the attachment
        # disappeared. A confirmed different backing vnode can be disowned.
        if item.get('n') and image.get('image-path'):
            name = item['n'].removesuffix(' (deleted)')
            if Path(name).resolve() == Path(image['image-path']).resolve():
                confirmed_different = True
    return False if confirmed_different else None


def check(path, expected):
    if path.is_symlink() or identity(path.stat()) != expected:
        raise RuntimeError('disk image identity changed; retaining ' + str(path))


def create(path, record, staging, arguments):
    # Reserve the publication inode ourselves. Native hdiutil replaces even an
    # existing -ov destination, so it writes into a private staging directory.
    with path.open('xb') as output:
        expected = identity(os.fstat(output.fileno()))
        record.write_text(json.dumps(expected))
        raw = Path(tempfile.mkdtemp(prefix='create.', dir=staging)) / 'image.dmg'
        # Arm retention before invoking the tool, including interruption before
        # it returns. No failed or interrupted create grants pathname ownership.
        retained_before = (staging / 'retain-create').exists()
        with (staging / 'retain-create').open('a') as retained:
            retained.write(str(raw) + '\n')
        result = subprocess.run(['hdiutil', 'create', *arguments, str(raw)])
        if result.returncode:
            # A failed tool provides no proof that an output is ours. In
            # particular, do not unlink a replacement published by another run.
            if raw.exists() or raw.is_symlink():
                if raw.is_symlink() or raw.stat().st_size:
                    (staging / 'create-unattributed-nonempty').touch()
                print('Unattributed create output; retaining ' + str(raw), file=sys.stderr)
            elif not retained_before:
                (staging / 'retain-create').unlink()
            return result.returncode
        with raw.open('rb') as source:
            raw_identity = identity(os.fstat(source.fileno()))
            shutil.copyfileobj(source, output)
        output.flush()
        check(path, expected)
        # The returned pathname does not prove who created this generation,
        # even when its bytes pass validation. Never claim its attachments or
        # delete it. Only the publication inode reserved above is ours.
        (staging / 'staged-image.json').write_text(json.dumps({
            'path': str(raw), 'identity': raw_identity, 'retain_before': retained_before}))
    return 0


def staging_inventory():
    for attempt in range(4):
        inventory = subprocess.run(['hdiutil', 'info', '-plist'], capture_output=True, check=True)
        if b'cannot get IO media array' not in inventory.stderr:
            return plistlib.loads(inventory.stdout).get('images', [])
        if attempt == 3:
            raise RuntimeError('incomplete create-output inventory; retaining staging')
        time.sleep(.1 * (attempt + 1))


def approve(staging, publication):
    saved = json.loads((staging / 'staged-image.json').read_text())
    raw = Path(saved['path'])
    with raw.open('rb') as source, publication.open('rb') as copied:
        if identity(os.fstat(source.fileno())) != saved['identity']:
            raise RuntimeError('staging identity changed before validation')
        if hashlib.file_digest(source, 'sha256').digest() != hashlib.file_digest(copied, 'sha256').digest():
            raise RuntimeError('staging bytes differ from the validated publication')
    check(raw, saved['identity'])


def finish(staging):
    saved = json.loads((staging / 'staged-image.json').read_text())
    raw = Path(saved['path'])
    check(raw, saved['identity'])
    # A successful create can also return an independently replaced image.
    # Matching bytes or a post-create inode is no authority to eject it.
    devices = {e['dev-entry'] for image in staging_inventory()
               if Path(image.get('image-path', '')).resolve() == raw.resolve()
               for e in image.get('system-entities', [])
               if re.fullmatch(r'/dev/disk[0-9]+', e.get('dev-entry', ''))}
    if devices:
        raise RuntimeError('unattributed create output attached as ' + ', '.join(sorted(devices))
                           + '; retaining ' + str(raw))


def checksum(path, record, sidecar):
    expected = json.loads(record.read_text())
    with path.open('rb') as source:
        if identity(os.fstat(source.fileno())) != expected:
            raise RuntimeError('disk image identity changed before checksum')
        digest = hashlib.file_digest(source, 'sha256').hexdigest()
        check(path, expected)
        sidecar.write_text(digest + '  ' + path.name + '\n')


if __name__ == '__main__':
    action, filename, ledger, *args = sys.argv[1:]
    path, record = Path(filename), Path(ledger)
    if action == 'create':
        sys.exit(create(path, record, Path(args[0]), args[1:]))
    if action == 'finish':
        finish(Path(args[0]))
        sys.exit(0)
    expected = json.loads(record.read_text())
    check(path, expected)
    if action == 'remove':
        # Check immediately before invoking unlink; no pathname-only permission.
        path.unlink()
        sys.exit(0)
    if action == 'approve':
        approve(Path(args[0]), path)
    if action == 'checksum':
        checksum(path, record, Path(args[0]))
