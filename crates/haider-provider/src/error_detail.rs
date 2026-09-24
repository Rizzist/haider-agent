//! Public provider diagnostics policy. Provider prose is untrusted account
//! data: this module is the only place that decides which provider-supplied
//! text may reach a durable [`haider_protocol::error::ErrorPresentation`].
//!
//! Entry points (all fail closed):
//! - [`sanitize_provider_error_detail`] — provider prose; called only by
//!   `ProviderError::with_provider_detail`, the single boundary every adapter
//!   (Anthropic, OpenAI, Gemini, ACP) passes its raw prose through.
//! - [`safe_error_type`] — provider error `type`/`code`, exact allowlist.
//! - [`safe_request_id`] — provider request-id header, exact shape.
//!
//! Adapters use [`http_error_prose`] / [`provider_error_message`] only to
//! locate raw prose (and to classify it); they never publish it themselves.

// Static, test-exercised redaction patterns must fail loudly if edited into
// invalid regexes; silently skipping one would expose untrusted provider text.
#![allow(clippy::expect_used)]

use haider_protocol::error::PROVIDER_DETAIL_WITHHELD;
use regex::{Captures, Regex};
use std::sync::LazyLock;

/// Durable protocol detail bound, measured after scrubbing.
const MAX_DETAIL_BYTES: usize = 512;
const REDACTED: &str = "[REDACTED]";

// These patterns identify values, not ordinary uses of words such as
// "tokens", "users", or "organization".
static URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)[A-Z][A-Z0-9+.-]*://[^\s<>"']+"#).expect("static URL regex")
});
static EMAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}").expect("static email regex")
});
static ACCOUNT_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(^|[^A-Z0-9])(?:org|acct|account|user|proj|project|workspace|team|tenant|credit|session)[_-][A-Z0-9_-]+")
    .expect("static account id regex")
});
static LABELED_VALUE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\b(?:organization|org|account|user|project|workspace|team|customer|tenant|email|cookie|session|prompt|request[_ -]?(?:body|id)|response[_ -]?body|token|secret|api[_ -]?key|authorization|access[_ -]?token|refresh[_ -]?token)\b(?:[_ -]?(?:id|name|named))?\s*[:=]\s*[^,;.]+|\b(?:organization|org|account|user|project|workspace|team|customer|tenant)\s+(?:id|name|named)\s+[^,;.]+"#).expect("static labeled value regex")
});
static ACCOUNT_LABEL: LazyLock<Regex> = LazyLock::new(|| {
    // Case-insensitive, Unicode word boundaries: `Organization`, `ORG` and
    // `org` all open a name context; `organizations`, `org_…` and
    // `account-level` do not (ids are handled by `ACCOUNT_ID`).
    Regex::new(
        r"(?i)\b(?:organization|organisation|org|account|user|project|workspace|team|customer|tenant)\b",
    )
    .expect("static account label regex")
});
/// Lower-case words that END an account label's name context instead of
/// being part of a name. Without this list "Your organization does not have
/// access to this model." would lose its useful predicate. Matching is exact
/// and case-sensitive: a title-cased or upper-cased stopword ("Does Labs")
/// may itself be a name and is scrubbed, because over-scrubbing an ambiguous
/// token is preferred to leaking it. Groups: auxiliaries/predicates,
/// adverbs, prepositions/conjunctions/determiners, and structural nouns or
/// participles that describe the account rather than name it.
const ACCOUNT_CONTEXT_END: &[&str] = &[
    // Auxiliaries and predicates.
    "does",
    "doesn't",
    "do",
    "don't",
    "did",
    "didn't",
    "is",
    "isn't",
    "are",
    "aren't",
    "was",
    "wasn't",
    "were",
    "weren't",
    "has",
    "hasn't",
    "have",
    "haven't",
    "had",
    "hadn't",
    "lacks",
    "lack",
    "cannot",
    "can't",
    "can",
    "could",
    "couldn't",
    "may",
    "might",
    "must",
    "should",
    "shouldn't",
    "will",
    "won't",
    "would",
    "wouldn't",
    "needs",
    "need",
    "requires",
    "reached",
    "exceeded",
    "exceeds",
    // Adverbs.
    "not",
    "no",
    "currently",
    "already",
    "still",
    "now",
    "only",
    "also",
    // Prepositions, conjunctions, determiners.
    "to",
    "for",
    "from",
    "with",
    "without",
    "in",
    "on",
    "at",
    "by",
    "of",
    "via",
    "as",
    "and",
    "or",
    "but",
    "that",
    "which",
    "who",
    "if",
    "because",
    "so",
    "than",
    "until",
    "the",
    "a",
    "an",
    "this",
    "your",
    "its",
    "please",
    // Structural nouns / participles about the account, not its name.
    "access",
    "role",
    "field",
    "message",
    "messages",
    "content",
    "settings",
    "balance",
    "limit",
    "limits",
    "quota",
    "billing",
    "plan",
    "tier",
    "level",
    "usage",
    "credits",
    "permissions",
    "owner",
    "admin",
    "administrator",
    "associated",
    "linked",
    "used",
    "specified",
    "provided",
    "configured",
];
/// A copula directly after the label ("Your organization is …") may introduce
/// a name ("is cedarbranch") as easily as a state. The word that follows must
/// then be a context-ending word or one of these known states; anything else
/// is scrubbed as a possible name.
const ACCOUNT_COPULAS: &[&str] = &["is", "was", "are", "were"];
const ACCOUNT_STATES: &[&str] = &[
    "disabled",
    "suspended",
    "deactivated",
    "inactive",
    "active",
    "restricted",
    "blocked",
    "limited",
    "locked",
    "archived",
    "deleted",
    "expired",
    "unverified",
    "verified",
    "pending",
    "required",
    "invalid",
    "unavailable",
    "ineligible",
    "eligible",
    "allowed",
    "permitted",
    "missing",
    "flagged",
    "unable",
    "over",
    "out",
    "past",
    "below",
    "above",
    "being",
    "using",
    "too",
    "rate",
];
/// Characters that end a name clause when they trail a token.
const CLAUSE_DELIMITERS: &[char] = &[',', ';', '.', ':', '!', '?', ')'];
static NAMED_LABEL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:organization|org|account|user|project|workspace|team|customer|tenant)\s+(?:id|name|named)\b").expect("static named label regex")
});
static OPAQUE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:[A-F0-9]{16,}|[A-Z0-9+/=_-]{20,})\b").expect("static opaque value regex")
});
static CREDENTIAL_LABEL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:authorization|proxy-authorization|bearer|x-api-key|x_api_key|api-key|api_key|apikey|access_token|access-token|refresh_token|refresh-token|cookie|set-cookie|echoed)\b\s*[:=]?").expect("static credential label regex")
});
static PRIVATE_LABEL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:prompt|request[_ -]?(?:body|id)|response[_ -]?body|token|secret|session|account|org|organization|user|project|workspace|team|tenant|credit|email)(?:[_ -]?(?:id|name))?\s*[:=]").expect("static private label regex")
});
static CREDENTIAL_PREFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:sk-|sess-|AKIA|ghp_|xoxb-|eyJ)[A-Za-z0-9_-]{8,}")
        .expect("static credential prefix regex")
});

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
const PUBLIC_PROVIDER_URL_HOSTS: &[&str] = &[
    "api.openai.com",
    "platform.openai.com",
    "help.openai.com",
    "status.openai.com",
    "api.anthropic.com",
    "console.anthropic.com",
    "docs.anthropic.com",
    "support.anthropic.com",
    "status.anthropic.com",
    "generativelanguage.googleapis.com",
    "ai.google.dev",
    "status.cloud.google.com",
    "api.deepseek.com",
    "platform.deepseek.com",
    "status.deepseek.com",
];
const LINK_REMOVED: &str = "[link removed]";

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

