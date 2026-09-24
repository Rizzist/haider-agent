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

/// A durable workspace receipt must not publish a raw path or a digest of a
/// file that an explicit read would hide. The caller can report an incomplete
/// receipt instead of exporting a guessable identity for that entry.
pub fn workspace_receipt_path_sensitive(path: &Path) -> bool {
    is_sensitive_path(path)
        || path
            .to_str()
            .is_some_and(|text| redact_output_text(text) != text)
}

/// Marker that replaces a path the agent must not learn verbatim.
pub(crate) const SENSITIVE_PATH_MARKER: &str = "[REDACTED:sensitive_path]";

/// Whether a path returned to an agent must be replaced by the marker. The
/// receipt detector also catches credential assignments embedded in
/// otherwise ordinary names (`password=<value>.txt`).
pub(crate) fn model_path_masked(path: &Path) -> bool {
    workspace_receipt_path_sensitive(path) || is_token_config_path(path)
}

/// One presentation rule for every path listing, summary, and error returned
/// to an agent.
pub(crate) fn model_visible_path(path: &Path) -> String {
    if model_path_masked(path) {
        SENSITIVE_PATH_MARKER.to_owned()
    } else {
        path.to_string_lossy().into_owned()
    }
}

/// Whether the output redactor would change any part of `bytes`. Mutation
/// producers use this to keep exact integrity facts (content digests, byte
/// counts) in the owner-local journal instead of agent-visible results.
pub fn content_redaction_affected(bytes: &[u8]) -> bool {
    let mut detector = crate::OutputRedactor::default();
    let _ = detector.push_bytes(bytes);
    let _ = detector.finish_bytes();
    detector.redactions_applied()
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
    /// A secret (open quote or PEM block) continues past the last line fed
    /// in. The redactor keeps its physical line breaks, so later line
    /// coordinates would count the hidden value's lines.
    pub(crate) fn spans_lines(&self) -> bool {
        self.private_key || self.quoted.active()
    }

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
    /// Class of the assignment that opened the quote; continuation lines keep it.
    kind: Option<SecretKind>,
    escaped: bool,
    remaining: usize,
    exhausted: bool,
}

impl QuotedValue {
    fn start(&mut self, quote: u8, kind: SecretKind) {
        self.quote = Some(quote);
        self.kind = Some(kind);
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
        let mut text = SecretKind::PrivateKey.marker().to_owned();
        if line.ends_with('\n') {
            text.push('\n');
        }
        return RedactedText {
            text,
            replacements: 1,
        };
    }
    redacted
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Span {
    start: usize,
    end: usize,
    kind: SecretKind,
}

/// Every class a redaction marker can name. Detection decides the class;
/// `marker` is the single authoritative table of the bytes a class renders
/// as. Every marker is pinned by `marker_table_bytes_are_pinned` (plus
/// `redaction_labels_v1.golden`), and the subset lockdown can emit is frozen
/// by its byte contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SecretKind {
    /// A PEM private-key block, in every policy.
    PrivateKey,
    /// Lockdown only: a bare base64 line. Default mode reports `HighEntropy`,
    /// since it cannot claim key material without PEM context.
    PrivateKeyMaterial,
    AwsAccessKey,
    /// A vendor `sk-` key or an explicit API-key assignment.
    ApiKey,
    GithubToken,
    SlackToken,
    GitlabToken,
    NpmToken,
    StripeApiKey,
    GoogleApiKey,
    /// Default mode: a parsed JWT shape (see `is_jwt`). Lockdown: any value
    /// its legacy known-secret regex matched without a vendor prefix.
    Jwt,
    BearerToken,
    BasicAuth,
    /// An explicit password/passphrase assignment or URL userinfo password.
    Password,
    /// Any other explicit credential assignment.
    SecretValue,
    /// Text accepted by the entropy detector, including qualifying invalid
    /// JWT lookalikes.
    HighEntropy,
}

