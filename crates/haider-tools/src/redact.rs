//! Shared secret-path filtering and deterministic content redaction.
//!
//! Preview consumers never receive recognized credentials. Callers retain raw
//! bytes only through owner-authorized CAS artifacts; this module produces the
//! first-send preview and never rewrites durable history.

use regex::Regex;
use std::path::Path;
use std::sync::OnceLock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RedactedText {
    pub text: String,
    pub replacements: usize,
}

pub(crate) fn redact_private_key_lines(input: &str) -> RedactedText {
    let mut private_key = false;
    let mut output = String::with_capacity(input.len());
    let mut replacements = 0usize;
    for line in input.split_inclusive('\n') {
        let (content, newline) = line
            .strip_suffix('\n')
            .map_or((line, ""), |content| (content, "\n"));
        let redacted = redact_line_with_private_key_state(content, &mut private_key);
        output.push_str(&redacted.text);
        output.push_str(newline);
        replacements = replacements.saturating_add(redacted.replacements);
    }
    RedactedText {
        text: output,
        replacements,
    }
}

/// Forced secret redaction for the provider-lockdown sandbox. The returned
/// text is the only form the restricted provider receives.
pub fn redact_lockdown_text(input: &str) -> String {
    redact_private_key_lines(input).text
}

pub(crate) fn redact_line_with_private_key_state(
    line: &str,
    private_key: &mut bool,
) -> RedactedText {
    let begins = line.contains("-----BEGIN") && line.contains("PRIVATE KEY-----");
    let ends = line.contains("-----END") && line.contains("PRIVATE KEY-----");
    if *private_key || begins {
        *private_key = !ends;
        return RedactedText {
            text: "[REDACTED:private_key]".into(),
            replacements: 1,
        };
    }
    redact_text(line)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Span {
    start: usize,
    end: usize,
    kind: &'static str,
}

/// Paths search/glob never reveal, even when hidden-file traversal is enabled.
pub(crate) fn is_sensitive_path(path: &Path) -> bool {
    let components = path
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    if components.iter().any(|component| {
        matches!(
            component.as_str(),
            ".aws" | ".ssh" | ".gnupg" | ".kube" | ".azure"
        )
    }) || components
        .windows(2)
        .any(|pair| pair == [".config", "gcloud"])
    {
        return true;
    }
    let Some(name) = components.last() else {
        return false;
    };
    name == ".env"
        || name.starts_with(".env.")
        || name == ".netrc"
        || matches!(name.as_str(), ".npmrc" | ".pypirc")
        || name.starts_with("id_rsa")
        || name.starts_with("credentials")
        || ["pem", "key", "p12", "jks", "keystore", "tfstate"]
            .iter()
            .any(|extension| name.ends_with(&format!(".{extension}")))
}

pub(crate) fn is_token_config_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name.to_ascii_lowercase().as_str(), ".npmrc" | ".pypirc"))
}

pub(crate) fn token_config_contains_secret(bytes: &[u8]) -> bool {
    let sniff = String::from_utf8_lossy(bytes).to_ascii_lowercase();
    sniff.lines().any(|line| {
        let compact = line.trim();
        !compact.starts_with('#')
            && [
                "token", "password", "passwd", "secret", "auth", "apikey", "api_key",
            ]
            .iter()
            .any(|name| compact.contains(name))
            && (compact.contains('=') || compact.contains(':'))
    })
}

