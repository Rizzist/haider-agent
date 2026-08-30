#![allow(clippy::expect_used)]

use super::{SyncOperation, SyncPolicy, sync_directory, sync_file, with_sync_test_hook};
use std::cell::RefCell;
use std::rc::Rc;

fn selected_file_operation(policy: SyncPolicy) -> SyncOperation {
    let operations = Rc::new(RefCell::new(Vec::new()));
    let observed = Rc::clone(&operations);
    let file = tempfile::tempfile().expect("temporary sync target");
    with_sync_test_hook(
        move |operation| observed.borrow_mut().push(operation),
        || sync_file(&file, policy).expect("intercept file sync"),
    );
    let operations = operations.borrow();
    assert_eq!(operations.len(), 1, "one policy selects one operation");
    operations[0]
}

#[cfg(target_vendor = "apple")]
#[test]
fn apple_sync_policies_select_full_barrier_and_plain_operations() {
    assert_eq!(
        selected_file_operation(SyncPolicy::Full),
        SyncOperation::FullFsync
    );
    assert_eq!(
        selected_file_operation(SyncPolicy::Barrier),
        SyncOperation::BarrierFsync
    );
    assert_eq!(
        selected_file_operation(SyncPolicy::Plain),
        SyncOperation::Fsync
    );
}

#[cfg(all(unix, not(target_vendor = "apple")))]
#[test]
fn non_apple_unix_sync_policies_all_select_fsync() {
    for policy in [SyncPolicy::Full, SyncPolicy::Barrier, SyncPolicy::Plain] {
        assert_eq!(selected_file_operation(policy), SyncOperation::Fsync);
    }
}

#[cfg(windows)]
#[test]
fn windows_file_policies_select_sync_all_and_directory_is_noop() {
    for policy in [SyncPolicy::Full, SyncPolicy::Barrier, SyncPolicy::Plain] {
        assert_eq!(selected_file_operation(policy), SyncOperation::SyncAll);
    }

    let operations = Rc::new(RefCell::new(Vec::new()));
    let observed = Rc::clone(&operations);
    let directory = tempfile::tempdir().expect("temporary directory sync target");
    with_sync_test_hook(
        move |operation| observed.borrow_mut().push(operation),
        || sync_directory(directory.path(), SyncPolicy::Full).expect("intercept directory sync"),
    );
    assert_eq!(*operations.borrow(), [SyncOperation::Noop]);
}

#[cfg(unix)]
#[test]
fn unix_directory_sync_uses_the_requested_file_policy() {
    let operations = Rc::new(RefCell::new(Vec::new()));
    let observed = Rc::clone(&operations);
    let directory = tempfile::tempdir().expect("temporary directory sync target");
    with_sync_test_hook(
        move |operation| observed.borrow_mut().push(operation),
        || sync_directory(directory.path(), SyncPolicy::Plain).expect("intercept directory sync"),
    );
    assert_eq!(*operations.borrow(), [SyncOperation::Fsync]);
}
