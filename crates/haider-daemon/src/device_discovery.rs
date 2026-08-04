//! Bounded, metadata-only discovery of first-party device OAuth stores.
//!
//! Parsers deserialize token strings directly into zeroizing buffers and
//! project only public metadata. Missing, oversized, and malformed stores are
//! deliberately indistinguishable from absent stores to the discovery RPC.

use std::fmt;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use haider_rpc::DeviceCredentialCandidateWire;
use serde::de::{IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use zeroize::Zeroizing;

use crate::oauth::{SecretJson, oauth_import_path};

const DISCOVERY_FILE_LIMIT: u64 = 256 * 1024;
const EXPIRING_WINDOW_MS: u64 = 5 * 60 * 1000;
const CLAUDE_DEFAULT_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const GEMINI_UNSUPPORTED_REASON: &str = "Gemini CLI OAuth credentials cannot be imported: Google does not support third-party use of Gemini CLI OAuth; use a Gemini API or Vertex AI credential instead";
const CLAUDE_CUSTOM_CLIENT_REASON: &str =
    "Claude Code credential uses a custom OAuth client that Haider cannot safely refresh";
const KIMI_DEVICE_REASON: &str =
    "Kimi Code credential is missing its matching first-party device identity";
const CLAUDE_UNVERIFIED_REASON: &str = "Claude OAuth path exists, but its credential shape is not verified by current first-party documentation";

#[derive(Debug, Clone)]
pub(crate) struct DeviceCandidate {
    pub wire: DeviceCredentialCandidateWire,
    pub import_source: Option<&'static str>,
}

pub(crate) fn discover_device_candidates(disabled: bool) -> Vec<DeviceCandidate> {
    if disabled || discovery_disabled_by_env() {
        return Vec::new();
    }
    let mut candidates = Vec::new();
    if let Some(candidate) = discover_codex() {
        candidates.push(candidate);
    }
    if let Some(candidate) = discover_claude() {
        candidates.push(candidate);
    }
    if let Some(candidate) = discover_claude_unverified_path() {
        candidates.push(candidate);
    }
    if let Some(candidate) = discover_kimi() {
        candidates.push(candidate);
    }
    if let Some(candidate) = discover_gemini() {
        candidates.push(candidate);
    }
    candidates
}

pub(crate) fn discovery_is_disabled(profile_disabled: bool) -> bool {
    profile_disabled || discovery_disabled_by_env()
}

pub(crate) fn candidate_by_id(disabled: bool, id: &str) -> Option<DeviceCandidate> {
    discover_device_candidates(disabled)
        .into_iter()
        .find(|candidate| candidate.wire.candidate == id)
}

fn discovery_disabled_by_env() -> bool {
    std::env::var("HAIDER_DEVICE_DISCOVERY_DISABLED")
        .ok()
        .is_some_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
}

#[derive(Deserialize)]
struct CodexFile {
    tokens: CodexTokens,
}

#[derive(Deserialize)]
struct CodexTokens {
    access_token: SecretJson,
    refresh_token: SecretJson,
    #[serde(default)]
    id_token: Option<SecretJson>,
    #[serde(default)]
    account_id: Option<String>,
}

#[derive(Default, Deserialize)]
struct CodexClaims {
    #[serde(default)]
    exp: Option<u64>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    chatgpt_account_id: Option<String>,
    #[serde(default, rename = "https://api.openai.com/auth")]
    openai_auth: Option<CodexAuthClaims>,
}

#[derive(Default, Deserialize)]
struct CodexAuthClaims {
    #[serde(default)]
    chatgpt_account_id: Option<String>,
}

fn discover_codex() -> Option<DeviceCandidate> {
    let path = oauth_import_path("codex").ok()?;
    let bytes = read_bounded(&path)?;
    let parsed: CodexFile = serde_json::from_slice(&bytes).ok()?;
    if parsed.tokens.access_token.0.is_empty() || parsed.tokens.refresh_token.0.is_empty() {
        return None;
    }
    let access = decode_jwt::<CodexClaims>(&parsed.tokens.access_token.0).unwrap_or_default();
    let identity = parsed
        .tokens
        .id_token
        .as_ref()
        .and_then(|token| decode_jwt::<CodexClaims>(&token.0))
        .unwrap_or_default();
    let account_label = identity
        .email
        .and_then(nonempty)
        .or_else(|| identity.chatgpt_account_id.and_then(nonempty))
        .or_else(|| {
            identity
                .openai_auth
                .and_then(|claims| claims.chatgpt_account_id.and_then(nonempty))
        })
        .or_else(|| parsed.tokens.account_id.and_then(nonempty));
    let expires_at_ms = access.exp.and_then(|seconds| seconds.checked_mul(1000));
    Some(candidate(
        "codex",
        "openai-oauth",
        "Codex",
        account_label,
        expires_at_ms,
        path,
        true,
        None,
    ))
}

#[derive(Deserialize)]
struct ClaudeFile {
    #[serde(rename = "claudeAiOauth")]
    oauth: ClaudeOauth,
}

#[derive(Deserialize)]
struct ClaudeOauth {
    #[serde(rename = "accessToken")]
    access_token: SecretJson,
    #[serde(rename = "refreshToken")]
    refresh_token: SecretJson,
    #[serde(rename = "expiresAt")]
    expires_at_ms: u64,
    scopes: Vec<String>,
    #[serde(default, rename = "clientId")]
    client_id: Option<String>,
}

fn discover_claude() -> Option<DeviceCandidate> {
    let path = oauth_import_path("claude-code").ok()?;
    let bytes = read_bounded(&path)?;
    let parsed: ClaudeFile = serde_json::from_slice(&bytes).ok()?;
    if parsed.oauth.access_token.0.is_empty()
        || parsed.oauth.refresh_token.0.is_empty()
        || parsed.oauth.expires_at_ms == 0
        || !parsed
            .oauth
            .scopes
            .iter()
            .any(|scope| scope == "user:inference")
    {
        return None;
    }
    let custom_client = parsed
        .oauth
        .client_id
        .as_deref()
        .is_some_and(|client_id| client_id != CLAUDE_DEFAULT_CLIENT_ID);
    Some(candidate(
        "claude-code",
        "anthropic-oauth",
        "Claude Code",
        None,
        Some(parsed.oauth.expires_at_ms),
        path,
        !custom_client,
        custom_client.then(|| CLAUDE_CUSTOM_CLIENT_REASON.to_owned()),
    ))
}

fn discover_claude_unverified_path() -> Option<DeviceCandidate> {
    let path = env_or_home("HAIDER_CLAUDE_OAUTH_PATH", ".claude/oauth")?;
    let bytes = read_bounded(&path)?;
    serde_json::from_slice::<JsonObject>(&bytes).ok()?;
    Some(candidate(
        "claude-oauth-unverified",
        "anthropic-oauth",
        "Claude OAuth (unverified path)",
        None,
        None,
        path,
        false,
        Some(CLAUDE_UNVERIFIED_REASON.to_owned()),
    ))
}

#[derive(Deserialize)]
struct KimiFile {
    access_token: SecretJson,
    refresh_token: SecretJson,
    expires_at: f64,
    scope: String,
    token_type: String,
}

fn discover_kimi() -> Option<DeviceCandidate> {
    let path = env_or_home("HAIDER_KIMI_CREDS_PATH", ".kimi/credentials/kimi-code.json")?;
    let bytes = read_bounded(&path)?;
    let parsed: KimiFile = serde_json::from_slice(&bytes).ok()?;
    let expires_at_ms = seconds_to_millis(parsed.expires_at)?;
    if parsed.access_token.0.is_empty()
        || parsed.refresh_token.0.is_empty()
        || parsed.scope.len() > 64 * 1024
        || !parsed.token_type.eq_ignore_ascii_case("bearer")
    {
        return None;
    }
    let device_id_ok = kimi_device_id_path()
        .and_then(|path| read_bounded(&path))
        .is_some_and(|bytes| valid_kimi_device_id(trim_ascii(&bytes)));
    Some(candidate(
        "kimi-code",
        "kimi-oauth",
        "Kimi Code",
        None,
        Some(expires_at_ms),
        path,
        device_id_ok,
        (!device_id_ok).then(|| KIMI_DEVICE_REASON.to_owned()),
    ))
}

#[derive(Deserialize)]
struct GeminiFile {
    #[serde(default)]
    access_token: Option<SecretJson>,
    #[serde(default)]
    refresh_token: Option<SecretJson>,
    #[serde(default)]
    expiry_date: Option<u64>,
}

fn discover_gemini() -> Option<DeviceCandidate> {
    let path = env_or_home("HAIDER_GEMINI_CREDS_PATH", ".gemini/oauth_creds.json")?;
    let bytes = read_bounded(&path)?;
    let parsed: GeminiFile = serde_json::from_slice(&bytes).ok()?;
    let has_access = parsed
        .access_token
        .as_ref()
        .is_some_and(|token| !token.0.is_empty());
    let has_refresh = parsed
        .refresh_token
        .as_ref()
        .is_some_and(|token| !token.0.is_empty());
    if !has_access && !has_refresh {
        return None;
    }
    Some(candidate(
        "gemini-cli",
        "gemini",
        "Gemini CLI",
        None,
        parsed.expiry_date.filter(|expiry| *expiry != 0),
        path,
        false,
        Some(GEMINI_UNSUPPORTED_REASON.to_owned()),
    ))
}

#[allow(clippy::too_many_arguments)]
fn candidate(
    source: &'static str,
    provider: &str,
    source_label: &str,
    account_label: Option<String>,
    expires_at_ms: Option<u64>,
    path: PathBuf,
    import_supported: bool,
    unsupported_reason: Option<String>,
) -> DeviceCandidate {
    let path_display = path.to_string_lossy().into_owned();
    let candidate = opaque_candidate_id(source, &path_display);
    DeviceCandidate {
        wire: DeviceCredentialCandidateWire {
            candidate,
            provider: provider.to_owned(),
            source_label: source_label.to_owned(),
            account_label,
            freshness: freshness(expires_at_ms),
            expires_at_ms,
            path: path_display,
            import_supported,
            unsupported_reason,
        },
        import_source: import_supported.then_some(source),
    }
}

fn opaque_candidate_id(source: &str, path: &str) -> String {
    let mut input = Vec::with_capacity(source.len() + path.len() + 32);
    input.extend_from_slice(b"haider-device-candidate-v1\0");
    input.extend_from_slice(source.as_bytes());
    input.push(0);
    input.extend_from_slice(path.as_bytes());
    format!("dc1_{}", blake3::hash(&input).to_hex())
}

fn freshness(expires_at_ms: Option<u64>) -> String {
    let Some(expiry) = expires_at_ms else {
        return "unknown".to_owned();
    };
    let Some(now) = now_ms() else {
        return "unknown".to_owned();
    };
    if expiry <= now {
        "expired"
    } else if expiry <= now.saturating_add(EXPIRING_WINDOW_MS) {
        "expiring"
    } else {
        "fresh"
    }
    .to_owned()
}

fn read_bounded(path: &Path) -> Option<Zeroizing<Vec<u8>>> {
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(DISCOVERY_FILE_LIMIT.saturating_add(1))
        .read_to_end(&mut bytes)
        .ok()?;
    (u64::try_from(bytes.len()).ok()? <= DISCOVERY_FILE_LIMIT).then_some(bytes)
}

pub(crate) fn env_or_home(env_name: &str, relative: &str) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(env_name).filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(path));
    }
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|home| home.join(relative))
}

