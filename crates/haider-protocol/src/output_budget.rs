//! Shared default output budget for session clients and providers, plus the
//! durable record of where a session's per-response budget came from.

use serde::{Deserialize, Serialize};

/// Default response budget before applying the selected model's ceiling.
pub const DEFAULT_OUTPUT_LIMIT: u64 = 30_000;

/// Values that pre-973 clients sent on `session.create` without the user
/// choosing them: the 972 headless default (4,096), the Android factory
/// (8,192), and the TUI/agent/mobile-bridge default (30,000). A legacy session
/// whose stored budget is one of these is treated as daemon-derived; any other
/// stored value can only have come from an explicit override.
pub const LEGACY_CLIENT_DEFAULT_OUTPUT_LIMITS: [u64; 3] = [4_096, 8_192, DEFAULT_OUTPUT_LIMIT];

/// Where a session's per-response output budget came from.
///
/// `Derived` budgets follow the selected model: every model selection
/// re-derives `min(DEFAULT_OUTPUT_LIMIT, model max)`. `UserSet` keeps the
/// user's requested value and applies it bounded by the selected model's
/// maximum, so switching back to a larger model restores the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionOutputBudgetSourceV1 {
    Derived,
    UserSet { requested: u64 },
}

impl SessionOutputBudgetSourceV1 {
    /// The source of a stored session budget. Metadata written before the
    /// source was recorded carries `None`: known client defaults are derived,
    /// any other positive value was an explicit override.
    #[must_use]
    pub fn classify(stored: Option<Self>, max_tokens: u64) -> Self {
        stored.unwrap_or(
            if max_tokens == 0 || LEGACY_CLIENT_DEFAULT_OUTPUT_LIMITS.contains(&max_tokens) {
                Self::Derived
            } else {
                Self::UserSet {
                    requested: max_tokens,
                }
            },
        )
    }

    /// The source a `session.create`/`session.select_model` request selects:
    /// zero asks the daemon to derive, a positive value is the user's own.
    #[must_use]
    pub const fn from_request(requested: u64) -> Self {
        if requested == 0 {
            Self::Derived
        } else {
            Self::UserSet { requested }
        }
    }

    /// Applies this source to one model row. A derived budget follows
    /// `model_max_output_tokens` (the row's projected maximum) and is never
    /// reported as clamped. A user-set request is bounded by
    /// `explicit_max_output_tokens`, the largest explicit budget the row
    /// admits: equal to the projected maximum when that maximum is sourced,
    /// larger when it is only an unverified guess. A clamp is reported.
    #[must_use]
    pub fn apply(
        self,
        model_max_output_tokens: u64,
        explicit_max_output_tokens: u64,
    ) -> SessionOutputBudgetV1 {
        match self {
            Self::Derived => SessionOutputBudgetV1 {
                max_tokens: DEFAULT_OUTPUT_LIMIT.min(model_max_output_tokens),
                source: self,
                clamped: None,
            },
            Self::UserSet { requested } if requested > explicit_max_output_tokens => {
                SessionOutputBudgetV1 {
                    max_tokens: explicit_max_output_tokens,
                    source: self,
                    clamped: Some(OutputBudgetClampV1 {
                        requested,
                        max_output_tokens: explicit_max_output_tokens,
                    }),
                }
            }
            Self::UserSet { requested } => SessionOutputBudgetV1 {
                max_tokens: requested,
                source: self,
                clamped: None,
            },
        }
    }
}

/// The effective per-response budget committed by a model selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionOutputBudgetV1 {
    /// Effective budget sent on every provider request.
    pub max_tokens: u64,
    pub source: SessionOutputBudgetSourceV1,
    /// Present when a user-set budget exceeded the selected model's maximum
    /// and was clamped to it. Clients show [`OutputBudgetClampV1::notice`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clamped: Option<OutputBudgetClampV1>,
}

/// Typed notice: the user's output budget did not fit the selected model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputBudgetClampV1 {
    pub requested: u64,
    pub max_output_tokens: u64,
}

impl OutputBudgetClampV1 {
    /// One-line user-facing wording shared by every client.
    #[must_use]
    pub fn notice(&self) -> String {
        format!(
            "Output budget {} exceeds this model's maximum; using {} per response. \
             Switching to a larger model restores {}.",
            self.requested, self.max_output_tokens, self.requested
        )
    }
}