impl SecretKind {
    const fn marker(self) -> &'static str {
        match self {
            Self::PrivateKey => "[REDACTED:private_key]",
            Self::PrivateKeyMaterial => "[REDACTED:private_key_material]",
            Self::AwsAccessKey => "[REDACTED:aws_access_key]",
            Self::ApiKey => "[REDACTED:api_key]",
            Self::GithubToken => "[REDACTED:github_token]",
            Self::SlackToken => "[REDACTED:slack_token]",
            Self::GitlabToken => "[REDACTED:gitlab_token]",
            Self::NpmToken => "[REDACTED:npm_token]",
            Self::StripeApiKey => "[REDACTED:stripe_api_key]",
            Self::GoogleApiKey => "[REDACTED:google_api_key]",
            Self::Jwt => "[REDACTED:jwt]",
            Self::BearerToken => "[REDACTED:bearer_token]",
            Self::BasicAuth => "[REDACTED:basic_auth]",
            Self::Password => "[REDACTED:password]",
            Self::SecretValue => "[REDACTED:secret_value]",
            Self::HighEntropy => "[REDACTED:high_entropy]",
        }
    }

    /// Classes produced by credential context. Such a value may continue
    /// across lines inside a quote, so it is replaced once per physical line.
    /// (A known-format `sk-` key is also `ApiKey`; it cannot contain a
    /// newline, so the per-line split leaves it a single span.)
    const fn is_per_line(self) -> bool {
        matches!(
            self,
            Self::SecretValue | Self::Password | Self::ApiKey | Self::BearerToken | Self::BasicAuth
        )
    }
}

/// Paths search/glob never reveal, even when hidden-file traversal is enabled.
pub(crate) fn is_sensitive_path(path: &Path) -> bool {
    if cfg!(feature = "android-standalone")
        && path
            .components()
            .any(|c| c.as_os_str() == ".haider-lockdown")
    {
        return true;
    }
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
    if has_pem_assignment_overlap(input) {
        // A quoted assignment may begin on an earlier physical line than the
        // PEM header. Use the stateful line path for that exceptional overlap
        // so the header, body, and line breaks agree with process output.
        redact_lines(input, RedactionPolicy::Standard)
    } else {
        redact_with_state(
            input,
            RedactionPolicy::Standard,
            &mut QuotedValue::default(),
        )
    }
}

fn has_pem_assignment_overlap(input: &str) -> bool {
    if !input.contains("PRIVATE KEY-----") {
        return false;
    }
    let Some(pem) = private_key_regex() else {
        return false;
    };
    let assignments = secret_assignment_spans(input, &mut QuotedValue::default());
    pem.find_iter(input).any(|key| {
        assignments
            .iter()
            .any(|value| value.start < key.end() && key.start() < value.end)
    })
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
        output.push_str(span.kind.marker());
        cursor = span.end;
        replacements = replacements.saturating_add(1);
    }
    output.push_str(&input[cursor..]);
    RedactedText {
        text: output,
        replacements,
    }
}

