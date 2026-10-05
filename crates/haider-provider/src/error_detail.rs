//! Public provider diagnostics policy. Provider prose is untrusted account
//! data: this module is the only place that decides which provider-supplied
//! text may reach a durable [`haider_protocol::error::ErrorPresentation`].
//!
//! Policy (orchestrator ruling 2, lane 973-provider-error-detail):
//! - [`classify_provider_prose_with`] publishes provider prose only when it matches
//!   a known template (`crate::error_templates`) whole; the rendering carries
//!   only typed-safe slots and is safe on every surface.
//! - Unknown prose is never published on a shareable surface. Shareable
//!   surfaces get the provider-class default explanation plus
//!   [`PROVIDER_DETAIL_WITHHELD`]; the raw text, after the credential
//!   redactor ([`local_raw_detail`]), is kept only in the owner-local
//!   `provider_raw_detail` field.
//! - [`safe_error_type`] — provider error `type`/`code`, exact allowlist.
//! - [`safe_request_id`] — provider request-id header, exact shape.
//!
//! Adapters use [`http_error_prose`] / [`provider_error_message`] only to
//! locate raw prose (and to classify it); they publish it only through
//! `ProviderError::with_provider_detail`.

use haider_protocol::error::PROVIDER_DETAIL_WITHHELD;

const REDACTED: &str = "[REDACTED]";

/// How one piece of provider prose may be published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProviderProse {
    /// Matched a known template; safe on every surface.
    Known(String),
    /// Unknown text: shareable surfaces withhold it; the credential-redacted
    /// raw text is kept for owner-local surfaces only.
    Unknown { local_raw: String },
    /// No usable prose (unparseable body, or only whitespace after cleanup).
    Withheld,
}

/// Classifies raw provider prose. Blank prose returns `None`.
#[cfg(test)]
pub(crate) fn classify_provider_prose(raw: &str) -> Option<ProviderProse> {
    classify_provider_prose_with(raw, &crate::error_templates::SlotEvidence::default())
}

/// Classifies raw provider prose against the templates, using the request
/// evidence gathered so far. Blank prose returns `None`.
pub(crate) fn classify_provider_prose_with(
    raw: &str,
    evidence: &crate::error_templates::SlotEvidence<'_>,
) -> Option<ProviderProse> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if raw == PROVIDER_DETAIL_WITHHELD {
        return Some(ProviderProse::Withheld);
    }
    if let Some(rendered) =
        crate::error_templates::render_known_provider_message_with(raw, evidence)
    {
        return Some(ProviderProse::Known(rendered));
    }
    let local_raw = local_raw_detail(raw);
    Some(if local_raw.trim().is_empty() {
        ProviderProse::Withheld
    } else {
        ProviderProse::Unknown { local_raw }
    })
}

/// Owner-local rendering of unknown provider text: the shared output
/// redactor plus the credential scanner run even though the text never
/// leaves this machine. The protocol layer then removes control/bidi
/// characters and bounds it.
pub(crate) fn local_raw_detail(raw: &str) -> String {
    let redacted = haider_tools::redact_output_text(raw);
    redact_credentials(&redacted).unwrap_or_else(|| REDACTED.to_owned())
}

/// Provider error `type`/`code` values that may be published verbatim. Any
/// other value is dropped (not shown, and not used for classification).
/// Sources: Anthropic API error types; OpenAI error types/codes (including
/// the Codex backend and compatible servers). Gemini uses only prose here.
const PUBLIC_PROVIDER_ERROR_TYPES: &[&str] = &[
    // Permission / authentication.
    "permission_error",
    "permission_denied",
    "insufficient_permissions",
    "invalid_request_error",
    "authentication_error",
    "invalid_api_key",
    // Rate, capacity and transport.
    "rate_limit_error",
    "rate_limit_exceeded",
    "overloaded_error",
    "api_error",
    "server_error",
    "timeout_error",
    "timeout",
    // Billing and quota.
    "insufficient_quota",
    "billing_hard_limit_reached",
    "credit_balance_too_low",
    // Account lifecycle.
    "account_deleted",
    "account_not_found",
    "account_deactivated",
    "account_revoked",
    "account_deleted_error",
    "account_not_found_error",
    "account_deactivated_error",
    "account_revoked_error",
    "organization_deactivated",
    // Context window.
    "context_length_exceeded",
    "context_window_exceeded",
    "model_context_window_exceeded",
    "prompt_too_long",
    "input_too_large",
    // Not found / size (Anthropic `not_found_error`, `request_too_large`;
    // OpenAI `model_not_found` code).
    "not_found_error",
    "request_too_large",
    "model_not_found",
];

