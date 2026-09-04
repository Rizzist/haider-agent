#![allow(clippy::expect_used)]

use std::ffi::OsStr;

use haider_rpc::{
    ModelInventoryAuthorityWire, ProviderApiFamilyWire, ProviderAvailabilityWire,
    ProviderSummaryWire, ProviderTrustWire,
};

use super::auto_hermetic::{ProviderLockdownPolicy, provider_policy_with_override, tools_for};

fn summary() -> ProviderSummaryWire {
    ProviderSummaryWire {
        provider: "local-benchmark".into(),
        api_family: ProviderApiFamilyWire::OpenAiChatCompletions,
        endpoint: Some("http://127.0.0.1:9000/v1".into()),
        response_open_timeout_ms: None,
        chunk_idle_timeout_ms: None,
        semantic_progress_timeout_ms: None,
        models: vec!["bench".into()],
        model_details: Vec::new(),
        inventory_fetched_at_ms: None,
        inventory_authority: ModelInventoryAuthorityWire::Advisory,
        auth_methods: Vec::new(),
        availability: ProviderAvailabilityWire::Available,
        availability_reason: None,
        default_model: Some("bench".into()),
        enabled: true,
        trust: ProviderTrustWire::Full,
    }
}

/// Boundary pin: only the ACTIVE provider summary reaches this function. The
/// trigger itself is the exact keyless resolver shape, never the existence of
/// some unrelated keyless provider in the profile.
#[test]
fn enabled_custom_no_auth_endpoint_activates_the_strict_envelope() {
    let policy = provider_policy_with_override(Some(&summary()), false, None);
    assert_eq!(policy, ProviderLockdownPolicy::AutoHermetic);
    let (lockdown, auto_hermetic) = policy.binding_bits();
    assert_eq!((lockdown, auto_hermetic), (true, true));
    assert_eq!(
        ProviderLockdownPolicy::from_binding(lockdown, auto_hermetic),
        ProviderLockdownPolicy::AutoHermetic,
        "the durable binding must restore the exact no-egress policy"
    );
    assert_eq!(
        policy.activation(true),
        Some(haider_rpc::LockdownActivationWire::AutoHermetic)
    );
    assert_eq!(
        policy.activation(false),
        Some(haider_rpc::LockdownActivationWire::AutoHermeticEligible)
    );
    assert_eq!(
        policy.reason(true),
        Some(super::auto_hermetic::AUTO_HERMETIC_REASON)
    );
    assert_eq!(
        policy.reason(false),
        Some(super::auto_hermetic::AUTO_HERMETIC_ELIGIBLE_REASON)
    );
    let tools = tools_for(policy);
    assert!(tools.iter().any(|tool| tool == "fs_read"));
    for egress in [
        "web_search",
        "web_fetch",
        "peer_list",
        "ssh_list",
        "spawn_subagent",
        "list_models",
    ] {
        assert!(!tools.iter().any(|tool| tool == egress), "{egress}");
    }
}

#[test]
fn trigger_boundaries_exclude_keyed_disabled_builtin_and_unsupported_profiles() {
    let mut keyed = summary();
    keyed.auth_methods = vec![haider_protocol::credential::AuthMethod::ApiKey];
    assert_eq!(
        provider_policy_with_override(Some(&keyed), false, None),
        ProviderLockdownPolicy::Full
    );

    let mut disabled = summary();
    disabled.enabled = false;
    assert_eq!(
        provider_policy_with_override(Some(&disabled), false, None),
        ProviderLockdownPolicy::Full
    );

    let mut builtin = summary();
    builtin.provider = haider_provider::ANTHROPIC_PROVIDER_NAME.into();
    assert_eq!(
        provider_policy_with_override(Some(&builtin), false, None),
        ProviderLockdownPolicy::Full
    );

    let mut unsupported = summary();
    unsupported.api_family = ProviderApiFamilyWire::GeminiGenerateContent;
    assert_eq!(
        provider_policy_with_override(Some(&unsupported), false, None),
        ProviderLockdownPolicy::Full
    );

    assert_eq!(
        provider_policy_with_override(Some(&summary()), true, None),
        ProviderLockdownPolicy::Full,
        "a stored active credential wins over the profile's no-auth capability"
    );
}

#[test]
fn exact_zero_override_disables_only_automatic_lockdown() {
    assert_eq!(
        provider_policy_with_override(Some(&summary()), false, Some(OsStr::new("0"))),
        ProviderLockdownPolicy::AutoHermeticDisabled
    );
    assert_eq!(
        provider_policy_with_override(Some(&summary()), false, Some(OsStr::new("false"))),
        ProviderLockdownPolicy::AutoHermetic
    );

    let mut configured = summary();
    configured.trust = ProviderTrustWire::Lockdown;
    assert_eq!(
        provider_policy_with_override(Some(&configured), false, None),
        ProviderLockdownPolicy::AutoHermetic,
        "configured lockdown must not weaken the automatic no-egress floor"
    );
    assert_eq!(
        provider_policy_with_override(Some(&configured), false, Some(OsStr::new("0"))),
        ProviderLockdownPolicy::Configured
    );
}
