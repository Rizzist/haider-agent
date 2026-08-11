#![allow(clippy::expect_used)]

use haider_protocol::cache::{
    CacheEpochTransitionReason, CacheEpochTransitionV1, CachePolicyMode, CachePolicySettingsV1,
};
use haider_protocol::provider::UsageScope;
use haider_protocol::session::SessionMetadataV1;

use crate::cache_policy::{assess_cache_change, blocks_change, combine_cache_change_warnings};

fn metadata(mode: CachePolicyMode, threshold: u64) -> SessionMetadataV1 {
    SessionMetadataV1 {
        cwd: "/tmp".into(),
        provider: "openai".into(),
        model: "gpt-5.6-terra".into(),
        max_tokens: 4096,
        system_prompt_version: Some("haider-system-v2".into()),
        permission_overrides: None,
        title: None,
        effort: None,
        fast: false,
        cache_policy: CachePolicySettingsV1 {
            mode,
            cold_cost_threshold_microusd: threshold,
        },
        created_at_ms: 1,
    }
}

fn scope(tokens: u64, auth: &str) -> UsageScope {
    UsageScope {
        provider: "openai".into(),
        model: "gpt-5.6-terra".into(),
        account_scope: None,
        auth_scope: auth.into(),
        cache_epoch: "warm-epoch".into(),
        stable_prefix_tokens: tokens,
        cache_boundaries: None,
        request_kind: Default::default(),
        run: None,
        agent: None,
        prefix_digests: None,
    }
}

/// LAW (CM3c): economy gates every destructive switch, balanced gates at
/// its configurable threshold and is the default, while mobility permits
/// the switch but still returns the surfaced cost assessment.
///
/// MUTATION CHECK (executed): make `CachePolicySettingsV1::default()` select
/// mobility; the default-mode assertion fails.
#[test]
fn cm3c_policy_modes_gate_or_surface_registry_cost() {
    assert_eq!(
        CachePolicySettingsV1::default().mode,
        CachePolicyMode::Balanced
    );
    let warm = scope(1_000_000, "api_key");
    let fields = vec!["model".to_owned()];

    let economy = assess_cache_change(
        &metadata(CachePolicyMode::Economy, u64::MAX),
        Some(&warm),
        "openai",
        "gpt-5.6-terra",
        Some("api_key"),
        fields.clone(),
        false,
    )
    .expect("economy assessment");
    assert!(economy.confirmation_required);
    assert_eq!(economy.invalidated_stable_tokens, 1_000_000);
    assert_eq!(economy.rewarm_cost_microusd, Some(2_300_000));

    let balanced_below = assess_cache_change(
        &metadata(CachePolicyMode::Balanced, 3_000_000),
        Some(&warm),
        "openai",
        "gpt-5.6-terra",
        Some("api_key"),
        fields.clone(),
        false,
    )
    .expect("balanced assessment");
    assert!(!balanced_below.confirmation_required);

    let mobility = assess_cache_change(
        &metadata(CachePolicyMode::Mobility, 0),
        Some(&warm),
        "openai",
        "gpt-5.6-terra",
        Some("api_key"),
        fields,
        false,
    )
    .expect("mobility assessment");
    assert!(!mobility.confirmation_required);
    assert_eq!(mobility.rewarm_cost_microusd, Some(2_300_000));
}

/// LAW (CM3a/CM3e): effort/thinking and fast/speed remain pinned in a warm
/// epoch under every policy, but an explicit confirmation can create the new
/// epoch; the assessment never represents a permanent refusal.
///
/// MUTATION CHECK (executed): make `blocks_change` always return false; the
/// unconfirmed-block assertion fails, demonstrating the silent-change kill.
#[test]
fn cm3a_tuning_is_gated_and_cm3e_confirmation_is_reversible() {
    let warning = assess_cache_change(
        &metadata(CachePolicyMode::Mobility, u64::MAX),
        Some(&scope(50_000, "api_key")),
        "openai",
        "gpt-5.6-terra",
        Some("api_key"),
        vec!["effort/thinking".into()],
        true,
    )
    .expect("warm tuning assessment");
    assert!(blocks_change(&warning, false));
    assert!(
        !blocks_change(&warning, true),
        "explicit new-epoch confirmation restores the ability to change"
    );
    assert!(warning.message().contains("repeat the same selection"));
    // The caller's confirmed second request bypasses only this preflight and
    // commits the requested setting; no setting/model is permanently pinned.
}