/// Request ids must start with one of these (Anthropic `request-id` and
/// OpenAI `x-request-id` both use `req_`).
const REQUEST_ID_PREFIXES: [&str; 2] = ["req_", "req-"];
/// Accepted request-id length in bytes; the upper bound mirrors the
/// protocol's `provider_request_id` bound (128 bytes).
const REQUEST_ID_BYTES: std::ops::RangeInclusive<usize> = 7..=128;
/// Case-insensitive substrings that reject a request id which may carry an
/// account/session identifier instead of an opaque request token.
const REQUEST_ID_ACCOUNT_MARKERS: &[&str] = &[
    "acct_", "account", "org_", "user_", "credit", "cookie", "session", "token", "prompt", "body",
    "email",
];

/// Only public provider or documentation origins have a useful host to show.
/// Exact matches prevent tenant subdomains (including `status.*` impostors)
/// from becoming part of a durable error or masked export.
pub(crate) const PUBLIC_PROVIDER_URL_HOSTS: &[&str] = &[
    "api.openai.com",
    "platform.openai.com",
    "help.openai.com",
    "status.openai.com",
    "api.anthropic.com",
    "console.anthropic.com",
    "docs.anthropic.com",
    "docs.claude.com",
    "platform.claude.com",
    "support.anthropic.com",
    "status.anthropic.com",
    "generativelanguage.googleapis.com",
    "ai.google.dev",
    "ai.dev",
    "status.cloud.google.com",
    "api.deepseek.com",
    "platform.deepseek.com",
    "status.deepseek.com",
];

/// Locates the provider's own error prose in an HTTP error body (JSON
/// envelope, or the raw UTF-8 body). Raw and untrusted: callers may classify
/// it but must publish it only through `ProviderError::with_provider_detail`.
pub(crate) fn http_error_prose(body: &[u8]) -> Option<String> {
    match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(value) => provider_error_message(&value)
            .map(str::to_owned)
            .or_else(|| Some(PROVIDER_DETAIL_WITHHELD.to_owned())),
        Err(_) => match std::str::from_utf8(body) {
            Ok(prose) if prose.trim_start().starts_with(['{', '[', '<']) => {
                Some(PROVIDER_DETAIL_WITHHELD.to_owned())
            }
            Ok(prose) => Some(prose.to_owned()),
            Err(_) => Some(PROVIDER_DETAIL_WITHHELD.to_owned()),
        },
    }
}

/// Locates the provider's own error prose in a parsed error value. Raw and
/// untrusted; see [`http_error_prose`].
pub(crate) fn provider_error_message(value: &serde_json::Value) -> Option<&str> {
    value
        .pointer("/error/message")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            value
                .pointer("/response/error/message")
                .and_then(serde_json::Value::as_str)
        })
        .or_else(|| value.get("message").and_then(serde_json::Value::as_str))
        .or_else(|| value.get("detail").and_then(serde_json::Value::as_str))
        .or_else(|| value.get("error").and_then(serde_json::Value::as_str))
        .or_else(|| {
            value
                .pointer("/response/error")
                .and_then(serde_json::Value::as_str)
        })
        .or_else(|| value.as_str())
}

/// Publishes a provider error `type`/`code` only when it is an exact member
/// of [`PUBLIC_PROVIDER_ERROR_TYPES`].
pub(crate) fn safe_error_type(value: &str) -> Option<&str> {
    PUBLIC_PROVIDER_ERROR_TYPES
        .contains(&value)
        .then_some(value)
}

