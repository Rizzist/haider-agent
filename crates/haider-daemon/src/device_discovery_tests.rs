//! Device-discovery laws (D1): metadata-only reports, silent skip of absent
//! or malformed stores, and the honest disabled state.
//!
//! Discovery resolves store paths from `HOME` plus per-store env overrides,
//! so every law body runs in a re-spawned child process whose `HOME` is a
//! fixture directory and whose override variables are scrubbed. The parent
//! arm of each test only builds fixtures and supervises the child.
#![allow(clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

use super::*;

const ENV_CHILD: &str = "HAIDER_TEST_DEVICE_DISCOVERY_ENV_CHILD";

/// Every environment variable discovery consults besides `HOME`. The child
/// starts from a scrubbed set so a developer's real overrides (or real
/// stores) can never leak into a law.
const DISCOVERY_ENV_VARS: &[&str] = &[
    "HAIDER_CODEX_AUTH_PATH",
    "HAIDER_CLAUDE_CREDS_PATH",
    "HAIDER_CLAUDE_OAUTH_PATH",
    "HAIDER_KIMI_CREDS_PATH",
    "HAIDER_KIMI_DEVICE_ID_PATH",
    "HAIDER_GEMINI_CREDS_PATH",
    "HAIDER_DEVICE_DISCOVERY_DISABLED",
];

fn run_in_isolated_home(test_name: &str, home: &Path, extra_env: &[(&str, &str)]) -> bool {
    if std::env::var_os(ENV_CHILD).is_some() {
        return false;
    }
    let mut command = std::process::Command::new(
        std::env::current_exe().expect("current daemon test executable"),
    );
    command
        .args(["--exact", test_name, "--nocapture"])
        .env(ENV_CHILD, "1")
        .env("HOME", home);
    for name in DISCOVERY_ENV_VARS {
        command.env_remove(name);
    }
    for (name, value) in extra_env {
        command.env(name, value);
    }
    let output = command.output().expect("spawn isolated discovery test");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("running 1 test"),
        "isolated discovery test failed or did not run\nstdout:\n{}\nstderr:\n{}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

fn fixture_home() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("hddisc")
        .tempdir_in("/tmp")
        .unwrap_or_else(|error| panic!("tempdir: {error}"))
}

fn write_store(home: &Path, relative: &str, bytes: &[u8]) {
    let path = home.join(relative);
    std::fs::create_dir_all(path.parent().expect("store parent")).expect("mkdir store parent");
    std::fs::write(path, bytes).expect("write store fixture");
}

/// A structurally real JWT (three base64url segments) with a fake signature.
/// The whole string is token material; only decoded public claims may appear
/// in a discovery report.
fn fake_jwt(payload: &serde_json::Value) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let body = URL_SAFE_NO_PAD.encode(payload.to_string().as_bytes());
    let signature = URL_SAFE_NO_PAD.encode(b"fake-signature-not-a-real-key");
    format!("{header}.{body}.{signature}")
}

const KIMI_DEVICE_ID_FIXTURE: &str = "6f2a9c31-77d4-4b8e-9a10-3c5de88f01ab";
const CLAUDE_NATIVE_FIXTURE: &[u8] = br#"{
  "claudeAiOauth": {
    "accessToken": "fake-claude-native-access-token",
    "refreshToken": "fake-claude-native-refresh-token",
    "expiresAt": 4102444800123,
    "scopes": ["user:inference"],
    "subscriptionType": "max"
  }
}"#;

struct StubClaudeNative {
    bytes: Option<Vec<u8>>,
    reads: AtomicUsize,
}

impl StubClaudeNative {
    fn with_bytes(bytes: &[u8]) -> Self {
        Self {
            bytes: Some(bytes.to_vec()),
            reads: AtomicUsize::new(0),
        }
    }

    fn unavailable() -> Self {
        Self {
            bytes: None,
            reads: AtomicUsize::new(0),
        }
    }
}

impl crate::oauth::ClaudeNativeCredentialStore for StubClaudeNative {
    fn read(&self) -> Option<crate::oauth::ClaudeCredentialInput> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.bytes
            .as_ref()
            .map(|bytes| crate::oauth::ClaudeCredentialInput {
                location: PathBuf::from("mock native store: Claude Code-credentials"),
                bytes: zeroize::Zeroizing::new(bytes.clone()),
            })
    }
}

