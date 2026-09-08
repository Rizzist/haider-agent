#![allow(clippy::expect_used)]
use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};

#[test]
fn timeout_retains_owner_and_waiters_observe_actual_quiescent_outcome() {
    let receipt = Arc::new(CompletionReceipt::default());
    let live = Arc::new(AtomicBool::new(true));
    let (stop, blocking_reader) = mpsc::channel();
    let owner_receipt = Arc::clone(&receipt);
    let owner_live = Arc::clone(&live);
    let owner = std::thread::spawn(move || {
        blocking_reader.recv().expect("explicit teardown release");
        owner_live.store(false, Ordering::Release);
        owner_receipt.finish(NativeStatus::ShutdownForced);
    });
    assert_eq!(receipt.wait_for(Duration::from_millis(1)), None);
    assert_eq!(receipt.get(), None);
    assert!(live.load(Ordering::Acquire));
    assert_eq!(receipt.wait_for(Duration::ZERO), None);
    assert_eq!(
        receipt.shutdown_result(Duration::ZERO),
        NativeStatus::ShutdownTimeout
    );
    stop.send(()).expect("release reader");
    assert_eq!(
        receipt.wait_for(Duration::from_secs(2)),
        Some(NativeStatus::ShutdownForced)
    );
    assert!(!live.load(Ordering::Acquire));
    owner.join().expect("owner completes");
    assert_eq!(
        receipt.shutdown_result(Duration::ZERO),
        NativeStatus::ShutdownForced
    );
    // Repeated shutdown reports the actual outcome, never the latest request.
    receipt.finish(NativeStatus::Ok);
    assert_eq!(
        receipt.wait_for(Duration::ZERO),
        Some(NativeStatus::ShutdownForced)
    );
}

#[test]
fn concurrent_waiters_share_the_same_failure_receipt() {
    let receipt = Arc::new(CompletionReceipt::default());
    let waiting = Arc::clone(&receipt);
    let waiter = std::thread::spawn(move || waiting.wait_for(Duration::from_secs(2)));
    receipt.finish(NativeStatus::StoreRecoveryFailed);
    assert_eq!(
        waiter.join().expect("waiter"),
        Some(NativeStatus::StoreRecoveryFailed)
    );
    assert_eq!(receipt.get(), Some(NativeStatus::StoreRecoveryFailed));
    assert_eq!(receipt.shutdown_result(Duration::ZERO), NativeStatus::Ok);
    assert_eq!(receipt.get(), Some(NativeStatus::StoreRecoveryFailed));
}
