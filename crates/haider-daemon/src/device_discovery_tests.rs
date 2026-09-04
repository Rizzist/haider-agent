#![allow(clippy::expect_used)]

//! Device-discovery laws (D1): metadata-only reports, silent skip of absent
//! or malformed stores, and the honest disabled state.
//!
//! Discovery resolves store paths from the platform home variable plus
//! per-store env overrides, so every law body runs in a re-spawned child
//! process whose home is a fixture directory and whose override variables are
//! scrubbed. The parent arm of each test only builds fixtures and supervises
//! the child.
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

use super::*;

const ENV_CHILD: &str = "HAIDER_TEST_DEVICE_DISCOVERY_ENV_CHILD";

/// Every environment variable discovery consults besides the platform home
/// variables. The child
/// starts from a scrubbed set so a developer's real overrides (or real
/// stores) can never leak into a law.
const DISCOVERY_ENV_VARS: &[&str] = &[
    "HAIDER_CODEX_AUTH_PATH",
    "HAIDER_CLAUDE_CREDS_PATH",
    "HAIDER_CLAUDE_OAUTH_PATH",
    "HAIDER_KIMI_CREDS_PATH",
    "HAIDER_KIMI_DEVICE_ID_PATH",
    "HAIDER_GROK_AUTH_PATH",
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
    #[cfg(windows)]
    command.env("USERPROFILE", home);
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
    #[cfg(unix)]
    {
        tempfile::Builder::new()
            .prefix("hddisc")
            .tempdir_in("/tmp")
            .unwrap_or_else(|error| panic!("tempdir: {error}"))
    }
    #[cfg(windows)]
    {
        tempfile::Builder::new()
            .prefix("hddisc")
            .tempdir()
            .unwrap_or_else(|error| panic!("tempdir: {error}"))
    }
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
    fn read(
        &self,
        _event: crate::oauth::ClaudeNativeReadEvent,
    ) -> Result<crate::oauth::ClaudeCredentialInput, crate::oauth::ClaudeNativeCredentialFailure>
    {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.bytes
            .as_ref()
            .map(|bytes| crate::oauth::ClaudeCredentialInput {
                location: PathBuf::from("mock native store: Claude Code-credentials"),
                bytes: zeroize::Zeroizing::new(bytes.clone()),
                native_owner: true,
            })
            .ok_or(crate::oauth::ClaudeNativeCredentialFailure::Missing)
    }
}

#[test]
fn strict_discovery_never_touches_native_store_when_file_is_absent() {
    let home = fixture_home();
    let missing_file = home.path().join(".claude/.credentials.json");
    let native = StubClaudeNative::with_bytes(CLAUDE_NATIVE_FIXTURE);
    assert!(discover_claude_at(&missing_file, &native).is_none());
    assert_eq!(native.reads.load(Ordering::SeqCst), 0);
}

#[test]
fn strict_discovery_links_readable_claude_file_as_policy_blocked_without_native_touch() {
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
    let candidate = discover_claude_at(&file, &native).expect("Claude file candidate");
    assert_eq!(native.reads.load(Ordering::SeqCst), 0);
    assert_eq!(candidate.wire.provider, "anthropic-oauth");
    assert_eq!(candidate.wire.expires_at_ms, Some(4_102_444_800_999));
    assert_eq!(
        candidate.wire.source_label,
        "Claude Code credential file (read-only)"
    );
    assert!(!candidate.wire.import_supported);
    assert!(candidate.wire.unsupported_reason.is_some());
    assert_eq!(candidate.import_source, None);
}

#[test]
fn native_denial_does_not_suppress_an_explicitly_readable_claude_file() {
    let home = fixture_home();
    let file = home.path().join(".claude/.credentials.json");
    write_store(
        home.path(),
        ".claude/.credentials.json",
        CLAUDE_NATIVE_FIXTURE,
    );
    let denied = StubClaudeNative::unavailable();
    let candidate = discover_claude_at(&file, &denied).expect("readable file survives denial");
    assert!(!candidate.wire.import_supported);
    assert_eq!(denied.reads.load(Ordering::SeqCst), 0);
}

fn codex_access_jwt() -> String {
    fake_jwt(&serde_json::json!({
        "exp": 4_102_444_800u64,
        "https://api.openai.com/auth": { "chatgpt_account_id": "acct-fake-codex-1" }
    }))
}

