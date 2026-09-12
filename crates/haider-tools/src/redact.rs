//! Shared secret-path filtering and deterministic content redaction.
//!
//! Preview consumers never receive recognized credentials. Callers retain raw
//! bytes only through owner-authorized CAS artifacts; this module produces the
//! first-send preview and never rewrites durable history.

use base64::Engine as _;
use regex::Regex;
use std::path::Path;
use std::sync::OnceLock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RedactedText {
    pub text: String,
    pub replacements: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoundedRedactedText {
    pub text: String,
    pub replacements: usize,
    pub full_len: usize,
}

pub(crate) fn redact_private_key_lines(input: &str) -> RedactedText {
    redact_lines(input, RedactionPolicy::Standard)
}

fn redact_lines(input: &str, policy: RedactionPolicy) -> RedactedText {
    let mut state = RedactionState::default();
    let mut output = String::with_capacity(input.len());
    let mut replacements = 0usize;
    for line in input.split_inclusive('\n') {
        let redacted = redact_line(line, &mut state, policy);
        output.push_str(&redacted.text);
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
    redact_lines(input, RedactionPolicy::Lockdown).text
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RedactionPolicy {
    Standard,
    ExplicitPath,
    Lockdown,
}

/// A call-local allow-list: only the exact named path receives identifier
/// exemptions. It grants no filesystem authority and cannot disable secret
/// patterns, PEM state, or the entropy check for unrecognized random values.
pub(crate) struct ExplicitReadPaths<'a>(&'a Path);

impl<'a> ExplicitReadPaths<'a> {
    pub(crate) fn new(path: &'a Path) -> Self {
        Self(path)
    }

    pub(crate) fn redact(&self, path: &Path, input: &str) -> RedactedText {
        let policy = if self.0 == path {
            RedactionPolicy::ExplicitPath
        } else {
            RedactionPolicy::Standard
        };
        redact_lines(input, policy)
    }
}

/// Secret-safe process/capture text. Redact the complete text before slicing;
/// page and chunk boundaries must never split a secret before classification.
pub fn redact_output_text(input: &str) -> String {
    redact_private_key_lines(input).text
}

/// The quote window shares the process-output ceiling. Once exhausted without
/// a closing quote, fail closed for the rest of this input/stream; do not scan
/// for a later delimiter or retain the discarded bytes.
const QUOTED_SECRET_MAX_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, Default)]
pub(crate) struct RedactionState {
    private_key: bool,
    quoted: QuotedValue,
}

impl RedactionState {
    pub(crate) fn discard_oversized_line(&mut self) {
        if self.quoted.active() {
            self.quoted.exhausted = true;
            self.private_key = false;
        } else {
            // Preserve the existing conservative PEM recovery policy.
            self.private_key = true;
        }
    }
}

#[derive(Clone, Debug, Default)]
struct QuotedValue {
    quote: Option<u8>,
    escaped: bool,
    remaining: usize,
    exhausted: bool,
}

impl QuotedValue {
    fn start(&mut self, quote: u8) {
        self.quote = Some(quote);
        self.escaped = false;
        self.remaining = QUOTED_SECRET_MAX_BYTES - 1;
    }

    fn active(&self) -> bool {
        self.quote.is_some() || self.exhausted
    }

    /// Consume bytes after the opening quote, including CR/LF and escapes.
    /// The same consumer handles complete text and streaming continuations.
    fn consume(&mut self, input: &str) -> usize {
        if self.exhausted {
            return input.len();
        }
        let Some(quote) = self.quote else {
            return 0;
        };
        for (index, byte) in input.bytes().take(self.remaining).enumerate() {
            if self.escaped {
                self.escaped = false;
            } else if byte == b'\\' {
                self.escaped = true;
            } else if byte == quote {
                self.quote = None;
                return index + 1;
            }
        }
        self.remaining = self.remaining.saturating_sub(input.len());
        self.exhausted = self.remaining == 0;
        input.len()
    }
}

