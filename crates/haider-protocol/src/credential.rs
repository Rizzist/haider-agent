//! Account/credential descriptors (§4.4). The secret NEVER appears in any
//! protocol type — descriptors carry aliases and vault references only.

use crate::ids::CredentialAlias;
use serde::{Deserialize, Serialize};

/// Informational, secret-free account facts captured at credential intake.
///
/// These fields are never authentication authority. In particular,
/// `verified = false` is the only honest value for claims decoded from a JWT
/// without checking its signature against the issuer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountIdentity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
    pub captured_at: u64,
    pub verified: bool,
}

impl AccountIdentity {
    /// Normalizes one untrusted informational claim before it can reach a
    /// descriptor or operator surface.
    #[must_use]
    pub fn sanitized_field(value: &str) -> Option<String> {
        let value: String = value
            .chars()
            .filter(|character| !character.is_control())
            .take(512)
            .collect();
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_owned())
    }

    /// A concise operator-facing label with no credential material.
    #[must_use]
    pub fn summary(&self) -> String {
        let principal = self
            .email
            .as_deref()
            .or(self.display_name.as_deref())
            .or(self.account_id.as_deref())
            .and_then(Self::sanitized_field)
            .unwrap_or_else(|| "unknown account".to_owned());
        self.plan
            .as_deref()
            .and_then(Self::sanitized_field)
            .map_or(principal.clone(), |plan| format!("{principal} · {plan}"))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CredentialDescriptor {
    pub alias: CredentialAlias,
    pub provider: String,
    /// Optional provider API root for endpoint-addressed adapters.
    ///
    /// Absent for provider-owned endpoints such as Anthropic and OpenAI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    pub auth_method: AuthMethod,
    /// Human account identity (email/handle) for display.
    pub identity: String,
    pub status: CredentialStatus,
    /// Exactly one credential is active per provider.
    pub active: bool,
    /// Operator-chosen display name (v0.0.938). Purely cosmetic: the `alias`
    /// remains the identity every door addresses, so renaming for display can
    /// never break a reference, a receipt, or a removal-in-flight. Absent
    /// until someone sets one — surfaces fall back to `identity`, then
    /// `alias`. Free-form text, bounded and control-stripped by the daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Additive v0.0.964 account identity. Older account rows omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_identity: Option<AccountIdentity>,
    /// Epoch milliseconds when Haider first committed this account. Rows
    /// written before v0.0.964 retain `None` rather than inventing a date.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    ApiKey,
    /// Explicit rename: snake_case would mangle the acronym to "o_auth".
    #[serde(rename = "oauth")]
    OAuth,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CredentialStatus {
    Ok,
    /// Rate-limited until the given epoch-ms (rotation ladder input).
    Limited {
        until_ms: u64,
    },
    Expired,
    Revoked,
    /// The cached credential remains durable, but its external owner store
    /// cannot currently be read. The daemon keeps using an unexpired cached
    /// access token and surfaces this state only once that token expires.
    NeedsAttention {
        reason: CredentialAttentionReason,
    },
}

/// Actionable external-owner failures carried by an account row. This is
/// deliberately credential-store vocabulary rather than a free-form error so
/// every client can render an honest recovery action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialAttentionReason {
    KeychainDenied,
    KeychainLocked,
    KeychainMissing,
    KeychainUnavailable,
}

/// Surfaced like a model change in the transcript (§4.4 rotation).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RotationEvent {
    pub provider: String,
    pub from: CredentialAlias,
    pub to: CredentialAlias,
    pub cause: RotationCause,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RotationCause {
    RateLimit,
    Error,
    Manual,
}
