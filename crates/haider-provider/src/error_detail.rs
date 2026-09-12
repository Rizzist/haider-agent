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