fn codex_id_jwt() -> String {
    fake_jwt(&serde_json::json!({
        "email": "person@example.invalid",
        "https://api.openai.com/auth": {
            "chatgpt_plan_type": "pro",
            "chatgpt_account_id": "acct-fake-codex-1"
        }
    }))
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
        ".grok/auth.json",
        br#"{
  "access_token": "fake-grok-access-token-d1",
  "refresh_token": "fake-grok-refresh-token-d1",
  "expires_in": 3600,
  "issuer": "https://auth.x.ai"
}"#,
    );
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
        5,
        "expected codex + claude + kimi + grok + gemini candidates, got {candidates:?}"
    );
    let codex = candidates
        .iter()
        .find(|candidate| candidate.wire.source == "codex")
        .expect("Codex candidate");
    let identity = codex.wire.identity.as_ref().expect("Codex identity");
    assert_eq!(identity.email.as_deref(), Some("person@example.invalid"));
    assert_eq!(identity.plan.as_deref(), Some("pro"));
    assert_eq!(identity.account_id.as_deref(), Some("acct-fake-codex-1"));
    assert!(!identity.verified);

    let response = serde_json::to_string(&haider_rpc::ResponseBody::AccountDeviceCandidates {
        discovery_disabled: false,
        candidates: candidates
            .iter()
            .map(|candidate| candidate.wire.clone())
            .collect(),
        adoption_available: Vec::new(),
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
        "fake-grok-access-token-d1",
        "fake-grok-refresh-token-d1",
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
    assert!(
        Path::new(&codex.wire.path).ends_with(Path::new(".codex").join("auth.json")),
        "Codex candidate path must end in the platform-native auth path: {}",
        codex.wire.path
    );
    assert!(codex.wire.import_supported);
    assert_eq!(codex.import_source, Some("codex"));

    let claude = by_provider("anthropic-oauth");
    assert_eq!(
        claude.wire.source_label,
        "Claude Code credential file (read-only)"
    );
    assert_eq!(claude.wire.expires_at_ms, Some(4_102_444_800_123));
    assert!(!claude.wire.import_supported);
    assert_eq!(claude.import_source, None);
    assert!(
        claude.wire.unsupported_reason.is_some(),
        "Claude Code subscription credentials remain read-only metadata"
    );

    let kimi = by_provider("kimi-oauth");
    assert_eq!(kimi.wire.expires_at_ms, Some(4_102_444_800_000));
    assert!(
        kimi.wire.import_supported,
        "valid device id supports import"
    );
    assert_eq!(kimi.import_source, Some("kimi-code"));

    let grok = by_provider("grok-oauth");
    assert_eq!(grok.wire.source_label, "Grok CLI");
    assert!(grok.wire.import_supported);
    assert_eq!(grok.import_source, Some("grok-cli"));

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

#[test]
fn candidate_id_stops_matching_when_the_source_login_changes() {
    let home = fixture_home();
    populated_home(home.path());
    if run_in_isolated_home(
        "device_discovery::tests::candidate_id_stops_matching_when_the_source_login_changes",
        home.path(),
        &[],
    ) {
        return;
    }

    let candidate = discover_device_candidates(false)
        .into_iter()
        .find(|candidate| candidate.wire.source == "codex")
        .expect("Codex candidate");
    let old_id = candidate.wire.candidate;
    let path = PathBuf::from(candidate.wire.path);
    let mut document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("read fixture")).expect("fixture JSON");
    document["tokens"]["refresh_token"] =
        serde_json::Value::String("changed-refresh-token".to_owned());
    std::fs::write(
        &path,
        serde_json::to_vec(&document).expect("encode fixture"),
    )
    .expect("update fixture");

    assert!(candidate_by_id(false, &old_id).is_none());
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

/// Structurally valid JSON without the required Codex token fields is a
/// distinct operator-facing condition, not malformed JSON.
#[test]
fn linked_codex_missing_required_fields_are_reported_as_missing_fields() {
    for document in [br#"{}"#.as_slice(), br#"{"tokens":{}}"#.as_slice()] {
        let result = linked_codex_material(
            Path::new("auth.json"),
            zeroize::Zeroizing::new(document.to_vec()),
            haider_accounts::CredentialStoreMode::File,
            None,
        );
        assert!(matches!(
            result,
            Err(LinkedSourceReadFailure::MissingFields)
        ));
    }
}

// ─────────────── linked Grok CLI / Kimi Code roots (970, layer B) ───────────
//
// Every fixture below is synthetic and built in a temp dir. No real
// credential store is ever opened, and no token value is asserted on — only
// the public projection and the typed health classification.

/// A structurally valid but entirely fake access token. Grok's `key` really
/// is a JWT, but the linked path never decodes it, so an opaque string is
/// enough — and pins that no claim is read out of it.
const GROK_ACCESS_FIXTURE: &str = "grok-access-token-fixture";
const KIMI_ACCESS_FIXTURE: &str = "kimi-access-token-fixture";
/// Present in every fixture and readable by serde in principle. The linked
/// shapes have no field for it, so it must never reach a bundle.
const ORIGIN_REFRESH_FIXTURE: &str = "REFRESH_MUST_NEVER_ENTER_A_LINKED_BUNDLE";
const GROK_SUBSCRIPTION_SCOPE: &str = "https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828";

fn linked_record(kind: CredentialSourceKind, root: &Path) -> CredentialSourceRecord {
    CredentialSourceRecord {
        id: format!("src1_{}", "7".repeat(64)),
        kind,
        root: std::fs::canonicalize(root).expect("canonical fixture root"),
        label: "fixture".to_owned(),
        enabled: true,
        store_mode: haider_accounts::CredentialStoreMode::Unknown,
        refresh_owner: kind.refresh_owner(),
        account_alias: None,
        last_scanned_at_ms: None,
        last_refreshed_at_ms: None,
        access_expires_at_ms: None,
        health: CredentialSourceHealth::Pending,
    }
}

fn grok_entry(auth_mode: &str, user_id: &str, email: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "key": GROK_ACCESS_FIXTURE,
        "refresh_token": ORIGIN_REFRESH_FIXTURE,
        "expires_at": "2099-01-01T00:00:00Z",
        "create_time": "2026-09-01T00:00:00Z",
        "auth_mode": auth_mode,
        "user_id": user_id,
        "email": email,
        "team_id": "team-fixture",
    })
}

