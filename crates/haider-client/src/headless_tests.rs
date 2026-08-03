#![allow(clippy::expect_used)]

use super::headless_submit_body;
use haider_rpc::haider_protocol::ids::SessionId;
use haider_rpc::{CommandId, RequestBody};

/// MUTATION CHECK: ignore the run-scoped trust bit or change ordinary turn
/// bytes. Expected RUNTIME failure: the concrete request variant observed by
/// this production builder is wrong.
#[test]
fn submit_builder_selects_hook_trust_without_changing_ordinary_turns() {
    let ordinary = headless_submit_body(
        false,
        CommandId::new("ordinary-command"),
        SessionId::new("session"),
        7,
        "ordinary".into(),
        Vec::new(),
    );
    assert!(matches!(ordinary, RequestBody::TurnSubmit { .. }));

    let trusted = headless_submit_body(
        true,
        CommandId::new("trusted-command"),
        SessionId::new("session"),
        7,
        "trusted".into(),
        Vec::new(),
    );
    assert!(matches!(
        trusted,
        RequestBody::TurnSubmitWithHookTrust {
            branch_id: None,
            ..
        }
    ));
}
