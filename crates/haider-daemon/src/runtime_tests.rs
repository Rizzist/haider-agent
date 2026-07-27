//! Unit tests for the drain barrier's arbitration helpers.
//!
//! These are crate-internal on purpose: `bounded_finalization` and
//! `barrier_breached` decide whether a shutdown may call itself graceful, and
//! the interesting cases (a step that completes exactly as the deadline
//! passes, a second signal that arrives during a step) are driven far more
//! precisely with synthetic futures than with a real daemon.

#![allow(clippy::expect_used)]

use super::*;
use std::future::pending;
use std::time::Duration;
use tokio::sync::watch;

fn shutdown_channel() -> (
    watch::Sender<ShutdownRequest>,
    watch::Receiver<ShutdownRequest>,
) {
    watch::channel(ShutdownRequest::Graceful {
        reason: "test drain".into(),
    })
}

#[tokio::test]
async fn a_step_that_completes_is_not_evidence_that_the_barrier_held() {
    let (_sender, shutdown) = shutdown_channel();
    let expired = tokio::time::Instant::now() - Duration::from_secs(1);
    let mut receiver = shutdown.clone();

    // The work is ready on the very first poll — the step DID complete, and its
    // result must be returned rather than thrown away...
    let completed = bounded_finalization(std::future::ready(7_u8), expired, &mut receiver).await;
    assert_eq!(completed, Some(7));
    // ...but the caller's arbitration is what decides the outcome, and it must
    // see the breach that the completed step said nothing about.
    assert!(
        barrier_breached(expired, &shutdown),
        "an expired deadline must be visible to post-step arbitration"
    );
}

#[tokio::test]
async fn a_pending_step_stops_at_the_deadline() {
    let (_sender, mut shutdown) = shutdown_channel();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(20);
    let abandoned = bounded_finalization(pending::<()>(), deadline, &mut shutdown).await;
    assert!(
        abandoned.is_none(),
        "the barrier deadline must end the wait"
    );
}

#[tokio::test]
async fn a_second_signal_during_a_step_forces_the_outcome() {
    let (sender, mut shutdown) = shutdown_channel();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    tokio::spawn(async move {
        sender.send_replace(ShutdownRequest::Forced {
            reason: "second signal".into(),
        });
        // Hold the sender so the channel stays open: this is a force, not a
        // dropped controller.
        std::future::pending::<()>().await;
    });

    let abandoned = bounded_finalization(pending::<()>(), deadline, &mut shutdown).await;
    assert!(
        abandoned.is_none(),
        "a force arriving mid-step must abandon the wait well before the deadline"
    );
}

/// MUTATION CHECK: drop the force arm from `barrier_breached` (leave only the
/// deadline comparison). Expected failure: the force delivered without any
/// `changed()` poll goes unseen and this assertion fails. Verified 2026-07-27.
#[tokio::test]
async fn a_force_that_arrives_during_a_synchronous_step_is_still_observed() {
    let (sender, shutdown) = shutdown_channel();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    assert!(!barrier_breached(deadline, &shutdown));

    // Nothing polls `changed()` here — this is the synchronous-step case (the
    // endpoint cleanup): arbitration must read the watch VALUE.
    sender.send_replace(ShutdownRequest::Forced {
        reason: "during cleanup".into(),
    });
    assert!(
        barrier_breached(deadline, &shutdown),
        "a force delivered while a synchronous step ran must still be seen"
    );
}

#[tokio::test]
async fn a_dropped_controller_is_not_a_second_signal() {
    let (sender, mut shutdown) = shutdown_channel();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(50);
    drop(sender);

    // Losing the ability to receive a force is not a force: the step keeps its
    // deadline, and work that finishes inside it still counts as completed.
    let completed = bounded_finalization(
        async {
            tokio::time::sleep(Duration::from_millis(5)).await;
            9_u8
        },
        deadline,
        &mut shutdown,
    )
    .await;
    assert_eq!(completed, Some(9));
}

#[tokio::test]
async fn work_finishing_inside_the_barrier_completes_normally() {
    let (_sender, mut shutdown) = shutdown_channel();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let completed = bounded_finalization(
        async {
            tokio::task::yield_now().await;
            "done"
        },
        deadline,
        &mut shutdown,
    )
    .await;
    assert_eq!(completed, Some("done"));
    assert!(!barrier_breached(deadline, &shutdown));
}
