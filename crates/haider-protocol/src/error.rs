//! Error taxonomy: stable codes, explicit retryability, structured details.
//! Headless exit codes map from these (documented in haider-cli).

use serde::{Deserialize, Deserializer, Serialize, Serializer};

const SUBCODE_LIMIT: usize = 64;
const TITLE_LIMIT: usize = 96;
const DETAIL_LIMIT: usize = 512;
const REQUEST_ID_LIMIT: usize = 128;

/// Stable, bounded machine-readable reason carried to every presentation
/// surface. Values are lowercase ASCII kebab tokens; invalid producer input
/// is normalized rather than copied verbatim into a journal or renderer.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ErrorSubcode(String);

impl ErrorSubcode {
    #[must_use]
    pub fn new(value: impl AsRef<str>) -> Self {
        let mut normalized = String::with_capacity(value.as_ref().len().min(SUBCODE_LIMIT));
        let mut last_was_dash = false;
        for byte in value.as_ref().bytes() {
            let next = match byte {
                b'a'..=b'z' | b'0'..=b'9' => Some(char::from(byte)),
                b'A'..=b'Z' => Some(char::from(byte.to_ascii_lowercase())),
                b'-' | b'_' | b' ' if !normalized.is_empty() && !last_was_dash => Some('-'),
                _ => None,
            };
            let Some(next) = next else { continue };
            if normalized.len() + next.len_utf8() > SUBCODE_LIMIT {
                break;
            }
            last_was_dash = next == '-';
            normalized.push(next);
        }
        while normalized.ends_with('-') {
            normalized.pop();
        }
        if normalized.is_empty() {
            normalized.push_str("unknown-error");
        }
        Self(normalized)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for ErrorSubcode {
    fn default() -> Self {
        Self::new("unknown-error")
    }
}

impl Serialize for ErrorSubcode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ErrorSubcode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self::new)
    }
}

/// Where the user-visible failure can be repaired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorScope {
    Turn,
    Session,
    Profile,
    Account,
    Tool,
}

/// Server-enumerated recovery vocabulary. Clients render only these actions;
/// they never infer a button by parsing provider prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorAction {
    Retry,
    Relogin,
    Reimport,
    EditKey,
    SwitchAccount,
    TopUp,
    Wait,
    ChooseModel,
    ContactAdmin,
    ContinuePartial,
    RetryFresh,
    None,
}

/// One safe presentation contract shared by durable run failures, failed tool
/// results, menus, RPC replay, headless output, and interactive clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ErrorPresentation {
    pub subcode: ErrorSubcode,
    pub title: String,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_http_status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_request_id: Option<String>,
    /// Relative delay supplied by the provider, in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
    /// Absolute Unix reset time derived from the provider delay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_at_ms: Option<u64>,
    pub scope: ErrorScope,
    pub allowed_actions: Vec<ErrorAction>,
}

impl ErrorPresentation {
    #[must_use]
    pub fn new(
        subcode: impl AsRef<str>,
        title: impl AsRef<str>,
        detail: impl AsRef<str>,
        scope: ErrorScope,
        allowed_actions: impl IntoIterator<Item = ErrorAction>,
    ) -> Self {
        let mut seen_actions = 0_u16;
        let mut allowed_actions =
            allowed_actions
                .into_iter()
                .fold(Vec::new(), |mut actions, action| {
                    let bit = error_action_bit(action);
                    if actions.len() < 16 && seen_actions & bit == 0 {
                        seen_actions |= bit;
                        actions.push(action);
                    }
                    actions
                });
        if allowed_actions.len() > 1 {
            allowed_actions.retain(|action| *action != ErrorAction::None);
        }
        if allowed_actions.is_empty() {
            allowed_actions.push(ErrorAction::None);
        }
        let title = bounded_public_text(title.as_ref(), TITLE_LIMIT);
        let detail = bounded_public_text(detail.as_ref(), DETAIL_LIMIT);
        Self {
            subcode: ErrorSubcode::new(subcode),
            title: if title.trim().is_empty() {
                "Something went wrong".into()
            } else {
                title
            },
            detail: if detail.trim().is_empty() {
                "The operation failed without a typed reason.".into()
            } else {
                detail
            },
            provider_http_status: None,
            provider_request_id: None,
            retry_after_ms: None,
            reset_at_ms: None,
            scope,
            allowed_actions,
        }
    }

    #[must_use]
    pub fn with_http_status(mut self, status: u16) -> Self {
        self.provider_http_status = Some(status);
        self
    }

    #[must_use]
    pub fn with_request_id(mut self, request_id: Option<&str>) -> Self {
        self.provider_request_id = request_id
            .map(|value| bounded_public_text(value, REQUEST_ID_LIMIT))
            .filter(|value| !value.is_empty());
        self
    }

