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
use haider_accounts::{
    CredentialSourceHealth, CredentialSourceKind, CredentialSourceRecord, CredentialStoreMode,
    OAuthIdentityV1, OAuthTokenBundleV1,
};
use haider_protocol::credential::AccountIdentity;
use haider_rpc::DeviceCredentialCandidateWire;
use serde::de::{IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use zeroize::Zeroizing;

#[cfg(test)]
use crate::oauth::PlatformClaudeNativeCredentialStore;
use crate::oauth::{
    ClaudeNativeCredentialStore, ClaudeNativeReadEvent, SecretJson, claude_subscription_identity,
    oauth_home_dir, oauth_import_path, parse_claude_credential_metadata,
};

const DISCOVERY_FILE_LIMIT: u64 = 256 * 1024;
const EXPIRING_WINDOW_MS: u64 = 5 * 60 * 1000;
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
    /// Exact external-store snapshot bound into `wire.candidate`. This stays
    /// daemon-local and is rechecked against the material read for import.
    pub content_fingerprint: Option<[u8; 32]>,
}

#[cfg(test)]
pub(crate) fn discover_device_candidates(disabled: bool) -> Vec<DeviceCandidate> {
    discover_device_candidates_with_native(
        disabled,
        &PlatformClaudeNativeCredentialStore::default(),
    )
}

pub(crate) fn discover_device_candidates_with_native(
    disabled: bool,
    native: &dyn ClaudeNativeCredentialStore,
) -> Vec<DeviceCandidate> {
    discover_device_candidates_with_native_event(disabled, native, ClaudeNativeReadEvent::Ordinary)
}

pub(crate) fn discover_device_candidates_with_native_event(
    disabled: bool,
    _native: &dyn ClaudeNativeCredentialStore,
    _event: ClaudeNativeReadEvent,
) -> Vec<DeviceCandidate> {
    if disabled || discovery_disabled_by_env() {
        return Vec::new();
    }
    let mut candidates = Vec::new();
    if let Some(candidate) = discover_codex() {
        candidates.push(candidate);
    }
    // Strict prompt-free mode does not call even a no-UI native-store probe.
    // A readable file is inspected directly and remains policy-blocked.
    if let Some(candidate) = discover_claude_file() {
        candidates.push(candidate);
    }
    if let Some(candidate) = discover_claude_unverified_path() {
        candidates.push(candidate);
    }
    if let Some(candidate) = discover_kimi() {
        candidates.push(candidate);
    }
    if let Some(candidate) = discover_grok() {
        candidates.push(candidate);
    }
    if let Some(candidate) = discover_gemini() {
        candidates.push(candidate);
    }
    if let Some(candidate) = discover_gcloud() {
        candidates.push(candidate);
    }
    candidates
}

pub(crate) fn discovery_is_disabled(profile_disabled: bool) -> bool {
    profile_disabled || discovery_disabled_by_env()
}

#[cfg(test)]
pub(crate) fn candidate_by_id(disabled: bool, id: &str) -> Option<DeviceCandidate> {
    candidate_by_id_with_native(
        disabled,
        id,
        &PlatformClaudeNativeCredentialStore::default(),
    )
}