fn assert_native_claude_discovered() {
    let home = fixture_home();
    let missing_file = home.path().join(".claude/.credentials.json");
    let native = StubClaudeNative::with_bytes(CLAUDE_NATIVE_FIXTURE);
    let candidate = discover_claude_at(&missing_file, &native).expect("native Claude candidate");
    assert_eq!(native.reads.load(Ordering::SeqCst), 1);
    assert_eq!(candidate.wire.provider, "anthropic-oauth");
    assert_eq!(candidate.wire.source_label, "Claude Code");
    assert_eq!(candidate.wire.freshness, "fresh");
    assert_eq!(candidate.wire.expires_at_ms, Some(4_102_444_800_123));
    assert!(candidate.wire.import_supported);
    assert_eq!(candidate.import_source, Some("claude-code"));
}

/// Platform-agnostic coverage for the seam shared by the cfg-gated macOS and
/// Windows adapters. The platform-specific laws below pin adapter selection.
#[test]
fn native_secure_store_seam_discovers_claude_when_file_is_absent() {
    assert_native_claude_discovered();
}

/// MUTATION CHECK: bypass the native seam after a file miss. Expected runtime
/// failure: the macOS-only candidate disappears.
#[cfg(target_os = "macos")]
#[test]
fn macos_keychain_seam_discovers_claude_when_file_is_absent() {
    assert_native_claude_discovered();
}

/// MUTATION CHECK: bypass the native seam after a file miss. Expected runtime
/// failure: the Windows-only candidate disappears.
#[cfg(target_os = "windows")]
#[test]
fn windows_credential_manager_seam_discovers_claude_when_file_is_absent() {
    assert_native_claude_discovered();
}

/// MUTATION CHECK: query the native store before the file. Expected runtime
/// failure: the read counter changes and the native expiry replaces the file.
#[test]
fn claude_file_short_circuits_the_native_store() {
    let home = fixture_home();
    let file = home.path().join(".claude/.credentials.json");
    write_store(
        home.path(),
        ".claude/.credentials.json",
        br#"{
  "claudeAiOauth": {
    "accessToken": "fake-claude-file-access-token",
    "refreshToken": "fake-claude-file-refresh-token",
    "expiresAt": 4102444800999,
    "scopes": ["user:inference"]
  }
}"#,
    );
    let native = StubClaudeNative::with_bytes(CLAUDE_NATIVE_FIXTURE);
    let candidate = discover_claude_at(&file, &native).expect("file Claude candidate");
    assert_eq!(native.reads.load(Ordering::SeqCst), 0);
    assert_eq!(candidate.wire.expires_at_ms, Some(4_102_444_800_999));
    assert_eq!(candidate.wire.path, file.to_string_lossy());
}

/// MUTATION CHECK: surface native absence/denial as a discovery error. Expected
/// runtime failure: either call panics or returns a synthetic candidate.
#[test]
fn claude_native_absent_or_denied_degrades_to_clean_not_found() {
    let home = fixture_home();
    let file = home.path().join(".claude/.credentials.json");
    let absent = StubClaudeNative::unavailable();
    let denied = StubClaudeNative::unavailable();
    let absent_error = match crate::oauth::load_claude_credential_input(&file, &absent) {
        Err(error) => error,
        Ok(_) => panic!("absent native store unexpectedly returned a credential"),
    };
    let denied_error = match crate::oauth::load_claude_credential_input(&file, &denied) {
        Err(error) => error,
        Ok(_) => panic!("denied native store unexpectedly returned a credential"),
    };
    assert_eq!(
        absent_error.code,
        haider_protocol::error::ErrorCode::CredentialMissing
    );
    assert_eq!(
        denied_error.code,
        haider_protocol::error::ErrorCode::CredentialMissing
    );
    assert!(discover_claude_at(&file, &absent).is_none());
    assert!(discover_claude_at(&file, &denied).is_none());
    assert_eq!(absent.reads.load(Ordering::SeqCst), 2);
    assert_eq!(denied.reads.load(Ordering::SeqCst), 2);
}

fn codex_access_jwt() -> String {
    fake_jwt(&serde_json::json!({
        "exp": 4_102_444_800u64,
        "https://api.openai.com/auth": { "chatgpt_account_id": "acct-fake-codex-1" }
    }))
}

fn codex_id_jwt() -> String {
    fake_jwt(&serde_json::json!({ "email": "person@example.invalid" }))
}