/// Redact provider prose before publication. Structural/credential tails are
/// removed, then the full output redactor, credential scanner and targeted
/// account-value scrubber process the remaining prose.
/// Unparseable fragments and residual unsafe shapes fail closed.
pub(crate) fn sanitize_provider_error_detail(detail: &str) -> Option<String> {
    if detail.chars().any(unsafe_unicode) {
        return Some(PROVIDER_DETAIL_WITHHELD.to_owned());
    }
    let detail = detail.trim();
    if detail.is_empty() {
        return None;
    }
    if detail == PROVIDER_DETAIL_WITHHELD {
        return Some(detail.to_owned());
    }
    // An HTML/XML fragment can contain arbitrary reflected request content.
    if detail.contains('<') {
        return Some(PROVIDER_DETAIL_WITHHELD.to_owned());
    }
    let detail = match scrub_body_fragment(detail) {
        Some(detail) => detail,
        None => return Some(PROVIDER_DETAIL_WITHHELD.to_owned()),
    };
    // Hide URLs while the general redactor scans prose: it treats even a
    // public hostname as opaque entropy. Only the parsed scheme and host are
    // restored after that pass; userinfo, path, query and fragment are gone.
    let (detail, urls, url_marker) = protect_url_hosts(&detail);
    let tail_label = [
        CREDENTIAL_LABEL.find(&detail).map(|found| found.start()),
        PRIVATE_LABEL.find(&detail).map(|found| found.start()),
        NAMED_LABEL.find(&detail).map(|found| found.start()),
    ]
    .into_iter()
    .flatten()
    .min();
    let detail = if let Some(start) = tail_label {
        if CREDENTIAL_PREFIX
            .find(&detail)
            .is_some_and(|prefix| prefix.start() < start)
        {
            return Some(PROVIDER_DETAIL_WITHHELD.to_owned());
        }
        let start = open_quote_before(&detail, start).unwrap_or(start);
        format!("{}{}", &detail[..start], REDACTED)
    } else if CREDENTIAL_PREFIX.is_match(&detail) {
        return Some(PROVIDER_DETAIL_WITHHELD.to_owned());
    } else {
        detail
    };
    let detail = scrub_account_names(&detail);
    let redacted = haider_tools::redact_output_text(&detail);
    let mut prose = match redact_credentials(&redacted) {
        Some(prose) => prose,
        None => return Some(PROVIDER_DETAIL_WITHHELD.to_owned()),
    };
    for (index, host) in urls.iter().enumerate() {
        prose = prose.replace(&format!("{url_marker}{index}~"), host);
    }
    prose = EMAIL.replace_all(&prose, REDACTED).into_owned();
    prose = ACCOUNT_ID
        .replace_all(&prose, |captures: &Captures<'_>| {
            format!(
                "{}{}",
                captures.get(1).map_or("", |part| part.as_str()),
                REDACTED
            )
        })
        .into_owned();
    prose = LABELED_VALUE.replace_all(&prose, REDACTED).into_owned();
    // Restored URL hosts and redactor output are checked again.
    prose = scrub_account_names(&prose);
    prose = OPAQUE.replace_all(&prose, REDACTED).into_owned();
    let prose = prose.trim();
    if prose.is_empty()
        || prose.len() > MAX_DETAIL_BYTES
        || prose.chars().any(unsafe_unicode)
        || EMAIL.is_match(prose)
        || ACCOUNT_ID.is_match(prose)
        || OPAQUE.is_match(prose)
    {
        return Some(PROVIDER_DETAIL_WITHHELD.to_owned());
    }
    Some(prose.to_owned())
}