pub(crate) fn redact_line_with_state(line: &str, state: &mut RedactionState) -> RedactedText {
    redact_line(line, state, RedactionPolicy::Standard)
}

fn redact_line(line: &str, state: &mut RedactionState, policy: RedactionPolicy) -> RedactedText {
    let begins = line.contains("-----BEGIN") && line.contains("PRIVATE KEY-----");
    let ends = line.contains("-----END") && line.contains("PRIVATE KEY-----");
    let was_quoted = state.quoted.active();
    // Even a line replaced by a PEM marker must advance quote/escape state:
    // a same-line BEGIN/END pair cannot expose the password's next line.
    let redacted = redact_with_state(line, policy, &mut state.quoted);
    if state.private_key || (begins && !(was_quoted && state.quoted.active())) {
        state.private_key = !ends;
        return RedactedText {
            text: if line.ends_with('\n') {
                "[REDACTED:private_key]\n".into()
            } else {
                "[REDACTED:private_key]".into()
            },
            replacements: 1,
        };
    }
    redacted
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

#[cfg(test)]
pub(crate) fn redact_text(input: &str) -> RedactedText {
    redact_with_state(
        input,
        RedactionPolicy::Standard,
        &mut QuotedValue::default(),
    )
}

fn redact_with_state(
    input: &str,
    policy: RedactionPolicy,
    quoted: &mut QuotedValue,
) -> RedactedText {
    let spans = redaction_spans(input, policy, quoted);
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

/// Produces the exact UTF-8 byte prefix of standard redaction without allocating
/// the complete redacted value. `full_len` is the byte length that complete
/// value would have had, so callers retain the existing truncation decision.
pub(crate) fn redact_text_bounded(input: &str, max_bytes: usize) -> BoundedRedactedText {
    let spans = redaction_spans(
        input,
        RedactionPolicy::Standard,
        &mut QuotedValue::default(),
    );
    if spans.is_empty() {
        return BoundedRedactedText {
            text: utf8_prefix(input, max_bytes).to_owned(),
            replacements: 0,
            full_len: input.len(),
        };
    }
    let mut output = String::with_capacity(max_bytes.min(input.len()));
    let mut cursor = 0;
    let mut replacements = 0usize;
    let mut full_len = 0usize;
    let mut prefix_complete = true;
    for span in spans {
        if span.start < cursor {
            continue;
        }
        let plain = &input[cursor..span.start];
        if prefix_complete {
            prefix_complete = push_bounded(&mut output, plain, max_bytes);
        }
        full_len = full_len.saturating_add(plain.len());
        for replacement in ["[REDACTED:", span.kind, "]"] {
            if prefix_complete {
                prefix_complete = push_bounded(&mut output, replacement, max_bytes);
            }
            full_len = full_len.saturating_add(replacement.len());
        }
        cursor = span.end;
        replacements = replacements.saturating_add(1);
    }
    let tail = &input[cursor..];
    if prefix_complete {
        let _ = push_bounded(&mut output, tail, max_bytes);
    }
    full_len = full_len.saturating_add(tail.len());
    BoundedRedactedText {
        text: output,
        replacements,
        full_len,
    }
}

fn redaction_spans(input: &str, policy: RedactionPolicy, quoted: &mut QuotedValue) -> Vec<Span> {
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
    if let Some(regex) = if policy == RedactionPolicy::Lockdown {
        known_secret_regex()
    } else {
        extended_secret_regex()
    } {
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
    // Explicit secret context wins even if the value resembles a digest or
    // a path. Lockdown retains its historical classifier byte-for-byte.
    if policy != RedactionPolicy::Lockdown
        && let Some(regex) = url_userinfo_regex()
    {
        for captures in regex.captures_iter(input) {
            if let Some(found) = captures.get(1) {
                if spans
                    .iter()
                    .any(|span| span.start == found.start() && span.end == found.end())
                {
                    continue;
                }
                if !spans.iter().any(|span| {
                    span.kind == "private_key"
                        && found.start() < span.end
                        && span.start < found.end()
                }) {
                    spans.retain(|span| found.start() >= span.end || span.start >= found.end());
                    spans.push(Span {
                        start: found.start(),
                        end: found.end(),
                        kind: "secret_value",
                    });
                }
            }
        }
    }
    if policy != RedactionPolicy::Lockdown {
        for span in secret_assignment_spans(input, quoted) {
            // Keep a known key's marker, but a PEM-looking continuation is
            // still inside the quoted value and must preserve its line ending.
            if spans.iter().any(|other| {
                other.kind != "private_key" && other.start == span.start && other.end == span.end
            }) {
                continue;
            }
            // An enclosing PEM block retains priority over assignments in its
            // body. A PEM-looking delimiter inside a quote is secret text.
            if spans.iter().any(|other| {
                other.kind == "private_key" && other.start < span.start && span.start < other.end
            }) {
                continue;
            }
            spans.retain(|other| span.start >= other.end || other.start >= span.end);
            spans.push(span);
        }
    }
    let candidates = if policy == RedactionPolicy::Lockdown {
        entropy_candidate_regex()
    } else {
        identifier_candidate_regex()
    };
    if let Some(regex) = candidates {
        for found in regex.find_iter(input) {
            if spans
                .iter()
                .any(|span| found.start() < span.end && span.start < found.end())
                || !looks_high_entropy(found.as_str())
                || (policy != RedactionPolicy::Lockdown
                    && (is_non_secret_carrier(found.as_str(), policy)
                        || is_public_run_id(&input[..found.start()], found.as_str())))
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
                || (policy != RedactionPolicy::Lockdown
                    && is_non_secret_carrier(found.as_str().trim(), policy))
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
    let mut output = Vec::with_capacity(spans.len());
    for span in spans {
        if span.kind == "secret_value" {
            push_secret_lines(&mut output, input, span.start, span.end);
        } else {
            output.push(span);
        }
    }
    output
}

fn push_bounded(output: &mut String, value: &str, max_bytes: usize) -> bool {
    let remaining = max_bytes.saturating_sub(output.len());
    let prefix = utf8_prefix(value, remaining);
    output.push_str(prefix);
    prefix.len() == value.len()
}

fn utf8_prefix(text: &str, max_bytes: usize) -> &str {
    let mut end = max_bytes.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
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
    if value.starts_with("AKIA") || value.starts_with("ASIA") {
        "aws_access_key"
    } else if value.starts_with("sk-") {
        "api_key"
    } else if value.starts_with("ghp_") || value.starts_with("github_pat_") {
        "github_token"
    } else if value.starts_with("xox") {
        "slack_token"
    } else {
        "jwt"
    }
}

fn extended_secret_regex() -> Option<&'static Regex> {
    static REGEX: OnceLock<Option<Regex>> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(
        r"(?:AKIA|ASIA)[0-9A-Z]{16}|sk-[A-Za-z0-9_-]{16,}|(?:ghp_|github_pat_)[A-Za-z0-9_]{20,}|xox[a-z]+-[A-Za-z0-9-]{10,}|eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}"
    ).ok()).as_ref()
}

fn secret_assignment_regex() -> Option<&'static Regex> {
    static REGEX: OnceLock<Option<Regex>> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(
        r#"(?i)(?:\b(?:[A-Za-z][A-Za-z0-9]*[_-])*(?:password|passwd|secret|client[_-]?secret|private[_-]?key|credentials|_?auth(?:[_-]?token)?|api[_-]?key|access[_-]?key|access[_-]?token|refresh[_-]?token|token|authorization)\b["']?\s*[:=]\s*|\b(?:Bearer|Basic)\s+)(["']|(?:Bearer|Basic)\s+[^\s"',;]+|[^\s"',;]+)"#
    ).ok()).as_ref()
}

fn secret_assignment_spans(input: &str, quoted: &mut QuotedValue) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut cursor = quoted.consume(input);
    if cursor > 0 {
        spans.push(Span {
            start: 0,
            end: cursor,
            kind: "secret_value",
        });
    }
    if quoted.active() {
        return spans;
    }
    if let Some(regex) = secret_assignment_regex() {
        for captures in regex.captures_iter(input) {
            let Some(value) = captures.get(1) else {
                continue;
            };
            if value.start() < cursor {
                continue;
            }
            let end = if matches!(value.as_str(), "\"" | "'") {
                quoted.start(input.as_bytes()[value.start()]);
                value.end() + quoted.consume(&input[value.end()..])
            } else {
                value.end()
            };
            spans.push(Span {
                start: value.start(),
                end,
                kind: "secret_value",
            });
            cursor = end;
            if quoted.active() {
                break;
            }
        }
    }
    spans
}

