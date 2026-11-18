//! Advisory locks with an Android fallback for Rust 1.95's unsupported File locks.

use std::fs::{File, TryLockError};
use std::io;

/// Owns an advisory lock and releases it before closing its descriptor.
///
/// CLOEXEC protects exec, but a concurrently forked child can retain the open
/// file description until exec. Explicit unlock makes this owner's lifetime
/// authoritative even while that duplicate remains open. Do not clone the
/// file to create an independent lock owner; clones share the same lock.
#[derive(Debug)]
#[must_use = "retain the guard until the exclusive operation is complete"]
pub struct FileLock {
    file: File,
    owner_pid: u32,
}

impl FileLock {
    pub fn shared(file: File) -> io::Result<Self> {
        lock_file_shared(&file)?;
        Ok(Self {
            file,
            owner_pid: std::process::id(),
        })
    }

    pub fn exclusive(file: File) -> io::Result<Self> {
        lock_file_exclusive(&file)?;
        Ok(Self {
            file,
            owner_pid: std::process::id(),
        })
    }

    pub fn try_exclusive(file: File) -> Result<Self, TryLockError> {
        try_lock_file_exclusive(&file)?;
        Ok(Self {
            file,
            owner_pid: std::process::id(),
        })
    }

    pub fn file(&self) -> &File {
        &self.file
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // A forked copy may close its descriptor, but must not unlock the
        // parent's shared open-file description while the owner is alive.
        if std::process::id() == self.owner_pid {
            let _ = unlock_file(&self.file);
        }
    }
}

#[cfg(unix)]
use std::os::fd::AsRawFd as _;

/// Acquire an exclusive advisory lock, blocking until it is available.
///
/// Prefer [`FileLock::exclusive`] for an owned lease. Callers of this borrowed
/// primitive must explicitly unlock before closing so a forked duplicate
/// cannot extend the logical owner's lifetime.
pub fn lock_file_exclusive(file: &File) -> io::Result<()> {
    prevent_lock_inheritance(file)?;
    #[cfg(target_os = "android")]
    {
        rustix::fs::flock(file, rustix::fs::FlockOperation::LockExclusive).map_err(Into::into)
    }
    #[cfg(not(target_os = "android"))]
    file.lock()
}

fn lock_file_shared(file: &File) -> io::Result<()> {
    prevent_lock_inheritance(file)?;
    #[cfg(target_os = "android")]
    {
        rustix::fs::flock(file, rustix::fs::FlockOperation::LockShared).map_err(Into::into)
    }
    #[cfg(not(target_os = "android"))]
    file.lock_shared()
}

/// Try an exclusive advisory lock without waiting for another owner.
///
/// Prefer [`FileLock::try_exclusive`] for an owned lease; this borrowed
/// primitive needs the same explicit release as [`lock_file_exclusive`].
pub fn try_lock_file_exclusive(file: &File) -> Result<(), TryLockError> {
    prevent_lock_inheritance(file).map_err(TryLockError::Error)?;
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

/// Explicitly release the advisory lock, including any forked duplicates.
/// Closing alone releases it only after every duplicate is closed; use
/// [`FileLock`] when release must follow the owner's lifetime.
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
    prevent_lock_inheritance(file)?;
    fcntl_record_lock(file, libc::F_SETLK, libc::F_WRLCK as libc::c_int).map(|_| ())
}

/// Attempts to add the POSIX owner-record lock without blocking.
#[cfg(unix)]
pub fn try_lock_file_owner_record(file: &File) -> Result<(), TryLockError> {
    prevent_lock_inheritance(file).map_err(TryLockError::Error)?;
    fcntl_record_lock(file, libc::F_SETLK, libc::F_WRLCK as libc::c_int)
        .map(|_| ())
        .map_err(|error| match error.raw_os_error() {
            Some(libc::EACCES | libc::EAGAIN) => TryLockError::WouldBlock,
            _ => TryLockError::Error(error),
        })
}

// Standard file opens already set this atomically. Also fence handles supplied
// by callers through raw/openat APIs before any lock can be acquired.
#[cfg(unix)]
fn prevent_lock_inheritance(file: &File) -> io::Result<()> {
    let flags = rustix::io::fcntl_getfd(file)?;
    rustix::io::fcntl_setfd(file, flags | rustix::io::FdFlags::CLOEXEC).map_err(Into::into)
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn prevent_lock_inheritance(file: &File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};

    // SAFETY: file owns a live handle; only the inheritance flag is changed.
    if unsafe { SetHandleInformation(file.as_raw_handle(), HANDLE_FLAG_INHERIT, 0) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
fn prevent_lock_inheritance(_file: &File) -> io::Result<()> {
    Ok(())
}

/// Releases the companion POSIX owner-record lock.
#[cfg(unix)]
pub fn unlock_file_owner_record(file: &File) -> io::Result<()> {
    fcntl_record_lock(file, libc::F_SETLK, libc::F_UNLCK as libc::c_int).map(|_| ())
}

/// Returns the process holding the companion POSIX owner-record lock.
///
/// `None` means the kernel reports no conflicting record lock. The result is
/// a point-in-time identity and must be paired with a retained process-exit
/// monitor plus a second lock-owner query before signaling.
#[cfg(unix)]
pub fn file_owner_record_pid(file: &File) -> io::Result<Option<u32>> {
    let lock = fcntl_record_lock(file, libc::F_GETLK, libc::F_WRLCK as libc::c_int)?;
    if libc::c_int::from(lock.l_type) == libc::F_UNLCK as libc::c_int {
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
    lock_type: libc::c_int,
) -> io::Result<libc::flock> {
    let mut lock = libc::flock {
        l_start: 0,
        l_len: 0,
        l_pid: 0,
        // `F_WRLCK`/`F_UNLCK` are `c_short` on macOS/Linux but `c_int` on
        // Android; `flock.l_type` follows the same per-target width, so the
        // constant is taken as `c_int` and narrowed to the field's type here.
        l_type: lock_type as _,
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

/// Inspects an inherited descriptor even if exec has already closed it.
/// macOS fdescfs metadata reports a synthetic device rather than the file's.
#[cfg(all(test, unix))]
#[allow(unsafe_code, clippy::unnecessary_cast)]
pub(super) fn test_descriptor_identity(fd: libc::c_int) -> io::Result<Option<(u64, u64)>> {
    // SAFETY: fstat accepts a closed descriptor and reports EBADF. Its output
    // pointer is writable, and we inspect the initialized stat only on success.
    // The casts normalize libc's different device/inode widths across targets.
    let result = unsafe {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if libc::fstat(fd, stat.as_mut_ptr()) == 0 {
            let stat = stat.assume_init();
            Ok((stat.st_dev as u64, stat.st_ino as u64))
        } else {
            Err(io::Error::last_os_error())
        }
    };
    match result {
        Ok(identity) => Ok(Some(identity)),
        Err(error) if error.raw_os_error() == Some(libc::EBADF) => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
#[path = "file_lock_tests.rs"]
mod tests;