fn populated_home(home: &Path) -> (String, String) {
    let access_jwt = codex_access_jwt();
    let id_jwt = codex_id_jwt();
    write_store(
        home,
        ".codex/auth.json",
        serde_json::json!({
            "tokens": {
                "access_token": access_jwt,
                "refresh_token": "fake-codex-refresh-token-d1",
                "id_token": id_jwt,
                "account_id": "acct-fake-codex-1"
            }
        })
        .to_string()
        .as_bytes(),
    );
    write_store(
        home,
        ".claude/.credentials.json",
        br#"{
  "claudeAiOauth": {
    "accessToken": "fake-claude-access-token-d1",
    "refreshToken": "fake-claude-refresh-token-d1",
    "expiresAt": 4102444800123,
    "scopes": ["user:inference"],
    "subscriptionType": "max"
  }
}"#,
    );
    write_store(
        home,
        ".kimi/credentials/kimi-code.json",
        br#"{
  "access_token": "fake-kimi-access-token-d1",
  "refresh_token": "fake-kimi-refresh-token-d1",
  "expires_at": 4102444800.0,
  "expires_in": 3600,
  "scope": "all",
  "token_type": "Bearer"
}"#,
    );
    write_store(home, ".kimi/device_id", KIMI_DEVICE_ID_FIXTURE.as_bytes());
    write_store(
        home,
        ".gemini/oauth_creds.json",
        br#"{
  "access_token": "fake-gemini-access-token-d1",
  "refresh_token": "fake-gemini-refresh-token-d1",
  "expiry_date": 4102444800123
}"#,
    );
    (access_jwt, id_jwt)
}

/// LAW: discovery_reports_metadata_never_token_bytes. Real-shaped fixture
/// stores (a codex auth.json with structurally real JWTs, a Claude Code
/// credential file, a kimi-code bundle plus device identity, gemini creds)
/// produce a report whose serialized wire bytes contain none of the token
/// material — while the public metadata (provider, account label, freshness,
/// path) does ride the response.
#[test]
fn discovery_reports_metadata_never_token_bytes() {
    let home = fixture_home();
    let (access_jwt, id_jwt) = populated_home(home.path());
    if run_in_isolated_home(
        "device_discovery::tests::discovery_reports_metadata_never_token_bytes",
        home.path(),
        &[],
    ) {
        return;
    }

    let access_jwt_check = access_jwt;
    let id_jwt_check = id_jwt;
    let candidates = discover_device_candidates(false);
    assert_eq!(
        candidates.len(),
        4,
        "expected codex + claude + kimi + gemini candidates, got {candidates:?}"
    );

    let response = serde_json::to_string(&haider_rpc::ResponseBody::AccountDeviceCandidates {
        discovery_disabled: false,
        candidates: candidates
            .iter()
            .map(|candidate| candidate.wire.clone())
            .collect(),
    })
    .expect("serialize discovery response");

    // No token material, in whole or in JWT segments, and no device identity.
    for secret in [
        access_jwt_check.as_str(),
        id_jwt_check.as_str(),
        "fake-codex-refresh-token-d1",
        "fake-claude-access-token-d1",
        "fake-claude-refresh-token-d1",
        "fake-kimi-access-token-d1",
        "fake-kimi-refresh-token-d1",
        "fake-gemini-access-token-d1",
        "fake-gemini-refresh-token-d1",
        KIMI_DEVICE_ID_FIXTURE,
    ] {
        assert!(
            !response.contains(secret),
            "discovery response leaked token material {secret}: {response}"
        );
    }
    for segment in access_jwt_check.split('.').chain(id_jwt_check.split('.')) {
        assert!(
            !response.contains(segment),
            "discovery response leaked a JWT segment: {response}"
        );
    }

    let by_provider = |provider: &str| {
        candidates
            .iter()
            .find(|candidate| candidate.wire.provider == provider)
            .unwrap_or_else(|| panic!("missing {provider} candidate"))
    };

    let codex = by_provider("openai-oauth");
    assert_eq!(codex.wire.source_label, "Codex");
    assert_eq!(
        codex.wire.account_label.as_deref(),
        Some("person@example.invalid"),
        "id-token email is the public account label"
    );
    assert_eq!(codex.wire.freshness, "fresh");
    assert_eq!(codex.wire.expires_at_ms, Some(4_102_444_800_000));
    assert!(codex.wire.path.ends_with(".codex/auth.json"));
    assert!(codex.wire.import_supported);
    assert_eq!(codex.import_source, Some("codex"));

    let claude = by_provider("anthropic-oauth");
    assert_eq!(claude.wire.source_label, "Claude Code");
    assert_eq!(claude.wire.expires_at_ms, Some(4_102_444_800_123));
    assert!(claude.wire.import_supported);
    assert_eq!(claude.import_source, Some("claude-code"));

    let kimi = by_provider("kimi-oauth");
    assert_eq!(kimi.wire.expires_at_ms, Some(4_102_444_800_000));
    assert!(
        kimi.wire.import_supported,
        "valid device id supports import"
    );
    assert_eq!(kimi.import_source, Some("kimi-code"));

    let gemini = by_provider("gemini");
    assert!(!gemini.wire.import_supported);
    assert!(gemini.import_source.is_none());
    let reason = gemini
        .wire
        .unsupported_reason
        .as_deref()
        .expect("gemini honest reason");
    assert!(
        reason.contains("cannot be imported"),
        "honest reason: {reason}"
    );

    // Opaque candidate ids: fixed shape, unique, and path-blind.
    for candidate in &candidates {
        assert_eq!(candidate.wire.candidate.len(), 68);
        assert!(candidate.wire.candidate.starts_with("dc1_"));
    }
}

