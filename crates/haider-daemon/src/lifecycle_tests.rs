//! Unit tests for the phase publisher itself.
//!
//! `tests/state_machine_tests.rs` pins the transition RELATION; these pin what
//! the publisher DOES with it, which is only reachable from inside the crate.

#![allow(clippy::expect_used)]

use super::*;

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
    publisher.publish(DaemonState::Ready);
    assert_eq!(readiness.current(), DaemonState::Ready);
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