fn push_secret_lines(spans: &mut Vec<Span>, input: &str, start: usize, end: usize) {
    // Preserve physical line numbering for fs_read, including empty lines.
    // Newlines count toward the quote window even though they remain visible.
    let mut cursor = start;
    for line in input[start..end].split_inclusive('\n') {
        let content = line.strip_suffix('\n').unwrap_or(line);
        if !content.is_empty() {
            spans.push(Span {
                start: cursor,
                end: cursor + content.len(),
                kind: "secret_value",
            });
        }
        cursor += line.len();
    }
}

/// Match the authority before path/query delimiters. Percent escapes remain
/// encoded; only the password capture is removed, preserving the username.
fn url_userinfo_regex() -> Option<&'static Regex> {
    static REGEX: OnceLock<Option<Regex>> = OnceLock::new();
    REGEX
        .get_or_init(|| {
            Regex::new(r#"(?i)\b[a-z][a-z0-9+.-]*://[^\s:/?#@"<>]+:([^\s/?#@"<>]*)@"#).ok()
        })
        .as_ref()
}

fn identifier_candidate_regex() -> Option<&'static Regex> {
    static REGEX: OnceLock<Option<Regex>> = OnceLock::new();
    REGEX
        .get_or_init(|| Regex::new(r"[A-Za-z0-9_+/.\\:~-]{26,}={0,2}").ok())
        .as_ref()
}