pub(crate) fn redact_text(input: &str) -> RedactedText {
    let mut spans = Vec::new();
    if let Some(regex) = private_key_regex() {
        for found in regex.find_iter(input) {
            spans.push(Span {
                start: found.start(),
                end: found.end(),
                kind: "private_key",
            });
        }
    }
    if let Some(regex) = known_secret_regex() {
        for found in regex.find_iter(input) {
            if spans
                .iter()
                .any(|span| found.start() < span.end && span.start < found.end())
            {
                continue;
            }
            spans.push(Span {
                start: found.start(),
                end: found.end(),
                kind: known_kind(found.as_str()),
            });
        }
    }
    if let Some(regex) = entropy_candidate_regex() {
        for found in regex.find_iter(input) {
            if spans
                .iter()
                .any(|span| found.start() < span.end && span.start < found.end())
                || !looks_high_entropy(found.as_str())
            {
                continue;
            }
            spans.push(Span {
                start: found.start(),
                end: found.end(),
                kind: "high_entropy",
            });
        }
    }
    if let Some(regex) = private_key_material_regex() {
        for found in regex.find_iter(input) {
            if spans
                .iter()
                .any(|span| found.start() < span.end && span.start < found.end())
                || !looks_high_entropy(found.as_str().trim())
            {
                continue;
            }
            spans.push(Span {
                start: found.start(),
                end: found.end(),
                kind: "private_key_material",
            });
        }
    }
    spans.sort_by_key(|span| (span.start, span.end));
    if spans.is_empty() {
        return RedactedText {
            text: input.to_owned(),
            replacements: 0,
        };
    }
    let mut output = String::with_capacity(input.len());
    let mut cursor = 0;
    let mut replacements = 0usize;
    for span in spans {
        if span.start < cursor {
            continue;
        }
        output.push_str(&input[cursor..span.start]);
        output.push_str("[REDACTED:");
        output.push_str(span.kind);
        output.push(']');
        cursor = span.end;
        replacements = replacements.saturating_add(1);
    }
    output.push_str(&input[cursor..]);
    RedactedText {
        text: output,
        replacements,
    }
}

fn private_key_regex() -> Option<&'static Regex> {
    static REGEX: OnceLock<Option<Regex>> = OnceLock::new();
    REGEX
        .get_or_init(|| {
            Regex::new(
                r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|\z)",
            )
            .ok()
        })
        .as_ref()
}

fn known_secret_regex() -> Option<&'static Regex> {
    static REGEX: OnceLock<Option<Regex>> = OnceLock::new();
    REGEX
        .get_or_init(|| {
            Regex::new(
                r"AKIA[0-9A-Z]{16}|sk-[A-Za-z0-9_-]{16,}|ghp_[A-Za-z0-9]{20,}|xoxb-[A-Za-z0-9-]{10,}|eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
            )
            .ok()
        })
        .as_ref()
}

fn entropy_candidate_regex() -> Option<&'static Regex> {
    static REGEX: OnceLock<Option<Regex>> = OnceLock::new();
    REGEX
        .get_or_init(|| Regex::new(r"[A-Za-z0-9_+/=-]{32,}").ok())
        .as_ref()
}

fn private_key_material_regex() -> Option<&'static Regex> {
    static REGEX: OnceLock<Option<Regex>> = OnceLock::new();
    REGEX
        .get_or_init(|| Regex::new(r"(?m)^[ \t]*[A-Za-z0-9+/]{16,}={0,2}[ \t]*$").ok())
        .as_ref()
}

fn known_kind(value: &str) -> &'static str {
    if value.starts_with("AKIA") {
        "aws_access_key"
    } else if value.starts_with("sk-") {
        "api_key"
    } else if value.starts_with("ghp_") {
        "github_token"
    } else if value.starts_with("xoxb-") {
        "slack_token"
    } else {
        "jwt"
    }
}

fn looks_high_entropy(value: &str) -> bool {
    let mut counts = [0usize; 256];
    for byte in value.bytes() {
        counts[usize::from(byte)] = counts[usize::from(byte)].saturating_add(1);
    }
    if counts.iter().filter(|count| **count > 0).count() < 8 {
        return false;
    }
    let length = value.len() as f64;
    let entropy = counts
        .iter()
        .filter(|count| **count > 0)
        .fold(0.0, |entropy, count| {
            let probability = *count as f64 / length;
            entropy - probability * probability.log2()
        });
    entropy >= 3.5
}

#[cfg(test)]
<<<<<<< HEAD
#[path = "redact_tests.rs"]
mod redact_tests;
=======
#[path = "redact/tests/redact_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "redact_lockdown_tests.rs"]
mod lockdown_tests;
>>>>>>> wave-965-d
