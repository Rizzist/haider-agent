//! Single-opener guard for a profile root. Owns the lock semantics:
//!
//! - Exclusivity comes from an advisory OS lock on
//!   `<root>/lock`, held for as long as the returned [`ProfileLock`] lives.
//!   Unix platforms use a POSIX record lock so `F_GETLK` can identify the
//!   exact owning PID. Linux and Android also retain the historical `flock`
//!   authority because the two namespaces are independent there. On macOS the
//!   namespaces conflict with each other, preserving exclusion with older
//!   daemons, while an in-process registry supplies same-process exclusion.
//!   Dropping it — including by process death or kill — releases the lock;
//!   there is no stale-lock state to clean up.
//! - A second opener fails fast with retryable `StoreLocked`.
//! - The lock file's bytes are never read or used for decisions; clean release
//!   truncates legacy contents. Diagnostic readers use
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
#[cfg(target_os = "macos")]
use std::sync::{Mutex, OnceLock};

#[cfg(test)]
#[path = "profile_lock_tests.rs"]
mod tests;

static NEXT_OWNER_STAGING: AtomicU64 = AtomicU64::new(0);

pub(crate) struct ProfileLock {
    file: File,
    owner_path: PathBuf,
    #[cfg(target_os = "macos")]
    lock_path: PathBuf,
}

impl Drop for ProfileLock {
    fn drop(&mut self) {
        // Remove the diagnostic while the singleton lock is still ours. That
        // ordering prevents a departing owner from deleting its successor's
        // freshly published token.
        let _ = fs::remove_file(&self.owner_path);
        // Clearing legacy inline diagnostics is part of clean release. The
        // advisory lock is authoritative, so this needs no durability sync.
        let _ = self.file.set_len(0);
        #[cfg(unix)]
        let _ = haider_platform::unlock_file_owner_record(&self.file);
        #[cfg(target_os = "macos")]
        release_process_local_lock(&self.lock_path);
        // Make the release boundary synchronous and explicit. Relying only
        // on handle close can leave a just-closed profile briefly contended
        // on macOS and Windows, where recovery immediately reopens it.
        #[cfg(not(target_os = "macos"))]
        let _ = haider_platform::unlock_file(&self.file);
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
        #[cfg(target_os = "macos")]
        let lock_path = fs::canonicalize(&path).map_err(|error| {
            store_error(
                ErrorCode::Internal,
                format!(
                    "cannot canonicalize profile lock {}: {error}",
                    path.display()
                ),
                false,
            )
        })?;
        #[cfg(target_os = "macos")]
        if !claim_process_local_lock(&lock_path) {
            return Err(store_error(
                ErrorCode::StoreLocked,
                format!("store profile is already open: {}", root.display()),
                true,
            ));
        }
        #[cfg(target_os = "macos")]
        let lock_result = haider_platform::try_lock_file_owner_record(&file);
        #[cfg(not(target_os = "macos"))]
        let lock_result = haider_platform::try_lock_file_exclusive(&file);
        match lock_result {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                #[cfg(target_os = "macos")]
                release_process_local_lock(&lock_path);
                return Err(store_error(
                    ErrorCode::StoreLocked,
                    format!("store profile is already open: {}", root.display()),
                    true,
                ));
            }
            Err(TryLockError::Error(error)) => {
                #[cfg(target_os = "macos")]
                release_process_local_lock(&lock_path);
                return Err(store_error(
                    ErrorCode::Internal,
                    format!("cannot lock profile {}: {error}", path.display()),
                    false,
                ));
            }
        }

        // Linux and Android keep `flock` and POSIX record locks in separate
        // namespaces. Hold both: `flock` remains compatible with older
        // daemons, while the record lock makes the owner PID queryable.
        // macOS unifies their conflicts, so its record lock already excludes
        // older `flock` owners and taking both here would self-conflict.
        #[cfg(all(unix, not(target_os = "macos")))]
        match haider_platform::try_lock_file_owner_record(&file) {
            Ok(()) => {}
            Err(error) => {
                let _ = haider_platform::unlock_file(&file);
                return Err(match error {
                    TryLockError::WouldBlock => store_error(
                        ErrorCode::StoreLocked,
                        format!("store profile is already open: {}", root.display()),
                        true,
                    ),
                    TryLockError::Error(error) => store_error(
                        ErrorCode::Internal,
                        format!("cannot record profile owner {}: {error}", path.display()),
                        false,
                    ),
                });
            }
        }

        let lock = Self {
            file,
            owner_path: root.join("lock.owner"),
            #[cfg(target_os = "macos")]
            lock_path,
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
            // The token is diagnostic only; the kernel lock, not token durability, is authoritative.
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

#[cfg(target_os = "macos")]
fn process_local_locks() -> &'static Mutex<std::collections::HashSet<PathBuf>> {
    static LOCKS: OnceLock<Mutex<std::collections::HashSet<PathBuf>>> = OnceLock::new();
    LOCKS.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

#[cfg(target_os = "macos")]
fn claim_process_local_lock(path: &Path) -> bool {
    process_local_locks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(path.to_path_buf())
}

#[cfg(target_os = "macos")]
fn release_process_local_lock(path: &Path) {
    process_local_locks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(path);
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
