use super::*;

/// The approval id is also the provider idempotency key and the durable
/// replay key, so its derivation is pinned to the published formula.
fn reference_approval_id(parts: &[&str]) -> String {
    let mut identity = blake3::Hasher::new();
    for part in parts {
        identity.update(&(part.len() as u64).to_be_bytes());
        identity.update(part.as_bytes());
    }
    format!("usage-reset-{}", identity.finalize().to_hex())
}

#[test]
fn approval_ids_are_stable_and_length_prefixed() {
    let rpc = ["session-a", "command-a"];
    let tool = ["session-a", "run-a", "call-a", "work-chatgpt"];
    assert_eq!(approval_id(&rpc), reference_approval_id(&rpc));
    assert_eq!(approval_id(&tool), reference_approval_id(&tool));
    assert_eq!(approval_id(&rpc), approval_id(&rpc));
    assert_ne!(
        approval_id(&["ab", "c"]),
        approval_id(&["a", "bc"]),
        "concatenation-equal coordinates must not collide"
    );
    assert!(approval_id(&rpc).starts_with("usage-reset-"));
}

#[test]
fn journal_identifiers_derive_from_the_approval_id() {
    let approval = "usage-reset-abc";
    assert_eq!(menu_id(approval).as_str(), "deferred-usage-reset-abc");
    assert_eq!(effect_id(approval).as_str(), "effect-usage-reset-abc");
    assert_eq!(
        event_id("dispatched", approval).as_str(),
        "usage-reset-dispatched-usage-reset-abc"
    );
    assert_eq!(notification_id(approval), "usage-reset-abc");
}

#[test]
fn approval_menu_is_nonblocking_human_only_and_secret_free() {
    let account = CredentialAlias::new("work-chatgpt");
    let menu = approval_menu("usage-reset-abc", &account);
    assert_eq!(menu.id, menu_id("usage-reset-abc"));
    assert_eq!(
        menu.kind,
        MenuKind::DeferredApproval {
            approval_id: "usage-reset-abc".into(),
            effect: EffectClass::UsageResetApply,
            account: Some(account.clone()),
            binding: None,
        }
    );
    assert!(!menu.blocking);
    assert_eq!(
        menu.ttl_ms, None,
        "the decision never expires into a default"
    );
    assert_eq!(menu.timeout_option, None);
    let options = menu
        .options
        .iter()
        .map(|option| (option.key.as_str(), option.decision))
        .collect::<Vec<_>>();
    assert_eq!(
        options,
        [
            (DENY_OPTION_KEY, Some(DecisionKind::RejectOnce)),
            (APPROVE_OPTION_KEY, Some(DecisionKind::AllowOnce)),
        ]
    );
    assert_eq!(menu.body[0], "Account: work-chatgpt");

    let intent = intent("usage-reset-abc", &account);
    assert_eq!(intent.effect, effect_id("usage-reset-abc"));
    assert_eq!(intent.class, EffectClass::UsageResetApply);
    assert_eq!(
        intent.args_digest,
        format!("blake3:{}", blake3::hash(b"work-chatgpt").to_hex())
    );
}

#[test]
fn reset_policy_denial_precedes_a_deferred_menu_even_with_auto_allow() {
    let account = CredentialAlias::new("work-chatgpt");
    let intent = intent("usage-reset-abc", &account);
    let mut policy = haider_tools::PermissionPolicy::default();
    policy.auto_allow_asks();
    assert_eq!(policy.denial_reason(&intent), None);
    policy.deny(EffectClass::UsageResetApply, "account reset denied");
    assert_eq!(
        policy.denial_reason(&intent).as_deref(),
        Some("account reset denied")
    );
}