pub(crate) fn candidate_by_id_with_native(
    disabled: bool,
    id: &str,
    native: &dyn ClaudeNativeCredentialStore,
) -> Option<DeviceCandidate> {
    discover_device_candidates_with_native(disabled, native)
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
    #[serde(default)]
    account_id: Option<String>,
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

/// Minimal read-through shape for an externally owned Codex login. Unknown
/// fields (notably `refresh_token`) are skipped by serde and never
/// materialized into Haider-owned memory.
#[derive(Deserialize)]
struct LinkedCodexFile {
    #[serde(default)]
    tokens: Option<LinkedCodexTokens>,
    #[serde(default)]
    account_id: Option<String>,
    #[serde(default)]
    last_refresh: Option<String>,
}

#[derive(Deserialize)]
struct LinkedCodexTokens {
    #[serde(default)]
    access_token: Option<SecretJson>,
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
    #[serde(default)]
    chatgpt_plan_type: Option<String>,
}

/// One source-owned credential generation. `encoded_bundle` is an ephemeral
/// access-only broker input: external refresh credentials are never copied.
pub(crate) struct LinkedSourceMaterial {
    pub identity: Option<AccountIdentity>,
    pub display_identity: String,
    pub last_refreshed_at_ms: Option<u64>,
    pub access_expires_at_ms: Option<u64>,
    pub health: CredentialSourceHealth,
    pub store_mode: CredentialStoreMode,
    pub encoded_bundle: Option<Zeroizing<Vec<u8>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinkedSourceReadFailure {
    SourceGone,
    Unreadable,
    SymlinkEscape,
    Oversized,
    PartialWrite,
    MissingFields,
    InvalidJson,
    Invalid,
    RequiresOriginClient,
}

/// Reads exactly one enrolled root. No native credential API is reachable
/// from this function; Claude file sources are metadata-only/policy-blocked.
pub(crate) fn read_linked_source(
    source: &CredentialSourceRecord,
) -> Result<LinkedSourceMaterial, LinkedSourceReadFailure> {
    let store_mode = linked_source_store_mode(source);
    if source.kind == CredentialSourceKind::CodexHome
        && matches!(
            store_mode,
            CredentialStoreMode::Keyring
                | CredentialStoreMode::Auto
                | CredentialStoreMode::Ephemeral
        )
    {
        return Err(LinkedSourceReadFailure::RequiresOriginClient);
    }
    let (path, bytes, metadata) = read_enrolled_file(source)?;
    let last_refreshed_at_ms = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .and_then(|duration| u64::try_from(duration.as_millis()).ok());
    match source.kind {
        CredentialSourceKind::CodexHome => {
            linked_codex_material(&path, bytes, store_mode, last_refreshed_at_ms)
        }
        CredentialSourceKind::ClaudeFile => {
            linked_claude_metadata(&path, &bytes, last_refreshed_at_ms, &source.id)
        }
    }
}

pub(crate) fn linked_source_store_mode(source: &CredentialSourceRecord) -> CredentialStoreMode {
    match source.kind {
        CredentialSourceKind::CodexHome => codex_store_mode(&source.root),
        CredentialSourceKind::ClaudeFile => CredentialStoreMode::File,
    }
}

fn read_enrolled_file(
    source: &CredentialSourceRecord,
) -> Result<(PathBuf, Zeroizing<Vec<u8>>, std::fs::Metadata), LinkedSourceReadFailure> {
    if !source.root.exists() {
        return Err(LinkedSourceReadFailure::SourceGone);
    }
    let path = source.credential_path();
    let canonical = std::fs::canonicalize(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            LinkedSourceReadFailure::SourceGone
        } else {
            LinkedSourceReadFailure::Unreadable
        }
    })?;
    if !canonical.starts_with(&source.root) {
        return Err(LinkedSourceReadFailure::SymlinkEscape);
    }
    let file = std::fs::File::open(&canonical).map_err(|_| LinkedSourceReadFailure::Unreadable)?;
    let metadata = file
        .metadata()
        .map_err(|_| LinkedSourceReadFailure::Unreadable)?;
    if !metadata.is_file() {
        return Err(LinkedSourceReadFailure::Invalid);
    }
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(DISCOVERY_FILE_LIMIT.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| LinkedSourceReadFailure::Unreadable)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > DISCOVERY_FILE_LIMIT {
        return Err(LinkedSourceReadFailure::Oversized);
    }
    Ok((canonical, bytes, metadata))
}

