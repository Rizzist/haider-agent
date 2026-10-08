//! Deferred human approval for spending a provider-issued usage reset credit.
//!
//! The flow is two-phase and never blocks the requester:
//! 1. `usage.reset_apply` (RPC) or the `usage_reset_apply` agent tool
//!    journals the effect intent, an `Ask` authorization and a nonblocking
//!    [`MenuKind::DeferredApproval`] menu, then returns `approval_pending`.
//! 2. A committed menu answer with accepted coordinates, handled by the session-hub RPC,
//!    authorizes the effect. Approval journals `Dispatched` before the single
//!    provider call and then a terminal typed result extension, which the
//!    agent reads by polling with the returned approval id.
//!
//! Identifiers here are derived from request coordinates only. The durable
//! approval binding carries a provider account id, never a bearer or credit id.

#[cfg(test)]
#[path = "usage_reset_tests.rs"]
mod tests;

use haider_protocol::credential::UsageResetAccountBinding;
use haider_protocol::effect::{EffectClass, EffectIntent};
use haider_protocol::ids::{CredentialAlias, EffectId, EventId, MenuId};
use haider_protocol::menu::{DecisionKind, Menu, MenuKind, MenuOption, MenuScope};
use haider_protocol::usage::{UsageResetApplyResultV1, UsageResetApplyStatusV1};
use haider_tools::PermissionPolicy;

pub(crate) const APPROVE_OPTION_KEY: &str = "approve";
pub(crate) const DENY_OPTION_KEY: &str = "deny";
/// Terminal reason when graceful drain fenced a claimed reset before its
/// provider task was admitted; no provider request was sent.
pub(crate) const DRAIN_FENCED_REASON: &str = "daemon_draining_provider_not_called";
pub(crate) const ACCOUNT_CHANGED_REASON: &str = "account_changed";

/// Both reset entry points check the same policy decision before parking an
/// approval. Ordinary Allow still proceeds to a human decision.
pub(crate) fn policy_denial_reason(
    policy: &PermissionPolicy,
    intent: &EffectIntent,
) -> Option<String> {
    policy.denial_reason(intent)
}

/// Stable approval id over length-prefixed request coordinates, so distinct
/// coordinate tuples can never collide by concatenation. The id doubles as
/// the provider idempotency key (`redeem_request_id`).
pub(crate) fn approval_id(coordinates: &[&str]) -> String {
    let mut identity = blake3::Hasher::new();
    for part in coordinates {
        identity.update(&(part.len() as u64).to_be_bytes());
        identity.update(part.as_bytes());
    }
    format!("usage-reset-{}", identity.finalize().to_hex())
}

pub(crate) fn menu_id(approval_id: &str) -> MenuId {
    MenuId::new(format!("deferred-{approval_id}"))
}

pub(crate) fn effect_id(approval_id: &str) -> EffectId {
    EffectId::new(format!("effect-{approval_id}"))
}

/// Deterministic journal event id for one stage of an approval. A replayed
/// stage therefore collides instead of appending a second fact.
pub(crate) fn event_id(stage: &str, approval_id: &str) -> EventId {
    EventId::new(format!("usage-reset-{stage}-{approval_id}"))
}

/// The approval id already carries the `usage-reset-` prefix. Older doubled
/// notification ids remain in persisted turns; only new delivery ids use
/// this form. Journal event and item ids retain their established formats.
pub(crate) fn notification_id(approval_id: &str) -> &str {
    approval_id
}

pub(crate) fn intent(approval_id: &str, account: &CredentialAlias) -> EffectIntent {
    bound_intent(approval_id, account, None)
}

pub(crate) fn bound_intent(
    approval_id: &str,
    account: &CredentialAlias,
    binding: Option<&UsageResetAccountBinding>,
) -> EffectIntent {
    let args_digest = if let Some(binding) = binding {
        let mut hasher = blake3::Hasher::new();
        for field in [
            account.as_str(),
            binding.provider.as_str(),
            binding.account_id.as_str(),
        ] {
            hasher.update(&(field.len() as u64).to_be_bytes());
            hasher.update(field.as_bytes());
        }
        hasher.update(&binding.incarnation.to_be_bytes());
        format!("blake3:{}", hasher.finalize().to_hex())
    } else {
        format!(
            "blake3:{}",
            blake3::hash(account.as_str().as_bytes()).to_hex()
        )
    };
    EffectIntent {
        effect: effect_id(approval_id),
        class: EffectClass::UsageResetApply,
        summary: format!("apply one usage reset credit for {account}"),
        args_digest,
        workspace_revision: None,
    }
}