/// Produces the exact UTF-8 byte prefix of standard redaction. The usual path
/// avoids allocating the complete rendering; a PEM/assignment overlap uses the
/// stateful line rendering so it cannot expose the body or drop line breaks.
/// `full_len` is the complete rendered byte length.
pub(crate) fn redact_text_bounded(input: &str, max_bytes: usize) -> BoundedRedactedText {
    if has_pem_assignment_overlap(input) {
        let redacted = redact_lines(input, RedactionPolicy::Standard);
        return BoundedRedactedText {
            text: utf8_prefix(&redacted.text, max_bytes).to_owned(),
            replacements: redacted.replacements,
            full_len: redacted.text.len(),
        };
    }
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
        let marker = span.kind.marker();
        if prefix_complete {
            prefix_complete = push_bounded(&mut output, marker, max_bytes);
        }
        full_len = full_len.saturating_add(marker.len());
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
                kind: SecretKind::PrivateKey,
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
                kind: if policy == RedactionPolicy::Lockdown {
                    lockdown_known_kind(found.as_str())
                } else {
                    default_known_kind(found.as_str())
                },
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
                if spans.iter().any(|span| {
                    span.kind != SecretKind::HighEntropy
                        && span.kind != SecretKind::SecretValue
                        && span.start == found.start()
                        && span.end == found.end()
                }) {
                    continue;
                }
                if !spans.iter().any(|span| {
                    span.kind == SecretKind::PrivateKey
                        && found.start() < span.end
                        && span.start < found.end()
                }) {
                    spans.retain(|span| found.start() >= span.end || span.start >= found.end());
                    spans.push(Span {
                        start: found.start(),
                        end: found.end(),
                        kind: SecretKind::Password,
                    });
                }
            }
        }
    }
    if policy != RedactionPolicy::Lockdown {
        for span in secret_assignment_spans(input, quoted) {
            // Keep a concrete known format's marker. An invalid JWT-shaped
            // value is only generic entropy or secret context, so a more
            // specific assignment field still wins.
            if spans.iter().any(|other| {
                other.kind != SecretKind::PrivateKey
                    && other.kind != SecretKind::HighEntropy
                    && other.kind != SecretKind::SecretValue
                    && other.start == span.start
                    && other.end == span.end
            }) {
                continue;
            }
            // Mask the union when a credential assignment and a PEM block
            // overlap. In particular, a value beginning at the PEM header
            // must never replace the block with a shorter assignment span;
            // a quote extending beyond the PEM end must stay hidden too.
            let pem_union = spans
                .iter()
                .filter(|other| {
                    other.kind == SecretKind::PrivateKey
                        && span.start < other.end
                        && other.start < span.end
                })
                .fold(None, |union: Option<Span>, other| {
                    let current = union.unwrap_or(span);
                    Some(Span {
                        start: current.start.min(other.start),
                        end: current.end.max(other.end),
                        kind: SecretKind::PrivateKey,
                    })
                });
            if let Some(pem_union) = pem_union {
                spans.retain(|other| pem_union.start >= other.end || other.start >= pem_union.end);
                push_secret_lines(
                    &mut spans,
                    input,
                    pem_union.start,
                    pem_union.end,
                    SecretKind::PrivateKey,
                );
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
                kind: SecretKind::HighEntropy,
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
                kind: if policy == RedactionPolicy::Lockdown {
                    SecretKind::PrivateKeyMaterial
                } else {
                    SecretKind::HighEntropy
                },
            });
        }
    }
    spans.sort_by_key(|span| (span.start, span.end));
    let mut output = Vec::with_capacity(spans.len());
    for span in spans {
        if span.kind.is_per_line() {
            push_secret_lines(&mut output, input, span.start, span.end, span.kind);
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

/// Vendor prefixes both classifiers recognize. The prefix sets are
/// disjoint, so table order does not change the result.
const COMMON_KNOWN_PREFIXES: &[(&[&str], SecretKind)] = &[
    (&["AKIA", "ASIA"], SecretKind::AwsAccessKey),
    (&["sk-"], SecretKind::ApiKey),
    (&["ghp_", "github_pat_"], SecretKind::GithubToken),
    (&["xox"], SecretKind::SlackToken),
];

/// Vendor prefixes only `extended_secret_regex` matches. Among that regex's
/// alternatives, only the GitLab token families begin with `gl`.
const EXTENDED_KNOWN_PREFIXES: &[(&[&str], SecretKind)] = &[
    (&["gl"], SecretKind::GitlabToken),
    (&["npm_"], SecretKind::NpmToken),
    (
        &["sk_live_", "pk_live_", "sk_test_", "rk_live_"],
        SecretKind::StripeApiKey,
    ),
    (&["AIza"], SecretKind::GoogleApiKey),
];

/// Upper bound on each JWT segment decoded while classifying process output.
const JWT_PART_MAX_BYTES: usize = 16 * 1024;

fn prefix_kind(value: &str, table: &[(&[&str], SecretKind)]) -> Option<SecretKind> {
    table
        .iter()
        .find(|(prefixes, _)| prefixes.iter().any(|prefix| value.starts_with(prefix)))
        .map(|(_, kind)| *kind)
}

// The restricted provider has an established byte contract. Keep its original
// classifier independent of the more precise labels used by ordinary previews.
fn lockdown_known_kind(value: &str) -> SecretKind {
    prefix_kind(value, COMMON_KNOWN_PREFIXES).unwrap_or(SecretKind::Jwt)
}

fn default_known_kind(value: &str) -> SecretKind {
    prefix_kind(value, COMMON_KNOWN_PREFIXES)
        .or_else(|| prefix_kind(value, EXTENDED_KNOWN_PREFIXES))
        .unwrap_or_else(|| {
            if is_jwt(value) {
                SecretKind::Jwt
            } else {
                if looks_high_entropy(value) {
                    SecretKind::HighEntropy
                } else {
                    SecretKind::SecretValue
                }
            }
        })
}

fn is_jwt(value: &str) -> bool {
    let mut parts = value.split('.');
    let (Some(header), Some(payload), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    // The marker describes a parsed JWT shape, not a verified signature.
    // Bound decoding and JSON parsing for attacker-controlled process output.
    if [header, payload, signature]
        .iter()
        .any(|part| part.len() > JWT_PART_MAX_BYTES)
    {
        return false;
    }
    let decoder = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let Ok(header) = decoder.decode(header) else {
        return false;
    };
    let Ok(payload) = decoder.decode(payload) else {
        return false;
    };
    decoder
        .decode(signature)
        .is_ok_and(|bytes| !bytes.is_empty())
        && serde_json::from_slice::<serde_json::Value>(&header)
            .ok()
            .is_some_and(|json| json.get("alg").is_some_and(serde_json::Value::is_string))
        && serde_json::from_slice::<serde_json::Value>(&payload)
            .ok()
            .is_some_and(|json| json.is_object())
}

fn extended_secret_regex() -> Option<&'static Regex> {
    static REGEX: OnceLock<Option<Regex>> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(
        r"(?:AKIA|ASIA)[0-9A-Z]{16}|sk-[A-Za-z0-9_-]{16,}|(?:ghp_|github_pat_)[A-Za-z0-9_]{20,}|xox[a-z]+-[A-Za-z0-9-]{10,}|(?:glpat|gloas|gldt|glrt|glrtr|glcbt|glptt|glft|glimt|glagent|glwt|glsoat|glffct)-[A-Za-z0-9_-]{16,}|npm_[A-Za-z0-9]{36}|(?:sk_live_|pk_live_|sk_test_|rk_live_)[A-Za-z0-9]{16,}|AIza[A-Za-z0-9_-]{35}|eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}"
    ).ok()).as_ref()
}

fn secret_assignment_regex() -> Option<&'static Regex> {
    static REGEX: OnceLock<Option<Regex>> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(
        r#"(?i)(?:\b(?:[A-Za-z][A-Za-z0-9]*[_-])*(?P<field>password|passwd|pass[_-]?phrase|secret|client[_-]?secret|private[_-]?key|credentials|_?auth(?:[_-]?token)?|api[_-]?key|access[_-]?key|access[_-]?token|refresh[_-]?token|token|authorization)\b["']?\s*[:=]\s*|\b(?P<scheme>Bearer|Basic)\s+)(?P<value>["']|(?:Bearer|Basic)\s+[^\s"',;]+|[^\s"',;]+)"#
    ).ok()).as_ref()
}