fn linked_codex_material(
    _path: &Path,
    bytes: Zeroizing<Vec<u8>>,
    store_mode: CredentialStoreMode,
    last_refreshed_at_ms: Option<u64>,
) -> Result<LinkedSourceMaterial, LinkedSourceReadFailure> {
    let parsed: LinkedCodexFile = serde_json::from_slice(&bytes).map_err(|error| {
        if error.is_eof() {
            LinkedSourceReadFailure::PartialWrite
        } else {
            LinkedSourceReadFailure::InvalidJson
        }
    })?;
    let last_refreshed_at_ms = parsed
        .last_refresh
        .as_deref()
        .and_then(parse_rfc3339_millis)
        .or(last_refreshed_at_ms);
    let tokens = parsed
        .tokens
        .ok_or(LinkedSourceReadFailure::MissingFields)?;
    let access_token = tokens
        .access_token
        .ok_or(LinkedSourceReadFailure::MissingFields)?;
    if access_token.0.is_empty() {
        return Err(LinkedSourceReadFailure::MissingFields);
    }
    let access = decode_jwt::<CodexClaims>(&access_token.0).unwrap_or_default();
    let id = tokens
        .id_token
        .as_ref()
        .and_then(|token| decode_jwt::<CodexClaims>(&token.0))
        .unwrap_or_default();
    let email = id.email.and_then(nonempty);
    let account_id = id
        .chatgpt_account_id
        .and_then(nonempty)
        .or_else(|| {
            id.openai_auth
                .as_ref()
                .and_then(|claims| claims.chatgpt_account_id.clone())
                .and_then(nonempty)
        })
        .or_else(|| tokens.account_id.clone().and_then(nonempty))
        .or_else(|| parsed.account_id.and_then(nonempty));
    if account_id.is_none() {
        return Err(LinkedSourceReadFailure::MissingFields);
    }
    let plan = id
        .openai_auth
        .and_then(|claims| claims.chatgpt_plan_type)
        .and_then(nonempty);
    let display_identity = email
        .clone()
        .or_else(|| account_id.clone())
        .unwrap_or_else(|| "Codex login".to_owned());
    let captured_at = last_refreshed_at_ms.unwrap_or(0);
    let identity = AccountIdentity {
        email,
        display_name: None,
        account_id: account_id.clone(),
        plan,
        issuer: Some("https://auth.openai.com".to_owned()),
        captured_at,
        verified: false,
    };
    let expires_at_ms = access.exp.and_then(|seconds| seconds.checked_mul(1000));
    let expires_at_ms = expires_at_ms.ok_or(LinkedSourceReadFailure::MissingFields)?;
    let observed_at_ms = now_ms().ok_or(LinkedSourceReadFailure::Invalid)?;
    let generation = last_refreshed_at_ms.unwrap_or(1).max(1);
    let subject = account_id.as_deref().unwrap_or(display_identity.as_str());
    let bundle = OAuthTokenBundleV1::new(
        haider_provider::OPENAI_OAUTH_PROVIDER_NAME.to_owned(),
        "https://auth.openai.com".to_owned(),
        "app_EMoamEEZ73f0CkXaXp7hrann".to_owned(),
        None,
        "Bearer".to_owned(),
        access_token.0,
        None,
        expires_at_ms,
        None,
        Vec::new(),
        OAuthIdentityV1 {
            subject_hash: blake3::hash(subject.as_bytes()).to_hex().to_string(),
            display_identity: display_identity.clone(),
        },
        generation,
    )
    .map_err(|_| LinkedSourceReadFailure::Invalid)?
    .with_account_identity(identity.clone());
    let encoded_bundle = bundle
        .encode()
        .map_err(|_| LinkedSourceReadFailure::Invalid)?;
    let health = if expires_at_ms <= observed_at_ms {
        CredentialSourceHealth::Expired
    } else {
        CredentialSourceHealth::Ready
    };
    Ok(LinkedSourceMaterial {
        identity: Some(identity),
        display_identity,
        last_refreshed_at_ms,
        access_expires_at_ms: Some(expires_at_ms),
        health,
        store_mode,
        encoded_bundle: Some(encoded_bundle),
    })
}

