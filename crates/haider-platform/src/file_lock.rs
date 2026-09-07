//! Advisory locks with an Android fallback for Rust 1.95's unsupported File locks.

use std::fs::{File, TryLockError};
use std::io;

/// Acquire an exclusive advisory lock, blocking until it is available.
pub fn lock_file_exclusive(file: &File) -> io::Result<()> {
    #[cfg(target_os = "android")]
    {
        rustix::fs::flock(file, rustix::fs::FlockOperation::LockExclusive).map_err(Into::into)
    }
    #[cfg(not(target_os = "android"))]
    file.lock()
}

/// Try an exclusive advisory lock without waiting for another owner.
pub fn try_lock_file_exclusive(file: &File) -> Result<(), TryLockError> {
    #[cfg(target_os = "android")]
    {
        rustix::fs::flock(file, rustix::fs::FlockOperation::NonBlockingLockExclusive).map_err(
            |error| {
                if error == rustix::io::Errno::WOULDBLOCK {
                    TryLockError::WouldBlock
                } else {
                    TryLockError::Error(error.into())
                }
            },
        )
    }
    #[cfg(not(target_os = "android"))]
    file.try_lock()
}

/// Explicitly release the advisory lock; closing the file also releases it.
pub fn unlock_file(file: &File) -> io::Result<()> {
    #[cfg(target_os = "android")]
    {
        rustix::fs::flock(file, rustix::fs::FlockOperation::Unlock).map_err(Into::into)
    }
    #[cfg(not(target_os = "android"))]
    file.unlock()
}

#[cfg(test)]
#[path = "file_lock_tests.rs"]
mod tests;
