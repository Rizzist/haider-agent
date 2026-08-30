//! Automatic hermetic policy for an active custom no-auth provider.
//!
//! This is a policy input to the existing lockdown turn binding, not a second
//! sandbox or lifecycle. Callers pass the summary for the provider that is
//! active at a session/turn boundary; configured-but-inactive providers never
//! reach this seam.

use std::ffi::OsStr;

use haider_rpc::{LockdownActivationWire, ProviderApiFamilyWire, ProviderSummaryWire};

pub(crate) const AUTO_HERMETIC_ENV: &str = "HAIDER_AUTO_HERMETIC";
pub(crate) const AUTO_HERMETIC_REASON: &str =
    "the active provider is an enabled custom no-auth endpoint";
pub(crate) const AUTO_HERMETIC_ELIGIBLE_REASON: &str = "the provider will enter automatic hermetic policy when selected because it is an enabled custom no-auth endpoint";
pub(crate) const AUTO_HERMETIC_OVERRIDE_REASON: &str =
    "automatic hermetic policy is disabled by HAIDER_AUTO_HERMETIC=0";

const AUTO_HERMETIC_TOOLS: &[&str] = &[
    "fs_read",
    "fs_glob",
    "fs_search",
    "fs_write",
    "request_input",
    "todo_write",
    "plan",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderLockdownPolicy {
    Full,
    Configured,
    AutoHermetic,
    AutoHermeticDisabled,
    UnknownProvider,
}

impl ProviderLockdownPolicy {
    pub(crate) const fn from_binding(lockdown: bool, auto_hermetic: bool) -> Self {
        if auto_hermetic {
            Self::AutoHermetic
        } else if lockdown {
            Self::Configured
        } else {
            Self::Full
        }
    }

    pub(crate) const fn binding_bits(self) -> (bool, bool) {
        (self.is_lockdown(), self.is_auto_hermetic())
    }

    pub(crate) const fn is_lockdown(self) -> bool {
        matches!(
            self,
            Self::Configured | Self::AutoHermetic | Self::UnknownProvider
        )
    }

    pub(crate) const fn is_auto_hermetic(self) -> bool {
        matches!(self, Self::AutoHermetic)
    }

    pub(crate) const fn activation(self, active: bool) -> Option<LockdownActivationWire> {
        match self {
            Self::Configured => Some(LockdownActivationWire::Configured),
            Self::AutoHermetic if active => Some(LockdownActivationWire::AutoHermetic),
            Self::AutoHermetic => Some(LockdownActivationWire::AutoHermeticEligible),
            Self::UnknownProvider => Some(LockdownActivationWire::Unknown),
            Self::Full | Self::AutoHermeticDisabled => None,
        }
    }

    pub(crate) const fn reason(self, active: bool) -> Option<&'static str> {
        match self {
            Self::Configured => Some("the provider is explicitly configured for lockdown"),
            Self::AutoHermetic if active => Some(AUTO_HERMETIC_REASON),
            Self::AutoHermetic => Some(AUTO_HERMETIC_ELIGIBLE_REASON),
            Self::AutoHermeticDisabled => Some(AUTO_HERMETIC_OVERRIDE_REASON),
            Self::UnknownProvider => Some("the active provider is unknown and fails closed"),
            Self::Full => None,
        }
    }
}

pub(crate) fn provider_policy(
    summary: Option<&ProviderSummaryWire>,
    has_active_credential: bool,
) -> ProviderLockdownPolicy {
    provider_policy_with_candidate(
        summary,
        summary.is_some_and(is_custom_no_auth_endpoint) && !has_active_credential,
        std::env::var_os(AUTO_HERMETIC_ENV).as_deref(),
    )
}

/// Evaluates the policy from the account adapter that was actually resolved
/// for this turn. Unlike provider-status eligibility, this fact cannot race a
/// later account snapshot mutation between adapter construction and binding.
pub(crate) fn provider_policy_for_active(
    summary: Option<&ProviderSummaryWire>,
    active_no_auth: bool,
) -> ProviderLockdownPolicy {
    provider_policy_with_candidate(
        summary,
        active_no_auth,
        std::env::var_os(AUTO_HERMETIC_ENV).as_deref(),
    )
}

#[cfg(test)]
pub(crate) fn provider_policy_with_override(
    summary: Option<&ProviderSummaryWire>,
    has_active_credential: bool,
    override_value: Option<&OsStr>,
) -> ProviderLockdownPolicy {
    provider_policy_with_candidate(
        summary,
        summary.is_some_and(is_custom_no_auth_endpoint) && !has_active_credential,
        override_value,
    )
}

fn provider_policy_with_candidate(
    summary: Option<&ProviderSummaryWire>,
    auto_hermetic_candidate: bool,
    override_value: Option<&OsStr>,
) -> ProviderLockdownPolicy {
    // A resolved keyless adapter is turn-boundary evidence: later profile
    // edits cannot make the already-built headerless endpoint disappear or
    // safely widen its envelope.
    if auto_hermetic_candidate && override_value != Some(OsStr::new("0")) {
        return ProviderLockdownPolicy::AutoHermetic;
    }
    let Some(summary) = summary else {
        return ProviderLockdownPolicy::UnknownProvider;
    };
    if !matches!(summary.trust, haider_rpc::ProviderTrustWire::Full) {
        ProviderLockdownPolicy::Configured
    } else if auto_hermetic_candidate {
        ProviderLockdownPolicy::AutoHermeticDisabled
    } else {
        ProviderLockdownPolicy::Full
    }
}

/// The G4a keyless-account profile shape, plus the non-built-in origin that
/// the resolver itself requires. [`provider_policy`] separately excludes an
/// active stored credential because the resolver gives that account priority.
/// The endpoint is the sole provider egress route; it may be loopback,
/// trusted LAN, or HTTPS without changing this policy.
pub(crate) fn is_custom_no_auth_endpoint(summary: &ProviderSummaryWire) -> bool {
    !haider_provider::BUILTIN_PROVIDER_NAMES.contains(&summary.provider.as_str())
        && summary.enabled
        && summary.endpoint.is_some()
        && summary.auth_methods.is_empty()
        && matches!(
            summary.api_family,
            ProviderApiFamilyWire::OpenAiChatCompletions | ProviderApiFamilyWire::AnthropicMessages
        )
}

pub(crate) fn tools_for(policy: ProviderLockdownPolicy) -> Vec<String> {
    if policy.is_auto_hermetic() {
        debug_assert!(
            AUTO_HERMETIC_TOOLS
                .iter()
                .all(|tool| crate::lockdown::tool_allowed(tool)),
            "the auto-hermetic envelope must remain a lockdown subset"
        );
        AUTO_HERMETIC_TOOLS
            .iter()
            .map(|tool| (*tool).to_owned())
            .collect()
    } else {
        crate::lockdown::allowed_tool_names()
    }
}

pub(crate) fn apply_to_turn(
    turn: &mut crate::lockdown::LockdownTurn,
    policy: ProviderLockdownPolicy,
) {
    if policy.is_auto_hermetic() {
        turn.tools_allowed = tools_for(policy);
    }
}