fn linked_claude_metadata(
    path: &Path,
    bytes: &[u8],
    last_refreshed_at_ms: Option<u64>,
    source_id: &str,
) -> Result<LinkedSourceMaterial, LinkedSourceReadFailure> {
    serde_json::from_slice::<serde::de::IgnoredAny>(bytes).map_err(|error| {
        if error.is_eof() {
            LinkedSourceReadFailure::PartialWrite
        } else {
            LinkedSourceReadFailure::InvalidJson
        }
    })?;
    let parsed = parse_claude_credential_metadata(path, bytes)
        .map_err(|_| LinkedSourceReadFailure::MissingFields)?;
    if !parsed.has_inference_scope {
        return Err(LinkedSourceReadFailure::MissingFields);
    }
    let (display_name, plan) = claude_subscription_identity(parsed.subscription_type.as_deref());
    let captured_at = last_refreshed_at_ms.unwrap_or(0);
    let synthetic = format!(
        "anthropic:{}",
        &source_id[source_id.len().saturating_sub(16)..]
    );
    Ok(LinkedSourceMaterial {
        identity: Some(AccountIdentity {
            email: None,
            display_name: Some(display_name.to_owned()),
            account_id: Some(synthetic),
            plan: plan.map(str::to_owned),
            issuer: Some("https://claude.ai".to_owned()),
            captured_at,
            verified: false,
        }),
        display_identity: display_name.to_owned(),
        last_refreshed_at_ms,
        access_expires_at_ms: Some(parsed.expires_at_ms),
        health: CredentialSourceHealth::RequiresOriginClient,
        store_mode: CredentialStoreMode::File,
        encoded_bundle: None,
    })
}

fn codex_store_mode(root: &Path) -> CredentialStoreMode {
    let path = root.join("config.toml");
    let Ok(bytes) = std::fs::read(path) else {
        return CredentialStoreMode::File;
    };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return CredentialStoreMode::Unknown;
    };
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or_default().trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "cli_auth_credentials_store" {
            continue;
        }
        return match value
            .trim()
            .trim_matches(['\'', '"'])
            .to_ascii_lowercase()
            .as_str()
        {
            "file" => CredentialStoreMode::File,
            "keyring" => CredentialStoreMode::Keyring,
            "auto" => CredentialStoreMode::Auto,
            "ephemeral" => CredentialStoreMode::Ephemeral,
            _ => CredentialStoreMode::Unknown,
        };
    }
    CredentialStoreMode::File
}

fn parse_rfc3339_millis(value: &str) -> Option<u64> {
    let timestamp =
        time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339).ok()?;
    u64::try_from(timestamp.unix_timestamp_nanos().checked_div(1_000_000)?).ok()
}

fn discover_codex() -> Option<DeviceCandidate> {
    let path = oauth_import_path("codex").ok()?;
    let bytes = read_bounded(&path)?;
    let parsed: CodexFile = serde_json::from_slice(&bytes).ok()?;
    if parsed.tokens.access_token.0.is_empty() || parsed.tokens.refresh_token.0.is_empty() {
        return None;
    }
    let access = decode_jwt::<CodexClaims>(&parsed.tokens.access_token.0).unwrap_or_default();
    let legacy_identity = parsed
        .tokens
        .id_token
        .as_ref()
        .and_then(|token| decode_jwt::<CodexClaims>(&token.0))
        .unwrap_or_default();
    let email = legacy_identity.email.and_then(nonempty);
    let account_id = legacy_identity
        .chatgpt_account_id
        .and_then(nonempty)
        .or_else(|| {
            legacy_identity
                .openai_auth
                .and_then(|claims| claims.chatgpt_account_id.and_then(nonempty))
        })
        .or_else(|| parsed.tokens.account_id.clone().and_then(nonempty))
        .or_else(|| parsed.account_id.clone().and_then(nonempty));
    let account_label = email.clone().or_else(|| account_id.clone());
    let captured_at = now_ms()?;
    let rich_identity = haider_provider::oauth_identity_source("openai-oauth")
        .and_then(|source| {
            source
                .identity_from_tokens(&haider_provider::OAuthTokens {
                    access_token: parsed.tokens.access_token.0.as_slice(),
                    refresh_token: Some(parsed.tokens.refresh_token.0.as_slice()),
                    id_token: parsed
                        .tokens
                        .id_token
                        .as_ref()
                        .map(|token| token.0.as_slice()),
                    captured_at,
                })
                .ok()
                .flatten()
        })
        .or_else(|| {
            (email.is_some() || account_id.is_some()).then(|| AccountIdentity {
                email,
                display_name: None,
                account_id,
                plan: None,
                issuer: Some("https://auth.openai.com".to_owned()),
                captured_at,
                verified: false,
            })
        });
    let expires_at_ms = access.exp.and_then(|seconds| seconds.checked_mul(1000));
    Some(candidate(
        "codex",
        "openai-oauth",
        "Codex",
        account_label,
        rich_identity,
        expires_at_ms,
        path,
        Some(*blake3::hash(&bytes).as_bytes()),
        true,
        None,
    ))
}