/// LAW (OAuth cost): subscription lanes retain tokens/equivalents and expose
/// only a clearly labeled hypothetical API-rate estimate.
#[test]
fn oauth_cache_warning_shows_labeled_api_rate_rewarm_equivalent() {
    let warning = assess_cache_change(
        &metadata(CachePolicyMode::Economy, 0),
        Some(&scope(1_000_000, "oauth_subscription")),
        "openai",
        "gpt-5.6-terra",
        Some("oauth_subscription"),
        vec!["model".into()],
        false,
    )
    .expect("oauth assessment");
    assert_eq!(warning.rewarm_cost_microusd, None);
    assert!(warning.rewarm_api_equivalent_cost_microusd.is_some());
    assert_eq!(warning.rewarm_base_input_equivalent_tokens, Some(1_150_000));
    assert!(warning.message().contains("≈$"));
    assert!(warning.message().contains("API rate (plan)"));
}

/// Account selection is profile-global, so its preflight adds the impact of
/// every warmed session using that provider before asking once.
#[test]
fn cm3c_account_switch_warning_aggregates_warmed_sessions() {
    let warnings = [100_000, 250_000]
        .into_iter()
        .map(|tokens| {
            assess_cache_change(
                &metadata(CachePolicyMode::Economy, 0),
                Some(&scope(tokens, "api_key")),
                "openai",
                "gpt-5.6-terra",
                Some("api_key"),
                vec!["account".into(), "auth".into()],
                false,
            )
            .expect("account warning")
        })
        .collect();
    let combined = combine_cache_change_warnings(warnings).expect("combined warning");
    assert!(combined.confirmation_required);
    assert_eq!(combined.invalidated_stable_tokens, 350_000);
    assert_eq!(combined.rewarm_cost_microusd, Some(805_000));
    assert_eq!(combined.rewarm_base_input_equivalent_tokens, Some(402_500));
    assert_eq!(combined.changed_fields, ["account", "auth"]);
}

/// LAW (CM3d): every non-compaction cause is named as a cold transition;
/// compaction is explicitly planned and never described as a failure.
///
/// MUTATION CHECK (executed): map `InstructionsChanged` to `cache miss`; its
/// exact named-transition assertion fails.
#[test]
fn cm3d_named_cache_busts_and_planned_compaction_labels() {
    let cases = [
        (
            CacheEpochTransitionReason::InstructionsChanged,
            "instructions changed",
        ),
        (
            CacheEpochTransitionReason::ToolPackChanged,
            "tool pack changed",
        ),
        (
            CacheEpochTransitionReason::SystemVersionChanged,
            "system version changed",
        ),
        (
            CacheEpochTransitionReason::WebToolDegradation,
            "web tool degraded",
        ),
    ];
    for (reason, name) in cases {
        let label = CacheEpochTransitionV1 {
            reason,
            planned: false,
            changed_fields: Vec::new(),
            invalidated_stable_tokens: 42,
            rewarm_cost_usd: None,
            rewarm_base_input_equivalent_tokens: None,
            transition_id: String::new(),
            from_cache_epoch: None,
            to_cache_epoch: None,
        }
        .display_label();
        assert!(label.contains(name), "{label}");
        assert!(label.contains("next turn cold"), "{label}");
    }
    let compaction = CacheEpochTransitionV1 {
        reason: CacheEpochTransitionReason::Compaction,
        planned: true,
        changed_fields: Vec::new(),
        invalidated_stable_tokens: 0,
        rewarm_cost_usd: None,
        rewarm_base_input_equivalent_tokens: None,
        transition_id: String::new(),
        from_cache_epoch: None,
        to_cache_epoch: None,
    };
    let label = compaction.display_label();
    assert!(compaction.planned);
    assert!(label.contains("planned cache epoch transition"));
    assert!(!label.contains("failure") && !label.contains("miss"));
}