fn is_non_secret_carrier(value: &str, policy: RedactionPolicy) -> bool {
    let value = value.trim_end_matches(['.', ':']);
    if is_identifier(value) || is_file_path(value) {
        return true;
    }
    // The exact explicit pointer additionally permits conventional tagged
    // identifiers. Never exempt an arbitrary labelled random token.
    if policy == RedactionPolicy::ExplicitPath
        && ["thread-", "run-", "session-", "thread_", "run_", "session_"]
            .iter()
            .any(|prefix| value.strip_prefix(prefix).is_some_and(is_identifier))
    {
        return true;
    }
    // One decoding layer, bounded to a small carrier. This is deliberately
    // not a general "printable base64" exemption (credentials are printable).
    if value.len() > 4096 {
        return false;
    }
    [
        base64::engine::general_purpose::STANDARD,
        base64::engine::general_purpose::STANDARD_NO_PAD,
        base64::engine::general_purpose::URL_SAFE_NO_PAD,
    ]
    .iter()
    .any(|engine| {
        engine
            .decode(value)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .is_some_and(|decoded| {
                (is_identifier(&decoded) || is_file_path(&decoded))
                    && !extended_secret_regex().is_some_and(|regex| regex.is_match(&decoded))
                    && !secret_assignment_regex().is_some_and(|regex| regex.is_match(&decoded))
                    && !is_sensitive_path(Path::new(&decoded))
            })
    })
}

