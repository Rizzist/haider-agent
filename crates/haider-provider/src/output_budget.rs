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
/// [`MAX_OUTPUT_LIMIT`] and kept strictly below the row's context window when
/// one is known (at most half the window when it would otherwise reach it).
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
    below_context_window(max_output_tokens, context_window)
}

/// The largest EXPLICIT per-response budget admitted for one row whose
/// projected maximum is `projected_max` (from [`model_output_limit`]).
///
/// A catalog declaration or a sourced table row is exact, so it is also the
/// explicit ceiling. When the projection is only the unverified
/// [`crate::UNKNOWN_OUTPUT_LIMIT`] guess (custom endpoints, older Claude,
/// unlisted Gemini, unknown IDs), that guess must not forbid a larger request
/// the provider may well accept: an explicit budget may go up to
/// [`MAX_OUTPUT_LIMIT`], still strictly below a known context window. The
/// provider-stated one-shot retry ([`provider_stated_output_limit`]) is the
/// backstop when the provider's real maximum is lower.
///
/// Daemon projections carry the projected value, not the raw declaration, so a
/// catalog that declares exactly the fallback value for an unsourced row is
/// treated as the guess; the retry covers that case too.
#[must_use]
pub fn explicit_output_ceiling(
    provider: &str,
    model: &str,
    projected_max: u64,
    context_window: Option<u64>,
) -> u64 {
    let unsourced_guess = !static_model_limits(provider, model).output_limit_sourced
        && projected_max == model_output_limit(provider, model, None, context_window);
    if unsourced_guess {
        below_context_window(MAX_OUTPUT_LIMIT, context_window).max(projected_max)
    } else {
        projected_max
    }
}

/// Keeps an output maximum strictly below a known context window: the worker
/// refuses a reserve that is not smaller than the window, so a maximum that
/// would reach it is cut to half the window, leaving room for input.
#[must_use]
fn below_context_window(max_output_tokens: u64, context_window: Option<u64>) -> u64 {
    match context_window {
        Some(context_window) if max_output_tokens >= context_window => (context_window / 2).max(1),
        _ => max_output_tokens,
    }
}

/// The output maximum a provider states when it rejects a request's
/// `max_tokens` as too large, parsed from the typed invalid-request error.
/// Returns `None` for every other error, and for a statement that does not
/// lower the budget (so a caller can retry at most once and never loops).
///
/// Recognised forms (verbatim provider wording):
/// - Anthropic: `max_tokens: 30000 > 16000, which is the maximum allowed
///   number of output tokens for <model>`
/// - OpenAI-compatible: `max_tokens is too large: 30000. This model supports
///   at most 16384 completion tokens, whereas you provided 30000.`
/// - DeepSeek: `Invalid max_tokens value, the valid range of max_tokens is
///   [1, 8192]`
#[must_use]
pub fn provider_stated_output_limit(error: &crate::ProviderError, requested: u64) -> Option<u64> {
    if error.kind != crate::ProviderErrorKind::InvalidRequest {
        return None;
    }
    // Adapters keep the provider's own wording in the presentation detail
    // and a sanitized summary in `message`; either may carry the statement.
    [error.presentation.detail.as_str(), error.message.as_str()]
        .into_iter()
        .find_map(stated_output_limit)
        .filter(|stated| *stated > 0 && *stated < requested)
}

fn stated_output_limit(message: &str) -> Option<u64> {
    number_before(
        message,
        ", which is the maximum allowed number of output tokens",
    )
    .or_else(|| tokens_after(message, "supports at most "))
    .or_else(|| {
        message
            .find("valid range of max_tokens is [")
            .and_then(|start| {
                let rest = &message[start..];
                rest.find(',')
                    .and_then(|comma| leading_number(rest[comma + 1..].trim_start()))
            })
    })
}

fn leading_number(text: &str) -> Option<u64> {
    let digits: String = text
        .chars()
        .take_while(|character| {
            character.is_ascii_digit() || *character == ',' || *character == '_'
        })
        .filter(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// The number after `marker`, only when a token unit follows it within the
/// next two words ("16384 completion tokens", "8192 tokens"), so that e.g.
/// "supports at most 10 images" never lowers the output budget.
fn tokens_after(message: &str, marker: &str) -> Option<u64> {
    let start = message.find(marker)? + marker.len();
    let rest = &message[start..];
    let number_len = rest
        .find(|character: char| {
            !(character.is_ascii_digit() || character == ',' || character == '_')
        })
        .unwrap_or(rest.len());
    let number = leading_number(&rest[..number_len])?;
    rest[number_len..]
        .split_whitespace()
        .take(2)
        .any(|word| word.to_ascii_lowercase().starts_with("token"))
        .then_some(number)
}

fn number_before(message: &str, marker: &str) -> Option<u64> {
    let end = message.find(marker)?;
    let prefix = message[..end].trim_end();
    let start = prefix
        .rfind(|character: char| !character.is_ascii_digit())
        .map_or(0, |index| index + 1);
    prefix[start..].parse().ok()
}