fn secret_assignment_spans(input: &str, quoted: &mut QuotedValue) -> Vec<Span> {
    let mut spans = Vec::new();
    let continuation_kind = quoted.kind.unwrap_or(SecretKind::SecretValue);
    let mut cursor = quoted.consume(input);
    if cursor > 0 {
        spans.push(Span {
            start: 0,
            end: cursor,
            kind: continuation_kind,
        });
    }
    if quoted.active() {
        return spans;
    }
    if let Some(regex) = secret_assignment_regex() {
        for captures in regex.captures_iter(input) {
            let Some(value) = captures.name("value") else {
                continue;
            };
            if value.start() < cursor {
                continue;
            }
            if is_existing_redaction_marker(value.as_str()) {
                cursor = value.end();
                continue;
            }
            let kind = assignment_kind(
                captures.name("field").map(|field| field.as_str()),
                captures.name("scheme").map(|scheme| scheme.as_str()),
                value.as_str(),
            );
            let end = if matches!(value.as_str(), "\"" | "'") {
                quoted.start(input.as_bytes()[value.start()], kind);
                value.end() + quoted.consume(&input[value.end()..])
            } else {
                value.end()
            };
            spans.push(Span {
                start: value.start(),
                end,
                kind,
            });
            cursor = end;
            if quoted.active() {
                break;
            }
        }
    }
    spans
}

fn is_existing_redaction_marker(value: &str) -> bool {
    use SecretKind::*;
    [
        PrivateKey,
        PrivateKeyMaterial,
        AwsAccessKey,
        ApiKey,
        GithubToken,
        SlackToken,
        GitlabToken,
        NpmToken,
        StripeApiKey,
        GoogleApiKey,
        Jwt,
        BearerToken,
        BasicAuth,
        Password,
        SecretValue,
        HighEntropy,
    ]
    .into_iter()
    .any(|kind| value == kind.marker())
}

/// Classify the terminal field captured by the winning assignment rule. A
/// namespace prefix such as PASSWORD_RESET_ has no authority over TOKEN.
fn assignment_kind(field: Option<&str>, scheme: Option<&str>, value: &str) -> SecretKind {
    if scheme.is_some_and(|scheme| scheme.eq_ignore_ascii_case("bearer"))
        || starts_with_auth_scheme(value, "bearer")
    {
        SecretKind::BearerToken
    } else if scheme.is_some_and(|scheme| scheme.eq_ignore_ascii_case("basic"))
        || starts_with_auth_scheme(value, "basic")
    {
        SecretKind::BasicAuth
    } else {
        match field.map(str::to_ascii_lowercase).as_deref() {
            Some("api_key" | "api-key" | "apikey") => SecretKind::ApiKey,
            Some("password" | "passwd" | "passphrase" | "pass_phrase" | "pass-phrase") => {
                SecretKind::Password
            }
            _ => SecretKind::SecretValue,
        }
    }
}

