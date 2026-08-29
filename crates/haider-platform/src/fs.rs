#[cfg(test)]
#[path = "fs_tests.rs"]
mod tests;

use std::fs::{DirBuilder, File, Metadata, OpenOptions};
use std::path::Path;

/// Durability strength for an explicit filesystem synchronization boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncPolicy {
    /// Flushes through volatile device caches on Apple and uses `fsync`
    /// semantics on other platforms.
    Full,
    /// Orders prior writes at the device on Apple and uses `fsync` semantics
    /// where `F_BARRIERFSYNC` is unavailable.
    Barrier,
    /// Uses plain `fsync` without Apple's whole-device cache flush.
    Plain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncOperation {
    #[cfg(target_vendor = "apple")]
    FullFsync,
    #[cfg(target_vendor = "apple")]
    BarrierFsync,
    #[cfg(unix)]
    Fsync,
    #[cfg(windows)]
    SyncAll,
    #[cfg(all(windows, test))]
    Noop,
}

#[cfg(test)]
type SyncTestHook = Box<dyn FnMut(SyncOperation)>;

#[cfg(test)]
std::thread_local! {
    static SYNC_TEST_HOOK: std::cell::RefCell<Option<SyncTestHook>> = const {
        std::cell::RefCell::new(None)
    };
}

#[cfg(test)]
fn intercept_sync_for_test(operation: SyncOperation) -> bool {
    SYNC_TEST_HOOK.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(hook) = slot.as_mut() else {
            return false;
        };
        hook(operation);
        true
    })
}

#[cfg(test)]
fn with_sync_test_hook<T>(
    hook: impl FnMut(SyncOperation) + 'static,
    action: impl FnOnce() -> T,
) -> T {
    let previous = SYNC_TEST_HOOK.with(|slot| slot.replace(Some(Box::new(hook))));
    let result = action();
    SYNC_TEST_HOOK.with(|slot| {
        slot.replace(previous);
    });
    result
}

#[cfg(unix)]
pub fn configure_file_mode(options: &mut OpenOptions, mode: u32) {
    use std::os::unix::fs::OpenOptionsExt as _;
    options.mode(mode);
}

#[cfg(windows)]
pub fn configure_file_mode(_options: &mut OpenOptions, _mode: u32) {}

#[cfg(unix)]
pub fn configure_directory_mode(builder: &mut DirBuilder, mode: u32) {
    use std::os::unix::fs::DirBuilderExt as _;
    builder.mode(mode);
}

#[cfg(windows)]
pub fn configure_directory_mode(_builder: &mut DirBuilder, _mode: u32) {}

#[cfg(unix)]
pub fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(windows)]
pub fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_readonly(mode & 0o200 == 0);
    std::fs::set_permissions(path, permissions)
}

#[cfg(unix)]
#[must_use]
pub fn metadata_mode(metadata: &Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt as _;
    metadata.mode()
}

#[cfg(windows)]
#[must_use]
pub fn metadata_mode(metadata: &Metadata) -> u32 {
    if metadata.permissions().readonly() {
        0o500
    } else {
        0o700
    }
}

#[cfg(unix)]
#[must_use]
pub fn metadata_is_current_user(metadata: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    metadata.uid() == crate::effective_user_id()
}

#[cfg(windows)]
#[must_use]
pub fn metadata_is_current_user(_metadata: &Metadata) -> bool {
    // Windows ownership is enforced through ACLs rather than a numeric UID.
    true
}

#[cfg(unix)]
#[must_use]
pub fn metadata_link_count(metadata: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    metadata.nlink()
}

#[cfg(windows)]
#[must_use]
pub fn metadata_link_count(_metadata: &Metadata) -> u64 {
    // Best effort until the Windows updater uses a handle-based link query.
    1
}

/// Synchronizes an open file according to `policy`.
pub fn sync_file(file: &File, policy: SyncPolicy) -> std::io::Result<()> {
    let operation = sync_operation(policy);
    #[cfg(test)]
    if intercept_sync_for_test(operation) {
        return Ok(());
    }
    execute_file_sync(file, operation)
}

/// Synchronizes directory-entry mutations according to `policy`.
#[cfg(unix)]
pub fn sync_directory(path: &Path, policy: SyncPolicy) -> std::io::Result<()> {
    File::open(path).and_then(|directory| sync_file(&directory, policy))
}

/// Windows has no portable directory flush handle in this seam. File syncs
/// still use `File::sync_all`; directory synchronization remains a safe no-op.
#[cfg(windows)]
pub fn sync_directory(_path: &Path, _policy: SyncPolicy) -> std::io::Result<()> {
    #[cfg(test)]
    if intercept_sync_for_test(SyncOperation::Noop) {
        return Ok(());
    }
    Ok(())
}

