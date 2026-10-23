#![allow(clippy::expect_used)]

use super::ProfileLock;
use haider_protocol::error::ErrorCode;

#[test]
fn profile_lock_remains_exclusive_across_two_openers() {
    let root = tempfile::tempdir().expect("profile root");
    let first = ProfileLock::acquire(root.path()).expect("first profile opener");
    let error = match ProfileLock::acquire(root.path()) {
        Ok(_) => panic!("second opener must be fenced"),
        Err(error) => error,
    };
    assert_eq!(error.code, ErrorCode::StoreLocked);

    drop(first);
    let reopened = ProfileLock::acquire(root.path()).expect("lock released with first opener");
    drop(reopened);
}

#[cfg(unix)]
#[test]
#[allow(unsafe_code)]
fn forked_profile_guard_cannot_remove_the_live_owners_diagnostics() {
    let root = tempfile::tempdir().expect("profile root");
    let mut owner = ProfileLock::acquire(root.path()).expect("parent profile lock");
    // SAFETY: the child invokes only the release body, which returns after getpid,
    // and _exit. It never drops heap-backed fields or touches the local mutex.
    let pid = unsafe {
        let pid = libc::fork();
        if pid == 0 {
            libc::alarm(60);
            owner.release();
            libc::_exit(0);
        }
        pid
    };
    assert!(
        pid > 0,
        "fork owned fixture: {}",
        std::io::Error::last_os_error()
    );
    let mut status = 0;
    // SAFETY: reap exactly our forked child; status is writable.
    assert_eq!(unsafe { libc::waitpid(pid, &raw mut status, 0) }, pid);
    assert_eq!(status, 0);
    assert!(
        root.path().join("lock.owner").exists(),
        "child must not remove parent diagnostics"
    );
    assert!(
        ProfileLock::acquire(root.path()).is_err(),
        "live parent remains exclusive"
    );
    drop(owner);
    assert!(!root.path().join("lock.owner").exists());
    drop(ProfileLock::acquire(root.path()).expect("parent release permits reopen"));
}
