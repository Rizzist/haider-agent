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
        owner_receipt.finish(NativeStatus::ShutdownForced, |_| ());
    });
    assert_eq!(receipt.wait_for(Duration::from_millis(1)), None);
    assert_eq!(receipt.get(), None);
    assert!(live.load(Ordering::Acquire));
    assert_eq!(receipt.wait_for(Duration::ZERO), None);
    stop.send(()).expect("release reader");
    assert_eq!(
        receipt.wait_for(Duration::from_secs(2)),
        Some(NativeStatus::ShutdownForced)
    );
    assert!(!live.load(Ordering::Acquire));
    owner.join().expect("owner completes");
    // Repeated shutdown reports the actual outcome, never the latest request.
    receipt.finish(NativeStatus::Ok, |_| ());
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
    receipt.finish(NativeStatus::StoreRecoveryFailed, |_| ());
    assert_eq!(
        waiter.join().expect("waiter"),
        Some(NativeStatus::StoreRecoveryFailed)
    );
    assert_eq!(receipt.get(), Some(NativeStatus::StoreRecoveryFailed));
}

#[test]
fn timeout_and_terminal_observation_publish_as_one_lifecycle_state() {
    let receipt = Arc::new(CompletionReceipt::new(Observation::phase("Ready", 19)));
    assert_eq!(receipt.wait_for(Duration::ZERO), None);
    let timeout = receipt.observe(|_| panic!("timeout stays latched until completion"));
    assert_eq!(timeout.error_code, Some("SHUTDOWN_TIMEOUT"));
    assert_eq!(timeout.daemon_generation, 19);

    let (publishing, began_publish) = mpsc::channel();
    let (release, finish_publish) = mpsc::channel();
    let owner_receipt = Arc::clone(&receipt);
    let owner = std::thread::spawn(move || {
        owner_receipt.finish(NativeStatus::Ok, |terminal| {
            assert_eq!(terminal.phase, "Stopped");
            publishing.send(()).expect("publish started");
            finish_publish.recv().expect("release final publication");
        });
    });
    began_publish
        .recv_timeout(Duration::from_secs(2))
        .expect("owner publishing");
    let waiting = Arc::clone(&receipt);
    let (entering, entered_wait) = mpsc::channel();
    let waiter = std::thread::spawn(move || {
        entering.send(()).expect("waiter starts");
        waiting.wait_for(Duration::ZERO)
    });
    entered_wait
        .recv_timeout(Duration::from_secs(2))
        .expect("waiter entering");
    release.send(()).expect("finish publication");
    owner.join().expect("owner");
    assert_eq!(waiter.join().expect("waiter"), Some(NativeStatus::Ok));
    receipt.fail(NativeStatus::Internal);
    let terminal = receipt.observe(|_| panic!("completed state cannot refresh"));
    assert_eq!(terminal.phase, "Stopped");
    assert_eq!(terminal.daemon_generation, 19);
    assert_eq!(terminal.error_code, None);
    assert_eq!(receipt.wait_for(Duration::ZERO), Some(NativeStatus::Ok));
}
