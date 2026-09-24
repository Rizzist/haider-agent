//! Per-response output-budget policy. The pinned per-model table lives in
//! `model_limits`; this module owns the session default and the bounds applied
//! when a model row's output maximum is projected.

use crate::model_limits::static_model_limits;

/// Budget a new session requests when the client does not override it:
/// the smaller of this value and the resolved model maximum. It keeps the
/// 30,000-token reserve the TUI (`SESSION_OUTPUT_CAP`) and the daemon's former
/// `session.create` ceiling already used. Clients send zero for derivation.
pub use haider_protocol::output_budget::DEFAULT_OUTPUT_LIMIT;
/// Largest response budget supported by any currently registered adapter
/// (the DeepSeek V4 row in `model_limits`). Daemon admission applies this after
/// catalog/model-specific resolution.
pub const MAX_OUTPUT_LIMIT: u64 = 384_000;

/// The output maximum projected for one provider/model row. A catalog's
/// declared value wins over the pinned table; either is bounded by
/// [`MAX_OUTPUT_LIMIT`] and by the row's context window when one is known.
#[must_use]
pub fn model_output_limit(
    provider: &str,
    model: &str,
    declared: Option<u64>,
    context_window: Option<u64>,
) -> u64 {
    let max_output_tokens = declared
        .unwrap_or_else(|| static_model_limits(provider, model).max_output_tokens)
        .min(MAX_OUTPUT_LIMIT);
    context_window.map_or(max_output_tokens, |context_window| {
        max_output_tokens.min(context_window)
    })
}
