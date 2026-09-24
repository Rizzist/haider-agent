//! Stable content for the no-progress continuation guard. These transforms are
//! used only for comparison: the journal and model still receive original data.
//! A changing count or ordinary word remains visible to the guard.

use regex::{Captures, Regex};
use serde_json::Value;
use std::sync::OnceLock;
use unicode_normalization::UnicodeNormalization;

// Dates seen in RFC3339, ISO-8601, and ordinary log lines. The pattern also
// accepts a comma fractional separator and slash-delimited log dates.
const TIMESTAMP_PATTERN: &str = r"\b(?:\d{4}[-/]\d{2}[-/]\d{2}|\d{2}/\d{2}/\d{4})[Tt ]\d{2}:\d{2}:\d{2}(?:[.,]\d+)?(?:[Zz]|[+-]\d{2}:?\d{2})?\b";
const SYSLOG_TIMESTAMP_PATTERN: &str = r"\b[A-Z][a-z]{2} +\d{1,2} +\d{2}:\d{2}:\d{2}\b";
const APACHE_TIMESTAMP_PATTERN: &str =
    r"\b\d{1,2}/[A-Z][a-z]{2}/\d{4}:\d{2}:\d{2}:\d{2}(?: [+-]\d{4})?\b";
const UUID_PATTERN: &str = r"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b";
// The field patterns handle both log-style `key=value` and JSON-style
// `"key": "value"`; a short numeric request_id is still an opaque ID.
const ID_FIELD_PATTERN: &str =
    r#"(?i)\b((?:request|trace|span|message|msg|call|toolu)[_-]?id)"?\s*[:=]\s*"?[A-Za-z0-9_-]+"?"#;
const EPOCH_FIELD_PATTERN: &str = r#"\b([A-Za-z_][A-Za-z0-9_]*)"?\s*[:=]\s*"?(\d{10}|\d{13})"?\b"#;
const PREFIXED_ID_PATTERN: &str = r"\b(?:req|msg|call|toolu)_[A-Za-z0-9_-]{6,}\b";
const GENERIC_ID_PATTERN: &str = r"\b[A-Za-z][A-Za-z0-9]{1,20}_[A-Za-z0-9]{16,}\b";
const LONG_HEX_PATTERN: &str = r"(?i)\b[0-9a-f]{16,}\b";
const LONG_BASE64_PATTERN: &str = r"\b[A-Za-z0-9+/]{16,}={0,2}";

#[allow(clippy::expect_used)] // Every source is a fixed, reviewed pattern above.
fn pattern(slot: &'static OnceLock<Regex>, source: &'static str) -> &'static Regex {
    slot.get_or_init(|| Regex::new(source).expect("constant fingerprint pattern"))
}

