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