#[cfg(target_vendor = "apple")]
fn sync_operation(policy: SyncPolicy) -> SyncOperation {
    match policy {
        SyncPolicy::Full => SyncOperation::FullFsync,
        SyncPolicy::Barrier => SyncOperation::BarrierFsync,
        SyncPolicy::Plain => SyncOperation::Fsync,
    }
}

#[cfg(all(unix, not(target_vendor = "apple")))]
fn sync_operation(_policy: SyncPolicy) -> SyncOperation {
    SyncOperation::Fsync
}

#[cfg(windows)]
fn sync_operation(_policy: SyncPolicy) -> SyncOperation {
    SyncOperation::SyncAll
}

#[cfg(target_vendor = "apple")]
#[allow(unsafe_code)]
fn execute_file_sync(file: &File, operation: SyncOperation) -> std::io::Result<()> {
    use std::os::fd::AsRawFd as _;

    let result = match operation {
        // SAFETY: `file` owns the live descriptor for the duration of the call;
        // F_FULLFSYNC takes no pointer arguments and does not transfer ownership.
        SyncOperation::FullFsync => unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) },
        // SAFETY: `file` owns the live descriptor for the duration of the call;
        // F_BARRIERFSYNC takes no pointer arguments and does not transfer ownership.
        SyncOperation::BarrierFsync => unsafe {
            libc::fcntl(file.as_raw_fd(), libc::F_BARRIERFSYNC)
        },
        SyncOperation::Fsync => return plain_fsync(file),
    };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if operation == SyncOperation::BarrierFsync && barrier_is_unsupported(&error) {
        return plain_fsync(file);
    }
    Err(error)
}

#[cfg(target_vendor = "apple")]
fn barrier_is_unsupported(error: &std::io::Error) -> bool {
    error
        .raw_os_error()
        .is_some_and(|code| code == libc::EINVAL || code == libc::ENOTSUP)
}

#[cfg(all(unix, not(target_vendor = "apple")))]
fn execute_file_sync(file: &File, _operation: SyncOperation) -> std::io::Result<()> {
    plain_fsync(file)
}

#[cfg(unix)]
fn plain_fsync(file: &File) -> std::io::Result<()> {
    rustix::fs::fsync(file).map_err(std::io::Error::from)
}

#[cfg(windows)]
fn execute_file_sync(file: &File, _operation: SyncOperation) -> std::io::Result<()> {
    file.sync_all()
}

/// Atomically publishes `source` at `target`, replacing an existing target.
///
/// Unix `rename(2)` already has replacement semantics. Windows'
/// `std::fs::rename` does not replace an existing file, so use `ReplaceFileW`:
/// unlike `MoveFileExW`, it merges the replaced file's ACLs, attributes,
/// encryption/compression state, and named streams into the staged file.
#[cfg(unix)]
pub fn replace_file(source: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::rename(source, target)
}

#[cfg(windows)]
#[allow(unsafe_code)]
pub fn replace_file(source: &Path, target: &Path) -> std::io::Result<()> {
    replace_file_impl(source, target, None)
}

/// Windows replacement with an explicit same-volume recovery name. When
/// `ReplaceFileW` reports one of its documented partial failure states, the
/// replaced file remains available at `backup` for caller reconciliation.
#[cfg(windows)]
pub fn replace_file_with_backup(
    source: &Path,
    target: &Path,
    backup: &Path,
) -> std::io::Result<()> {
    replace_file_impl(source, target, Some(backup))
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn replace_file_impl(source: &Path, target: &Path, backup: Option<&Path>) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND};
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW, ReplaceFileW,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain([0])
        .collect::<Vec<_>>();
    let target = target
        .as_os_str()
        .encode_wide()
        .chain([0])
        .collect::<Vec<_>>();
    let backup = backup.map(|backup| {
        backup
            .as_os_str()
            .encode_wide()
            .chain([0])
            .collect::<Vec<_>>()
    });
    // SAFETY: all UTF-16 buffers are NUL-terminated and live for the call;
    // optional backup is either null or another live NUL-terminated buffer.
    let replaced = unsafe {
        ReplaceFileW(
            target.as_ptr(),
            source.as_ptr(),
            backup
                .as_ref()
                .map_or(std::ptr::null(), |backup| backup.as_ptr()),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if replaced != 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    // `ReplaceFileW` demands an existing target, so first-time publishes
    // (vault secrets, fresh registries) reach here with FILE/PATH_NOT_FOUND.
    // `MoveFileExW` covers creation and, via REPLACE_EXISTING, the race
    // where the target appears between the two calls.
    if !matches!(
        error.raw_os_error(),
        Some(code) if code == ERROR_FILE_NOT_FOUND as i32 || code == ERROR_PATH_NOT_FOUND as i32
    ) {
        return Err(error);
    }
    // SAFETY: source and target are live NUL-terminated UTF-16 buffers, and
    // MoveFileExW neither retains their pointers nor transfers ownership.
    let moved = unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}