#[cfg(test)]
pub(crate) fn approval_menu(approval_id: &str, account: &CredentialAlias) -> Menu {
    bound_approval_menu(approval_id, account, None)
}

pub(crate) fn bound_approval_menu(
    approval_id: &str,
    account: &CredentialAlias,
    binding: Option<&UsageResetAccountBinding>,
) -> Menu {
    let account_label = binding.map_or_else(
        || format!("Account: {account}"),
        |binding| {
            let short: String = binding
                .account_id
                .chars()
                .filter(|character| !character.is_control())
                .take(24)
                .collect();
            let suffix = if binding.account_id.chars().count() > 24 {
                "…"
            } else {
                ""
            };
            format!("Account: {account} · {short}{suffix}")
        },
    );
    Menu {
        id: menu_id(approval_id),
        kind: MenuKind::DeferredApproval {
            approval_id: approval_id.to_owned(),
            effect: EffectClass::UsageResetApply,
            account: Some(account.clone()),
            binding: binding.cloned(),
        },
        title: "Apply a usage reset credit?".into(),
        body: vec![
            account_label,
            "This spends one limited provider-issued reset credit. Work continues while this decision is pending.".into(),
        ],
        // Deny is option 0 and the default local Enter selection. Keyless
        // wire answers are rejected as invalid_frame, without a Deny event.
        options: vec![
            MenuOption {
                key: DENY_OPTION_KEY.into(),
                label: "Deny".into(),
                detail: Some("Do not spend a reset credit.".into()),
                decision: Some(DecisionKind::RejectOnce),
            },
            MenuOption {
                key: APPROVE_OPTION_KEY.into(),
                label: "Apply one reset".into(),
                detail: Some("Spend one applicable reset credit now.".into()),
                decision: Some(DecisionKind::AllowOnce),
            },
        ],
        blocking: false,
        scope: MenuScope::Session,
        origin: "deferred_approval_v1".into(),
        ttl_ms: None,
        timeout_option: None,
    }
}

/// The option key a committed answer selected. Only the explicit `approve`
/// key approves. The wire rejects keyless answers before this helper; the
/// fallback remains conservative for historical journal reads.
pub(crate) fn answered_option_key(option_key: Option<&str>) -> &str {
    option_key.unwrap_or(DENY_OPTION_KEY)
}

pub(crate) fn result(
    approval_id: &str,
    account: &CredentialAlias,
    status: UsageResetApplyStatusV1,
) -> UsageResetApplyResultV1 {
    UsageResetApplyResultV1 {
        approval_id: approval_id.to_owned(),
        account: account.clone(),
        status,
        windows_reset: None,
        reason: None,
    }
}

pub(crate) fn failed(
    approval_id: &str,
    account: &CredentialAlias,
    reason: impl Into<String>,
) -> UsageResetApplyResultV1 {
    UsageResetApplyResultV1 {
        reason: Some(reason.into()),
        ..result(approval_id, account, UsageResetApplyStatusV1::Failed)
    }
}

/// Journal facts about one parked approval that has no terminal result yet.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct UnresolvedApproval {
    /// Option key of the committed RPC answer, if any.
    pub answer: Option<String>,
    pub answer_generation: Option<u64>,
    pub dispatched_generation: Option<u64>,
}

/// Status of a parked approval without a terminal result. This is a pure
/// read: only the human-facing RPC answer path can consume a credit, and it
/// commits `Dispatched` before the HTTP call. A poll never retries an
/// ambiguous post-restart outcome, even with the provider idempotency key,
/// so the daemon cannot fire the consume request twice.
pub(crate) fn unresolved_status(
    approval_id: &str,
    account: &CredentialAlias,
    facts: &UnresolvedApproval,
    current_generation: u64,
) -> UsageResetApplyResultV1 {
    let interrupted_before_dispatch = facts.answer.as_deref() == Some(APPROVE_OPTION_KEY)
        && facts.answer_generation != Some(current_generation)
        && facts.dispatched_generation.is_none();
    let status = if facts.answer.as_deref() == Some(DENY_OPTION_KEY) {
        UsageResetApplyStatusV1::Denied
    } else if interrupted_before_dispatch {
        UsageResetApplyStatusV1::Failed
    } else {
        UsageResetApplyStatusV1::ApprovalPending
    };
    UsageResetApplyResultV1 {
        reason: if interrupted_before_dispatch {
            Some("approval_interrupted_before_dispatch".into())
        } else {
            facts
                .dispatched_generation
                .filter(|generation| *generation != current_generation)
                .map(|_| "provider_outcome_unconfirmed".into())
        },
        ..result(approval_id, account, status)
    }
}
