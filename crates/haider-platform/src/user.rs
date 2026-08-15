use std::path::Path;

#[cfg(unix)]
#[must_use]
pub fn effective_user_id() -> u32 {
    rustix::process::geteuid().as_raw()
}

#[cfg(windows)]
#[must_use]
pub fn effective_user_id() -> u32 {
    0
}

#[cfg(unix)]
#[must_use]
pub fn is_owner_private_directory(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            metadata.is_dir()
                && metadata.uid() == effective_user_id()
                && metadata.mode() & 0o077 == 0
        }
        Err(_) => false,
    }
}

#[cfg(windows)]
#[must_use]
pub fn is_owner_private_directory(path: &Path) -> bool {
    path.is_dir()
}
