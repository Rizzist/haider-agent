//! Focused pins for hook acknowledgement retention transitions.

use super::*;

// MUTATION CHECK: omit removal of a previously blocked scope after its retry
// ACK commits. Expected failure: terminal cleanup remains ineligible forever.
#[test]
fn successful_retry_releases_ack_pending_retention() {
    let scope = (
        SessionId::new("ack-retention-session"),
        RunId::new("ack-retention-run"),
    );
    let mut blocked = HashSet::from([scope.clone()]);
    let ordered = HashSet::from([scope.clone()]);
    let terminal = HashSet::from([scope.clone()]);

    let terminal = reconcile_run_ack_retention(&mut blocked, ordered, terminal, true);

    assert!(
        !blocked.contains(&scope),
        "a committed retry ACK must release the retained scope"
    );
    assert!(
        terminal.contains(&scope),
        "terminal trust cleanup must resume after the retained scope is released"
    );
}

#[test]
fn failed_ack_flush_retains_the_ordered_scope() {
    let scope = (
        SessionId::new("ack-flush-session"),
        RunId::new("ack-flush-run"),
    );
    let mut blocked = HashSet::new();

    let terminal = reconcile_run_ack_retention(
        &mut blocked,
        HashSet::from([scope.clone()]),
        HashSet::from([scope.clone()]),
        false,
    );

    assert!(
        blocked.contains(&scope),
        "a failed durable flush must retain the scope"
    );
    assert!(
        terminal.is_empty(),
        "a failed flush cannot authorize cleanup"
    );
}

#[test]
fn current_page_handling_failure_keeps_a_previously_blocked_scope() {
    let scope = (
        SessionId::new("ack-handler-session"),
        RunId::new("ack-handler-run"),
    );
    let mut blocked = HashSet::new();
    let mut ordered = HashSet::from([scope.clone()]);
    retain_failed_run_ack(&mut blocked, &mut ordered, scope.clone());
    assert!(
        !ordered.contains(&scope),
        "a handling failure cannot remain in the successful ACK set"
    );
    assert!(
        blocked.contains(&scope),
        "the same failure must retain the scope for a later retry"
    );

    let terminal =
        reconcile_run_ack_retention(&mut blocked, ordered, HashSet::from([scope.clone()]), true);

    assert!(
        blocked.contains(&scope),
        "a handler failure in this page must keep the scope retained"
    );
    assert!(
        terminal.is_empty(),
        "blocked terminal cleanup must stay deferred"
    );
}
