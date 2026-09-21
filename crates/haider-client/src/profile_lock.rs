//! Read-only inspection of the daemon's existing OS profile lock.
//!
//! The lock file is neither created nor truncated. Its advisory OS lock is
//! the authority; diagnostic owner bytes never participate in the decision.

use std::fs::OpenOptions;
use std::fs::TryLockError;
use std::io;
#[cfg(unix)]
use std::io::Read as _;

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};

pub const DAEMON_PID_FILE: &str = "haiderd.pid";

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PidFileIdentity {
    pub device: u64,
    pub inode: u64,
    pub pid: u32,
}

pub fn profile_lock_held(store_dir: &std::path::Path) -> Result<bool, String> {
    let lock_path = store_dir.join("lock");
    let file = match OpenOptions::new().read(true).write(true).open(&lock_path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(format!(
                "cannot open existing profile lock {}: {error}",
                lock_path.display()
            ));
        }
    };
    match haider_platform::try_lock_file_exclusive(&file) {
        Ok(()) => {
            haider_platform::unlock_file(&file).map_err(|error| {
                format!(
                    "cannot release profile lock probe {}: {error}",
                    lock_path.display()
                )
            })?;
            Ok(false)
        }
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(error)) => Err(format!(
            "cannot inspect profile lock {}: {error}",
            lock_path.display()
        )),
    }
}

/// Asks the kernel which process owns the profile's POSIX-visible lock.
/// The zero-byte lock file and `lock.owner` diagnostics are never parsed.
#[cfg(unix)]
pub fn profile_lock_owner_pid(store_dir: &std::path::Path) -> Result<Option<u32>, String> {
    let lock_path = store_dir.join("lock");
    let file = match OpenOptions::new().read(true).write(true).open(&lock_path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "cannot open existing profile lock {}: {error}",
                lock_path.display()
            ));
        }
    };
    haider_platform::file_owner_record_pid(&file).map_err(|error| {
        format!(
            "cannot query kernel owner for profile lock {}: {error}",
            lock_path.display()
        )
    })
}

/// Reads the runtime PID file only when one stable, owner-controlled inode was
/// observed through both the open handle and public path.
///
/// This is corroboration for a kernel lock-owner PID, never authority for
/// choosing a signal target.
#[cfg(unix)]
pub fn pid_file_identity(runtime_dir: &std::path::Path) -> Result<Option<PidFileIdentity>, String> {
    let path = runtime_dir.join(DAEMON_PID_FILE);
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    let mut file = match options.open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "cannot open daemon PID file {}: {error}",
                path.display()
            ));
        }
    };
    let handle = file.metadata().map_err(|error| {
        format!(
            "cannot identify daemon PID file {}: {error}",
            path.display()
        )
    })?;
    if !handle.is_file() || !haider_platform::metadata_is_current_user(&handle) {
        return Err(format!(
            "daemon PID file is not an owner-controlled regular file: {}",
            path.display()
        ));
    }
    let path_metadata = std::fs::symlink_metadata(&path).map_err(|error| {
        format!(
            "cannot re-identify daemon PID file {}: {error}",
            path.display()
        )
    })?;
    if handle.dev() != path_metadata.dev() || handle.ino() != path_metadata.ino() {
        return Err(format!(
            "daemon PID file identity changed while reading: {}",
            path.display()
        ));
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(64)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read daemon PID file {}: {error}", path.display()))?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| format!("daemon PID file is not UTF-8: {}", path.display()))?;
    let pid = text
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|pid| *pid != 0)
        .ok_or_else(|| format!("daemon PID file has an invalid PID: {}", path.display()))?;
    Ok(Some(PidFileIdentity {
        device: handle.dev(),
        inode: handle.ino(),
        pid,
    }))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::profile_lock_held;
    use std::fs;
    use std::fs::OpenOptions;

    #[test]
    fn lock_probe_never_creates_a_profile_or_changes_owner_diagnostics() {
        let root = tempfile::tempdir().expect("tempdir");
        let absent = root.path().join("absent-profile");
        assert!(!profile_lock_held(&absent).expect("missing profile is unlocked"));
        assert!(!absent.exists(), "a probe must not materialize the profile");
        assert!(!profile_lock_held(root.path()).expect("missing lock is unlocked"));
        assert!(!root.path().join("lock").exists());

        let lock_path = root.path().join("lock");
        let owner_path = root.path().join("lock.owner");
        fs::write(&lock_path, b"legacy diagnostics").expect("lock bytes");
        fs::write(&owner_path, b"owner diagnostics").expect("owner bytes");
        {
            let owner = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&lock_path)
                .expect("open");
            haider_platform::try_lock_file_exclusive(&owner).expect("take lock");
            assert!(profile_lock_held(root.path()).expect("held lock"));
            haider_platform::unlock_file(&owner).expect("unlock");
        }
        assert!(!profile_lock_held(root.path()).expect("released lock"));
        assert_eq!(
            fs::read(lock_path).expect("read lock"),
            b"legacy diagnostics"
        );
        assert_eq!(
            fs::read(owner_path).expect("read owner"),
            b"owner diagnostics"
        );
    }
}
