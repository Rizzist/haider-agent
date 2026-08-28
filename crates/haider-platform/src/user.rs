use std::path::Path;
use std::path::PathBuf;

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

/// Places a per-profile runtime below an owner-only UID directory when its
/// requested parent is a foreign-owned shared sticky directory such as
/// `/tmp`. Other foreign-owned parents remain unchanged so the endpoint
/// preparation boundary rejects them instead of silently trusting them.
#[cfg(unix)]
#[must_use]
pub fn owner_scoped_runtime_directory(path: &Path) -> PathBuf {
    use std::os::unix::fs::MetadataExt as _;

    let Some(parent) = path.parent() else {
        return path.to_path_buf();
    };
    // The conventional macOS `/tmp` path is a symlink to `/private/tmp`.
    // Classification may follow that outer alias; the derived UID directory
    // itself is still opened and validated with NOFOLLOW before use.
    let Ok(metadata) = std::fs::metadata(parent) else {
        return path.to_path_buf();
    };
    if !metadata.file_type().is_dir() {
        return path.to_path_buf();
    }
    owner_scoped_runtime_directory_for_metadata(
        path,
        metadata.uid(),
        metadata.mode(),
        effective_user_id(),
    )
}

#[cfg(unix)]
pub(crate) fn owner_scoped_runtime_directory_for_metadata(
    path: &Path,
    parent_uid: u32,
    parent_mode: u32,
    effective_uid: u32,
) -> PathBuf {
    const SHARED_STICKY_BITS: u32 = 0o1002;
    let owner_directory_name = format!("haider-{effective_uid}");
    if path.parent().and_then(Path::file_name) == Some(std::ffi::OsStr::new(&owner_directory_name))
    {
        // Normalization is shared by launcher and daemon; an already-derived
        // path must stay unchanged so a hostile pre-existing UID directory is
        // rejected by the later NOFOLLOW/owner check, not nested around.
        return path.to_path_buf();
    }
    if parent_uid == effective_uid || parent_mode & SHARED_STICKY_BITS != SHARED_STICKY_BITS {
        return path.to_path_buf();
    }
    let Some(name) = path.file_name() else {
        return path.to_path_buf();
    };
    let Some(parent) = path.parent() else {
        return path.to_path_buf();
    };
    parent.join(format!("haider-{effective_uid}")).join(name)
}

#[cfg(windows)]
#[must_use]
pub fn owner_scoped_runtime_directory(path: &Path) -> PathBuf {
    path.to_path_buf()
}

#[cfg(windows)]
#[must_use]
pub fn is_owner_private_directory(path: &Path) -> bool {
    path.is_dir()
}

#[cfg(all(test, unix))]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    #[test]
    fn sticky_root_derivation_is_idempotent_for_a_hostile_uid_directory() {
        let profile = Path::new("/tmp/profile-scope");
        let derived = owner_scoped_runtime_directory_for_metadata(profile, 0, 0o1777, 501);
        assert_eq!(derived, Path::new("/tmp/haider-501/profile-scope"));

        let repeated = owner_scoped_runtime_directory_for_metadata(&derived, 0, 0o1777, 501);
        assert_eq!(repeated, derived);
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn macos_tmp_alias_is_classified_through_its_sticky_target() {
        use std::os::unix::fs::MetadataExt as _;

        let target = std::fs::metadata("/tmp").expect("read /tmp target metadata");
        if target.uid() == effective_user_id() {
            return;
        }
        assert_eq!(
            owner_scoped_runtime_directory(Path::new("/tmp/profile-scope")),
            Path::new(&format!(
                "/tmp/haider-{}/profile-scope",
                effective_user_id()
            ))
        );
    }
}
