//! Account/credential descriptors (§4.4). The secret NEVER appears in any
//! protocol type — descriptors carry aliases and vault references only.

use crate::ids::CredentialAlias;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CredentialDescriptor {
    pub alias: CredentialAlias,
    pub provider: String,
    pub auth_method: AuthMethod,
    /// Human account identity (email/handle) for display.
    pub identity: String,
    pub status: CredentialStatus,
    /// Exactly one credential is active per provider.
    pub active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
