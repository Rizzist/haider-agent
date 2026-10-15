"""Stable backing-file identities for the macOS installer and its test tracer."""
import hashlib
from contextlib import ExitStack
import json
import os
import plistlib
import re
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import tempfile
import time


def identity(stat):
    return [stat.st_dev, stat.st_ino]


def file_digest(source, algorithm):
    """Hash an open descriptor on both stock macOS Python 3.9 and newer Python."""
    native = getattr(hashlib, 'file_digest', None)
    if native is not None:
        return native(source, algorithm)
    digest = hashlib.new(algorithm)
    for chunk in iter(lambda: source.read(1024 * 1024), b''):
        digest.update(chunk)
    return digest


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


def expected_identity(record):
    saved = json.loads(record.read_text())
    return saved['identity'] if isinstance(saved, dict) else saved


def create(path, record, staging, arguments):
    # Reserve the publication inode ourselves. Native hdiutil replaces even an
    # existing -ov destination, so it writes into a private staging directory.
    with path.open('xb') as output, ExitStack() as descriptors:
        expected = identity(os.fstat(output.fileno()))
        record.write_text(json.dumps(expected))
        withdrawal = record.with_suffix('.withdraw.json')
        withdrawal.write_text(json.dumps({'identity': expected, 'sha256': hashlib.sha256(b'').hexdigest()}))
        raw = Path(tempfile.mkdtemp(prefix='create.', dir=staging)) / 'image.dmg'
        # Bind the source directory before the tool runs. A later directory
        # replacement must not redirect the copy to another caller's files.
        stage_fd = os.open(raw.parent, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        descriptors.callback(os.close, stage_fd)
        # Arm retention before invoking the tool, including interruption before
        # it returns. No failed or interrupted create grants pathname ownership.
        retained_before = (staging / 'retain-create').exists()
        with (staging / 'retain-create').open('a') as retained:
            retained.write(str(raw) + '\n')
        result = subprocess.run(['hdiutil', 'create', *arguments, str(raw)])
        if result.returncode:
            # A failed tool grants no attachment ownership. Record a regular
            # partial for the guarded cleanup callback; attachments, symlinks
            # and changed generations must be retained instead of retried.
            if raw.exists() or raw.is_symlink():
                if not raw.is_symlink() and stat.S_ISREG(raw.stat().st_mode):
                    (staging / 'staged-image.json').write_text(json.dumps({
                        'path': str(raw), 'identity': identity(raw.stat()),
                        'directory': identity(os.fstat(stage_fd))}))
                print('Create output requires identity-checked cleanup: ' + str(raw), file=sys.stderr)
            elif not retained_before:
                (staging / 'retain-create').unlink()
            return result.returncode
        # Native output is untrusted input, not deletion authority. Refuse
        # symlinks and special files without reading through either of them.
        flags = os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK
        source_fd = os.open(raw.name, flags, dir_fd=stage_fd)
        with os.fdopen(source_fd, 'rb') as source:
            if not stat.S_ISREG(os.fstat(source.fileno()).st_mode):
                raise RuntimeError('native create output is not a regular file; retaining ' + str(raw))
            raw_identity = identity(os.fstat(source.fileno()))
            withdrawal.write_text(json.dumps({'identity': expected, 'copying': True}))
            shutil.copyfileobj(source, output)
            source.seek(0)
            digest = file_digest(source, 'sha256').hexdigest()
        output.flush()
        check(path, expected)
        withdrawal.write_text(json.dumps({'identity': expected, 'sha256': digest}))
        # Record the staging generation for identity-checked, unattached
        # cleanup. This record grants no authority to eject any attachment;
        # only the reserved publication inode is used for our attach.
        (staging / 'staged-image.json').write_text(json.dumps({
            'path': str(raw), 'identity': raw_identity,
            'directory': identity(os.fstat(stage_fd))}))
    return 0


def staging_inventory():
    for attempt in range(4):
        inventory = subprocess.run(['hdiutil', 'info', '-plist'], capture_output=True, check=True)
        if b'cannot get IO media array' not in inventory.stderr:
            return plistlib.loads(inventory.stdout).get('images', [])
        if attempt == 3:
            raise RuntimeError('incomplete create-output inventory; retaining staging')
        time.sleep(.1 * (attempt + 1))


def open_image_users(path, descriptor):
    """Cross-check inventory against readers of the pinned vnode.

    A warning-free hdiutil plist can silently omit an attachment. lsof selects
    by file identity (including case/Unicode aliases); our held descriptor is
    the positive control that the query actually accounted for this file.
    Unknown readers are uncertainty, never permission to delete a backing file.
    """
    result = subprocess.run(['/usr/sbin/lsof', '-nP', '-F0pfDti', '--', str(path)],
                            capture_output=True, timeout=60)
    if result.returncode or result.stderr:
        raise RuntimeError('incomplete backing-file inventory; retaining ' + str(path))
    expected = identity(os.fstat(descriptor))
    pid = None
    current = {}
    records = []
    for field in result.stdout.split(b'\0'):
        field = field.lstrip(b'\n')
        if not field:
            continue
        key, value = chr(field[0]), os.fsdecode(field[1:])
        if key in ('p', 'f'):
            records.append((pid, current))
            current = {}
        if key == 'p':
            pid = int(value)
        current[key] = value
    records.append((pid, current))
    users = {pid for pid, item in records if item.get('f', '')[:1].isdigit()
             and item.get('t') == 'REG' and 'D' in item and 'i' in item
             and [int(item['D'], 16), int(item['i'])] == expected}
    if os.getpid() not in users:
        raise RuntimeError('pinned image absent from backing-file inventory; retaining ' + str(path))
    return users - {os.getpid(), os.getppid()}


def require_unattached(path, expected, history):
    """Refuse deletion of any attached or incompletely inventoried generation.

    Native staging attachments are never ours to eject. Observations survive
    later cleanup calls; an omitted live device cannot erase a prior refusal.
    """
    check(path, expected)
    flags = os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK
    with os.fdopen(os.open(path, flags), 'rb') as source:
        if identity(os.fstat(source.fileno())) != expected:
            raise RuntimeError('image identity changed before attachment audit')
        images = staging_inventory()
        devices = set()
        inventoried = set()
        for image in images:
            current = {e['dev-entry'] for e in image.get('system-entities', [])
                       if re.fullmatch(r'/dev/disk[0-9]+', e.get('dev-entry', ''))}
            inventoried.update(current)
            try:
                same_file = identity(Path(image['image-path']).stat()) == expected
            except (OSError, KeyError):
                same_file = False
            if same_file:
                devices.update(current)
        seen = set(json.loads(history.read_text())) if history.exists() else set()
        seen.update(devices)
        history.write_text(json.dumps(sorted(seen)))
        if devices:
            raise RuntimeError('unattributed create output attached as ' + ', '.join(sorted(devices))
                               + '; retaining ' + str(path))
        if any(Path(device).exists() and device not in inventoried for device in seen):
            raise RuntimeError('incomplete previously observed image inventory; retaining ' + str(path))
        # Even an always-omitted attachment still has its backing vnode open.
        if open_image_users(path, source.fileno()):
            raise RuntimeError('incomplete image inventory: backing file still open; retaining ' + str(path))
        check(path, expected)


def finish(staging):
    saved = json.loads((staging / 'staged-image.json').read_text())
    raw = Path(saved['path'])
    check(raw, saved['identity'])
    # A successful create can also return an independently replaced image.
    # Matching bytes or a post-create inode is no authority to eject it.
    require_unattached(raw, saved['identity'], staging / 'seen-create-devices.json')


def clean_staging(staging):
    """Remove only the inventoried, identity-checked native staging entry.

    A returned staging file grants no attachment ownership. finish() refuses
    any attachment before this unlink; a changed file or directory is retained.
    """
    saved = json.loads((staging / 'staged-image.json').read_text())
    raw = Path(saved['path'])
    with ExitStack() as descriptors:
        directory_fd = os.open(raw.parent, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        descriptors.callback(os.close, directory_fd)
        if identity(os.fstat(directory_fd)) != saved['directory']:
            raise RuntimeError('staging directory identity changed; retaining ' + str(raw))
        finish(staging)
        if identity(os.stat(raw.name, dir_fd=directory_fd, follow_symlinks=False)) != saved['identity']:
            raise RuntimeError('staging image identity changed; retaining ' + str(raw))
        os.unlink(raw.name, dir_fd=directory_fd)
    raw.parent.rmdir()
    (staging / 'staged-image.json').unlink()
    (staging / 'retain-create').unlink(missing_ok=True)


def package(path, record, source_path, staging, payload, version, target, verifier):
    """Validate, embed and publish one pinned package generation."""
    with source_path.open('rb') as source, path.open('xb') as output:
        expected = identity(os.fstat(output.fileno()))
        record.write_text(json.dumps({'identity': expected, 'copying': True}))
        digest = file_digest(source, 'sha256').digest()
        source.seek(0)
        shutil.copyfileobj(source, output)
        output.flush()
        record.write_text(json.dumps({'identity': expected, 'sha256': digest.hex()}))
        # Snapshot the private, signature-checked source, never the publication.
        # Both copies must still match the bytes read from that descriptor.
        source.seek(0)
        snapshot = staging / 'validated.pkg'
        with snapshot.open('xb') as validated:
            shutil.copyfileobj(source, validated)
    # pkgutil does not expand /dev/fd inputs correctly on macOS. Give it a
    # private snapshot copied from the checked descriptor, pin that snapshot,
    # and check its identity and bytes across expansion. Embed from the same
    # snapshot descriptor; never reopen the mutable publication for its bytes.
    with path.open('rb') as source:
        if identity(os.fstat(source.fileno())) != expected:
            raise RuntimeError('package identity changed before validation')
        with snapshot.open('rb') as validated:
            snapshot_identity = identity(os.fstat(validated.fileno()))
            if file_digest(validated, 'sha256').digest() != digest:
                raise RuntimeError('private package bytes changed while copying')
            subprocess.run(['pkgutil', '--expand-full', str(snapshot),
                            str(staging / 'expanded')], check=True)
            check(snapshot, snapshot_identity)
            validated.seek(0)
            if file_digest(validated, 'sha256').digest() != digest:
                raise RuntimeError('validated package bytes changed')
            subprocess.run([sys.executable, str(verifier), str(staging / 'expanded'),
                            str(payload), version, target], check=True)
            check(path, expected)
            validated.seek(0)
            with (staging / 'image' / path.name).open('xb') as embedded:
                shutil.copyfileobj(validated, embedded)
            check(snapshot, snapshot_identity)
        check(path, expected)
        source.seek(0)
        if file_digest(source, 'sha256').digest() != digest:
            raise RuntimeError('published package bytes changed since validation')
        record.write_text(json.dumps({'identity': expected, 'sha256': digest.hex()}))


def seal(path, record, descriptor):
    """Remember the pinned DMG bytes after stapling and before validation."""
    expected = expected_identity(record)
    with descriptor.open('rb') as source:
        if identity(os.fstat(source.fileno())) != expected:
            raise RuntimeError('disk image identity changed before validation')
        source.seek(0)
        digest = file_digest(source, 'sha256').hexdigest()
        check(path, expected)
        record.write_text(json.dumps({'identity': expected, 'sha256': digest}))


def withdraw(path, record):
    """Withdraw only this failed run's unchanged package and its own sidecar."""
    saved = json.loads(record.read_text())
    sidecar = saved.get('sidecar')
    if sidecar:
        target = Path(sidecar['path'])
        check(target, sidecar['identity'])
        target.unlink()
    check(path, saved['identity'])
    with path.open('rb') as source:
        if identity(os.fstat(source.fileno())) != saved['identity']:
            raise RuntimeError('package identity changed before withdrawal')
        if not saved.get('copying') and file_digest(source, 'sha256').hexdigest() != saved['sha256']:
            raise RuntimeError('package bytes changed; retaining ' + str(path))
        check(path, saved['identity'])
        path.unlink()


def compare(path, record, embedded):
    expected = expected_identity(record)
    with path.open('rb') as source:
        if identity(os.fstat(source.fileno())) != expected:
            raise RuntimeError('package identity changed before comparison')
        # cmp receives the validated package descriptor, not a reopened path.
        result = subprocess.run(['cmp', '/dev/fd/' + str(source.fileno()), str(embedded)],
                                pass_fds=(source.fileno(),))
        if result.returncode:
            return result.returncode
        check(path, expected)
    return 0


def checksum(path, record, sidecar):
    expected = expected_identity(record)
    saved = json.loads(record.read_text())
    with path.open('rb') as source:
        if identity(os.fstat(source.fileno())) != expected:
            raise RuntimeError('disk image identity changed before checksum')
        digest = file_digest(source, 'sha256').hexdigest()
        if isinstance(saved, dict) and digest != saved['sha256']:
            kind = 'package' if path.suffix == '.pkg' else 'disk image'
            raise RuntimeError('published ' + kind + ' bytes changed since validation')
        check(path, expected)
        flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW
        with os.fdopen(os.open(sidecar, flags, 0o644), 'w') as output:
            if isinstance(saved, dict):
                saved['sidecar'] = {'path': str(sidecar), 'identity': identity(os.fstat(output.fileno()))}
                record.write_text(json.dumps(saved))
            output.write(digest + '  ' + path.name + '\n')
            output.flush()
            check(sidecar, identity(os.fstat(output.fileno())))
        check(path, expected)


if __name__ == '__main__':
    action, filename, ledger, *args = sys.argv[1:]
    path, record = Path(filename), Path(ledger)
    if action == 'create':
        sys.exit(create(path, record, Path(args[0]), args[1:]))
    if action == 'finish':
        finish(Path(args[0]))
        sys.exit(0)
    if action == 'clean':
        clean_staging(Path(args[0]))
        sys.exit(0)
    if action == 'package':
        package(path, record, Path(args[0]), Path(args[1]), Path(args[2]),
                args[3], args[4], Path(args[5]))
        sys.exit(0)
    if action == 'seal':
        seal(path, record, Path(args[0]))
        sys.exit(0)
    if action == 'withdraw':
        withdraw(path, record)
        sys.exit(0)
    if action == 'withdraw-image':
        if path.exists() or path.is_symlink():
            require_unattached(path, expected_identity(record), record.with_suffix('.devices.json'))
            saved = json.loads(record.read_text())
            withdraw(path, record if isinstance(saved, dict) else record.with_suffix('.withdraw.json'))
        sys.exit(0)
    expected = expected_identity(record)
    check(path, expected)
    if action == 'remove':
        # Check immediately before invoking unlink; no pathname-only permission.
        require_unattached(path, expected, record.with_suffix('.devices.json'))
        path.unlink()
        sys.exit(0)
    if action == 'checksum':
        checksum(path, record, Path(args[0]))
    if action == 'compare':
        sys.exit(compare(path, record, Path(args[0])))
