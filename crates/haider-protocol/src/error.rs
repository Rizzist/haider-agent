//! Error taxonomy: stable codes, explicit retryability, structured details.
//! Headless exit codes map from these (documented in haider-cli).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HaiderError {
    pub code: ErrorCode,
    pub message: String,
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    // usage / client
    InvalidArgument,
    UnknownMethod,
    ProtocolMismatch,
    // auth / accounts
    Unauthorized,
    CredentialMissing,
    CredentialLimited,
    // session / run lifecycle
    SessionNotFound,
    RunNotActive,
    MenuNotFound,
    MenuAlreadyAnswered,
    SingleWriterViolation,
    Busy,
    RevisionConflict,
    LoopLimit,
    // providers
    ProviderError,
    ProviderTimeout,
    // storage
    StoreCorrupt,
    StoreLocked,
    // effects / permissions
    PermissionDenied,
    EffectUnknownOutcome,
    // internal
    Internal,
    /// Forward-compat catch-all: unknown codes from newer peers land here.
    #[serde(other)]
    Unknown,
}

impl HaiderError {
    pub fn new(code: ErrorCode, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code,
            message: message.into(),
            retryable,
            details: None,
        }
    }
}
