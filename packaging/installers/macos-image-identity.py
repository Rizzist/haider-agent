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


def helper_file_sets(output):
    """Split a native -F0 listing into sets; None when any set is uncertain.

    NUL ends a field and a newline right after it ends the set, so each file
    set is judged on its own: fields of different records can never pair. A
    regular file on a numeric descriptor must carry its own D and i. Other
    descriptors (cwd, txt, pipes, sockets) are not backing files and are
    skipped, but a set without its leading f has lost its boundary.
    """
    fields = output.split(b'\0')
    if fields.pop() not in (b'', b'\n'):
        return None
    sets = []
    for field in fields:
        if not sets or field[:1] == b'\n':
            field = field[1:] if field[:1] == b'\n' else field
            sets.append({})
        key, value = field[:1], field[1:]
        if not key or key in sets[-1]:
            return None
        sets[-1][key] = value
    records = []
    for item in sets:
        if list(item) == [b'p']:
            continue
        if list(item)[:1] != [b'f']:
            return None
        if not re.fullmatch(rb'[0-9]+', item[b'f']):
            continue
        if b't' not in item:
            return None
        if item[b't'] != b'REG':
            continue
        device, inode = item.get(b'D', b''), item.get(b'i', b'')
        if (not re.fullmatch(rb'(?:0[xX])?[0-9a-fA-F]+', device) or
                not re.fullmatch(rb'[0-9]+', inode)):
            return None
        name = os.fsdecode(item[b'n']) if b'n' in item else None
        records.append((name, [int(device, 16), int(inode)]))
    return records


def backing_identity(image, expected):
    """Inspect the helper's open vnode, never stat its mutable backing pathname.

    Native attach plists have only system-entities on supported macOS versions.
    hdiutil info supplies the helper PID; lsof supplies the actual open inode.
    Missing helper/descriptor information grants no ownership.
    """
    pid = image.get('hdid-pid')
    if not isinstance(pid, int) or pid <= 0:
        return None
    try:
        result = subprocess.run(['/usr/sbin/lsof', '-a', '-p', str(pid), '-nP', '-F0fDtin'],
                                capture_output=True, timeout=60)
    except (OSError, subprocess.TimeoutExpired):
        return None
    if result.returncode or result.stderr:
        return None
    records = helper_file_sets(result.stdout)
    if records is None:
        return None
    confirmed_different = False
    for name, observed in records:
        if observed == expected:
            return True
        # A missing descriptor is uncertainty, not evidence that the attachment
        # disappeared. A confirmed different backing vnode can be disowned.
        if name and image.get('image-path'):
            name = name.removesuffix(' (deleted)')
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


class InventoryError(RuntimeError):
    def __init__(self, message, retryable=False):
        super().__init__(message + '; retaining image output')
        self.retryable = retryable


def inventory_snapshot():
    """One bounded query; a failed or malformed inventory grants no ownership.

    Busy statuses/diagnostics and the known incomplete-media warning may be
    retried four times by the caller. Other failures stop cleanup immediately.
    """
    try:
        result = subprocess.run(['hdiutil', 'info', '-plist'], capture_output=True, timeout=60)
    except (OSError, subprocess.TimeoutExpired) as error:
        raise InventoryError('hdiutil attachment inventory unavailable: ' + str(error)) from None
    busy = result.returncode == 16 or re.search(
        rb'resource busy|device busy|resource temporarily unavailable|\bebusy\b',
        result.stderr, re.IGNORECASE)
    if result.returncode or busy:
        detail = os.fsdecode(result.stderr).strip()
        raise InventoryError('hdiutil attachment inventory failed (exit %s): %s' %
                             (result.returncode, detail), retryable=bool(busy))
    if b'cannot get IO media array' in result.stderr:
        raise InventoryError('hdiutil attachment inventory remained incomplete', retryable=True)
    if result.stderr:
        raise InventoryError('hdiutil attachment inventory warning: ' + os.fsdecode(result.stderr).strip())
    try:
        info = plistlib.loads(result.stdout)
        images = info['images']
        if not isinstance(images, list) or any(
                not isinstance(image, dict) or
                not isinstance(image.get('image-path'), str) or
                ('hdid-pid' in image and type(image['hdid-pid']) is not int) or
                not isinstance(image.get('system-entities'), list) or
                any(not isinstance(entity, dict) or any(
                    key in entity and not isinstance(entity[key], str)
                    for key in ('dev-entry', 'mount-point'))
                    for entity in image.get('system-entities', []))
                for image in images):
            raise ValueError('invalid image entries')
    except Exception:
        # plistlib also raises AttributeError for malformed dates, and its
        # decoder errors vary between stock macOS Python and newer versions.
        # No undecodable inventory grants permission to remove an image.
        raise InventoryError('invalid hdiutil attachment inventory') from None
    return images


