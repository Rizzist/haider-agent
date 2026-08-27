//! Single-opener guard for a profile root. Owns the lock semantics:
//!
//! - Exclusivity comes from an advisory OS lock (`File::try_lock`) on
//!   `<root>/lock`, held for as long as the returned [`ProfileLock`] lives.
//!   Dropping it — including by process death or kill — releases the lock;
//!   there is no stale-lock state to clean up.
//! - A second opener fails fast with retryable `StoreLocked`.
//! - The lock file's bytes are never read or used for decisions; new files are
//!   empty and legacy contents may remain untouched. Diagnostic readers use
//!   only the separate human-readable owner token (pid, timestamp), atomically
//!   published at `<root>/lock.owner`; nothing ever reads it to make decisions.
//!   Normal release removes it
//!   best-effort; process death may leave a harmless stale token that the next
//!   owner atomically replaces.

use crate::{StoreResult, now_ms, store_error};
use haider_protocol::error::ErrorCode;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(test)]
#[path = "profile_lock_tests.rs"]
mod tests;

static NEXT_OWNER_STAGING: AtomicU64 = AtomicU64::new(0);

pub(crate) struct ProfileLock {
    file: File,
    owner_path: PathBuf,
}

impl Drop for ProfileLock {
    fn drop(&mut self) {
        // Remove the diagnostic while the singleton lock is still ours. That
        // ordering prevents a departing owner from deleting its successor's
        // freshly published token.
        let _ = fs::remove_file(&self.owner_path);
        // Make the release boundary synchronous and explicit. Relying only
        // on handle close can leave a just-closed profile briefly contended
        // on macOS and Windows, where recovery immediately reopens it.
        let _ = self.file.unlock();
    }
}

impl ProfileLock {
    /// Takes the exclusive profile lock, or fails with `StoreLocked` if
    /// another live process holds it.
    pub(crate) fn acquire(root: &Path) -> StoreResult<Self> {
        let path = root.join("lock");
        // Open without truncating: changing file length before locking would
        // mutate the singleton authority while another process owns it.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| {
                store_error(
                    ErrorCode::Internal,
                    format!("cannot open profile lock {}: {error}", path.display()),
                    false,
                )
            })?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(store_error(
                    ErrorCode::StoreLocked,
                    format!("store profile is already open: {}", root.display()),
                    true,
                ));
            }
            Err(TryLockError::Error(error)) => {
                return Err(store_error(
                    ErrorCode::Internal,
                    format!("cannot lock profile {}: {error}", path.display()),
                    false,
                ));
            }
        }

        let lock = Self {
            file,
            owner_path: root.join("lock.owner"),
        };
        let token = format!("pid={}\ncreated_at_ms={}\n", std::process::id(), now_ms()?);
        lock.publish_owner(root, token.as_bytes())?;
        Ok(lock)
    }

    /// Publishes diagnostics only after the OS lock is held. A same-directory
    /// staged file plus the platform replacement primitive keeps readers from
    /// observing a truncated or partially written token.
    fn publish_owner(&self, root: &Path, token: &[u8]) -> StoreResult<()> {
        const MAX_STAGING_ATTEMPTS: usize = 16;

        for _ in 0..MAX_STAGING_ATTEMPTS {
            let sequence = NEXT_OWNER_STAGING.fetch_add(1, Ordering::Relaxed);
            let staged_path =
                root.join(format!(".lock.owner-{}-{sequence}.tmp", std::process::id()));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            haider_platform::configure_file_mode(&mut options, 0o600);
            let mut staged = match options.open(&staged_path) {
                Ok(staged) => staged,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(owner_error(&self.owner_path, error)),
            };
            // The token is diagnostic only; flock, not token durability, is authoritative.
            let result = staged.write_all(token);
            drop(staged);
            let result =
                result.and_then(|()| haider_platform::replace_file(&staged_path, &self.owner_path));
            if let Err(error) = result {
                let _ = fs::remove_file(&staged_path);
                return Err(owner_error(&self.owner_path, error));
            }
            return Ok(());
        }

        Err(owner_error(
            &self.owner_path,
            std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "profile owner staging names are exhausted",
            ),
        ))
    }
}

fn owner_error(path: &Path, error: std::io::Error) -> haider_protocol::error::HaiderError {
    store_error(
        ErrorCode::Internal,
        format!(
            "cannot publish profile owner diagnostics {}: {error}",
            path.display()
        ),
        false,
    )
}