/// Publishes a provider request id only when it is a bounded `req_`/`req-`
/// ASCII token (`[A-Za-z0-9_-]`) free of account markers.
pub(crate) fn safe_request_id(value: &str) -> Option<&str> {
    (REQUEST_ID_BYTES.contains(&value.len())
        && REQUEST_ID_PREFIXES
            .iter()
            .any(|prefix| value.starts_with(prefix))
        && !contains_marker(value, REQUEST_ID_ACCOUNT_MARKERS)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')))
    .then_some(value)
}

fn contains_marker(value: &str, markers: &[&str]) -> bool {
    let lower = value.to_ascii_lowercase();
    markers.iter().any(|marker| lower.contains(marker))
}

fn redact_credentials(detail: &str) -> Option<String> {
    let mut spans = credential_spans(detail).into_iter().peekable();
    let mut output = String::new();
    let mut offset = 0;
    for piece in detail.split_inclusive(char::is_whitespace) {
        let word = piece.trim_end_matches(char::is_whitespace);
        let whitespace = &piece[word.len()..];
        while spans.peek().is_some_and(|span| span.end <= offset) {
            spans.next();
        }
        let secret = spans
            .peek()
            .is_some_and(|span| span.start < offset + word.len());
        if !word.is_empty() && secret {
            output.push_str("[REDACTED]");
        } else {
            output.push_str(word);
        }
        output.push_str(whitespace);
        offset += piece.len();
    }
    (!output.trim().is_empty()).then_some(output)
}

fn normalize_label(word: &str) -> String {
    word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
        .to_ascii_lowercase()
}

const SECRET_LABELS: &[&str] = &[
    "proxy-authorization",
    "authorization",
    "x-api-key",
    "x_api_key",
    "api-key",
    "api_key",
    "apikey",
    "access_token",
    "access-token",
    "refresh_token",
    "refresh-token",
];

fn is_secret_label(value: &str) -> bool {
    SECRET_LABELS
        .iter()
        .any(|label| value.eq_ignore_ascii_case(label))
}

#[derive(Clone, Copy)]
struct CredentialStart {
    introducer: usize,
    value: usize,
    redact: usize,
    authorization: bool,
    assignment: bool,
}

// Discovery passes raw input and offsets. Only the consumer classifies words,
// selects quoted contents, or determines a credential's redaction boundaries.
enum CredentialCandidate {
    Introduced(CredentialStart),
    Word(usize),
    Quote(usize),
}

fn credential_starts(detail: &str) -> Vec<CredentialStart> {
    // Discover introductions before consuming values so the earliest quoted
    // value owns any apparent labels/assignments inside its matching quote.
    let mut starts: Vec<_> = detail
        .match_indices([':', '='])
        .filter_map(|(separator, _)| {
            let prefix = detail[..separator]
                .trim_end_matches(|c: char| c.is_whitespace() || matches!(c, '\'' | '"'));
            SECRET_LABELS.iter().chain([&"bearer"]).find_map(|label| {
                let start = prefix.len().checked_sub(label.len())?;
                if !prefix.get(start..)?.eq_ignore_ascii_case(label) {
                    return None;
                }
                let value = separator + 1;
                Some(CredentialStart {
                    introducer: start,
                    value,
                    redact: start,
                    authorization: label.ends_with("authorization") || *label == "bearer",
                    assignment: true,
                })
            })
        })
        .collect();
    let mut offset = 0;
    for piece in detail.split_inclusive(char::is_whitespace) {
        let word = piece.trim_end_matches(char::is_whitespace);
        let normalized = normalize_label(word);
        let assigned = starts.iter().any(|start| {
            start.assignment && (offset..offset + word.len()).contains(&start.introducer)
        });
        if !assigned && (normalized == "bearer" || is_secret_label(&normalized)) {
            // Do not discard punctuation-separated labels: the consumer
            // normalizes every ':'/'='/whitespace combination before choosing
            // its quoted or unquoted grammar. An actual assignment above owns
            // its label, including its authorization/non-authorization policy.
            starts.push(CredentialStart {
                introducer: offset,
                value: offset + word.len(),
                redact: offset + word.len(),
                authorization: true,
                assignment: false,
            });
        }
        let (start, _) = consume_credential_value(detail, CredentialCandidate::Word(offset), None);
        if let Some(start) = start {
            starts.push(start);
        }
        offset += piece.len();
    }
    // Known prefixes can occur anywhere in a quoted value, including unknown
    // assignments such as echoed="opaque head sk-... opaque tail".
    let mut quotes = detail.match_indices(['\'', '"']).peekable();
    while let Some((value, _)) = quotes.next() {
        let (start, end) =
            consume_credential_value(detail, CredentialCandidate::Quote(value), None);
        if let Some(start) = start {
            starts.push(start);
        }
        while quotes.peek().is_some_and(|(quote, _)| *quote < end) {
            quotes.next();
        }
    }
    starts.sort_by_key(|start| start.introducer);
    starts
}

fn credential_spans(detail: &str) -> Vec<std::ops::Range<usize>> {
    let starts = credential_starts(detail);
    let mut headers = starts.iter().filter(|start| start.assignment).peekable();
    let mut spans = Vec::new();
    let mut consumed_until = 0;
    for start in &starts {
        if start.introducer < consumed_until {
            continue;
        }
        while headers
            .peek()
            .is_some_and(|header| header.introducer < start.value)
        {
            headers.next();
        }
        let next_header = headers.peek().map(|header| header.introducer);
        let (consumed, end) =
            consume_credential_value(detail, CredentialCandidate::Introduced(*start), next_header);
        if let Some(consumed) = consumed {
            spans.push(consumed.redact..end);
        }
        consumed_until = end;
    }
    spans
}

// The only credential-value consumer. Discovery supplies raw input and candidate
// offsets, never pre-trimmed words or selected quote contents. Prefix thresholds,
// value starts, quote classification and value ends all belong to this function.
// The public presentation applies the 512-byte UTF-8 bound after redaction.
fn consume_credential_value(
    detail: &str,
    candidate: CredentialCandidate,
    next_header: Option<usize>,
) -> (Option<CredentialStart>, usize) {
    // This classifier is private to the consumer and shared by its raw-word and
    // quoted-value paths. Keep the established prefix length/dot thresholds.
    let known_prefix = |value: &str| {
        let lowercase = value.to_ascii_lowercase();
        let value = lowercase
            .trim_end_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-');
        let jwt_separator = value.rmatch_indices('.').nth(1).map(|(offset, _)| offset);
        value.char_indices().find_map(|(start, _)| {
            let token = &value[start..];
            ((token.starts_with("sk-") && token.len() >= 12)
                || (token.starts_with("sess-") && token.len() >= 16)
                || (token.starts_with("akia")
                    && token[4..]
                        .bytes()
                        .take_while(u8::is_ascii_alphanumeric)
                        .take(16)
                        .count()
                        >= 16)
                || (token.starts_with("ghp_")
                    && token[4..]
                        .bytes()
                        .take_while(u8::is_ascii_alphanumeric)
                        .take(20)
                        .count()
                        >= 20)
                || (token.starts_with("xoxb-")
                    && token[5..]
                        .bytes()
                        .take_while(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
                        .take(10)
                        .count()
                        >= 10)
                || (token.starts_with("eyj")
                    && token.len() >= 24
                    && jwt_separator.is_some_and(|separator| start < separator)))
            .then_some(start)
        })
    };
    let quoted_candidate = matches!(candidate, CredentialCandidate::Quote(_));
    let mut start = match candidate {
        CredentialCandidate::Introduced(start) => start,
        CredentialCandidate::Word(offset) => {
            let value = &detail[offset..];
            let word_end = value.find(char::is_whitespace).unwrap_or(value.len());
            let Some(prefix) = known_prefix(&value[..word_end]) else {
                return (None, offset);
            };
            CredentialStart {
                introducer: offset + prefix,
                value: offset + prefix,
                redact: offset,
                authorization: false,
                assignment: false,
            }
        }
        CredentialCandidate::Quote(value) => {
            // Apostrophes in diagnostic words do not open quoted values.
            if detail.as_bytes()[value] == b'\''
                && detail[..value]
                    .chars()
                    .next_back()
                    .is_some_and(char::is_alphanumeric)
                && detail[value + 1..]
                    .chars()
                    .next()
                    .is_some_and(char::is_alphanumeric)
            {
                return (None, value + 1);
            }
            CredentialStart {
                introducer: value,
                value,
                redact: value,
                authorization: false,
                assignment: false,
            }
        }
    };
    if start.assignment && detail[start.value..].starts_with(char::is_whitespace) {
        start.redact = start.value;
    }
    let schemes = ["bearer", "basic", "token"];
    let mut cursor = start.value;
    let end = loop {
        // Normalize label and scheme separators before any grammar selection.
        let value = detail[cursor..]
            .trim_start_matches(|c: char| c.is_whitespace() || matches!(c, ':' | '='));
        cursor = detail.len() - value.len();
        let Some(&first) = value.as_bytes().first() else {
            break cursor;
        };
        if matches!(first, b'"' | b'\'') {
            let mut escaped = false;
            let mut close = None;
            for (offset, byte) in value.bytes().enumerate().skip(1) {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == first {
                    close = Some(offset);
                    break;
                }
            }
            let Some(close) = close else {
                break detail.len();
            };
            cursor += close + 1;
            if start.authorization
                && schemes
                    .iter()
                    .any(|scheme| value[1..close].eq_ignore_ascii_case(scheme))
            {
                // Separately quoted schemes still introduce the same value.
                continue;
            }
            break cursor;
        }
        if start.authorization
            && let Some(scheme) = schemes.iter().find(|scheme| {
                value
                    .get(..scheme.len())
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case(scheme))
            })
        {
            // Preserve support for touching/stacked schemes (BearerBasic...).
            cursor += scheme.len();
            continue;
        }
        // Only unquoted assignments can end at a subsequent header or use
        // punctuation to delimit a multiword value. Quoted values ignore both.
        let next_header = next_header.filter(|header| *header >= cursor);
        let limit = next_header.unwrap_or(detail.len());
        let credential = &detail[cursor..limit];
        let end = if start.assignment {
            credential
                .find([';', ',', '\n', '\r', '"', '\'', '{', '}', '[', ']'])
                .or_else(|| next_header.map(|_| credential.len()))
        } else {
            None
        }
        .unwrap_or_else(|| {
            credential
                .find(char::is_whitespace)
                .unwrap_or(credential.len())
        });
        break cursor + end;
    };
    if quoted_candidate
        && !detail[start.value..end]
            .split_whitespace()
            .any(|word| known_prefix(word).is_some())
    {
        return (None, end);
    }
    (Some(start), end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_prose_renders_unknown_prose_stays_local_and_blank_is_none() {
        assert_eq!(
            classify_provider_prose("  Overloaded  "),
            Some(ProviderProse::Known("Overloaded".into()))
        );
        assert_eq!(
            classify_provider_prose("Your organization quillmere has no access."),
            Some(ProviderProse::Unknown {
                local_raw: "Your organization quillmere has no access.".into()
            })
        );
        let Some(ProviderProse::Unknown { local_raw }) =
            classify_provider_prose("Credential Bearer sk-provider-secret-value was rejected")
        else {
            panic!("unknown prose");
        };
        assert!(
            !local_raw.contains("sk-provider-secret-value"),
            "{local_raw}"
        );
        assert_eq!(
            classify_provider_prose(PROVIDER_DETAIL_WITHHELD),
            Some(ProviderProse::Withheld)
        );
        assert_eq!(classify_provider_prose("   "), None);
    }
}
