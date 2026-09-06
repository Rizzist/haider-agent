//! Read-only inspection of the daemon's existing OS profile lock.
//!
//! The lock file is neither created nor truncated. Its advisory OS lock is
//! the authority; diagnostic owner bytes never participate in the decision.

use std::fs::{OpenOptions, TryLockError};
use std::io;

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
    match file.try_lock() {
        Ok(()) => {
            file.unlock().map_err(|error| {
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

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::profile_lock_held;
    use std::fs::{self, OpenOptions};

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
        let owner = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .expect("open");
        owner.try_lock().expect("take lock");
        assert!(profile_lock_held(root.path()).expect("held lock"));
        owner.unlock().expect("unlock");
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