#[test]
fn bound_reset_intent_and_menu_pin_account_id_and_incarnation() {
    let alias = CredentialAlias::new("work-chatgpt");
    let binding = haider_protocol::credential::UsageResetAccountBinding {
        provider: "openai-oauth".into(),
        account_id: "workspace-a".into(),
        incarnation: 7,
    };
    let menu = bound_approval_menu("approval-a", &alias, Some(&binding));
    assert!(matches!(menu.kind, MenuKind::DeferredApproval {
        binding: Some(ref captured), ..
    } if captured == &binding));
    assert!(menu.body[0].contains("workspace-a"));
    let first = bound_intent("approval-a", &alias, Some(&binding));
    let mut changed = binding.clone();
    changed.incarnation += 1;
    assert_ne!(
        first.args_digest,
        bound_intent("approval-a", &alias, Some(&changed)).args_digest
    );
    changed = binding.clone();
    changed.account_id = "workspace-b".into();
    assert_ne!(
        first.args_digest,
        bound_intent("approval-a", &alias, Some(&changed)).args_digest
    );
    for account_id in ["WORKSPACE-A", "workspace-a ", "workspace-a\0"] {
        changed.account_id = account_id.into();
        assert_ne!(
            first.args_digest,
            bound_intent("approval-a", &alias, Some(&changed)).args_digest
        );
    }
    let long = "same-label-prefix-1234567890-a";
    changed.account_id = long.into();
    let long_intent = bound_intent("approval-a", &alias, Some(&changed));
    changed.account_id = "same-label-prefix-1234567890-b".into();
    assert_ne!(
        long_intent.args_digest,
        bound_intent("approval-a", &alias, Some(&changed)).args_digest
    );
}

/// LAW: only the explicit `approve` key approves. A keyless wire answer is
/// rejected before this helper, so a positional default cannot spend.
///
/// MUTATION CHECK: map a keyless index-0 (or index-1) answer to approve.
#[test]
fn only_the_explicit_approve_key_approves() {
    assert_eq!(answered_option_key(Some("approve")), APPROVE_OPTION_KEY);
    assert_eq!(answered_option_key(Some("deny")), DENY_OPTION_KEY);
    assert_eq!(answered_option_key(None), DENY_OPTION_KEY);
    assert_eq!(answered_option_key(Some("other")), "other");
    let menu = approval_menu("usage-reset-abc", &CredentialAlias::new("work-chatgpt"));
    assert_eq!(
        menu.options[0].key, DENY_OPTION_KEY,
        "deny is the default option 0"
    );
    assert_eq!(menu.options[1].key, APPROVE_OPTION_KEY);
}

#[test]
fn unresolved_status_never_reports_an_unconfirmed_spend_as_applied() {
    let account = CredentialAlias::new("work-chatgpt");
    let status = |answer: Option<&str>,
                  answer_generation: Option<u64>,
                  dispatched_generation: Option<u64>| {
        let facts = UnresolvedApproval {
            answer: answer.map(str::to_owned),
            answer_generation,
            dispatched_generation,
        };
        let result = unresolved_status("usage-reset-abc", &account, &facts, 7);
        assert_eq!(result.approval_id, "usage-reset-abc");
        assert_eq!(result.account, account);
        assert_eq!(result.windows_reset, None);
        (result.status, result.reason)
    };
    use UsageResetApplyStatusV1::{ApprovalPending, Denied, Failed};
    assert_eq!(status(None, None, None), (ApprovalPending, None));
    assert_eq!(status(Some("deny"), Some(7), None), (Denied, None));
    assert_eq!(
        status(Some("approve"), Some(7), None),
        (ApprovalPending, None),
        "the approving RPC is still finishing in this generation"
    );
    assert_eq!(
        status(Some("approve"), Some(6), None),
        (
            Failed,
            Some("approval_interrupted_before_dispatch".to_owned())
        )
    );
    assert_eq!(
        status(Some("approve"), Some(7), Some(7)),
        (ApprovalPending, None)
    );
    assert_eq!(
        status(Some("approve"), Some(6), Some(6)),
        (
            ApprovalPending,
            Some("provider_outcome_unconfirmed".to_owned())
        ),
        "an ambiguous post-restart dispatch is reported, never retried"
    );
}

#[test]
fn failed_results_carry_only_the_bounded_reason() {
    let account = CredentialAlias::new("work-chatgpt");
    let result = failed("usage-reset-abc", &account, "no_reset_credit");
    assert_eq!(result.status, UsageResetApplyStatusV1::Failed);
    assert_eq!(result.reason.as_deref(), Some("no_reset_credit"));
    assert_eq!(result.windows_reset, None);
}
