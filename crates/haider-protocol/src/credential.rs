//! Account/credential descriptors (§4.4). The secret NEVER appears in any
//! protocol type — descriptors carry aliases and vault references only.

use crate::ids::CredentialAlias;
use serde::{Deserialize, Serialize};

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