fn write_grok_root(root: &Path, document: &serde_json::Value) {
    std::fs::create_dir_all(root).expect("create Grok root");
    std::fs::write(
        root.join("auth.json"),
        serde_json::to_vec(document).expect("encode Grok fixture"),
    )
    .expect("write Grok fixture");
}

fn kimi_document(access_token: &str, expires_at: f64) -> serde_json::Value {
    serde_json::json!({
        "access_token": access_token,
        "refresh_token": ORIGIN_REFRESH_FIXTURE,
        "expires_at": expires_at,
        "expires_in": 3600,
        "scope": "openid offline_access",
        "token_type": "Bearer",
    })
}

fn write_kimi_slot(root: &Path, file_name: &str, document: &serde_json::Value) {
    let directory = root.join("credentials");
    std::fs::create_dir_all(&directory).expect("create Kimi credentials dir");
    std::fs::write(
        directory.join(file_name),
        serde_json::to_vec(document).expect("encode Kimi fixture"),
    )
    .expect("write Kimi fixture");
}

fn linked_bundle(material: &LinkedSourceMaterial) -> haider_accounts::OAuthTokenBundleV1 {
    let encoded = material
        .encoded_bundle
        .as_ref()
        .expect("linked material carries an access bundle");
    haider_accounts::OAuthTokenBundleV1::decode(encoded).expect("decode linked bundle")
}

/// LAW: an `xai::api_key` entry is an API key, not an OAuth grant. It never
/// becomes a linked OAuth account, and a file holding BOTH links exactly the
/// OIDC subscription login.
#[test]
fn grok_api_key_entries_never_become_a_linked_oauth_account() {
    let home = fixture_home();
    let api_only = home.path().join("grok-api-only");
    write_grok_root(
        &api_only,
        &serde_json::json!({
            "xai::api_key": {
                "key": GROK_ACCESS_FIXTURE,
                "auth_mode": "api_key",
                "user_id": "api-key-user",
            }
        }),
    );
    assert!(matches!(
        read_linked_source(&linked_record(CredentialSourceKind::GrokHome, &api_only)),
        Err(LinkedSourceReadFailure::MissingFields)
    ));

    let mixed = home.path().join("grok-mixed");
    write_grok_root(
        &mixed,
        &serde_json::json!({
            "xai::api_key": {
                "key": GROK_ACCESS_FIXTURE,
                "auth_mode": "api_key",
                "user_id": "api-key-user",
            },
            GROK_SUBSCRIPTION_SCOPE: grok_entry("oidc", "user-oidc", Some("pilot@example.test")),
        }),
    );
    let material = read_linked_source(&linked_record(CredentialSourceKind::GrokHome, &mixed))
        .expect("mixed store links its OIDC login");
    assert_eq!(material.display_identity, "pilot@example.test");
    assert_eq!(material.health, CredentialSourceHealth::Ready);
    let identity = material.identity.as_ref().expect("Grok identity");
    assert_eq!(identity.account_id.as_deref(), Some("xai:user-oidc"));
    assert_eq!(identity.issuer.as_deref(), Some("https://auth.x.ai"));
    assert_eq!(
        identity.plan, None,
        "the tier is never guessed from the store; the live meter owns it"
    );
}