fn collapse_whitespace(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(super) fn assistant_text(input: &str) -> String {
    collapse_whitespace(&input.nfkc().collect::<String>())
}

/// Canonical JSON keeps object-key ordering stable and normalizes only string
/// values; JSON numbers and booleans retain their exact meaning.
pub(super) fn arguments(input: &Value) -> String {
    fn normalize(value: &Value) -> Value {
        match value {
            Value::String(text) => Value::String(collapse_whitespace(text)),
            Value::Array(items) => Value::Array(items.iter().map(normalize).collect()),
            Value::Object(fields) => Value::Object(
                fields
                    .iter()
                    .map(|(key, value)| (key.clone(), normalize(value)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
    normalize(input).to_string()
}

fn epoch_field(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [
        "time",
        "timestamp",
        "ts",
        "created",
        "updated",
        "createdat",
        "updatedat",
    ]
    .iter()
    .any(|suffix| name == *suffix || name.ends_with(&format!("_{suffix}")))
        || name.ends_with("_created_at")
        || name.ends_with("_updated_at")
        || matches!(name.as_str(), "created_at" | "updated_at")
}

/// Mask volatile metadata in model-visible tool output. Replacement markers
/// preserve which kind of field was present, so adding a field still counts as
/// progress. Ordinary numeric counts and prose are never deliberately masked.
pub(super) fn result(input: &str) -> String {
    static TIMESTAMP: OnceLock<Regex> = OnceLock::new();
    static SYSLOG_TIMESTAMP: OnceLock<Regex> = OnceLock::new();
    static APACHE_TIMESTAMP: OnceLock<Regex> = OnceLock::new();
    static UUID: OnceLock<Regex> = OnceLock::new();
    static ID_FIELD: OnceLock<Regex> = OnceLock::new();
    static EPOCH_FIELD: OnceLock<Regex> = OnceLock::new();
    static PREFIXED_ID: OnceLock<Regex> = OnceLock::new();
    static GENERIC_ID: OnceLock<Regex> = OnceLock::new();
    static LONG_HEX: OnceLock<Regex> = OnceLock::new();
    static LONG_BASE64: OnceLock<Regex> = OnceLock::new();

    let mut text = collapse_whitespace(input);
    text = pattern(&TIMESTAMP, TIMESTAMP_PATTERN)
        .replace_all(&text, "<timestamp>")
        .into_owned();
    text = pattern(&SYSLOG_TIMESTAMP, SYSLOG_TIMESTAMP_PATTERN)
        .replace_all(&text, "<timestamp>")
        .into_owned();
    text = pattern(&APACHE_TIMESTAMP, APACHE_TIMESTAMP_PATTERN)
        .replace_all(&text, "<timestamp>")
        .into_owned();
    text = pattern(&EPOCH_FIELD, EPOCH_FIELD_PATTERN)
        .replace_all(&text, |caps: &Captures<'_>| {
            if epoch_field(&caps[1]) {
                format!("{}=<epoch>", &caps[1])
            } else {
                caps[0].to_owned()
            }
        })
        .into_owned();
    text = pattern(&UUID, UUID_PATTERN)
        .replace_all(&text, "<uuid>")
        .into_owned();
    text = pattern(&ID_FIELD, ID_FIELD_PATTERN)
        .replace_all(&text, "${1}=<id>")
        .into_owned();
    text = pattern(&PREFIXED_ID, PREFIXED_ID_PATTERN)
        .replace_all(&text, "<id>")
        .into_owned();
    text = pattern(&GENERIC_ID, GENERIC_ID_PATTERN)
        .replace_all(&text, "<id>")
        .into_owned();
    text = pattern(&LONG_HEX, LONG_HEX_PATTERN)
        .replace_all(&text, |caps: &Captures<'_>| {
            if caps[0]
                .bytes()
                .any(|byte| byte.is_ascii_hexdigit() && byte.is_ascii_alphabetic())
            {
                "<hex>".to_owned()
            } else {
                caps[0].to_owned()
            }
        })
        .into_owned();
    pattern(&LONG_BASE64, LONG_BASE64_PATTERN)
        .replace_all(&text, |caps: &Captures<'_>| {
            let token = &caps[0];
            if token.bytes().any(|byte| byte.is_ascii_alphabetic())
                && token
                    .bytes()
                    .any(|byte| byte.is_ascii_digit() || matches!(byte, b'+' | b'='))
            {
                "<base64>".to_owned()
            } else {
                token.to_owned()
            }
        })
        .into_owned()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use serde_json::json;

    #[test]
    fn assistant_text_normalizes_unicode_and_space_but_preserves_words() {
        assert_eq!(assistant_text("  Ｓａｍｅ\n  answer  "), "Same answer");
        assert_eq!(assistant_text("\u{2003}\n "), "");
        assert_ne!(assistant_text("count 1"), assistant_text("count 2"));
    }

    #[test]
    fn arguments_order_keys_and_collapse_string_space() {
        let a = json!({"query": "same  topic ", "limit": 5, "nested": [" a\nb "]});
        let b: Value =
            serde_json::from_str(r#"{"nested":["a b"],"limit":5,"query":" same topic"}"#).unwrap();
        assert_eq!(arguments(&a), arguments(&b));
        assert_ne!(
            arguments(&a),
            arguments(&json!({"query":"new topic", "limit":5}))
        );
    }

    #[test]
    fn result_masks_named_volatile_patterns() {
        let pairs = [
            ("at 2026-09-24T00:00:01Z", "at 2026-09-24T00:00:02+03:30"),
            ("2026/09/24 12:34:01,123", "2026/09/24 12:34:02,456"),
            ("Sep 24 12:34:01", "Sep 24 12:34:02"),
            ("24/Sep/2026:12:34:01 +0000", "24/Sep/2026:12:34:02 +0000"),
            ("created_at_time=1727136000", "created_at_time=1727136001"),
            ("created_at=1727136000", "created_at=1727136001"),
            ("createdAt=1727136000", "createdAt=1727136001"),
            ("updated_ts=1727136000000", "updated_ts=1727136000001"),
            (
                "id 550e8400-e29b-41d4-a716-446655440000",
                "id 550e8400-e29b-41d4-a716-446655440001",
            ),
            ("request_id=1 trace_id=abc", "request_id=2 trace_id=def"),
            ("req_0123456789abcdef", "req_0123456789abcdee"),
            ("token_0123456789ABCDEF", "token_0123456789ABCDEE"),
            ("hash=0123456789abcdef", "hash=0123456789abcdee"),
            ("data=SGVsbG8xMjM0NTY3ODkw", "data=SGVsbG8xMjM0NTY3ODkx"),
        ];
        for (first, second) in pairs {
            assert_eq!(result(first), result(second), "{first} vs {second}");
        }
    }

    #[test]
    fn result_keeps_real_count_and_text_progress() {
        assert_ne!(result("count=1"), result("count=2"));
        assert_ne!(result("new page A"), result("new page B"));
        assert_ne!(
            result("total=1234567890123456"),
            result("total=1234567890123457")
        );
        assert_ne!(result("counts=1727136000"), result("counts=1727136001"));
        assert_ne!(
            result("path=/Users/rizzist/Developer"),
            result("path=/Users/rizzist/Documents")
        );
    }
}