fn discover_claude_file() -> Option<DeviceCandidate> {
    let path = oauth_import_path("claude-code").ok()?;
    discover_claude_file_at(&path)
}

#[cfg(test)]
pub(crate) fn discover_claude_at(
    path: &Path,
    _native: &dyn ClaudeNativeCredentialStore,
) -> Option<DeviceCandidate> {
    discover_claude_file_at(path)
}

fn discover_claude_file_at(path: &Path) -> Option<DeviceCandidate> {
    let bytes = read_bounded(path)?;
    let parsed = parse_claude_credential_metadata(path, &bytes).ok()?;
    if !parsed.has_inference_scope {
        return None;
    }
    let content_fingerprint = *blake3::hash(&bytes).as_bytes();
    let (display_name, plan) = claude_subscription_identity(parsed.subscription_type.as_deref());
    Some(candidate(
        "claude-code",
        "anthropic-oauth",
        "Claude Code credential file (read-only)",
        None,
        Some(AccountIdentity {
            email: None,
            display_name: Some(display_name.to_owned()),
            account_id: None,
            plan: plan.map(str::to_owned),
            issuer: Some("https://claude.ai".to_owned()),
            captured_at: now_ms()?,
            verified: false,
        }),
        Some(parsed.expires_at_ms),
        path.to_path_buf(),
        Some(content_fingerprint),
        false,
        Some(if parsed.custom_client {
            CLAUDE_CUSTOM_CLIENT_REASON.to_owned()
        } else {
            "Claude Code subscription credentials remain owned by the official client and cannot be imported by Haider".to_owned()
        }),
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
        None,
        path,
        None,
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
        None,
        Some(expires_at_ms),
        path,
        None,
        device_id_ok,
        (!device_id_ok).then(|| KIMI_DEVICE_REASON.to_owned()),
    ))
}

#[derive(Deserialize)]
#[serde(untagged)]
enum GrokFile {
    Bare(SecretJson),
    Bundle {
        access_token: SecretJson,
        #[serde(default)]
        refresh_token: Option<SecretJson>,
        #[serde(default)]
        expires_in: Option<u64>,
        #[serde(default)]
        issuer: Option<String>,
    },
}