/// LAW: several OIDC entries are resolved by the consumer subscription
/// authority; a store that stays ambiguous is a typed failure, never a guess.
#[test]
fn grok_multiple_oidc_entries_prefer_the_subscription_authority_then_fail_typed() {
    let home = fixture_home();
    let resolvable = home.path().join("grok-two-authorities");
    write_grok_root(
        &resolvable,
        &serde_json::json!({
            GROK_SUBSCRIPTION_SCOPE: grok_entry("oidc", "user-consumer", Some("a@example.test")),
            "https://accounts.x.ai/sign-in": grok_entry("oidc", "user-legacy", Some("b@example.test")),
        }),
    );
    let material = read_linked_source(&linked_record(CredentialSourceKind::GrokHome, &resolvable))
        .expect("the consumer authority wins");
    assert_eq!(material.display_identity, "a@example.test");
    let bundle = linked_bundle(&material);
    assert_eq!(bundle.issuer, "https://auth.x.ai");
    assert_eq!(bundle.audience, "b1a00492-073a-47ea-816f-4c329264a828");

    let ambiguous = home.path().join("grok-two-consumer");
    write_grok_root(
        &ambiguous,
        &serde_json::json!({
            GROK_SUBSCRIPTION_SCOPE: grok_entry("oidc", "user-one", Some("a@example.test")),
            "https://auth.x.ai::22222222-2222-4222-8222-222222222222":
                grok_entry("oidc", "user-two", Some("b@example.test")),
        }),
    );
    assert!(matches!(
        read_linked_source(&linked_record(CredentialSourceKind::GrokHome, &ambiguous)),
        Err(LinkedSourceReadFailure::Invalid)
    ));
}

/// LAW (load-bearing): a linked Grok or Kimi bundle carries the access token
/// only. The origin refresh token is present in both fixtures and must not
/// survive into the bundle in any form.
#[test]
fn linked_grok_and_kimi_bundles_never_carry_the_origin_refresh_token() {
    let home = fixture_home();
    let grok_root = home.path().join("grok-refresh");
    write_grok_root(
        &grok_root,
        &serde_json::json!({
            GROK_SUBSCRIPTION_SCOPE: grok_entry("oidc", "user-refresh", Some("r@example.test")),
        }),
    );
    let kimi_root = home.path().join("kimi-refresh");
    write_kimi_slot(
        &kimi_root,
        "kimi-code.json",
        &kimi_document(KIMI_ACCESS_FIXTURE, 4_102_444_800.0),
    );

    for record in [
        linked_record(CredentialSourceKind::GrokHome, &grok_root),
        linked_record(CredentialSourceKind::KimiCodeHome, &kimi_root),
    ] {
        let material = read_linked_source(&record).expect("linked material");
        let bundle = linked_bundle(&material);
        assert!(
            bundle.refresh_token().is_none(),
            "{} must never copy the origin refresh token",
            record.kind.as_str()
        );
        let encoded = material
            .encoded_bundle
            .as_ref()
            .expect("encoded linked bundle");
        assert!(
            !encoded
                .windows(ORIGIN_REFRESH_FIXTURE.len())
                .any(|window| window == ORIGIN_REFRESH_FIXTURE.as_bytes()),
            "the origin refresh token must not appear anywhere in the bundle bytes"
        );
    }
}