pub(crate) fn kimi_device_id_path() -> Option<PathBuf> {
    env_or_home("HAIDER_KIMI_DEVICE_ID_PATH", ".kimi/device_id")
}

pub(crate) fn valid_kimi_device_id(value: &[u8]) -> bool {
    let (version_index, variant_index) = match value.len() {
        32 => (12, 16),
        36 if value.get(8) == Some(&b'-')
            && value.get(13) == Some(&b'-')
            && value.get(18) == Some(&b'-')
            && value.get(23) == Some(&b'-') =>
        {
            (14, 19)
        }
        _ => return false,
    };
    value.iter().enumerate().all(|(index, byte)| {
        if value.len() == 36 && matches!(index, 8 | 13 | 18 | 23) {
            *byte == b'-'
        } else {
            byte.is_ascii_hexdigit()
        }
    }) && value[version_index] == b'4'
        && matches!(
            value[variant_index].to_ascii_lowercase(),
            b'8' | b'9' | b'a' | b'b'
        )
}

pub(crate) fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |index| index.saturating_add(1));
    &bytes[start..end]
}

fn seconds_to_millis(seconds: f64) -> Option<u64> {
    if !seconds.is_finite() || seconds <= 0.0 || seconds > (u64::MAX / 1000) as f64 {
        return None;
    }
    Some((seconds * 1000.0) as u64)
}

fn decode_jwt<T: serde::de::DeserializeOwned>(token: &[u8]) -> Option<T> {
    let token = std::str::from_utf8(token).ok()?;
    let payload = token.split('.').nth(1)?;
    let mut decoded = Zeroizing::new(Vec::new());
    URL_SAFE_NO_PAD
        .decode_vec(payload.as_bytes(), &mut decoded)
        .ok()?;
    serde_json::from_slice(&decoded).ok()
}

fn nonempty(value: String) -> Option<String> {
    (!value.trim().is_empty()).then_some(value)
}

fn now_ms() -> Option<u64> {
    let duration = SystemTime::now().duration_since(UNIX_EPOCH).ok()?;
    u64::try_from(duration.as_millis()).ok()
}

struct JsonObject;

impl<'de> Deserialize<'de> for JsonObject {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ObjectVisitor;
        impl<'de> Visitor<'de> for ObjectVisitor {
            type Value = JsonObject;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(JsonObject)
            }
        }
        deserializer.deserialize_map(ObjectVisitor)
    }
}

#[cfg(test)]
#[path = "device_discovery_tests.rs"]
mod tests;
