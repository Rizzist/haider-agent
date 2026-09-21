use super::*;
use std::fs::OpenOptions;

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
    try_lock_file_exclusive(&second).map_err(io::Error::from)?;
    assert!(matches!(
        try_lock_file_exclusive(&first),
        Err(TryLockError::WouldBlock)
    ));
    drop(second);
    try_lock_file_exclusive(&first).map_err(io::Error::from)?;
    unlock_file(&first)
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
    let deadline = Instant::now() + Duration::from_secs(5);
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
    let deadline = Instant::now() + Duration::from_secs(5);
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