def staging_inventory():
    for attempt in range(4):
        try:
            return inventory_snapshot()
        except InventoryError as error:
            if not error.retryable or attempt == 3:
                raise
            time.sleep(.1 * (attempt + 1))


def image_reader_records(output):
    """Validate every lsof process/file set before trusting any reader PID.

    In -F0 output, NUL terminates fields and newline terminates sets. A process
    set owns subsequent file sets until the next process set, so a lost process
    set silently credits its files to the previous process: callers must verify
    descriptors, never trust a PID. Compact listings
    must repeat p for each file: without set boundaries, inheriting a PID could
    hide a missing process header. Neither form may end in a partial set.
    """
    if b'\n' in output:
        sets = output.split(b'\n')
        if sets.pop() != b'':
            raise ValueError('unterminated reader set')
        sets = [item.split(b'\0') for item in sets]
    else:
        fields = output.split(b'\0')
        if fields.pop() != b'':
            raise ValueError('unterminated reader field')
        sets = []
        while fields:
            record, fields = fields[:5], fields[5:]
            if [field[:1] for field in record] != [b'p', b'f', b't', b'D', b'i']:
                raise ValueError('incomplete compact reader record')
            sets.extend([[record[0], b''], [*record[1:], b'']])
    pid = None
    records = []
    processes, descriptors = set(), set()
    has_file = False
    for fields in sets:
        if fields.pop() != b'':
            raise ValueError('unterminated reader field')
        keys = [field[:1] for field in fields]
        if keys == [b'p']:
            if pid is not None and not has_file:
                raise ValueError('process without reader records')
            value = fields[0][1:]
            if not re.fullmatch(rb'[0-9]+', value) or int(value) <= 0:
                raise ValueError('invalid reader PID')
            pid = int(value)
            if pid in processes:
                raise ValueError('duplicate reader process')
            processes.add(pid)
            has_file = False
        elif keys == [b'f', b't', b'D', b'i'] and pid is not None:
            fd, kind, device, inode = [field[1:] for field in fields]
            if (not re.fullmatch(rb'[0-9]+', fd) or kind != b'REG' or
                    not re.fullmatch(rb'(?:0[xX])?[0-9a-fA-F]+', device) or
                    not re.fullmatch(rb'[0-9]+', inode)):
                raise ValueError('invalid reader file fields')
            descriptor = (pid, int(fd))
            if descriptor in descriptors:
                raise ValueError('duplicate reader descriptor')
            descriptors.add(descriptor)
            records.append((pid, int(fd), [int(device, 16), int(inode)]))
            has_file = True
        else:
            raise ValueError('incomplete or out-of-order reader set')
    if not has_file:
        raise ValueError('missing reader file set')
    return records


def declared_parent_descriptors():
    """Descriptors the parent declares it holds on the image (builder: fd 9)."""
    value = os.environ.get('HAIDER_IMAGE_PARENT_FDS', '')
    if not value:
        return set()
    if not re.fullmatch(r'[0-9]+(?:,[0-9]+)*', value):
        raise ValueError('invalid declared parent descriptors')
    return {int(fd) for fd in value.split(',')}


def holds_image(fd, expected):
    """Confirm in-process that a descriptor is open on the pinned vnode."""
    try:
        return identity(os.fstat(fd)) == expected
    except (OSError, OverflowError):
        return False


def inherited_parent_descriptors():
    """Capture declared handles before an action can reuse a closed number."""
    try:
        declared = declared_parent_descriptors()
    except ValueError as error:
        raise RuntimeError('invalid declared parent descriptors; retaining image output') from error
    inherited = {}
    for fd in declared:
        try:
            inherited[fd] = identity(os.fstat(fd))
        except (OSError, OverflowError):
            pass
    return inherited


