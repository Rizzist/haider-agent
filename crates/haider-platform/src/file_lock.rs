//! Advisory locks with an Android fallback for Rust 1.95's unsupported File locks.

use std::fs::{File, TryLockError};
use std::io;

#[cfg(unix)]
use std::os::fd::AsRawFd as _;

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

/// Adds a process-associated POSIX write lock to a file.
///
/// Profile stores use this so an external recovery client can use `F_GETLK`
/// to ask the kernel for the exact owner PID. Linux and Android hold it beside
/// their historical `flock`; macOS uses it as the authority because its
/// `flock` conflicts with record locks but does not expose an owning PID.
#[cfg(unix)]
pub fn lock_file_owner_record(file: &File) -> io::Result<()> {
    fcntl_record_lock(file, libc::F_SETLK, libc::F_WRLCK).map(|_| ())
}

/// Attempts to add the POSIX owner-record lock without blocking.
#[cfg(unix)]
pub fn try_lock_file_owner_record(file: &File) -> Result<(), TryLockError> {
    fcntl_record_lock(file, libc::F_SETLK, libc::F_WRLCK)
        .map(|_| ())
        .map_err(|error| match error.raw_os_error() {
            Some(libc::EACCES | libc::EAGAIN) => TryLockError::WouldBlock,
            _ => TryLockError::Error(error),
        })
}

/// Releases the companion POSIX owner-record lock.
#[cfg(unix)]
pub fn unlock_file_owner_record(file: &File) -> io::Result<()> {
    fcntl_record_lock(file, libc::F_SETLK, libc::F_UNLCK).map(|_| ())
}

/// Returns the process holding the companion POSIX owner-record lock.
///
/// `None` means the kernel reports no conflicting record lock. The result is
/// a point-in-time identity and must be paired with a retained process-exit
/// monitor plus a second lock-owner query before signaling.
#[cfg(unix)]
pub fn file_owner_record_pid(file: &File) -> io::Result<Option<u32>> {
    let lock = fcntl_record_lock(file, libc::F_GETLK, libc::F_WRLCK)?;
    if lock.l_type == libc::F_UNLCK {
        return Ok(None);
    }
    u32::try_from(lock.l_pid)
        .ok()
        .filter(|pid| *pid != 0)
        .map(Some)
        .ok_or_else(|| io::Error::other("kernel returned an invalid profile lock owner PID"))
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn fcntl_record_lock(
    file: &File,
    command: libc::c_int,
    lock_type: libc::c_short,
) -> io::Result<libc::flock> {
    let mut lock = libc::flock {
        l_start: 0,
        l_len: 0,
        l_pid: 0,
        l_type: lock_type,
        l_whence: libc::SEEK_SET as _,
    };
    // SAFETY: `file` keeps the descriptor live, and `lock` is a fully
    // initialized writable `flock` for the duration of this synchronous call.
    let result = unsafe { libc::fcntl(file.as_raw_fd(), command, &raw mut lock) };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(lock)
    }
}

#[cfg(test)]
#[path = "file_lock_tests.rs"]
mod tests;