/// LAW: absent_or_malformed_stores_are_skipped_silently. An absent store, a
/// truncated JSON store, a wrong-shape store, a non-JSON store, and an
/// oversized store are all indistinguishable from an empty device: discovery
/// returns nothing and surfaces no error.
#[test]
fn absent_or_malformed_stores_are_skipped_silently() {
    let home = fixture_home();
    // codex: absent entirely (no ~/.codex at all).
    // claude credentials: truncated JSON.
    write_store(
        home.path(),
        ".claude/.credentials.json",
        b"{ \"claudeAiOauth\": ",
    );
    // claude unverified oauth path: present but not JSON.
    write_store(home.path(), ".claude/oauth", b"not-json at all");
    // kimi: valid JSON, wrong shape (refresh_token missing) — with a valid
    // device identity so only the credential shape causes the skip.
    write_store(
        home.path(),
        ".kimi/credentials/kimi-code.json",
        br#"{
  "access_token": "fake-kimi-access-malformed",
  "expires_at": 4102444800.0,
  "scope": "all",
  "token_type": "Bearer"
}"#,
    );
    write_store(
        home.path(),
        ".kimi/device_id",
        KIMI_DEVICE_ID_FIXTURE.as_bytes(),
    );
    // gemini: syntactically valid but beyond the bounded-read limit.
    let mut oversized = Vec::with_capacity(300 * 1024);
    oversized.extend_from_slice(
        br#"{"access_token":"fake-gemini-access-oversized","refresh_token":"fake-gemini-refresh-oversized","pad":""#,
    );
    oversized.resize(300 * 1024, b'a');
    oversized.extend_from_slice(b"\"}");
    write_store(home.path(), ".gemini/oauth_creds.json", &oversized);
    if run_in_isolated_home(
        "device_discovery::tests::absent_or_malformed_stores_are_skipped_silently",
        home.path(),
        &[],
    ) {
        return;
    }

    let candidates = discover_device_candidates(false);
    assert!(
        candidates.is_empty(),
        "absent/malformed stores must be skipped silently, got {candidates:?}"
    );
    // Import lookups over the same device stay honest: nothing is findable.
    let probe = format!("dc1_{}", "0".repeat(64));
    assert!(candidate_by_id(false, &probe).is_none());
}

/// The profile `discovery_disabled` switch is an honest configured-off state:
/// the same device that discovers a store with the switch off reports nothing
/// with it on, and candidate lookup (the import path) is fenced too.
#[test]
fn discovery_profile_switch_disables_and_stays_honest() {
    let home = fixture_home();
    populated_home(home.path());
    if run_in_isolated_home(
        "device_discovery::tests::discovery_profile_switch_disables_and_stays_honest",
        home.path(),
        &[],
    ) {
        return;
    }

    let enabled = discover_device_candidates(false);
    assert!(!enabled.is_empty(), "fixture home must discover stores");
    let candidate_id = enabled[0].wire.candidate.clone();
    assert!(candidate_by_id(false, &candidate_id).is_some());

    assert!(discover_device_candidates(true).is_empty());
    assert!(candidate_by_id(true, &candidate_id).is_none());
    assert!(discovery_is_disabled(true));
    assert!(!discovery_is_disabled(false));
}

/// `HAIDER_DEVICE_DISCOVERY_DISABLED` disables discovery for the whole daemon
/// process even when the profile allows it, and the reported state is honest.
#[test]
fn discovery_env_switch_disables_even_when_profile_allows() {
    let home = fixture_home();
    populated_home(home.path());
    if run_in_isolated_home(
        "device_discovery::tests::discovery_env_switch_disables_even_when_profile_allows",
        home.path(),
        &[("HAIDER_DEVICE_DISCOVERY_DISABLED", "true")],
    ) {
        return;
    }

    assert!(discover_device_candidates(false).is_empty());
    assert!(discovery_is_disabled(false));
    let probe = format!("dc1_{}", "0".repeat(64));
    assert!(candidate_by_id(false, &probe).is_none());
}