/// LAW: after a rejected refresh the Kimi CLI rewrites the file IN PLACE
/// with empty tokens and `expires_at: 0`. That is a logged-out store, not a
/// usable credential, and it must never yield a resolvable bundle.
#[test]
fn kimi_revocation_tombstone_is_revoked_and_yields_no_bundle() {
    let home = fixture_home();
    let root = home.path().join("kimi-tombstone");
    write_kimi_slot(
        &root,
        "kimi-code.json",
        &serde_json::json!({
            "access_token": "",
            "refresh_token": "",
            "expires_at": 0,
            "expires_in": 0,
            "scope": "openid offline_access",
            "token_type": "Bearer",
        }),
    );
    let material = read_linked_source(&linked_record(CredentialSourceKind::KimiCodeHome, &root))
        .expect("a tombstone is a readable state, not a read failure");
    assert_eq!(material.health, CredentialSourceHealth::Revoked);
    assert!(material.encoded_bundle.is_none());
    assert_eq!(material.access_expires_at_ms, None);
    assert!(material.identity.is_some(), "the account stays visible");
}

/// LAW: Kimi writes ABSOLUTE unix SECONDS; the record keeps milliseconds.
/// The synthesized account coordinate is derived from the enrolled ROOT, so
/// a token rotation keeps one account instead of forking a second one.
#[test]
fn kimi_expiry_converts_seconds_to_millis_and_identity_survives_rotation() {
    let home = fixture_home();
    let root = home.path().join("kimi-rotation");
    write_kimi_slot(
        &root,
        "kimi-code.json",
        &kimi_document(KIMI_ACCESS_FIXTURE, 4_102_444_800.0),
    );
    let record = linked_record(CredentialSourceKind::KimiCodeHome, &root);
    let before = read_linked_source(&record).expect("first generation");
    assert_eq!(before.access_expires_at_ms, Some(4_102_444_800_000));
    assert_eq!(before.health, CredentialSourceHealth::Ready);
    assert_eq!(
        before.store_mode,
        haider_accounts::CredentialStoreMode::File
    );

    write_kimi_slot(
        &root,
        "kimi-code.json",
        &kimi_document("kimi-rotated-access-token", 4_102_448_400.0),
    );
    let after = read_linked_source(&record).expect("rotated generation");
    assert_eq!(after.access_expires_at_ms, Some(4_102_448_400_000));
    assert_eq!(
        after.identity.as_ref().and_then(|id| id.account_id.clone()),
        before
            .identity
            .as_ref()
            .and_then(|id| id.account_id.clone()),
        "a rotation must not fork a second Kimi account"
    );
    assert_eq!(after.display_identity, before.display_identity);
    assert_eq!(
        linked_bundle(&after).identity.subject_hash,
        linked_bundle(&before).identity.subject_hash
    );
}

/// LAW: a non-default region slot (`kimi-code-env-<digest>.json`) is linked
/// when it is UNIQUE. Several candidates would mean guessing which login the
/// operator meant, so that is a typed failure.
#[test]
fn kimi_unique_endpoint_slot_is_linked_and_several_are_a_typed_failure() {
    let home = fixture_home();
    let unique = home.path().join("kimi-one-slot");
    write_kimi_slot(
        &unique,
        "kimi-code-env-0123456789abcdef.json",
        &kimi_document(KIMI_ACCESS_FIXTURE, 4_102_444_800.0),
    );
    let material = read_linked_source(&linked_record(CredentialSourceKind::KimiCodeHome, &unique))
        .expect("unique endpoint slot links");
    assert_eq!(material.access_expires_at_ms, Some(4_102_444_800_000));

    let several = home.path().join("kimi-two-slots");
    for name in [
        "kimi-code-env-0123456789abcdef.json",
        "kimi-code-env-fedcba9876543210.json",
    ] {
        write_kimi_slot(
            &several,
            name,
            &kimi_document(KIMI_ACCESS_FIXTURE, 4_102_444_800.0),
        );
    }
    assert!(matches!(
        read_linked_source(&linked_record(CredentialSourceKind::KimiCodeHome, &several)),
        Err(LinkedSourceReadFailure::Invalid)
    ));
}

