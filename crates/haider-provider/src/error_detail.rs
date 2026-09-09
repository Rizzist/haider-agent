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
        if !word.is_empty() && (secret || contains_provider_secret(&normalize_word(word))) {
            output.push_str("[REDACTED]");
        } else {
            output.push_str(word);
        }
        output.push_str(whitespace);
        offset += piece.len();
    }
    (!output.trim().is_empty()).then_some(output)
}

fn normalize_word(word: &str) -> String {
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

struct CredentialStart {
    introducer: usize,
    value: usize,
    redact: usize,
    authorization: bool,
    assignment: bool,
}

fn credential_starts(detail: &str) -> Vec<CredentialStart> {
    // Discover introductions before consuming values so the earliest quoted
    // value owns any apparent labels/assignments inside its matching quote.
    let mut starts: Vec<_> = detail
        .match_indices([':', '='])
        .filter_map(|(separator, _)| {
            let prefix = detail[..separator]
                .trim_end_matches(|c: char| c.is_whitespace() || matches!(c, '\'' | '"'));
            SECRET_LABELS.iter().find_map(|label| {
                let start = prefix.len().checked_sub(label.len())?;
                if !prefix.get(start..)?.eq_ignore_ascii_case(label) {
                    return None;
                }
                let value = detail.len()
                    - detail[separator + 1..]
                        .trim_start_matches(char::is_whitespace)
                        .len();
                Some(CredentialStart {
                    introducer: start,
                    value,
                    redact: if value == separator + 1 { start } else { value },
                    authorization: label.ends_with("authorization"),
                    assignment: true,
                })
            })
        })
        .collect();
    let mut offset = 0;
    for piece in detail.split_inclusive(char::is_whitespace) {
        let word = piece.trim_end_matches(char::is_whitespace);
        let normalized = normalize_word(word);
        let assigned = word
            .rsplit_once([':', '='])
            .is_some_and(|(_, tail)| !tail.chars().any(|c| c.is_ascii_alphanumeric()));
        if !assigned && (normalized == "bearer" || is_secret_label(&normalized)) {
            let value = detail.len()
                - detail[offset + word.len()..]
                    .trim_start_matches(char::is_whitespace)
                    .len();
            // A separated ':'/'=' belongs to the assignment already found.
            if value < detail.len() && !detail[value..].starts_with([':', '=']) {
                starts.push(CredentialStart {
                    introducer: offset,
                    value,
                    redact: value,
                    authorization: true,
                    assignment: false,
                });
            }
        }
        offset += piece.len();
    }
    // Known prefixes can occur anywhere in a quoted value, including unknown
    // assignments such as echoed="opaque head sk-... opaque tail".
    let mut quotes = detail.match_indices(['\'', '"']).peekable();
    while let Some((value, quote)) = quotes.next() {
        // Apostrophes in diagnostic words do not open quoted values.
        if quote == "'"
            && detail[..value]
                .chars()
                .next_back()
                .is_some_and(char::is_alphanumeric)
            && detail[value + 1..]
                .chars()
                .next()
                .is_some_and(char::is_alphanumeric)
        {
            continue;
        }
        let Some(length) = quoted_value_end(&detail[value..], false) else {
            continue;
        };
        let end = value + length;
        if detail[value..end]
            .split_whitespace()
            .any(|word| contains_provider_secret(&normalize_word(word)))
        {
            starts.push(CredentialStart {
                introducer: value,
                value,
                redact: value,
                authorization: false,
                assignment: false,
            });
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
        let end = if let Some(end) = quoted_value_end(&detail[start.value..], start.authorization) {
            start.value + end
        } else {
            while headers
                .peek()
                .is_some_and(|header| header.introducer < start.value)
            {
                headers.next();
            }
            let next_header = headers.peek().map(|header| header.introducer);
            let limit = next_header.unwrap_or(detail.len());
            let trim = |c: char| c.is_whitespace() || matches!(c, '\'' | '"');
            let mut credential = detail[start.value..limit].trim_start_matches(trim);
            if start.authorization {
                while let Some(rest) = strip_authorization_scheme(credential) {
                    credential = rest.trim_start_matches(trim);
                }
            }
            // Assignments may delimit an unquoted multiword value. Standalone
            // labels consume one credential, preserving subsequent prose.
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
            limit - credential.len() + end
        };
        spans.push(start.redact..end);
        consumed_until = end;
    }
    spans
}

// All value-consuming paths use this matching-quote/escape grammar. Separately
// quoted or stacked authorization schemes introduce the same opaque value.
fn quoted_value_end(value: &str, authorization: bool) -> Option<usize> {
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
