use super::*;
use std::fs::OpenOptions;

#[cfg(unix)]
#[test]
fn supplied_inheritable_descriptors_are_fenced_before_locking() -> io::Result<()> {
    for mode in 0..5 {
        let directory = tempfile::tempdir()?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(directory.path().join("supplied-lock"))?;
        rustix::io::fcntl_setfd(&file, rustix::io::FdFlags::empty())?;
        assert!(!rustix::io::fcntl_getfd(&file)?.contains(rustix::io::FdFlags::CLOEXEC));
        if mode < 2 || mode == 4 {
            let lock = if mode == 0 {
                FileLock::exclusive(file)?
            } else if mode == 4 {
                FileLock::shared(file)?
            } else {
                FileLock::try_exclusive(file).map_err(io::Error::from)?
            };
            assert!(rustix::io::fcntl_getfd(lock.file())?.contains(rustix::io::FdFlags::CLOEXEC));
        } else {
            if mode == 2 {
                lock_file_owner_record(&file)?;
            } else {
                try_lock_file_owner_record(&file).map_err(io::Error::from)?;
            }
            assert!(rustix::io::fcntl_getfd(&file)?.contains(rustix::io::FdFlags::CLOEXEC));
            unlock_file_owner_record(&file)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
const OWNER_RECORD_HELPER_ENV: &str = "HAIDER_OWNER_RECORD_HELPER";
#[cfg(target_os = "macos")]
const FLOCK_OWNER_HELPER_ENV: &str = "HAIDER_FLOCK_OWNER_HELPER";

#[test]
fn file_lock_fences_another_opener_until_unlock_or_close() -> io::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("lock");
    let open = || {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
    };
    let first = open()?;
    let second = open()?;
    lock_file_exclusive(&first)?;
    assert!(matches!(
        try_lock_file_exclusive(&second),
        Err(TryLockError::WouldBlock)
    ));
    unlock_file(&first)?;
    let second = FileLock::try_exclusive(second).map_err(io::Error::from)?;
    assert!(matches!(
        try_lock_file_exclusive(&first),
        Err(TryLockError::WouldBlock)
    ));
    drop(second);
    try_lock_file_exclusive(&first).map_err(io::Error::from)?;
    unlock_file(&first)
}

/// A raw close is deliberately the negative control: CLOEXEC does not protect
/// the fork-to-exec window. The owned guard must release across that window.
#[cfg(unix)]
#[test]
fn forked_descriptor_does_not_outlive_the_lock_owner() -> io::Result<()> {
    assert_fork_release(false, false)?;
    assert_fork_release(true, false)
}

#[cfg(unix)]
#[test]
fn forked_shared_descriptor_does_not_outlive_the_lock_owner() -> io::Result<()> {
    assert_fork_release(false, true)?;
    assert_fork_release(true, true)
}

#[test]
fn shared_owners_coexist_and_fence_writers_until_the_last_owner_releases() -> io::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("shared-lock");
    let open = || {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
    };
    let first = FileLock::shared(open()?)?;
    let second = FileLock::shared(open()?)?;
    let writer = open()?;
    assert!(matches!(
        try_lock_file_exclusive(&writer),
        Err(TryLockError::WouldBlock)
    ));
    let duplicate = first.file().try_clone()?;
    drop(first);
    assert!(matches!(
        try_lock_file_exclusive(&writer),
        Err(TryLockError::WouldBlock)
    ));
    drop(second);
    try_lock_file_exclusive(&writer).map_err(io::Error::from)?;
    unlock_file(&writer)?;
    drop(duplicate);
    Ok(())
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn assert_fork_release(owned: bool, shared: bool) -> io::Result<()> {
    use std::io::{Read as _, Write as _};
    use std::os::unix::net::UnixStream;

    struct ForkChild(Option<libc::pid_t>);
    impl Drop for ForkChild {
        fn drop(&mut self) {
            if let Some(pid) = self.0.take() {
                // SAFETY: the unreaped owned child pins this PID. On a failed
                // assertion terminate/reap only that child, never a peer.
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                    libc::waitpid(pid, std::ptr::null_mut(), 0);
                }
            }
        }
    }

    let directory = tempfile::tempdir()?;
    let path = directory.path().join("fork-lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)?;
    let probe = OpenOptions::new().read(true).write(true).open(&path)?;
    if shared {
        lock_file_shared(&file)?;
    } else {
        lock_file_exclusive(&file)?;
    }
    let lock_fd = file.as_raw_fd();
    let (mut parent, child) = UnixStream::pair()?;
    parent.set_read_timeout(Some(std::time::Duration::from_secs(60)))?;
    let child_fd = child.as_raw_fd();
    // SAFETY: after fork the child calls only async-signal-safe libc functions,
    // then _exit, without running Rust destructors or accessing runtime locks.
    let pid = unsafe {
        let pid = libc::fork();
        if pid == 0 {
            libc::alarm(60);
            let mut byte = 1_u8;
            if libc::write(child_fd, (&raw const byte).cast(), 1) != 1 {
                libc::_exit(2);
            }
            let result = libc::read(child_fd, (&raw mut byte).cast(), 1);
            // The raw-close control can also have copies in another parallel
            // fork fixture. This direct kernel unlock releases that exact
            // description after the parent records its contention result.
            let unlocked = libc::flock(lock_fd, libc::LOCK_UN);
            libc::_exit(if result == 1 && unlocked == 0 { 0 } else { 3 });
        }
        pid
    };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut child_guard = ForkChild(Some(pid));
    drop(child);
    let mut ready = [0];
    parent.read_exact(&mut ready)?;
    let cloexec = rustix::io::fcntl_getfd(&file)?.contains(rustix::io::FdFlags::CLOEXEC);
    assert!(cloexec, "standard file opens must be close-on-exec");
    if owned {
        if shared {
            drop(FileLock::shared(file)?);
        } else {
            drop(FileLock::try_exclusive(file).map_err(io::Error::from)?);
        }
    } else {
        drop(file);
    }
    let reacquired = try_lock_file_exclusive(&probe);
    parent.write_all(&[1])?;
    let mut status = 0;
    // SAFETY: reap this exact owned child with a writable status integer.
    let waited = unsafe { libc::waitpid(pid, &raw mut status, 0) };
    if waited == pid {
        child_guard.0 = None;
    }
    assert_eq!(waited, pid);
    assert_eq!(status, 0);
    if owned {
        reacquired.map_err(io::Error::from)?;
    } else {
        assert!(matches!(reacquired, Err(TryLockError::WouldBlock)));
        try_lock_file_exclusive(&probe).map_err(io::Error::from)?;
    }
    unlock_file(&probe)
}

#[cfg(unix)]
#[test]
#[allow(unsafe_code)]
fn closing_a_forked_guard_cannot_unlock_the_live_parent() -> io::Result<()> {
    assert_child_guard_does_not_unlock_parent(false)
}

#[cfg(unix)]
#[test]
fn closing_a_forked_shared_guard_cannot_unlock_the_live_parent() -> io::Result<()> {
    assert_child_guard_does_not_unlock_parent(true)
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn assert_child_guard_does_not_unlock_parent(shared: bool) -> io::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("live-parent-lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)?;
    let lock = if shared {
        FileLock::shared(file)?
    } else {
        FileLock::try_exclusive(file).map_err(io::Error::from)?
    };
    // SAFETY: the copied guard's child drop only checks getpid and closes its
    // descriptor; it skips flock. _exit avoids all other Rust destructors.
    let pid = unsafe {
        let pid = libc::fork();
        if pid == 0 {
            drop(lock);
            libc::_exit(0);
        }
        pid
    };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut status = 0;
    // SAFETY: reap exactly the child above using a writable status integer.
    let waited = unsafe { libc::waitpid(pid, &raw mut status, 0) };
    assert_eq!(waited, pid);
    assert_eq!(status, 0);
    let probe = OpenOptions::new().read(true).write(true).open(&path)?;
    assert!(matches!(
        try_lock_file_exclusive(&probe),
        Err(TryLockError::WouldBlock)
    ));
    drop(lock);
    try_lock_file_exclusive(&probe).map_err(io::Error::from)?;
    unlock_file(&probe)
}

#[cfg(unix)]
#[test]
#[allow(unsafe_code)]
fn exec_and_spawn_do_not_retain_the_lock_descriptor() -> io::Result<()> {
    use std::io::{Read as _, Write as _};
    use std::os::unix::fs::MetadataExt as _;
    use std::os::unix::process::CommandExt as _;
    use std::process::{Command, Stdio};
    const HELPER: &str = "HAIDER_LOCK_EXEC_HELPER";
    const READY: &str = "lock descriptor absent after exec";
    if let Ok(identity) = std::env::var(HELPER) {
        let identity = identity
            .split(':')
            .map(|part| part.parse::<u64>())
            .collect::<Result<Vec<_>, _>>()
            .map_err(io::Error::other)?;
        let fd = libc::c_int::try_from(identity[0]).map_err(io::Error::other)?;
        if let Some(actual) = super::test_descriptor_identity(fd)? {
            assert_ne!(
                actual,
                (identity[1], identity[2]),
                "lock descriptor survived exec"
            );
        }
        println!("{READY}");
        std::io::stdout().flush()?;
        std::io::stdin().read_to_end(&mut Vec::new())?;
        return Ok(());
    }

    for (shell, shared) in [(false, false), (true, false), (false, true), (true, true)] {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("exec-lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        let metadata = file.metadata()?;
        let descriptor = file.as_raw_fd();
        rustix::io::fcntl_setfd(&file, rustix::io::FdFlags::empty())?;
        let lock = if shared {
            FileLock::shared(file)?
        } else {
            FileLock::exclusive(file)?
        };
        // The fresh image checks the exact descriptor identity, then remains
        // gated on stdin while its parent releases the lock and probes it.
        let executable = std::env::current_exe()?;
        let shell_binary = if cfg!(target_os = "android") {
            "/system/bin/sh"
        } else {
            "/bin/sh"
        };
        let mut command = Command::new(if shell {
            std::path::Path::new(shell_binary)
        } else {
            &executable
        });
        if shell {
            command
                .args(["-c", "exec \"$@\"", "haider-lock-exec"])
                .arg(&executable);
            // SAFETY: the empty callback performs no post-fork work. Its
            // presence forces std's fork/exec path, independent of any
            // platform-specific posix_spawn descriptor-closing defaults.
            unsafe { command.pre_exec(|| Ok(())) };
        }
        command
            .args([
                "exec_and_spawn_do_not_retain_the_lock_descriptor",
                "--nocapture",
            ])
            .env(
                HELPER,
                format!("{descriptor}:{}:{}", metadata.dev(), metadata.ino()),
            );
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("helper stdout is absent"))?;
        let flags = rustix::fs::fcntl_getfl(&stdout)?;
        rustix::fs::fcntl_setfl(&stdout, flags | rustix::fs::OFlags::NONBLOCK)?;
        let deadline = std::time::Instant::now() + crate::TEST_WAIT_TIMEOUT;
        let mut receipt = Vec::new();
        let ready = loop {
            let mut buffer = [0; 1024];
            match stdout.read(&mut buffer) {
                Ok(0) => break false,
                Ok(count) => receipt.extend_from_slice(&buffer[..count]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
            if String::from_utf8_lossy(&receipt).contains(READY) {
                break true;
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                break false;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        drop(lock);
        let probe = OpenOptions::new().read(true).write(true).open(&path)?;
        let acquired = try_lock_file_exclusive(&probe);
        drop(child.stdin.take());
        let status = child.wait()?;
        rustix::fs::fcntl_setfl(&stdout, flags)?;
        child.stdout = Some(stdout);
        let output = child.wait_with_output()?;
        assert!(
            ready && status.success(),
            "exec helper failed: ready={ready}; stdout={}; stderr={}",
            String::from_utf8_lossy(&receipt),
            String::from_utf8_lossy(&output.stderr)
        );
        acquired.map_err(io::Error::from)?;
        unlock_file(&probe)?;
    }
    Ok(())
}

/// Exercise the real background-process descriptor sweep and std's fork/exec
/// launch. A gate after the sweep makes the suite's short inheritance window
/// deterministic without sleeping or weakening the production spawn path.
#[cfg(unix)]
#[test]
#[allow(unsafe_code)]
fn background_spawn_fork_window_does_not_extend_the_lock_owner() -> io::Result<()> {
    use std::io::{Read as _, Write as _};
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::CommandExt as _;
    use std::process::Stdio;
    for owned in [false, true] {
        let root = tempfile::tempdir()?;
        let path = root.path().join("background-lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        lock_file_exclusive(&file)?;
        let lock_fd = file.as_raw_fd();
        let (guard, raw) = if owned {
            (
                Some(FileLock::try_exclusive(file).map_err(io::Error::from)?),
                None,
            )
        } else {
            (None, Some(file))
        };
        let probe = OpenOptions::new().read(true).write(true).open(&path)?;
        let (mut parent, child_gate) = UnixStream::pair()?;
        parent.set_read_timeout(Some(crate::TEST_WAIT_TIMEOUT))?;
        let parent_fd = parent.as_raw_fd();
        let gate_fd = child_gate.as_raw_fd();
        let cat = if cfg!(target_os = "android") {
            "/system/bin/cat"
        } else {
            "/bin/cat"
        };
        let mut command = tokio::process::Command::new(cat);
        crate::configure_background_process(&mut command);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: the child gate calls only async-signal-safe libc operations
        // and constructs an inline raw-errno error. Its owning socket stays
        // alive in the launching thread until the exec barrier completes.
        unsafe {
            command.as_std_mut().pre_exec(move || {
                libc::alarm(60);
                // This child must not keep the peer endpoint alive on a
                // parent's failed readiness observation.
                libc::close(parent_fd);
                let mut byte = 1_u8;
                if libc::write(gate_fd, (&raw const byte).cast(), 1) != 1
                    || libc::read(gate_fd, (&raw mut byte).cast(), 1) != 1
                {
                    return Err(io::Error::from_raw_os_error(libc::EPIPE));
                }
                // Reset the intentional raw-close control even if another
                // parallel fork copied this same open-file description.
                if libc::flock(lock_fd, libc::LOCK_UN) != 0 {
                    return Err(io::Error::from_raw_os_error(libc::EIO));
                }
                libc::alarm(0);
                Ok(())
            });
        }
        let launch = std::thread::spawn(move || {
            let launched = command.as_std_mut().spawn();
            drop(child_gate);
            launched
        });
        let mut ready = [0];
        parent.read_exact(&mut ready)?;
        drop(guard);
        drop(raw);
        let acquired_during_fork = try_lock_file_exclusive(&probe);
        parent.write_all(&[1])?;
        let mut child = launch
            .join()
            .map_err(|_| io::Error::other("spawn fixture thread panicked"))??;
        drop(child.stdin.take());
        assert!(child.wait()?.success());
        if owned {
            acquired_during_fork.map_err(io::Error::from)?;
        } else {
            assert!(matches!(
                acquired_during_fork,
                Err(TryLockError::WouldBlock)
            ));
            try_lock_file_exclusive(&probe).map_err(io::Error::from)?;
        }
        unlock_file(&probe)?;
    }
    Ok(())
}

#[cfg(windows)]
#[test]
#[allow(unsafe_code)]
fn lock_file_handles_are_not_inheritable() -> io::Result<()> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Foundation::{
        GetHandleInformation, HANDLE_FLAG_INHERIT, SetHandleInformation,
    };

    let directory = tempfile::tempdir()?;
    for mode in 0..3 {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(directory.path().join(format!("windows-lock-{mode}")))?;
        let mut flags = 0;
        // SAFETY: the standard open owns a live handle and flags is writable.
        if unsafe { GetHandleInformation(file.as_raw_handle(), &raw mut flags) } == 0 {
            return Err(io::Error::last_os_error());
        }
        assert_eq!(
            flags & HANDLE_FLAG_INHERIT,
            0,
            "ordinary opens are non-inheritable"
        );
        // SAFETY: plant an inheritable caller-supplied live handle. Acquisition
        // must clear the bit as well as protecting ordinary non-inheritable opens.
        if unsafe {
            SetHandleInformation(
                file.as_raw_handle(),
                HANDLE_FLAG_INHERIT,
                HANDLE_FLAG_INHERIT,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let lock = if mode == 2 {
            FileLock::shared(file)?
        } else if mode == 1 {
            FileLock::try_exclusive(file).map_err(io::Error::from)?
        } else {
            FileLock::exclusive(file)?
        };
        flags = 0;
        // SAFETY: the guard owns a live handle and flags is writable.
        if unsafe { GetHandleInformation(lock.file().as_raw_handle(), &raw mut flags) } == 0 {
            return Err(io::Error::last_os_error());
        }
        assert_eq!(flags & HANDLE_FLAG_INHERIT, 0);
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn owner_record_helper() -> io::Result<()> {
    use std::io::Read as _;

    let Some(path) = std::env::var_os(OWNER_RECORD_HELPER_ENV) else {
        return Ok(());
    };
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    lock_file_owner_record(&file)?;
    let ready = std::env::var_os("HAIDER_OWNER_RECORD_READY")
        .ok_or_else(|| io::Error::other("owner-record helper ready path is absent"))?;
    std::fs::write(ready, b"ready")?;
    let mut byte = [0_u8; 1];
    let _ = std::io::stdin().read(&mut byte)?;
    unlock_file_owner_record(&file)
}

#[cfg(target_os = "macos")]
#[test]
fn flock_owner_helper() -> io::Result<()> {
    use std::io::Read as _;

    let Some(path) = std::env::var_os(FLOCK_OWNER_HELPER_ENV) else {
        return Ok(());
    };
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    lock_file_exclusive(&file)?;
    let ready = std::env::var_os("HAIDER_FLOCK_OWNER_READY")
        .ok_or_else(|| io::Error::other("flock-owner helper ready path is absent"))?;
    std::fs::write(ready, b"ready")?;
    let mut byte = [0_u8; 1];
    let _ = std::io::stdin().read(&mut byte)?;
    unlock_file(&file)
}

#[cfg(unix)]
#[test]
fn owner_record_query_returns_the_exact_cross_process_pid() -> io::Result<()> {
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let directory = tempfile::tempdir()?;
    let path = directory.path().join("lock");
    let ready = directory.path().join("ready");
    std::fs::write(&path, b"")?;
    let mut child = std::process::Command::new(std::env::current_exe()?)
        .args(["owner_record_helper", "--nocapture"])
        .env(OWNER_RECORD_HELPER_ENV, &path)
        .env("HAIDER_OWNER_RECORD_READY", &ready)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + crate::TEST_WAIT_TIMEOUT;
    while !ready.exists() {
        if let Some(status) = child.try_wait()? {
            return Err(io::Error::other(format!(
                "owner-record helper exited before readiness: {status}"
            )));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "owner-record helper readiness timed out",
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let probe = OpenOptions::new().read(true).write(true).open(&path)?;
    assert_eq!(file_owner_record_pid(&probe)?, Some(child.id()));
    drop(child.stdin.take());
    assert!(child.wait()?.success());
    assert_eq!(file_owner_record_pid(&probe)?, None);
    Ok(())
}

#[cfg(target_os = "macos")]
#[test]
fn posix_record_lock_conflicts_with_cross_process_flock_on_macos() -> io::Result<()> {
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let directory = tempfile::tempdir()?;
    let path = directory.path().join("lock");
    let ready = directory.path().join("ready");
    std::fs::write(&path, b"")?;
    let mut child = std::process::Command::new(std::env::current_exe()?)
        .args(["flock_owner_helper", "--nocapture"])
        .env(FLOCK_OWNER_HELPER_ENV, &path)
        .env("HAIDER_FLOCK_OWNER_READY", &ready)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + crate::TEST_WAIT_TIMEOUT;
    while !ready.exists() {
        if let Some(status) = child.try_wait()? {
            return Err(io::Error::other(format!(
                "flock-owner helper exited before readiness: {status}"
            )));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "flock-owner helper readiness timed out",
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let probe = OpenOptions::new().read(true).write(true).open(&path)?;
    assert!(matches!(
        try_lock_file_owner_record(&probe),
        Err(TryLockError::WouldBlock)
    ));
    drop(child.stdin.take());
    assert!(child.wait()?.success());
    try_lock_file_owner_record(&probe).map_err(io::Error::from)?;
    unlock_file_owner_record(&probe)?;
    Ok(())
}
