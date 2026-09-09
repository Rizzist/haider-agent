//! Public provider diagnostics, with credential-shaped tokens removed.

pub(crate) fn http_error_detail(body: &[u8]) -> Option<String> {
    match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(value) => provider_error_message(&value).and_then(sanitize_provider_error_detail),
        Err(_) => std::str::from_utf8(body)
            .ok()
            .and_then(sanitize_provider_error_detail),
    }
}

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

pub(crate) fn sanitize_provider_error_detail(detail: &str) -> Option<String> {
    let spans = header_secret_spans(detail);
    let mut spans = spans.into_iter().peekable();
    let mut output = String::new();
    let mut redact_next = false;
    let mut offset = 0;
    for piece in detail.split_inclusive(char::is_whitespace) {
        let word = piece.trim_end_matches(char::is_whitespace);
        let whitespace = &piece[word.len()..];
        while spans.peek().is_some_and(|span| span.range.end <= offset) {
            spans.next();
        }
        let header_secret = spans
            .peek()
            .is_some_and(|span| span.range.start < offset + word.len());
        let quoted_secret_ended = spans
            .peek()
            .is_some_and(|span| span.quoted && span.range.end <= offset + word.len());
        offset += piece.len();
        if word.is_empty() {
            output.push_str(whitespace);
            continue;
        }
        let normalized = word
            .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
            .to_ascii_lowercase();
        if header_secret || redact_next || contains_provider_secret(&normalized) {
            output.push_str("[REDACTED]");
            redact_next = (header_secret || redact_next)
                && !quoted_secret_ended
                && is_authorization_scheme(&normalized);
        } else {
            output.push_str(word);
            redact_next = normalized == "bearer" || is_secret_label(&normalized);
        }
        output.push_str(whitespace);
    }
    (!output.trim().is_empty()).then_some(output)
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

struct SecretSpan {
    range: std::ops::Range<usize>,
    quoted: bool,
}

fn header_secret_spans(detail: &str) -> Vec<SecretSpan> {
    // Locate every header independently of whitespace: the preceding credential
    // may be adjacent to another header, including inside compact JSON.
    let mut headers = detail
        .match_indices([':', '='])
        .filter_map(|(separator, _)| {
            let prefix = detail[..separator]
                .trim_end_matches(|c: char| c.is_whitespace() || matches!(c, '\'' | '"'));
            SECRET_LABELS.iter().find_map(|label| {
                let start = prefix.len().checked_sub(label.len())?;
                prefix.get(start..)?.eq_ignore_ascii_case(label).then_some((
                    start,
                    separator + 1,
                    label.ends_with("authorization"),
                ))
            })
        })
        .peekable();
    let mut spans = Vec::new();
    while let Some((header_start, value_start, authorization)) = headers.next() {
        let value = &detail[value_start..];
        let value_text = value.trim_start_matches(char::is_whitespace);
        let text_start = detail.len() - value_text.len();
        let start = if value.len() == value_text.len() {
            header_start
        } else {
            text_start
        };
        if let Some(end) = quoted_credential_end(value_text, authorization) {
            let end = text_start + end;
            spans.push(SecretSpan {
                range: start..end,
                quoted: true,
            });
            // Apparent headers inside a quoted credential belong to that
            // credential; only resume header matching after its closing quote.
            while headers.peek().is_some_and(|header| header.0 < end) {
                headers.next();
            }
            continue;
        }
        let next_header = headers.peek().map(|header| header.0);
        let limit = next_header.unwrap_or(detail.len());
        let value_text = &detail[text_start..limit];
        let mut credential = value_text.trim_start_matches(['"', '\'']);
        if authorization {
            credential = strip_authorization_scheme(credential).unwrap_or(credential);
        }
        credential =
            credential.trim_start_matches(|c: char| c.is_whitespace() || matches!(c, '\'' | '"'));
        let credential_start = limit - credential.len();
        // Explicit separators bound the whole value, including whitespace in
        // an echoed credential. Without a delimiter, retain diagnostic prose
        // after the credential's first word, as for a standalone Bearer token.
        let end = credential
            .find([';', ',', '\n', '\r', '"', '\'', '{', '}', '[', ']'])
            .or_else(|| next_header.map(|_| credential.len()))
            .unwrap_or_else(|| {
                credential
                    .find(char::is_whitespace)
                    .unwrap_or(credential.len())
            });
        spans.push(SecretSpan {
            range: start..credential_start + end,
            quoted: false,
        });
    }
    spans
}

fn quoted_credential_end(value: &str, authorization: bool) -> Option<usize> {
    let mut credential = value;
    loop {
        if let Some(end) = closing_quote_end(credential) {
            if !authorization || !is_authorization_scheme(&credential[..end]) {
                return Some(value.len() - credential.len() + end);
            }
            // A separately quoted scheme still introduces a credential.
            credential = &credential[end..];
        } else if authorization {
            credential = strip_authorization_scheme(credential.trim_start_matches(['"', '\'']))?;
        } else {
            return None;
        }
        credential = credential.trim_start_matches(char::is_whitespace);
    }
}

fn strip_authorization_scheme(value: &str) -> Option<&str> {
    ["bearer", "basic", "token"].iter().find_map(|scheme| {
        // A scheme may touch its credential (Bearer<token>).
        value
            .get(..scheme.len())?
            .eq_ignore_ascii_case(scheme)
            .then_some(&value[scheme.len()..])
    })
}

fn closing_quote_end(value: &str) -> Option<usize> {
    let quote = *value.as_bytes().first()?;
    if !matches!(quote, b'"' | b'\'') {
        return None;
    }
    let mut escaped = false;
    for (offset, byte) in value.bytes().enumerate().skip(1) {
        if escaped {
            escaped = false;
        } else if byte == b'\\' {
            escaped = true;
        } else if byte == quote {
            return Some(offset + 1);
        }
    }
    None
}

fn is_authorization_scheme(value: &str) -> bool {
    let value = value.trim_matches(['"', '\'']);
    ["bearer", "basic", "token"]
        .iter()
        .any(|scheme| value.eq_ignore_ascii_case(scheme))
}

fn contains_provider_secret(value: &str) -> bool {
    // Match inside punctuation/assignments too. Keep the established sk-/sess-
    // thresholds and include the known token prefixes in haider-tools/redact.rs.
    // A JWT prefix must precede at least two dots in the same word. Locate that
    // boundary once instead of rescanning the suffix at every possible prefix.
    let jwt_separator = value.rmatch_indices('.').nth(1).map(|(offset, _)| offset);
    value.char_indices().any(|(start, _)| {
        let token = &value[start..];
        (token.starts_with("sk-") && token.len() >= 12)
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
                && jwt_separator.is_some_and(|separator| start < separator))
    })
}
