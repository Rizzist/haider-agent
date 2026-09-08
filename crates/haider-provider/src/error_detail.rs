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
    let mut output = String::new();
    let mut redact_next = false;
    for piece in detail.split_inclusive(char::is_whitespace) {
        let word = piece.trim_end_matches(char::is_whitespace);
        let whitespace = &piece[word.len()..];
        if word.is_empty() {
            output.push_str(whitespace);
            continue;
        }
        let normalized = word
            .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
            .to_ascii_lowercase();
        if redact_next {
            output.push_str("[REDACTED]");
            redact_next = is_authorization_scheme(&normalized);
        } else if let Some((label, secret)) = inline_provider_secret(word) {
            output.push_str("[REDACTED]");
            // A compact header can end at its scheme; the credential is
            // still in the following word, including any surrounding quotes.
            redact_next =
                label.eq_ignore_ascii_case("authorization") && is_authorization_scheme(secret);
        } else if looks_like_provider_secret(&normalized) {
            output.push_str("[REDACTED]");
        } else {
            output.push_str(word);
            redact_next = matches!(
                normalized.as_str(),
                "bearer" | "authorization" | "api_key" | "access_token" | "refresh_token"
            );
        }
        output.push_str(whitespace);
    }
    (!output.trim().is_empty()).then_some(output)
}

fn inline_provider_secret(value: &str) -> Option<(&str, &str)> {
    let value = value.trim_start_matches(['"', '\'', '{', '[']);
    let (label, secret) = value.split_once(['=', ':'])?;
    let label = label.trim_end_matches(['"', '\'']);
    let is_secret_label = ["authorization", "api_key", "access_token", "refresh_token"]
        .iter()
        .any(|expected| label.eq_ignore_ascii_case(expected));
    (is_secret_label && !secret.is_empty()).then_some((label, secret))
}

fn is_authorization_scheme(value: &str) -> bool {
    let value = value.trim_matches(['"', '\'']);
    ["bearer", "basic", "token"]
        .iter()
        .any(|scheme| value.eq_ignore_ascii_case(scheme))
}

fn looks_like_provider_secret(value: &str) -> bool {
    (value.starts_with("sk-") && value.len() >= 12)
        || (value.starts_with("sess-") && value.len() >= 16)
        || (value.starts_with("eyj")
            && value.len() >= 24
            && value.bytes().filter(|byte| *byte == b'.').count() >= 2)
}
