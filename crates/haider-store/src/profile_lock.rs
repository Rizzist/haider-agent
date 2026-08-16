//! Single-opener guard for a profile root. Owns the lock semantics:
//!
//! - Exclusivity comes from an advisory OS lock (`File::try_lock`) on
//!   `<root>/lock`, held for as long as the returned [`ProfileLock`] lives.
//!   Dropping it — including by process death or kill — releases the lock;
//!   there is no stale-lock state to clean up.
//! - A second opener fails fast with retryable `StoreLocked`.
//! - The lock file's contents (pid, timestamp) are diagnostics for humans;
//!   nothing ever reads them to make decisions.

use crate::{StoreResult, now_ms, store_error};
use haider_protocol::error::ErrorCode;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

pub(crate) struct ProfileLock {
    _file: File,
}

impl Drop for ProfileLock {
    fn drop(&mut self) {
        // Make the release boundary synchronous and explicit. Relying only
        // on handle close can leave a just-closed profile briefly contended
        // on macOS and Windows, where recovery immediately reopens it.
        let _ = self._file.unlock();
    }
}

impl ProfileLock {
    /// Takes the exclusive profile lock, or fails with `StoreLocked` if
    /// another live process holds it.
    pub(crate) fn acquire(root: &Path) -> StoreResult<Self> {
        let path = root.join("lock");
        let token = format!("pid={}\ncreated_at_ms={}\n", std::process::id(), now_ms()?);
        // Open without truncating: until the lock below is held, the file may
        // belong to a live owner whose diagnostic token must not be clobbered.
        let mut file = OpenOptions::new()
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

        // The lock is ours; now it is safe to replace the previous owner's
        // token with this process's diagnostics.
        if let Err(error) = file.set_len(0).and_then(|()| {
            file.seek(SeekFrom::Start(0))?;
            file.write_all(token.as_bytes())?;
            file.sync_all()
        }) {
            return Err(store_error(
                ErrorCode::Internal,
                format!("cannot persist profile lock {}: {error}", path.display()),
                false,
            ));
        }

        Ok(Self { _file: file })
    }
}