fn unsafe_unicode(ch: char) -> bool {
    ch.is_control()
        || (ch.is_alphanumeric() && !ch.is_ascii())
        || matches!(
            ch,
            '\u{00ad}'
                | '\u{200b}'..='\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2060}'..='\u{206f}'
                | '\u{feff}'
        )
}

/// Replaces the name that follows an account label (organization, org,
/// workspace, project, team, tenant, account, user, customer) with
/// `[REDACTED]`, keeping the label and the rest of the sentence. A quoted
/// name is removed through its closing quote (to the end if unclosed); an
/// unquoted name runs until a clause delimiter or a context-ending word.
fn scrub_account_names(detail: &str) -> String {
    let mut output = String::with_capacity(detail.len());
    let mut cursor = 0;
    for label in ACCOUNT_LABEL.find_iter(detail) {
        if label.start() < cursor {
            continue;
        }
        if let Some(name) = account_name_span(detail, label.end()) {
            output.push_str(&detail[cursor..name.start]);
            output.push_str(REDACTED);
            cursor = name.end;
        }
    }
    output.push_str(&detail[cursor..]);
    output
}

fn closing_quote(open: char) -> Option<char> {
    match open {
        '\'' | '"' | '`' => Some(open),
        '\u{201c}' => Some('\u{201d}'),
        '\u{2018}' => Some('\u{2019}'),
        '\u{ab}' => Some('\u{bb}'),
        _ => None,
    }
}

fn account_name_span(detail: &str, label_end: usize) -> Option<std::ops::Range<usize>> {
    let rest = &detail[label_end..];
    // The label must be followed by whitespace, optionally after an
    // appositive comma or dash ("organization, cedarbranch, has …").
    let separator = rest.trim_start_matches([',', '-', '\u{2013}', '\u{2014}']);
    let skipped = rest.len() - separator.len();
    if skipped > 1 || !separator.starts_with(char::is_whitespace) {
        return None;
    }
    let mut position = label_end + skipped + (separator.len() - separator.trim_start().len());
    let mut span: Option<std::ops::Range<usize>> = None;
    let mut first = true;
    let mut after_copula = false;
    while position < detail.len() {
        let rest = &detail[position..];
        let open = rest.chars().next()?;
        if let Some(close) = closing_quote(open) {
            let body = position + open.len_utf8();
            let end = detail[body..]
                .find(close)
                .map_or(detail.len(), |found| body + found + close.len_utf8());
            let start = span.as_ref().map_or(position, |span| span.start);
            return Some(start..end);
        }
        let token_len = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let token = &rest[..token_len];
        let word = token.trim_end_matches(CLAUSE_DELIMITERS);
        if word.is_empty() {
            break;
        }
        let copula = first && ACCOUNT_COPULAS.contains(&word);
        let ends_context = !copula
            && (ACCOUNT_CONTEXT_END.contains(&word)
                || (after_copula && ACCOUNT_STATES.contains(&word)));
        if ends_context {
            break;
        }
        if !copula {
            let end = position + word.len();
            span = Some(span.map_or(position..end, |span| span.start..end));
        }
        if word.len() < token.len() {
            break;
        }
        after_copula = copula;
        first = false;
        let next = &rest[token_len..];
        position += token_len + (next.len() - next.trim_start().len());
    }
    span
}

