#![allow(clippy::expect_used)]

//! Unit tests for the phase publisher itself.
//!
//! `tests/state_machine_tests.rs` pins the transition RELATION; these pin what
//! the publisher DOES with it, which is only reachable from inside the crate.

use super::*;

/// MUTATION CHECK: disable the guard in `StatePublisher::publish` (e.g.
/// `if false && !prior.can_transition_to(&state)`). Expected failure: the
/// illegal edge lands and `current()` returns `Stopped` instead of
/// `Recovering`. Verified 2026-07-27.
#[test]
fn publish_refuses_an_illegal_edge_in_every_build_and_keeps_the_last_legal_phase() {
    let (publisher, readiness) = StatePublisher::channel();
    assert_eq!(readiness.current(), DaemonState::Starting);

    // Legal: Starting -> Recovering.
    publisher.publish(DaemonState::Recovering);
    assert_eq!(readiness.current(), DaemonState::Recovering);

    // Illegal: Recovering -> Stopped is not an edge in the diagram. The
    // publisher must drop it rather than advertise a phase clients would
    // reason about wrongly — and it must do so with assertions compiled out.
    publisher.publish(DaemonState::Stopped);
    assert_eq!(
        readiness.current(),
        DaemonState::Recovering,
        "an illegal transition must leave the observable phase untouched"
    );

    // The publisher is still usable for the legal edge that follows.
    publisher.mark_store_open();
    publisher.mark_recovery_done();
    publisher.mark_providers_loaded();
    publisher.mark_session_hub_accepting_turns();
    publisher.publish(DaemonState::Ready);
    assert_eq!(readiness.current(), DaemonState::Ready);
    let snapshot = readiness.snapshot();
    assert!(snapshot.ready);
    assert!(snapshot.ready_since_unix_ms.is_some());
    assert!(snapshot.providers_loaded);

    assert!(publisher.publish(DaemonState::Draining {
        reason: "test".into(),
        deadline_unix_ms: 1,
    }));
    assert_eq!(
        readiness.snapshot(),
        DaemonReadinessSnapshot {
            ready: false,
            ready_since_unix_ms: None,
            providers_loaded: true,
        },
        "the positive predicate must fall when the live lifecycle leaves Ready"
    );
}

/// MUTATION CHECK: remove any prerequisite from `READY_PREREQUISITES`, or
/// let a bare `Recovering -> Ready` publication through. Expected failure:
/// the premature publication succeeds instead of remaining Recovering.
#[test]
fn ready_publication_requires_every_startup_prerequisite() {
    let prerequisites = ["store", "recovery", "providers", "session hub"];
    for (missing, name) in prerequisites.into_iter().enumerate() {
        let (publisher, readiness) = StatePublisher::channel();
        assert!(publisher.publish(DaemonState::Recovering));
        if missing != 0 {
            publisher.mark_store_open();
        }
        if missing != 1 {
            publisher.mark_recovery_done();
        }
        if missing != 2 {
            publisher.mark_providers_loaded();
        }
        if missing != 3 {
            publisher.mark_session_hub_accepting_turns();
        }

        assert!(
            !publisher.publish(DaemonState::Ready),
            "Ready publication must fail without {name}"
        );
        assert_eq!(readiness.current(), DaemonState::Recovering);
        assert_eq!(
            readiness.snapshot(),
            DaemonReadinessSnapshot {
                ready: false,
                ready_since_unix_ms: None,
                providers_loaded: missing != 2,
            }
        );
    }

    let (publisher, readiness) = StatePublisher::channel();
    assert!(publisher.publish(DaemonState::Recovering));
    publisher.mark_store_open();
    publisher.mark_recovery_done();
    publisher.mark_providers_loaded();
    publisher.mark_session_hub_accepting_turns();
    assert!(publisher.publish(DaemonState::Ready));
    assert!(readiness.snapshot().ready);
}

#[test]
fn publish_refuses_a_self_transition_and_every_edge_out_of_a_terminal_state() {
    let (publisher, readiness) = StatePublisher::channel();
    publisher.publish(DaemonState::Recovering);
    publisher.publish(DaemonState::Recovering);
    assert_eq!(readiness.current(), DaemonState::Recovering);

    publisher.publish(DaemonState::Failed {
        message: "boom".into(),
    });
    publisher.publish(DaemonState::Ready);
    publisher.publish(DaemonState::Stopped);
    assert_eq!(
        readiness.current(),
        DaemonState::Failed {
            message: "boom".into()
        },
        "Failed is terminal: nothing may publish past it"
    );
}

#[test]
fn launcher_death_is_retained_as_typed_idle_shutdown_reason() {
    let (shutdown, receiver, _) = ShutdownHandle::channel();
    assert!(shutdown.request_when_idle(ShutdownReason::ClientVanished));
    assert!(matches!(
        &*receiver.borrow(),
        ShutdownRequest::GracefulWhenIdle {
            reason: ShutdownReason::ClientVanished
        }
    ));
}

#[test]
fn launcher_death_can_arm_a_bounded_idle_linger() {
    let (shutdown, receiver, observer) = ShutdownHandle::channel();
    let idle_ttl = Duration::from_millis(250);
    assert!(shutdown.request_after_idle(ShutdownReason::ClientVanished, idle_ttl));
    assert!(matches!(
        &*receiver.borrow(),
        ShutdownRequest::GracefulAfterIdle {
            reason: ShutdownReason::ClientVanished,
            idle_ttl: observed,
        } if *observed == idle_ttl
    ));
    let selected_automatic_request = receiver.borrow().clone();

    shutdown.request_graceful();
    assert!(observer.operator_stop_requested());
    assert!(matches!(
        selected_automatic_request,
        ShutdownRequest::GracefulAfterIdle {
            reason: ShutdownReason::ClientVanished,
            ..
        }
    ));
    assert!(matches!(
        &*receiver.borrow(),
        ShutdownRequest::Graceful {
            reason: ShutdownReason::Message(reason)
        } if reason == "authenticated daemon.shutdown RPC"
    ));

    shutdown.request_graceful();
    assert!(matches!(
        &*receiver.borrow(),
        ShutdownRequest::Graceful {
            reason: ShutdownReason::Message(reason)
        } if reason == "authenticated daemon.shutdown RPC"
    ));
}
