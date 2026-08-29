#![allow(clippy::expect_used)]

use super::*;

/// MUTATION CHECK: classify two control attachments as outside-session or
/// select either one. Expected failure: the direct RPC could bypass the other
/// session's scope or provider-lockdown ceiling.
#[test]
fn direct_ssh_context_is_absent_unique_or_ambiguous() {
    let first = SessionId::new("session-first");
    let second = SessionId::new("session-second");
    assert_eq!(direct_ssh_session(&[]), DirectSshSession::OutsideSession);
    assert_eq!(
        direct_ssh_session(std::slice::from_ref(&first)),
        DirectSshSession::Session(&first)
    );
    assert_eq!(
        direct_ssh_session(&[first, second]),
        DirectSshSession::Ambiguous
    );
}