fn starts_with_auth_scheme(value: &str, scheme: &str) -> bool {
    let Some(head) = value.get(..scheme.len()) else {
        return false;
    };
    head.eq_ignore_ascii_case(scheme)
        && value[scheme.len()..]
            .chars()
            .next()
            .is_some_and(char::is_whitespace)
}

fn push_secret_lines(
    spans: &mut Vec<Span>,
    input: &str,
    start: usize,
    end: usize,
    kind: SecretKind,
) {
    // Preserve physical line numbering for fs_read, including empty lines.
    // Newlines count toward the quote window even though they remain visible.
    let mut cursor = start;
    for line in input[start..end].split_inclusive('\n') {
        let content = line.strip_suffix('\n').unwrap_or(line);
        if !content.is_empty() {
            spans.push(Span {
                start: cursor,
                end: cursor + content.len(),
                kind,
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
    if is_identifier(value) || is_code_identifier(value) || is_file_path(value) {
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

/// Ordinary source-code identifiers — snake_case, CamelCase, SCREAMING_CASE,
/// kebab-case, and dotted/`::`-joined paths of those — are not secrets even at
/// high entropy (972 precision fix: `unittest -v` names such as
/// `test_unreadable_file_exit_code (test_wordfreq.OrderingTests)`).
/// Word shape is the discriminator: identifiers spell words, while random
/// tokens interleave case and digit runs. Requiring at least two words keeps
/// every single opaque blob (hex, base58, base64, lowercase noise) redacted,
/// and credential context, known secret patterns, PEM state, and the lockdown
/// classifier all still win before this exemption is consulted.
fn is_code_identifier(value: &str) -> bool {
    if value.len() > 128
        || !value
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-' | b':'))
    {
        return false;
    }
    let mut words = 0usize;
    for segment in value.split(['_', '.', '-', ':']) {
        let bytes = segment.as_bytes();
        // Skip trivial components: `__`/`::` separators, single letters, and
        // short version/index atoms ("v2", "972"). They count as no words.
        if bytes.len() <= 1
            || (bytes.len() <= 5
                && bytes[0].is_ascii_alphanumeric()
                && bytes[1..].iter().all(u8::is_ascii_digit))
        {
            continue;
        }
        match segment_camel_words(segment) {
            Some(count) => words = words.saturating_add(count),
            None => return false,
        }
    }
    words >= 2
}

/// One separator-free identifier segment. Camel humps need a real lowercase
/// tail after a single capital, acronym runs need two capitals, and digits
/// may appear only as one short trailing run ("Tests", "wordfreq",
/// "HTTPServer", "URLs", "sha256"). Returns the number of words spelled, or
/// `None` when the segment does not scan as words.
fn segment_camel_words(segment: &str) -> Option<usize> {
    // Uppercase words in the regression corpus top out at seven bytes
    // (`PROCESS`). Runs of twelve or more are much more likely to be opaque
    // payloads than source-code acronyms, including when followed by digits.
    const MAX_ACRONYM_WORD_BYTES: usize = 11;

    let bytes = segment.as_bytes();
    let mut words = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index].is_ascii_digit() {
            let trailing =
                bytes[index..].iter().all(u8::is_ascii_digit) && bytes.len() - index <= 4;
            return (index > 0 && trailing).then_some(words);
        }
        let upper = bytes[index..]
            .iter()
            .take_while(|byte| byte.is_ascii_uppercase())
            .count();
        let lower = bytes[index + upper..]
            .iter()
            .take_while(|byte| byte.is_ascii_lowercase())
            .count();
        let digits = bytes[index + upper + lower..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if lower == 0 && upper.saturating_add(digits) > MAX_ACRONYM_WORD_BYTES {
            return None;
        }
        words = words.saturating_add(match (upper, lower) {
            (2..=MAX_ACRONYM_WORD_BYTES, tail) if tail >= 2 => 2,
            (2..=MAX_ACRONYM_WORD_BYTES, _) => 1,
            (0 | 1, tail) if tail >= 2 => 1,
            // A single trailing capital ("optionA"), never a leading one.
            (1, 0) if index > 0 && index + 1 == bytes.len() => 1,
            _ => return None,
        });
        index += upper + lower;
    }
    Some(words)
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