fn is_identifier(value: &str) -> bool {
    if let Some(digest) = value.strip_prefix("HEAD:") {
        return digest.len() == 40 && digest.bytes().all(|byte| byte.is_ascii_hexdigit());
    }
    if let Some(uuid) = value
        .strip_prefix("urn:uuid:")
        .or_else(|| value.strip_prefix("thread-"))
    {
        return is_uuid(uuid);
    }
    let value = value
        .strip_prefix("sha256:")
        .or_else(|| value.strip_prefix("blake3:"))
        .unwrap_or(value);
    if matches!(value.len(), 32 | 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return true;
    }
    if is_uuid(value) || is_cid_v0(value) {
        return true;
    }
    value.len() == 26
        && value.as_bytes()[0] <= b'7'
        && value
            .bytes()
            .all(|byte| b"0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(&byte.to_ascii_uppercase()))
}

fn is_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(i, byte)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

/// CIDv0 is base58btc of a 34-byte SHA-256 multihash (0x12, 0x20, digest).
/// An arbitrary base58/random token is not an identifier exemption.
fn is_cid_v0(value: &str) -> bool {
    if value.len() != 46 || !value.starts_with("Qm") {
        return false;
    }
    let mut decoded = [0u8; 34];
    for byte in value.bytes() {
        let Some(digit) = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
            .iter()
            .position(|candidate| *candidate == byte)
        else {
            return false;
        };
        let mut carry = digit as u16;
        for place in decoded.iter_mut().rev() {
            carry += u16::from(*place) * 58;
            *place = carry as u8;
            carry >>= 8;
        }
        if carry != 0 {
            return false;
        }
    }
    decoded[..2] == [0x12, 0x20]
}

/// The public run_id field supplies context that a bare random value lacks.
/// Run IDs may use mixed-case alphanumerics, including non-base58 letters.
fn is_public_run_id(prefix: &str, value: &str) -> bool {
    prefix.strip_suffix("run_id=").is_some_and(|before| {
        !before.ends_with(|c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
    }) && (26..=64).contains(&value.len())
        && value.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

fn is_file_path(value: &str) -> bool {
    let value = value.trim_end_matches(['.', ':']);
    if value.contains('=')
        || value.contains("://")
        || !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/\\._-~:".contains(c))
    {
        return false;
    }
    let qualified = value.starts_with("./")
        || value.starts_with("../")
        || value.starts_with("~/")
        || value.contains('\\')
        || (value.len() > 3
            && value.as_bytes()[0].is_ascii_alphabetic()
            && value.as_bytes()[1] == b':'
            && value.as_bytes()[2] == b'/');
    if qualified {
        return true;
    }
    // A directory-free UUID filename needs a conventional extension, not just
    // a dot appended to random bytes. Credential-shaped suffixes still win.
    if let Some((stem, extensions)) = value.split_once('.')
        && is_uuid(stem)
        && extensions.split('.').all(|extension| {
            (1..=10).contains(&extension.len())
                && extension.bytes().all(|byte| byte.is_ascii_lowercase())
        })
    {
        return true;
    }
    let mut components = value.trim_start_matches('/').split('/');
    let first = components.next().unwrap_or_default();
    let has_directory = components.next().is_some();
    let has_extension = Path::new(value).extension().is_some();
    // A slash alone is also legal in random base64. Require a filename
    // extension or a conventional directory root (workspace, Users, etc.).
    // Digest/UUID directory roots are already recognized non-secret carriers.
    let named_directory = first.len() >= 3
        && first.as_bytes()[0].is_ascii_alphabetic()
        && first.as_bytes()[1..]
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || matches!(*byte, b'_' | b'-'));
    (has_extension && (has_directory || value.starts_with('/')))
        || (has_directory && (named_directory || is_identifier(first)))
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
#[path = "redact_tests.rs"]
mod redact_tests;

#[cfg(test)]
#[path = "redact_lockdown_tests.rs"]
mod lockdown_tests;