/// Discovers both auth.json layouts written by official Grok CLI releases.
/// Only freshness and the public source label leave this parser.
fn discover_grok() -> Option<DeviceCandidate> {
    let path = env_or_home("HAIDER_GROK_AUTH_PATH", ".grok/auth.json")?;
    let bytes = read_bounded(&path)?;
    let parsed: GrokFile = serde_json::from_slice(&bytes).ok()?;
    let expires_at_ms = match parsed {
        GrokFile::Bare(token) => {
            if token.0.is_empty() {
                return None;
            }
            None
        }
        GrokFile::Bundle {
            access_token,
            refresh_token,
            expires_in,
            issuer,
        } => {
            if access_token.0.is_empty()
                || refresh_token
                    .as_ref()
                    .is_some_and(|token| token.0.is_empty())
                || issuer
                    .as_deref()
                    .is_some_and(|value| value != "https://auth.x.ai")
            {
                return None;
            }
            expires_in.and_then(|seconds| now_ms()?.checked_add(seconds.checked_mul(1000)?))
        }
    };
    Some(candidate(
        "grok-cli",
        haider_provider::GROK_OAUTH_PROVIDER_NAME,
        "Grok CLI",
        None,
        None,
        expires_at_ms,
        path,
        None,
        true,
        None,
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
        None,
        parsed.expiry_date.filter(|expiry| *expiry != 0),
        path,
        None,
        false,
        Some(GEMINI_UNSUPPORTED_REASON.to_owned()),
    ))
}

/// The gcloud import-source key consumed by the account actor's dedicated
/// import arm (G4b, LV2).
pub(crate) const GCLOUD_IMPORT_SOURCE: &str = "gcloud";

/// Google Cloud ADC via the local gcloud installation (G4b): discovery only
/// checks that `application_default_credentials.json` EXISTS — the file is
/// never read (its refresh token belongs to gcloud, not Haider). Import
/// runs `gcloud auth print-access-token` and vaults the RESULT; the broker
/// re-runs the same command when the ~1h token expires.
fn discover_gcloud() -> Option<DeviceCandidate> {
    // Probe order: the haider test/ops override, then gcloud's own
    // CLOUDSDK_CONFIG, then the default `~/.config/gcloud`.
    let config_dir = match std::env::var_os("HAIDER_GCLOUD_CONFIG_DIR")
        .or_else(|| std::env::var_os("CLOUDSDK_CONFIG"))
        .filter(|value| !value.is_empty())
    {
        Some(path) => PathBuf::from(path),
        None => env_or_home("HAIDER_GCLOUD_CONFIG_DIR", ".config/gcloud")?,
    };
    let path = config_dir.join("application_default_credentials.json");
    if !path.is_file() {
        return None;
    }
    Some(candidate(
        GCLOUD_IMPORT_SOURCE,
        haider_provider::VERTEX_PROVIDER_NAME,
        "Google Cloud (gcloud ADC)",
        None,
        None,
        None,
        path,
        None,
        true,
        None,
    ))
}

#[allow(clippy::too_many_arguments)]
fn candidate(
    source: &'static str,
    provider: &str,
    source_label: &str,
    account_label: Option<String>,
    identity: Option<haider_protocol::credential::AccountIdentity>,
    expires_at_ms: Option<u64>,
    path: PathBuf,
    content_fingerprint: Option<[u8; 32]>,
    import_supported: bool,
    unsupported_reason: Option<String>,
) -> DeviceCandidate {
    let path_display = path.to_string_lossy().into_owned();
    let candidate = opaque_candidate_id(source, &path_display, content_fingerprint);
    DeviceCandidate {
        wire: DeviceCredentialCandidateWire {
            candidate,
            source: source.to_owned(),
            provider: provider.to_owned(),
            source_label: source_label.to_owned(),
            account_label,
            identity,
            freshness: freshness(expires_at_ms),
            expires_at_ms,
            path: path_display,
            import_supported,
            unsupported_reason,
        },
        import_source: import_supported.then_some(source),
        content_fingerprint,
    }
}

fn opaque_candidate_id(source: &str, path: &str, content_fingerprint: Option<[u8; 32]>) -> String {
    let mut input = Vec::with_capacity(source.len() + path.len() + 32);
    input.extend_from_slice(b"haider-device-candidate-v1\0");
    input.extend_from_slice(source.as_bytes());
    input.push(0);
    input.extend_from_slice(path.as_bytes());
    if let Some(fingerprint) = content_fingerprint {
        input.push(0);
        input.extend_from_slice(&fingerprint);
    }
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
    oauth_home_dir()
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
    AccountIdentity::sanitized_field(&value)
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
