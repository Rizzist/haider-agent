use std::fs::{DirBuilder, Metadata, OpenOptions};
use std::path::Path;

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

#[cfg(unix)]
pub fn sync_directory(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path).and_then(|directory| directory.sync_all())
}

#[cfg(windows)]
pub fn sync_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
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