def image_users_snapshot(path, descriptor, inherited=None):
    """Cross-check inventory against readers of the pinned vnode.

    A warning-free hdiutil plist can silently omit an attachment. lsof selects
    by file identity (including case/Unicode aliases); our held descriptor is
    the positive control that the query actually accounted for this file.
    Unknown readers are uncertainty, never permission to delete a backing file.
    """
    try:
        result = subprocess.run(['/usr/sbin/lsof', '-nP', '-F0pfDti', '--', str(path)],
                                capture_output=True, timeout=60)
    except (OSError, subprocess.TimeoutExpired) as error:
        raise RuntimeError('backing-file inventory unavailable; retaining ' + str(path)) from error
    if result.returncode or result.stderr:
        raise RuntimeError('incomplete backing-file inventory; retaining ' + str(path))
    expected = identity(os.fstat(descriptor))
    try:
        records = image_reader_records(result.stdout)
    except ValueError as error:
        raise RuntimeError('incomplete backing-file inventory; retaining ' + str(path)) from error
    # A reader whose process set was lost is credited to the preceding
    # process, so nothing is excluded by PID. Only descriptors verified to be
    # ours, or the parent's declared descriptors confirmed through the
    # inherited copy, are excluded; every other record is a reader.
    own, parent = os.getpid(), os.getppid()
    confirmed = {fd for fd, entry_identity in (inherited or {}).items()
                 if entry_identity == expected and holds_image(fd, expected)}
    users = set()
    for pid, fd, observed in records:
        if observed != expected:
            raise RuntimeError('unattributed backing-file record; retaining ' + str(path))
        if pid == own:
            if not holds_image(fd, expected):
                raise RuntimeError('unverified own backing-file descriptor; retaining ' + str(path))
        elif pid != parent or fd not in confirmed:
            users.add(pid)
    if (own, descriptor) not in {(pid, fd) for pid, fd, observed in records}:
        raise RuntimeError('pinned image absent from backing-file inventory; retaining ' + str(path))
    return users


def open_image_users(path, descriptor, inherited=None):
    for attempt in range(4):
        try:
            return image_users_snapshot(path, descriptor, inherited)
        except RuntimeError:
            if attempt == 3:
                raise
            time.sleep(.1 * (attempt + 1))



def helper_has_exited(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return True
    except PermissionError:
        pass
    return False


def require_unattached(path, expected, history, inherited=None):
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
        helpers_record = history.with_suffix('.helpers.json')
        helpers = json.loads(helpers_record.read_text()) if helpers_record.exists() else {}
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
                pid = image.get('hdid-pid')
                if isinstance(pid, int) and pid > 0:
                    for device in current:
                        helpers[device] = sorted(set(helpers.get(device, [])) | {pid})
        seen = set(json.loads(history.read_text())) if history.exists() else set()
        seen.update(devices)
        history.write_text(json.dumps(sorted(seen)))
        helpers_record.write_text(json.dumps(helpers))
        if devices:
            raise RuntimeError('unattributed create output attached as ' + ', '.join(sorted(devices))
                               + '; retaining ' + str(path))
        # A remembered name can be recycled into a non-image device. Only a
        # departed recorded helper AND a clear pinned-vnode audit below can
        # establish that the old attachment ended; absence from info cannot.
        missing = {device for device in seen if Path(device).exists() and device not in inventoried}
        if any(not helpers.get(device) for device in missing):
            raise RuntimeError('incomplete previously observed image inventory; retaining ' + str(path))
        for attempt in range(4):
            if all(helper_has_exited(pid) for device in missing for pid in helpers[device]):
                break
            if attempt == 3:
                raise RuntimeError('incomplete previously observed image inventory; retaining ' + str(path))
            time.sleep(.1 * (attempt + 1))
        # Even an always-omitted attachment still has its backing vnode open.
        if open_image_users(path, source.fileno(), inherited):
            raise RuntimeError('incomplete image inventory: backing file still open; retaining ' + str(path))
        check(path, expected)


def finish(staging, inherited=None):
    saved = json.loads((staging / 'staged-image.json').read_text())
    raw = Path(saved['path'])
    check(raw, saved['identity'])
    # A successful create can also return an independently replaced image.
    # Matching bytes or a post-create inode is no authority to eject it.
    require_unattached(raw, saved['identity'], staging / 'seen-create-devices.json', inherited)


def clean_staging(staging, inherited=None):
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
        finish(staging, inherited)
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


def main():
    inherited = inherited_parent_descriptors()
    action, filename, ledger, *args = sys.argv[1:]
    path, record = Path(filename), Path(ledger)
    if action == 'create':
        sys.exit(create(path, record, Path(args[0]), args[1:]))
    if action == 'finish':
        finish(Path(args[0]), inherited)
        sys.exit(0)
    if action == 'clean':
        clean_staging(Path(args[0]), inherited)
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
            require_unattached(path, expected_identity(record), record.with_suffix('.devices.json'), inherited)
            saved = json.loads(record.read_text())
            withdraw(path, record if isinstance(saved, dict) else record.with_suffix('.withdraw.json'))
        sys.exit(0)
    expected = expected_identity(record)
    check(path, expected)
    if action == 'remove':
        # Check immediately before invoking unlink; no pathname-only permission.
        require_unattached(path, expected, record.with_suffix('.devices.json'), inherited)
        path.unlink()
        sys.exit(0)
    if action == 'checksum':
        checksum(path, record, Path(args[0]))
    if action == 'compare':
        sys.exit(compare(path, record, Path(args[0])))


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        print(str(error) + ('' if 'retaining' in str(error) else '; retaining image output'),
              file=sys.stderr)
        sys.exit(1)