    #[must_use]
    pub fn with_retry_after(mut self, retry_after_ms: Option<u64>, now_ms: u64) -> Self {
        self.retry_after_ms = retry_after_ms;
        self.reset_at_ms = retry_after_ms.and_then(|delay| now_ms.checked_add(delay));
        self
    }
}

const fn error_action_bit(action: ErrorAction) -> u16 {
    1 << match action {
        ErrorAction::Retry => 0,
        ErrorAction::Relogin => 1,
        ErrorAction::Reimport => 2,
        ErrorAction::EditKey => 3,
        ErrorAction::SwitchAccount => 4,
        ErrorAction::TopUp => 5,
        ErrorAction::Wait => 6,
        ErrorAction::ChooseModel => 7,
        ErrorAction::ContactAdmin => 8,
        ErrorAction::ContinuePartial => 9,
        ErrorAction::RetryFresh => 10,
        ErrorAction::None => 11,
    }
}

#[derive(Deserialize)]
struct RawErrorPresentation {
    #[serde(default)]
    subcode: ErrorSubcode,
    #[serde(default)]
    title: String,
    #[serde(default)]
    detail: String,
    #[serde(default)]
    provider_http_status: Option<u16>,
    #[serde(default)]
    provider_request_id: Option<String>,
    #[serde(default)]
    retry_after_ms: Option<u64>,
    #[serde(default)]
    reset_at_ms: Option<u64>,
    #[serde(default)]
    scope: Option<ErrorScope>,
    #[serde(default)]
    allowed_actions: Vec<ErrorAction>,
}

impl<'de> Deserialize<'de> for ErrorPresentation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawErrorPresentation::deserialize(deserializer)?;
        let mut presentation = Self::new(
            raw.subcode.as_str(),
            raw.title,
            raw.detail,
            raw.scope.unwrap_or(ErrorScope::Turn),
            raw.allowed_actions,
        );
        presentation.provider_http_status = raw.provider_http_status;
        presentation.provider_request_id = raw
            .provider_request_id
            .map(|value| bounded_public_text(&value, REQUEST_ID_LIMIT))
            .filter(|value| !value.is_empty());
        presentation.retry_after_ms = raw.retry_after_ms;
        presentation.reset_at_ms = raw.reset_at_ms;
        Ok(presentation)
    }
}

impl Default for ErrorPresentation {
    fn default() -> Self {
        Self::new(
            "unknown-error",
            "Something went wrong",
            "The operation failed without a typed reason.",
            ErrorScope::Turn,
            [ErrorAction::None],
        )
    }
}

fn bounded_public_text(value: &str, limit: usize) -> String {
    let mut bounded = String::with_capacity(value.len().min(limit));
    for character in value.chars() {
        let sanitized = if character.is_control() && character != '\n' {
            ' '
        } else {
            character
        };
        if bounded.len().saturating_add(sanitized.len_utf8()) > limit {
            break;
        }
        bounded.push(sanitized);
    }
    bounded
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HaiderError {
    pub code: ErrorCode,
    pub message: String,
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presentation: Option<ErrorPresentation>,
}

impl std::fmt::Display for HaiderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for HaiderError {}

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
    WorkflowUnfinished,
    GraphAlreadyActive,
    GraphNotActive,
    GraphWrongNode,
    // providers
    ProviderError,
    ProviderTimeout,
    VisionUnsupported,
    // storage
    StoreCorrupt,
    StoreLocked,
    StoreFull,
    StoreReadOnly,
    StoreUnavailable,
    // effects / permissions
    PermissionDenied,
    EffectUnknownOutcome,
    // internal
    Internal,
    /// A daemon-enforced headless run budget reached its durable limit.
    BudgetExhausted,
    /// Forward-compat catch-all: unknown codes from newer peers land here.
    #[serde(other)]
    Unknown,
}