/// If a sensitive label occurs inside an echoed quoted value, the text
/// before the label belongs to that same value and must be removed too.
fn open_quote_before(detail: &str, end: usize) -> Option<usize> {
    let mut open = None;
    let mut escaped = false;
    for (index, ch) in detail[..end].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && open.is_some() {
            escaped = true;
            continue;
        }
        if !matches!(ch, '\'' | '"') {
            continue;
        }
        if ch == '\''
            && detail[..index]
                .chars()
                .next_back()
                .is_some_and(char::is_alphanumeric)
            && detail[index + 1..]
                .chars()
                .next()
                .is_some_and(char::is_alphanumeric)
        {
            continue;
        }
        match open {
            Some((_, quote)) if quote == ch => open = None,
            None => open = Some((index, ch)),
            _ => {}
        }
    }
    open.map(|(index, _)| index)
}

fn url_host(captures: &Captures<'_>) -> String {
    let original = captures.get(0).map_or("", |capture| capture.as_str());
    let value = original.trim_end_matches(['.', ',', ';', ')', '!']);
    let punctuation = &original[value.len()..];
    url::Url::parse(value)
        .ok()
        .and_then(|parsed| {
            parsed.host_str().and_then(|host| {
                (matches!(parsed.scheme(), "http" | "https")
                    && PUBLIC_PROVIDER_URL_HOSTS.contains(&host))
                .then(|| format!("{}://{host}{punctuation}", parsed.scheme()))
            })
        })
        .unwrap_or_else(|| format!("{LINK_REMOVED}{punctuation}"))
}

fn protect_url_hosts(detail: &str) -> (String, Vec<String>, String) {
    // Pick a marker absent from the input so provider prose cannot impersonate
    // one and cause a different host to be restored into arbitrary text.
    let mut marker = "~u".to_owned();
    while detail.contains(&marker) {
        marker.push('u');
    }
    let mut hosts = Vec::new();
    let protected = URL
        .replace_all(detail, |captures: &Captures<'_>| {
            let index = hosts.len();
            hosts.push(url_host(captures));
            format!("{marker}{index}~")
        })
        .into_owned();
    (protected, hosts, marker)
}

/// A JSON/body echo is removed as one unit. A missing close delimiter is
/// ambiguous: publishing even its suffix could expose request content.
fn scrub_body_fragment(detail: &str) -> Option<String> {
    let mut output = String::new();
    let mut cursor = 0;
    while let Some(relative) = detail[cursor..].find(['{', '[']) {
        let start = cursor + relative;
        if detail[cursor..start].contains(['}', ']']) {
            return None;
        }
        output.push_str(&detail[cursor..start]);
        if detail[start..].starts_with(LINK_REMOVED) {
            output.push_str(LINK_REMOVED);
            cursor = start + LINK_REMOVED.len();
            continue;
        }
        let mut stack = Vec::new();
        let mut quoted = false;
        let mut escaped = false;
        let mut end = None;
        for (offset, ch) in detail[start..].char_indices() {
            if quoted {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    quoted = false;
                }
                continue;
            }
            match ch {
                '"' => quoted = true,
                '{' => stack.push('}'),
                '[' => stack.push(']'),
                '}' | ']' if stack.pop() != Some(ch) => return None,
                '}' | ']' if stack.is_empty() => {
                    end = Some(start + offset + ch.len_utf8());
                    break;
                }
                _ => {}
            }
        }
        cursor = end?;
        output.push_str(REDACTED);
    }
    if detail[cursor..].contains(['}', ']']) {
        return None;
    }
    output.push_str(&detail[cursor..]);
    Some(output)
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

    /// Adapters hand raw prose to the single `with_provider_detail` boundary;
    /// re-applying the policy to its own output must never change it.
    #[test]
    fn sanitizing_is_idempotent_so_one_boundary_suffices() {
        for prose in [
            "You do not have permission to use this model.",
            "  Rate limit reached  ",
            "Overloaded",
            "Unsupported parameter: service_tier",
            "Credential Bearer sk-provider-secret-value was rejected",
            "missing apikey.",
            "contact owner@example.test",
            "See https://example.test/path?token=private for details.",
            "The account (acct_973_private_account) expired.",
            PROVIDER_DETAIL_WITHHELD,
        ] {
            let once = sanitize_provider_error_detail(prose);
            assert_eq!(
                once.as_deref().and_then(sanitize_provider_error_detail),
                once
            );
        }
        assert_eq!(sanitize_provider_error_detail("   "), None);
    }
}
