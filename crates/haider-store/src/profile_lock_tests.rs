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
