#![allow(clippy::expect_used)]

use super::{PermissionPolicy, PolicyDecision, denied_effect_error};
use haider_protocol::effect::{EffectClass, EffectIntent};
use haider_protocol::ids::EffectId;

fn process_intent() -> EffectIntent {
    EffectIntent {
        effect: EffectId::new("effect-lockdown"),
        class: EffectClass::ProcessExec,
        summary: "run arbitrary command".to_owned(),
        args_digest: "digest-lockdown".to_owned(),
        workspace_revision: None,
    }
}

/// MUTATION CHECK: evaluate the ordinary allowlist before `hard_denylist`, or
/// let `set(Allow)` clear the hard rule. Expected failure: either assertion
/// observes Allow and a user grant has lifted the daemon ceiling.
#[test]
fn hard_ceiling_wins_even_after_user_allow() {
    let intent = process_intent();
    let mut policy = PermissionPolicy::default();
    policy.hard_deny(
        EffectClass::ProcessExec,
        "provider lockdown cannot be lifted",
    );
    policy.allow(EffectClass::ProcessExec);
    policy.always_allow(&intent);

    assert!(matches!(
        policy.decision(&intent),
        PolicyDecision::Deny { ref reason }
            if reason == "provider lockdown cannot be lifted"
    ));
    policy.set(EffectClass::ProcessExec, PolicyDecision::Allow);
    assert!(matches!(
        policy.decision(&intent),
        PolicyDecision::Deny { .. }
    ));
    assert!(matches!(
        denied_effect_error(&policy, &intent, "provider lockdown cannot be lifted".into()),
        crate::ToolError::RefusedByLockdown { ref tool, .. } if tool == "process_exec"
    ));
}

#[test]
fn peer_send_hard_ceiling_wins_over_user_allow() {
    let intent = EffectIntent {
        effect: EffectId::new("effect-lockdown-peer"),
        class: EffectClass::PeerMessage,
        summary: "send to a peer".to_owned(),
        args_digest: "digest-lockdown-peer".to_owned(),
        workspace_revision: None,
    };
    let mut policy = PermissionPolicy::default();
    policy.hard_deny(
        EffectClass::PeerMessage,
        "provider lockdown cannot peer-send",
    );
    policy.allow(EffectClass::PeerMessage);

    assert!(matches!(
        denied_effect_error(&policy, &intent, "provider lockdown cannot peer-send".into()),
        crate::ToolError::RefusedByLockdown { ref tool, .. } if tool == "peer_send"
    ));
}