impl ErrorCode {
    /// Stable snake-case name used by protocol-facing text projections.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidArgument => "invalid_argument",
            Self::UnknownMethod => "unknown_method",
            Self::ProtocolMismatch => "protocol_mismatch",
            Self::Unauthorized => "unauthorized",
            Self::CredentialMissing => "credential_missing",
            Self::CredentialLimited => "credential_limited",
            Self::SessionNotFound => "session_not_found",
            Self::RunNotActive => "run_not_active",
            Self::MenuNotFound => "menu_not_found",
            Self::MenuAlreadyAnswered => "menu_already_answered",
            Self::SingleWriterViolation => "single_writer_violation",
            Self::Busy => "busy",
            Self::RevisionConflict => "revision_conflict",
            Self::LoopLimit => "loop_limit",
            Self::WorkflowUnfinished => "workflow_unfinished",
            Self::GraphAlreadyActive => "graph_already_active",
            Self::GraphNotActive => "graph_not_active",
            Self::GraphWrongNode => "graph_wrong_node",
            Self::ProviderError => "provider_error",
            Self::ProviderTimeout => "provider_timeout",
            Self::VisionUnsupported => "vision_unsupported",
            Self::StoreCorrupt => "store_corrupt",
            Self::StoreLocked => "store_locked",
            Self::StoreFull => "store_full",
            Self::StoreReadOnly => "store_read_only",
            Self::StoreUnavailable => "store_unavailable",
            Self::PermissionDenied => "permission_denied",
            Self::EffectUnknownOutcome => "effect_unknown_outcome",
            Self::Internal => "internal",
            Self::BudgetExhausted => "budget_exhausted",
            Self::Unknown => "unknown",
        }
    }

    /// Stable kebab-case form used by [`ErrorPresentation::subcode`].
    #[must_use]
    pub const fn as_subcode(self) -> &'static str {
        match self {
            Self::InvalidArgument => "invalid-argument",
            Self::UnknownMethod => "unknown-method",
            Self::ProtocolMismatch => "protocol-mismatch",
            Self::Unauthorized => "unauthorized",
            Self::CredentialMissing => "credential-missing",
            Self::CredentialLimited => "credential-limited",
            Self::SessionNotFound => "session-not-found",
            Self::RunNotActive => "run-not-active",
            Self::MenuNotFound => "menu-not-found",
            Self::MenuAlreadyAnswered => "menu-already-answered",
            Self::SingleWriterViolation => "single-writer-violation",
            Self::Busy => "busy",
            Self::RevisionConflict => "revision-conflict",
            Self::LoopLimit => "loop-limit",
            Self::WorkflowUnfinished => "workflow-unfinished",
            Self::GraphAlreadyActive => "graph-already-active",
            Self::GraphNotActive => "graph-not-active",
            Self::GraphWrongNode => "graph-wrong-node",
            Self::ProviderError => "provider-error",
            Self::ProviderTimeout => "provider-timeout",
            Self::VisionUnsupported => "vision-unsupported",
            Self::StoreCorrupt => "store-corrupt",
            Self::StoreLocked => "store-locked",
            Self::StoreFull => "store-full",
            Self::StoreReadOnly => "store-read-only",
            Self::StoreUnavailable => "store-unavailable",
            Self::PermissionDenied => "permission-denied",
            Self::EffectUnknownOutcome => "effect-unknown-outcome",
            Self::Internal => "internal",
            Self::BudgetExhausted => "budget-exhausted",
            Self::Unknown => "unknown",
        }
    }
}

impl HaiderError {
    pub fn new(code: ErrorCode, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code,
            message: message.into(),
            retryable,
            details: None,
            presentation: None,
        }
    }

    #[must_use]
    pub fn with_presentation(mut self, presentation: ErrorPresentation) -> Self {
        self.presentation = Some(presentation);
        self
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn error_presentation_deserialize_rebounds_utf8_and_actions() {
        let input = serde_json::json!({
            "subcode": "UPPER unsafe value",
            "title": "🦀".repeat(100),
            "detail": "secret\u{0000}".repeat(600),
            "provider_request_id": "🦀".repeat(100),
            "scope": "account",
            "allowed_actions": ["retry", "retry", "none"]
        });
        let presentation: ErrorPresentation =
            serde_json::from_value(input).expect("bounded presentation");
        assert!(presentation.title.len() <= TITLE_LIMIT);
        assert!(presentation.detail.len() <= DETAIL_LIMIT);
        assert!(presentation.provider_request_id.as_ref().unwrap().len() <= REQUEST_ID_LIMIT);
        assert_eq!(presentation.subcode.as_str(), "upper-unsafe-value");
        assert_eq!(presentation.allowed_actions, vec![ErrorAction::Retry]);
        assert!(!presentation.detail.contains('\0'));
    }

    #[test]
    fn error_presentation_never_exposes_empty_public_copy() {
        let presentation = ErrorPresentation::new(
            "provider-error",
            "\n",
            "",
            ErrorScope::Turn,
            std::iter::empty(),
        );
        assert!(!presentation.title.trim().is_empty());
        assert!(!presentation.detail.trim().is_empty());
        assert_eq!(presentation.allowed_actions, vec![ErrorAction::None]);
    }

    #[test]
    fn workflow_unfinished_has_stable_typed_wire_names() {
        assert_eq!(
            ErrorCode::WorkflowUnfinished.as_str(),
            "workflow_unfinished"
        );
        assert_eq!(
            ErrorCode::WorkflowUnfinished.as_subcode(),
            "workflow-unfinished"
        );
        assert_eq!(
            serde_json::to_value(ErrorCode::WorkflowUnfinished).expect("serialize code"),
            serde_json::json!("workflow_unfinished")
        );
    }

    #[test]
    fn haider_error_display_uses_its_message() {
        let error = HaiderError::new(ErrorCode::Internal, "displayed failure", false);

        assert_eq!(error.to_string(), "displayed failure");
    }
}