/// LAW: a Kimi root that exists with no readable credential document is
/// reported honestly. The legacy Python CLI kept logins in a deprecated
/// keyring Haider cannot enumerate, so absence means "ask the origin
/// client", never a fabricated account and never "no login".
#[test]
fn kimi_root_without_a_readable_credential_requires_the_origin_client() {
    let home = fixture_home();
    let empty_root = home.path().join("kimi-empty");
    std::fs::create_dir_all(&empty_root).expect("create empty Kimi root");
    assert!(matches!(
        read_linked_source(&linked_record(
            CredentialSourceKind::KimiCodeHome,
            &empty_root
        )),
        Err(LinkedSourceReadFailure::RequiresOriginClient)
    ));

    let empty_credentials = home.path().join("kimi-empty-credentials");
    std::fs::create_dir_all(empty_credentials.join("credentials"))
        .expect("create empty credentials dir");
    assert!(matches!(
        read_linked_source(&linked_record(
            CredentialSourceKind::KimiCodeHome,
            &empty_credentials
        )),
        Err(LinkedSourceReadFailure::RequiresOriginClient)
    ));
}

/// LAW: every existing failure classification still applies to the new
/// kinds. A truncated document is a partial write by the origin client and
/// stays distinguishable from malformed JSON, an oversized store, a symlink
/// that escapes the enrolled root, and structurally valid JSON with no
/// linkable login.
#[test]
fn every_linked_health_state_classifies_for_grok_and_kimi_roots() {
    let home = fixture_home();

    let gone = home.path().join("grok-gone");
    std::fs::create_dir_all(&gone).expect("create Grok root");
    assert!(matches!(
        read_linked_source(&linked_record(CredentialSourceKind::GrokHome, &gone)),
        Err(LinkedSourceReadFailure::SourceGone)
    ));

    for (name, kind, body, expected) in [
        (
            "grok-partial",
            CredentialSourceKind::GrokHome,
            br#"{"https://auth.x.ai::c": {"key": "abc""#.to_vec(),
            LinkedSourceReadFailure::PartialWrite,
        ),
        (
            "grok-invalid",
            CredentialSourceKind::GrokHome,
            b"not json at all".to_vec(),
            LinkedSourceReadFailure::InvalidJson,
        ),
        (
            "grok-missing",
            CredentialSourceKind::GrokHome,
            br#"{"https://auth.x.ai::c": {"auth_mode": "oidc"}}"#.to_vec(),
            LinkedSourceReadFailure::MissingFields,
        ),
        (
            "kimi-partial",
            CredentialSourceKind::KimiCodeHome,
            br#"{"access_token": "abc""#.to_vec(),
            LinkedSourceReadFailure::PartialWrite,
        ),
        (
            "kimi-invalid",
            CredentialSourceKind::KimiCodeHome,
            b"not json at all".to_vec(),
            LinkedSourceReadFailure::InvalidJson,
        ),
        (
            "kimi-missing",
            CredentialSourceKind::KimiCodeHome,
            br#"{"scope": "openid"}"#.to_vec(),
            LinkedSourceReadFailure::MissingFields,
        ),
        (
            "grok-oversized",
            CredentialSourceKind::GrokHome,
            vec![b' '; usize::try_from(DISCOVERY_FILE_LIMIT).unwrap_or(usize::MAX) + 1],
            LinkedSourceReadFailure::Oversized,
        ),
        (
            "kimi-oversized",
            CredentialSourceKind::KimiCodeHome,
            vec![b' '; usize::try_from(DISCOVERY_FILE_LIMIT).unwrap_or(usize::MAX) + 1],
            LinkedSourceReadFailure::Oversized,
        ),
    ] {
        let root = home.path().join(name);
        let path = root.join(kind.credential_relative_path());
        std::fs::create_dir_all(path.parent().expect("credential parent")).expect("create root");
        std::fs::write(&path, &body).expect("write fixture");
        assert_eq!(
            read_linked_source(&linked_record(kind, &root)).err(),
            Some(expected),
            "{name}"
        );
    }

    #[cfg(unix)]
    {
        let outside = home.path().join("outside-auth.json");
        std::fs::write(&outside, b"{}").expect("write outside credential");
        for (name, kind) in [
            ("grok-escape", CredentialSourceKind::GrokHome),
            ("kimi-escape", CredentialSourceKind::KimiCodeHome),
        ] {
            let root = home.path().join(name);
            let path = root.join(kind.credential_relative_path());
            std::fs::create_dir_all(path.parent().expect("credential parent"))
                .expect("create root");
            std::os::unix::fs::symlink(&outside, &path).expect("symlink escape fixture");
            assert_eq!(
                read_linked_source(&linked_record(kind, &root)).err(),
                Some(LinkedSourceReadFailure::SymlinkEscape),
                "{name}"
            );
        }
    }
}
