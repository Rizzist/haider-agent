//! Pins the lifecycle transition diagram documented on `DaemonState`.
//!
//! The relation is contract for W3b2+ (clients reason about phases through
//! `Welcome`/readiness), so the full matrix is asserted: the documented
//! forward and failure edges are legal, everything else — including
//! self-transitions and any edge out of `Failed`/`Stopped` — is not.

use haider_daemon::DaemonState;

fn all_states() -> Vec<DaemonState> {
    vec![
        DaemonState::Starting,
        DaemonState::Recovering,
        DaemonState::Ready,
        DaemonState::Draining {
            reason: "test".into(),
            deadline_unix_ms: 1,
        },
        DaemonState::Failed {
            message: "test".into(),
        },
        DaemonState::Stopped,
    ]
}

#[test]
fn lifecycle_permits_exactly_the_documented_forward_and_failure_edges() {
    const STARTING: usize = 0;
    const RECOVERING: usize = 1;
    const READY: usize = 2;
    const DRAINING: usize = 3;
    const FAILED: usize = 4;
    const STOPPED: usize = 5;
    let legal = [
        (STARTING, RECOVERING),
        (STARTING, DRAINING), // shutdown before the profile lock
        (STARTING, FAILED),
        (RECOVERING, READY),
        (RECOVERING, DRAINING), // shutdown before the listener
        (RECOVERING, FAILED),
        (READY, DRAINING),
        (READY, FAILED),
        (DRAINING, STOPPED),
        (DRAINING, FAILED),
    ];

    let states = all_states();
    for (from_index, from) in states.iter().enumerate() {
        for (to_index, to) in states.iter().enumerate() {
            assert_eq!(
                from.can_transition_to(to),
                legal.contains(&(from_index, to_index)),
                "transition {from:?} -> {to:?} disagrees with the documented diagram"
            );
        }
    }
}
