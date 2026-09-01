#![allow(clippy::expect_used)]

use std::sync::Condvar;
use std::sync::atomic::{AtomicBool, AtomicUsize};

use futures_util::StreamExt as _;
use haider_accounts::{AccountsResult, MemoryVault};
use haider_protocol::credential::CredentialStatus;
use tokio::sync::{Notify, Semaphore};

use super::*;
use crate::session_hub::{FrameSendError, FrameSink};

#[test]
fn macos_keychain_ui_is_reserved_for_explicit_import_intent() {
    for event in [
        ClaudeNativeReadEvent::Ordinary,
        ClaudeNativeReadEvent::AdoptionDiscovery,
        ClaudeNativeReadEvent::Significant,
    ] {
        let plan = event.macos_keychain_query_plan();
        assert!(plan.skip_authenticated_attribute_items);
        assert!(plan.skip_authenticated_data_items);
        assert_eq!(
            plan.allow_interactive_data_fallback,
            matches!(event, ClaudeNativeReadEvent::Significant)
        );
    }

    assert_eq!(
        ClaudeNativeReadEvent::Ordinary.credential_interaction_resolution(),
        haider_core::InteractionResolution::FailClosed
    );
    assert_eq!(
        ClaudeNativeReadEvent::AdoptionDiscovery.credential_interaction_resolution(),
        haider_core::InteractionResolution::FailClosed
    );
    assert_eq!(
        ClaudeNativeReadEvent::Significant.credential_interaction_resolution(),
        haider_core::InteractionResolution::AwaitHuman
    );
}

const CODE_SENTINEL: &str = "AUTH_CODE_SENTINEL_51d2";
const ACCESS_SENTINEL: &str = "ACCESS_TOKEN_SENTINEL_834a";
const REFRESH_SENTINEL: &str = "REFRESH_TOKEN_SENTINEL_1c72";
const ID_SENTINEL: &str = "ID_TOKEN_SENTINEL_97e1";
const RAW_ERROR_SENTINEL: &str = "RAW_TOKEN_ERROR_SENTINEL_29af";
const RAW_BODY_SENTINEL: &str = "RAW_TOKEN_BODY_SENTINEL_a83c";
const SCOPES: &str = "openid inference profile";
const AUDIENCE: &str = "fake-resource";

const CLAUDE_SECURE_STORE_FIXTURE: &[u8] = br#"{
  "claudeAiOauth": {
    "accessToken": "fake-claude-secure-store-access",
    "refreshToken": "fake-claude-secure-store-refresh",
    "expiresAt": 4102444800123,
    "scopes": ["user:inference"],
    "subscriptionType": "max"
  }
}"#;

/// Catalog/spec drift pin.
///
/// MUTATION CHECK: drop any entry from `oauth_import_source_catalog` before
/// returning it. Expected runtime failure: this exact four-source assertion
/// reports the missing source (including `grok-cli`).
#[test]
fn oauth_import_source_catalog_contains_every_supported_spec() {
    let native = OAuthTestClaudeNative::unavailable();
    let catalog = oauth_import_source_catalog(&native);
    let mut sources = catalog
        .iter()
        .map(|entry| entry.source.as_str())
        .collect::<Vec<_>>();
    sources.sort_unstable();
    assert_eq!(
        sources,
        ["claude-code", "codex", "grok-cli", "kimi-code"],
        "the published catalog must contain all four sanctioned import sources"
    );
    for entry in catalog {
        let spec = oauth_import_source_spec(&entry.source).expect("catalog source has a spec");
        assert_eq!(entry.provider, spec.provider);
        assert_eq!(entry.default_alias, spec.default_alias);
    }
}

/// Missing-source availability pin.
///
/// MUTATION CHECK: construct an unavailable entry with
/// `unavailable_reason: None`. Expected runtime failure: the reason-presence
/// assertion below fails before a client can lose its branchable code.
#[test]
fn absent_oauth_import_credentials_have_a_typed_reason() {
    let directory = tempfile::tempdir().expect("catalog fixture directory");
    let spec = oauth_import_source_spec("codex").expect("codex spec");
    let entry =
        oauth_import_source_file_entry(spec, Some(directory.path().join("missing-auth.json")));

    assert!(!entry.available);
    let reason = entry
        .unavailable_reason
        .expect("available=false always carries a reason");
    assert_eq!(reason.code, OAuthImportSourceUnavailableCodeWire::NotFound);
    assert!(!reason.message.is_empty());
}

/// Present-source availability pin.
///
/// MUTATION CHECK: attach an unavailable reason to the successful branch of
/// `oauth_import_source_file_entry`. Expected runtime failure: the final
/// assertion observes the contradictory reason.
#[test]
fn present_oauth_import_credentials_have_no_unavailable_reason() {
    let directory = tempfile::tempdir().expect("catalog fixture directory");
    let path = directory.path().join("auth.json");
    std::fs::write(&path, b"credential-present").expect("write credential fixture");
    let spec = oauth_import_source_spec("codex").expect("codex spec");
    let entry = oauth_import_source_file_entry(spec, Some(path));

    assert!(entry.available);
    assert_eq!(entry.unavailable_reason, None);
}

struct OAuthTestClaudeNative {
    bytes: Option<Vec<u8>>,
    reads: AtomicUsize,
}

impl OAuthTestClaudeNative {
    fn new(bytes: &[u8]) -> Self {
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

impl ClaudeNativeCredentialStore for OAuthTestClaudeNative {
    fn read(
        &self,
        _event: ClaudeNativeReadEvent,
    ) -> Result<ClaudeCredentialInput, ClaudeNativeCredentialFailure> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.bytes
            .as_ref()
            .map(|bytes| ClaudeCredentialInput {
                location: PathBuf::from("mock secure store: Claude Code-credentials"),
                bytes: Zeroizing::new(bytes.clone()),
                native_owner: true,
            })
            .ok_or(ClaudeNativeCredentialFailure::Missing)
    }
}

struct FailingClaudeNative {
    failure: ClaudeNativeCredentialFailure,
    reads: AtomicUsize,
}

impl ClaudeNativeCredentialStore for FailingClaudeNative {
    fn read(
        &self,
        _event: ClaudeNativeReadEvent,
    ) -> Result<ClaudeCredentialInput, ClaudeNativeCredentialFailure> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Err(self.failure)
    }
}

/// LAW D1: one ordinary read failure arms the boot-scoped cooldown. Provider
/// read-throughs receive the same typed denial without calling the raw store
/// again; an explicit/significant refresh gets exactly one new attempt.
#[test]
fn denied_native_read_is_cooled_down_until_a_significant_event() {
    let raw = Arc::new(FailingClaudeNative {
        failure: ClaudeNativeCredentialFailure::Denied,
        reads: AtomicUsize::new(0),
    });
    let access = ClaudeNativeCredentialAccess::new(raw.clone());

    assert!(matches!(
        access.read(ClaudeNativeReadEvent::Ordinary),
        Err(ClaudeNativeCredentialFailure::Denied)
    ));
    assert!(matches!(
        access.read(ClaudeNativeReadEvent::Ordinary),
        Err(ClaudeNativeCredentialFailure::Denied)
    ));
    assert_eq!(raw.reads.load(Ordering::SeqCst), 1);

    assert!(matches!(
        access.read(ClaudeNativeReadEvent::Significant),
        Err(ClaudeNativeCredentialFailure::Denied)
    ));
    assert_eq!(raw.reads.load(Ordering::SeqCst), 2);
}

struct SuccessfulClaudeNative {
    reads: AtomicUsize,
}

impl ClaudeNativeCredentialStore for SuccessfulClaudeNative {
    fn read(
        &self,
        _event: ClaudeNativeReadEvent,
    ) -> Result<ClaudeCredentialInput, ClaudeNativeCredentialFailure> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(ClaudeCredentialInput {
            location: PathBuf::from("mock-keychain"),
            bytes: Zeroizing::new(br#"{"oauthAccessToken":"mock"}"#.to_vec()),
            native_owner: true,
        })
    }
}

/// LAW A4/D1: one metadata discovery read hands its bytes to the immediate
/// candidate lookup exactly once; later ordinary reads still reach the owner.
#[test]
fn adoption_discovery_is_handed_to_candidate_lookup_without_a_second_store_call() {
    let raw = Arc::new(SuccessfulClaudeNative {
        reads: AtomicUsize::new(0),
    });
    let access = ClaudeNativeCredentialAccess::new(raw.clone());

    access
        .read(ClaudeNativeReadEvent::AdoptionDiscovery)
        .expect("discovery reads the native owner");
    access
        .read(ClaudeNativeReadEvent::Ordinary)
        .expect("candidate lookup receives the one-shot handoff");
    assert_eq!(raw.reads.load(Ordering::SeqCst), 1);

    access
        .read(ClaudeNativeReadEvent::Ordinary)
        .expect("later read-through returns to the live owner");
    assert_eq!(raw.reads.load(Ordering::SeqCst), 2);
}

struct ReleasableBlockingClaudeNative {
    calls: AtomicUsize,
    started: std::sync::mpsc::SyncSender<()>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
}

impl ClaudeNativeCredentialStore for ReleasableBlockingClaudeNative {
    fn read(
        &self,
        _event: ClaudeNativeReadEvent,
    ) -> Result<ClaudeCredentialInput, ClaudeNativeCredentialFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let _ = self.started.send(());
        let _ = self
            .release
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .recv();
        Err(ClaudeNativeCredentialFailure::Missing)
    }
}

/// P0 regression pin: a platform API that never returns is a typed source
/// timeout, and subsequent observational discovery uses the cached outcome
/// instead of leaking one new stuck OS thread per status call.
#[test]
fn native_store_timeout_is_typed_and_cooled_down_for_discovery() {
    let (started, observed_start) = std::sync::mpsc::sync_channel(1);
    let (release, blocked) = std::sync::mpsc::sync_channel(1);
    let raw = Arc::new(ReleasableBlockingClaudeNative {
        calls: AtomicUsize::new(0),
        started,
        release: Mutex::new(blocked),
    });
    let access =
        ClaudeNativeCredentialAccess::new_with_timeout(raw.clone(), Duration::from_millis(25));

    assert!(matches!(
        access.read(ClaudeNativeReadEvent::AdoptionDiscovery),
        Err(ClaudeNativeCredentialFailure::TimedOut)
    ));
    observed_start
        .recv_timeout(Duration::from_secs(1))
        .expect("raw platform read started");
    assert!(matches!(
        access.read(ClaudeNativeReadEvent::AdoptionDiscovery),
        Err(ClaudeNativeCredentialFailure::TimedOut)
    ));
    assert_eq!(raw.calls.load(Ordering::SeqCst), 1);
    let _ = release.send(());
}

struct BlockingFirstClaudeNative {
    calls: AtomicUsize,
    first_started: std::sync::mpsc::SyncSender<()>,
    first_release: Mutex<std::sync::mpsc::Receiver<()>>,
}

impl ClaudeNativeCredentialStore for BlockingFirstClaudeNative {
    fn read(
        &self,
        _event: ClaudeNativeReadEvent,
    ) -> Result<ClaudeCredentialInput, ClaudeNativeCredentialFailure> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            let _ = self.first_started.send(());
            let _ = self
                .first_release
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .recv();
            return Err(ClaudeNativeCredentialFailure::Missing);
        }
        Err(ClaudeNativeCredentialFailure::Denied)
    }
}

/// P0 lock-scope pin: while one raw platform read is blocked, a newer explicit
/// read reaches the injected store. Holding `state` across `store.read` makes
/// this second call miss the deadline.
#[test]
fn native_platform_read_never_holds_the_access_mutex() {
    let (first_started, observed_first) = std::sync::mpsc::sync_channel(1);
    let (first_release, blocked_first) = std::sync::mpsc::sync_channel(1);
    let raw = Arc::new(BlockingFirstClaudeNative {
        calls: AtomicUsize::new(0),
        first_started,
        first_release: Mutex::new(blocked_first),
    });
    let access = Arc::new(ClaudeNativeCredentialAccess::new_with_timeout(
        raw.clone(),
        Duration::from_secs(1),
    ));
    let first_access = Arc::clone(&access);
    let first =
        std::thread::spawn(move || first_access.read(ClaudeNativeReadEvent::AdoptionDiscovery));
    observed_first
        .recv_timeout(Duration::from_secs(1))
        .expect("first raw read started");

    let began = std::time::Instant::now();
    assert!(matches!(
        access.read(ClaudeNativeReadEvent::Significant),
        Err(ClaudeNativeCredentialFailure::Denied)
    ));
    assert!(
        began.elapsed() < Duration::from_millis(250),
        "the second platform read waited behind the access mutex"
    );
    let _ = first_release.send(());
    assert!(matches!(
        first.join().expect("join first access call"),
        Err(ClaudeNativeCredentialFailure::Missing)
    ));
    assert_eq!(raw.calls.load(Ordering::SeqCst), 2);
}

struct StubFixedResolver {
    address: SocketAddr,
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl FixedDnsResolver for StubFixedResolver {
    async fn resolve(&self, host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
        assert_eq!(host, OPENAI_JWKS_HOST);
        assert_eq!(port, 443);
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(vec![self.address])
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FakeMode {
    Success,
    Denied,
    TokenRedirect,
    Malformed,
    Oversized,
    InvalidGrant,
    Transient,
    AmbiguousTransport,
    ScopeMismatch,
    IssuerMismatch,
    AudienceMismatch,
    NonceMismatch,
    RefreshIssuerMismatch,
    RefreshAudienceMismatch,
    RefreshResourceMismatch,
    RefreshOmitTokenAndExpiry,
    VerifierMismatch,
    SlowExchange,
}

#[derive(Clone)]
struct AuthSeen {
    redirect_uri: String,
    state: String,
    challenge: String,
    nonce: String,
    audience: String,
}

struct FakeState {
    mode: FakeMode,
    issuer: String,
    auth: Mutex<Option<AuthSeen>>,
    token_calls: AtomicUsize,
    redirect_target_calls: AtomicUsize,
    refresh_calls: AtomicUsize,
    saw_client_secret: AtomicBool,
    token_encodings: Mutex<Vec<(String, String)>>,
    refresh_token_fingerprints: Mutex<Vec<[u8; 32]>>,
    msh_headers: Mutex<Vec<HashMap<String, String>>>,
    expect_refresh_binding: AtomicBool,
    expect_code_state: AtomicBool,
    verifiers: Mutex<Vec<String>>,
    refresh_gate: Option<Arc<Semaphore>>,
    refresh_started: Notify,
    durable: AtomicBool,
    resource_before_durable: AtomicUsize,
}

struct FakeOAuthServer {
    address: SocketAddr,
    state: Arc<FakeState>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeOAuthServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeOAuthServer {
    async fn start(mode: FakeMode, gated_refresh: bool) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("fake bind");
        let address = listener.local_addr().expect("fake address");
        let state = Arc::new(FakeState {
            mode,
            issuer: format!("http://{address}"),
            auth: Mutex::new(None),
            token_calls: AtomicUsize::new(0),
            redirect_target_calls: AtomicUsize::new(0),
            refresh_calls: AtomicUsize::new(0),
            saw_client_secret: AtomicBool::new(false),
            token_encodings: Mutex::new(Vec::new()),
            refresh_token_fingerprints: Mutex::new(Vec::new()),
            msh_headers: Mutex::new(Vec::new()),
            expect_refresh_binding: AtomicBool::new(true),
            expect_code_state: AtomicBool::new(false),
            verifiers: Mutex::new(Vec::new()),
            refresh_gate: (gated_refresh || mode == FakeMode::SlowExchange)
                .then(|| Arc::new(Semaphore::new(0))),
            refresh_started: Notify::new(),
            durable: AtomicBool::new(false),
            resource_before_durable: AtomicUsize::new(0),
        });
        let state_for_task = Arc::clone(&state);
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let state = Arc::clone(&state_for_task);
                tokio::spawn(async move {
                    serve_fake_request(stream, state).await;
                });
            }
        });
        Self {
            address,
            state,
            task,
        }
    }

    fn registration(&self, verifier: Arc<dyn OAuthIdentityVerifier>) -> OAuthProviderRegistration {
        OAuthProviderRegistration::new(
            "fake-oauth",
            self.state.issuer.clone(),
            format!("http://{}/authorize", self.address),
            format!("http://{}/token", self.address),
            "haider-public-fake",
            ["openid", "inference", "profile"].map(str::to_owned),
            AUDIENCE,
            Some("fake-api-resource".into()),
            true,
            verifier,
        )
        .expect("registration")
    }

    fn release_refresh(&self) {
        if let Some(gate) = &self.state.refresh_gate {
            gate.add_permits(32);
        }
    }
}

async fn serve_fake_request(mut stream: TcpStream, state: Arc<FakeState>) {
    let Some((method, target, authorization, content_type, headers, body)) =
        read_http_request(&mut stream).await
    else {
        return;
    };
    if target == "/device_authorization" || target == "/token" {
        state
            .msh_headers
            .lock()
            .expect("MSH header lock")
            .push(headers);
    }
    if target == "/device_authorization" {
        assert_eq!(method, "POST");
        assert_eq!(
            content_type.as_deref(),
            Some("application/x-www-form-urlencoded")
        );
        let fields = url::form_urlencoded::parse(body.as_bytes())
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect::<HashMap<_, _>>();
        assert_eq!(fields.len(), 1);
        assert_eq!(
            fields.get("client_id").map(String::as_str),
            Some("haider-public-fake")
        );
        let response = serde_json::json!({
            "device_code": "DEVICE_CODE_SENTINEL_54bf",
            "user_code": "ABCD-EFGH",
            "verification_uri_complete": format!("{}/verify?user_code=ABCD-EFGH", state.issuer),
            "expires_in": 60,
            "interval": 1
        })
        .to_string();
        write_http(&mut stream, 200, &[], response.as_bytes()).await;
        return;
    }
    if target.starts_with("/authorize") {
        let parsed = Url::parse(&format!("http://fake{target}")).expect("authorize URL");
        let params = parsed
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect::<HashMap<_, _>>();
        assert_eq!(method, "GET");
        assert_eq!(
            params.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(
            params.get("client_id").map(String::as_str),
            Some("haider-public-fake")
        );
        let seen = AuthSeen {
            redirect_uri: params["redirect_uri"].clone(),
            state: params["state"].clone(),
            challenge: params["code_challenge"].clone(),
            nonce: params["nonce"].clone(),
            audience: params["audience"].clone(),
        };
        *state.auth.lock().expect("auth lock") = Some(seen.clone());
        let terminal = if state.mode == FakeMode::Denied {
            format!("error=access_denied&state={}", seen.state)
        } else {
            format!("code={CODE_SENTINEL}&state={}", seen.state)
        };
        let separator = if seen.redirect_uri.contains('?') {
            '&'
        } else {
            '?'
        };
        write_http(
            &mut stream,
            302,
            &[(
                "Location",
                format!("{}{separator}{terminal}", seen.redirect_uri),
            )],
            b"",
        )
        .await;
        return;
    }
    if target == "/token-redirect-target" {
        state.redirect_target_calls.fetch_add(1, Ordering::SeqCst);
        write_http(&mut stream, 500, &[], b"must not follow").await;
        return;
    }
    if target == "/resource" {
        if authorization.as_deref() != Some("Bearer ACCESS_ROTATED_SENTINEL_3a19") {
            write_http(&mut stream, 400, &[], b"wrong bearer").await;
            return;
        }
        if !state.durable.load(Ordering::SeqCst) {
            state.resource_before_durable.fetch_add(1, Ordering::SeqCst);
            write_http(&mut stream, 409, &[], b"not durable").await;
        } else {
            write_http(&mut stream, 200, &[], b"ok").await;
        }
        return;
    }
    if target != "/token" {
        write_http(&mut stream, 404, &[], b"not found").await;
        return;
    }
    let token_call = state.token_calls.fetch_add(1, Ordering::SeqCst);
    let fields = if content_type.as_deref() == Some("application/json") {
        serde_json::from_str::<HashMap<String, String>>(&body).expect("JSON token request")
    } else {
        url::form_urlencoded::parse(body.as_bytes())
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect::<HashMap<_, _>>()
    };
    let grant = fields.get("grant_type").map(String::as_str).unwrap_or("");
    state
        .token_encodings
        .lock()
        .expect("token encoding lock")
        .push((grant.to_owned(), content_type.clone().unwrap_or_default()));
    if grant == "urn:ietf:params:oauth:grant-type:device_code" {
        assert_eq!(fields.len(), 3);
        assert_eq!(
            fields.get("device_code").map(String::as_str),
            Some("DEVICE_CODE_SENTINEL_54bf")
        );
        assert_eq!(
            fields.get("client_id").map(String::as_str),
            Some("haider-public-fake")
        );
        if token_call == 0 {
            write_http(
                &mut stream,
                400,
                &[],
                br#"{"error":"authorization_pending"}"#,
            )
            .await;
        } else if token_call == 1 {
            write_http(&mut stream, 400, &[], br#"{"error":"slow_down"}"#).await;
        } else {
            let response = serde_json::json!({
                "access_token": ACCESS_SENTINEL,
                "refresh_token": REFRESH_SENTINEL,
                "token_type": "Bearer",
                "expires_in": 600,
                "refresh_expires_in": 3600,
                "scope": ""
            })
            .to_string();
            write_http(&mut stream, 200, &[], response.as_bytes()).await;
        }
        return;
    }
    if grant == "refresh_token" {
        state.refresh_calls.fetch_add(1, Ordering::SeqCst);
        state
            .refresh_token_fingerprints
            .lock()
            .expect("refresh token fingerprint lock")
            .push(
                *blake3::hash(
                    fields
                        .get("refresh_token")
                        .map_or(&[][..], |token| token.as_bytes()),
                )
                .as_bytes(),
            );
        if state.expect_refresh_binding.load(Ordering::SeqCst) {
            assert_eq!(fields.get("audience").map(String::as_str), Some(AUDIENCE));
            assert_eq!(
                fields.get("resource").map(String::as_str),
                Some("fake-api-resource"),
                "refresh requests must bind the configured resource"
            );
        } else {
            assert!(!fields.contains_key("audience"));
            assert!(!fields.contains_key("resource"));
        }
        state.refresh_started.notify_one();
        if let Some(gate) = &state.refresh_gate {
            let permit = gate.acquire().await.expect("gate");
            permit.forget();
        }
    }
    if fields.contains_key("client_secret") {
        state.saw_client_secret.store(true, Ordering::SeqCst);
    }
    if state.mode == FakeMode::TokenRedirect {
        write_http(
            &mut stream,
            302,
            &[(
                "Location",
                format!(
                    "http://{}/token-redirect-target",
                    state.issuer.trim_start_matches("http://")
                ),
            )],
            b"",
        )
        .await;
        return;
    }
    if state.mode == FakeMode::Malformed {
        write_http(&mut stream, 200, &[], b"{not-json").await;
        return;
    }
    if state.mode == FakeMode::Oversized {
        write_http(&mut stream, 200, &[], &vec![b'x'; TOKEN_RESPONSE_LIMIT + 1]).await;
        return;
    }
    if state.mode == FakeMode::InvalidGrant {
        write_http(
            &mut stream,
            400,
            &[],
            format!(r#"{{"error":"invalid_grant","detail":"{RAW_ERROR_SENTINEL}"}}"#).as_bytes(),
        )
        .await;
        return;
    }
    if state.mode == FakeMode::Transient {
        write_http(&mut stream, 503, &[], RAW_BODY_SENTINEL.as_bytes()).await;
        return;
    }
    if state.mode == FakeMode::AmbiguousTransport {
        // The endpoint consumed the request but closed before declaring an
        // outcome. A rotating client must treat this as possibly spent and
        // must never replay the token.
        return;
    }
    if grant == "authorization_code" {
        let seen = state
            .auth
            .lock()
            .expect("auth lock")
            .clone()
            .expect("authorize first");
        assert_eq!(fields.get("code").map(String::as_str), Some(CODE_SENTINEL));
        assert_eq!(
            fields.get("redirect_uri").map(String::as_str),
            Some(seen.redirect_uri.as_str())
        );
        if state.expect_code_state.load(Ordering::SeqCst) {
            assert_eq!(
                fields.get("state").map(String::as_str),
                Some(seen.state.as_str()),
                "provider-declared code exchange must echo state"
            );
        } else {
            assert!(!fields.contains_key("state"));
        }
        let verifier = fields.get("code_verifier").cloned().unwrap_or_default();
        state
            .verifiers
            .lock()
            .expect("verifier lock")
            .push(verifier.clone());
        let computed = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let verifier_matches = computed == seen.challenge;
        if !verifier_matches || state.mode == FakeMode::VerifierMismatch {
            write_http(&mut stream, 400, &[], br#"{"error":"invalid_grant"}"#).await;
            return;
        }
        if state.mode == FakeMode::SlowExchange
            && let Some(gate) = &state.refresh_gate
        {
            let permit = gate.acquire().await.expect("exchange gate");
            permit.forget();
        }
        let issuer = if state.mode == FakeMode::IssuerMismatch {
            "https://wrong-issuer.invalid".to_owned()
        } else {
            state.issuer.clone()
        };
        let audience = if state.mode == FakeMode::AudienceMismatch {
            "wrong-audience"
        } else {
            seen.audience.as_str()
        };
        let nonce = if state.mode == FakeMode::NonceMismatch {
            "wrong-nonce"
        } else {
            seen.nonce.as_str()
        };
        let claims = serde_json::json!({
            "marker": ID_SENTINEL,
            "issuer": issuer,
            "audience": audience,
            "nonce": nonce,
            "subject": "fake-subject",
            "display": "person@example.invalid"
        })
        .to_string();
        let scope = if state.mode == FakeMode::ScopeMismatch {
            "openid profile"
        } else {
            SCOPES
        };
        let response = serde_json::json!({
            "access_token": ACCESS_SENTINEL,
            "refresh_token": REFRESH_SENTINEL,
            "token_type": "Bearer",
            "expires_in": 60,
            "refresh_expires_in": 3600,
            "scope": scope,
            "id_token": claims
        })
        .to_string();
        write_http(&mut stream, 200, &[], response.as_bytes()).await;
        return;
    }
    assert_eq!(grant, "refresh_token");
    let mut response = serde_json::json!({
        "access_token": "ACCESS_ROTATED_SENTINEL_3a19",
        "refresh_token": "REFRESH_ROTATED_SENTINEL_8c21",
        "token_type": "Bearer",
        "expires_in": 120,
        "refresh_expires_in": 3600,
        "scope": SCOPES
    });
    let response = response.as_object_mut().expect("refresh response object");
    match state.mode {
        FakeMode::RefreshIssuerMismatch => {
            response.insert(
                "issuer".into(),
                serde_json::Value::String("https://wrong-issuer.invalid".into()),
            );
        }
        FakeMode::RefreshAudienceMismatch => {
            response.insert(
                "audience".into(),
                serde_json::Value::String("wrong-audience".into()),
            );
        }
        FakeMode::RefreshResourceMismatch => {
            response.insert(
                "resource".into(),
                serde_json::Value::String("wrong-resource".into()),
            );
        }
        FakeMode::RefreshOmitTokenAndExpiry => {
            response.remove("refresh_token");
            response.remove("refresh_expires_in");
        }
        _ => {}
    }
    let response = serde_json::Value::Object(response.clone()).to_string();
    write_http(&mut stream, 200, &[], response.as_bytes()).await;
}

async fn read_http_request(
    stream: &mut TcpStream,
) -> Option<(
    String,
    String,
    Option<String>,
    Option<String>,
    HashMap<String, String>,
    String,
)> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 1024];
    let header_end = loop {
        let count = stream.read(&mut chunk).await.ok()?;
        if count == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..count]);
        if bytes.len() > 512 * 1024 {
            return None;
        }
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = std::str::from_utf8(&bytes[..header_end]).ok()?;
    let first = headers.split("\r\n").next()?;
    let mut parts = first.split(' ');
    let method = parts.next()?.to_owned();
    let target = parts.next()?.to_owned();
    let content_length = headers
        .split("\r\n")
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    let authorization = headers.split("\r\n").find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("authorization")
            .then(|| value.trim().to_owned())
    });
    let content_type = headers.split("\r\n").find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-type")
            .then(|| value.trim().to_ascii_lowercase())
    });
    let header_map = headers
        .split("\r\n")
        .skip(1)
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.to_ascii_lowercase(), value.trim().to_owned()))
        })
        .collect::<HashMap<_, _>>();
    while bytes.len() < header_end.saturating_add(content_length) {
        let count = stream.read(&mut chunk).await.ok()?;
        if count == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    let body = String::from_utf8(bytes[header_end..header_end + content_length].to_vec()).ok()?;
    Some((
        method,
        target,
        authorization,
        content_type,
        header_map,
        body,
    ))
}

async fn write_http(stream: &mut TcpStream, status: u16, headers: &[(&str, String)], body: &[u8]) {
    let reason = match status {
        200 => "OK",
        302 => "Found",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await.expect("fake head");
    stream.write_all(body).await.expect("fake body");
    let _ = stream.shutdown().await;
}

/// MUTATION CHECK: remove the JWKS fixed-origin resolver and endpoint
/// validation. The identity-error/resolver assertions fail for the unpinned
/// client.
/// Verified by revert on 2026-07-29.
#[tokio::test]
async fn openai_jwks_private_dns_answer_is_rejected_before_key_use() {
    let resolver = Arc::new(StubFixedResolver {
        address: SocketAddr::from(([169, 254, 169, 254], 443)),
        calls: AtomicUsize::new(0),
    });
    let verifier = OpenAiIdentityVerifier::new_for_test(OPENAI_JWKS_ENDPOINT, resolver.clone());
    let error = verifier
        .verify(
            b"eyJhbGciOiJSUzI1NiIsImtpZCI6ImF1ZGl0LWtleSJ9.e30.AA",
            OAuthIdentityExpectation {
                issuer: "https://auth.openai.com",
                audience: "audit-audience",
                nonce: b"audit-nonce",
            },
        )
        .await
        .expect_err("private JWKS DNS answer must reject identity verification");
    assert_eq!(error.code, "identity_verifier_unavailable");
    assert_eq!(
        resolver.calls.load(Ordering::SeqCst),
        1,
        "JWKS host must resolve through the fixed-origin guard"
    );
}

/// MUTATION CHECK (W5b.2a review P3): delete ONLY the
/// `.dns_resolver(Arc::clone(&origin_guard))` line from the verifier's
/// client (the full-guard removal is killed by the private-DNS test
/// above). Expected runtime failure: the connection resolves through
/// system DNS instead of the guard, the guard's connection count stays 0,
/// and the count assertion below dies — the exact public-first DNS-rebind
/// residual the P3 named.
#[tokio::test]
async fn openai_jwks_connection_resolves_through_the_pinned_guard() {
    // TEST-NET-1 (RFC 5737): PUBLIC by classification, so the preflight
    // accepts it — but reserved, so the connect fails without touching
    // the real network. The verdict rides the COUNT, not the error.
    let resolver = Arc::new(StubFixedResolver {
        address: SocketAddr::from(([192, 0, 2, 1], 443)),
        calls: AtomicUsize::new(0),
    });
    let verifier = OpenAiIdentityVerifier::new_for_test(OPENAI_JWKS_ENDPOINT, resolver);
    // The exact error code is ENVIRONMENT-DEPENDENT — a clean network
    // fails the connect (`identity_verifier_unavailable`), while a
    // TLS-intercepting middlebox can serve a real JWKS and fail on the
    // unknown kid instead. Both are failures; the verdict below rides the
    // resolution COUNT, which the mutation zeroes in every environment.
    verifier
        .verify(
            b"eyJhbGciOiJSUzI1NiIsImtpZCI6ImF1ZGl0LWtleSJ9.e30.AA",
            OAuthIdentityExpectation {
                issuer: "https://auth.openai.com",
                audience: "audit-audience",
                nonce: b"audit-nonce",
            },
        )
        .await
        .expect_err("a fabricated kid must never verify");
    let guard = verifier.last_guard().expect("verify built a guard");
    assert!(
        guard.connection_resolution_count() >= 1,
        "the CONNECTION must resolve through the pinned guard, not system DNS"
    );
}

/// MUTATION CHECK: restore `response.bytes()` plus a post-read size check.
/// The named timeout assertion fails while that reader waits for the rest of
/// the deliberately unfinished oversized chunked response.
/// Verified by revert on 2026-07-29.
#[tokio::test]
async fn openai_jwks_chunked_body_stops_at_limit() {
    let chunk_len = 16 * 1024;
    let bounded_chunks = futures_util::stream::iter(
        (0..(TOKEN_RESPONSE_LIMIT / chunk_len))
            .map(move |_| Ok::<_, std::io::Error>(bytes::Bytes::from(vec![b'x'; chunk_len]))),
    );
    let over_limit = futures_util::stream::once(async { Ok(bytes::Bytes::from(vec![b'x'])) });
    let unfinished = futures_util::stream::pending::<Result<bytes::Bytes, std::io::Error>>();
    let body = reqwest::Body::wrap_stream(bounded_chunks.chain(over_limit).chain(unfinished));
    let response: reqwest::Response = http::Response::builder()
        .header(http::header::TRANSFER_ENCODING, "chunked")
        .body(body)
        .expect("chunked JWKS response")
        .into();
    assert_eq!(
        response.content_length(),
        None,
        "fixture must exercise a response without Content-Length"
    );
    let error = tokio::time::timeout(Duration::from_secs(2), bounded_jwks_response(response))
        .await
        .expect("JWKS reader must stop when the streaming limit is crossed")
        .expect_err("oversized chunked JWKS must be rejected");
    assert_eq!(error.code, "identity_keys_malformed");
}

struct FakeIdentityVerifier;

#[async_trait::async_trait]
impl OAuthIdentityVerifier for FakeIdentityVerifier {
    async fn verify(
        &self,
        id_token: &[u8],
        expected: OAuthIdentityExpectation<'_>,
    ) -> Result<OAuthIdentityV1, OAuthPublicError> {
        #[derive(Deserialize)]
        struct Claims {
            marker: String,
            issuer: String,
            audience: String,
            nonce: String,
            subject: String,
            display: String,
        }
        let claims: Claims = serde_json::from_slice(id_token)
            .map_err(|_| OAuthPublicError::new("id_token_malformed", false))?;
        if claims.marker != ID_SENTINEL
            || claims.issuer != expected.issuer
            || claims.audience != expected.audience
            || !constant_time_equal(claims.nonce.as_bytes(), expected.nonce)
        {
            return Err(OAuthPublicError::new("identity_claim_mismatch", false));
        }
        Ok(OAuthIdentityV1 {
            subject_hash: blake3::hash(claims.subject.as_bytes()).to_hex().to_string(),
            display_identity: claims.display,
        })
    }
}

struct CaptureSink {
    sent: mpsc::UnboundedSender<WireFrame>,
}

impl FrameSink for CaptureSink {
    fn try_send(&self, frame: WireFrame) -> Result<(), FrameSendError> {
        self.sent.send(frame).map_err(|_| FrameSendError)
    }
}

#[derive(Default)]
struct RefusingRequiredResponseSink {
    attempts: AtomicUsize,
    closed: AtomicBool,
}

impl FrameSink for RefusingRequiredResponseSink {
    fn try_send(&self, _frame: WireFrame) -> Result<(), FrameSendError> {
        self.attempts.fetch_add(1, Ordering::AcqRel);
        Err(FrameSendError)
    }

    fn close_after_required_delivery_failure(&self) {
        self.closed.store(true, Ordering::Release);
    }
}

/// MUTATION CHECK: restore the terminal OAuth response to a discarded
/// `let _ = route.sink.try_send(...)`. Expected failure: the flow is live and
/// the reply was refused, but `closed` remains false so the caller gets no
/// response or reconnect signal.
#[tokio::test]
async fn refused_terminal_oauth_response_closes_after_state_advances() {
    let server = FakeOAuthServer::start(FakeMode::Success, false).await;
    let coordinator = OAuthCoordinator::new(
        "terminal-response-instance".into(),
        OAuthProviderCatalog::with_test_registrations([
            server.registration(Arc::new(FakeIdentityVerifier))
        ])
        .expect("catalog"),
        OAuthCoordinatorConfig {
            max_flows: 1,
            max_invalid_callbacks: 1,
            flow_ttl: Duration::from_secs(5),
        },
    )
    .expect("coordinator");
    let sink = Arc::new(RefusingRequiredResponseSink::default());
    coordinator
        .try_start(
            "terminal-response-connection",
            "fake-oauth".into(),
            "terminal-response-alias".into(),
            "terminal-response-attempt".into(),
            OAuthRoute {
                request_id: RequestId::new("terminal-response-request"),
                sink: sink.clone(),
            },
        )
        .expect("start handoff");

    tokio::time::timeout(Duration::from_secs(2), async {
        while sink.attempts.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("terminal response attempt");
    assert_eq!(
        coordinator.flow_count(),
        1,
        "OAuth state advances before its terminal start response is admitted"
    );
    assert!(
        sink.closed.load(Ordering::Acquire),
        "refusal must close the transport so the caller reconnects and rereads state"
    );
    assert!(coordinator.shutdown().await);
}

async fn coordinator_for(
    server: &FakeOAuthServer,
    ttl: Duration,
) -> (OAuthCoordinator, mpsc::UnboundedReceiver<WireFrame>) {
    let registration = server.registration(Arc::new(FakeIdentityVerifier));
    coordinator_for_registration(registration, ttl).await
}

async fn coordinator_for_registration(
    registration: OAuthProviderRegistration,
    ttl: Duration,
) -> (OAuthCoordinator, mpsc::UnboundedReceiver<WireFrame>) {
    // Start under the registration's OWN provider id so per-provider start
    // behavior (the composed redirect shape) is exercised live, not skipped.
    let provider = registration.provider_id.clone();
    let catalog = OAuthProviderCatalog::with_test_registrations([registration]).expect("catalog");
    let coordinator = OAuthCoordinator::new(
        "daemon-instance-test".into(),
        catalog,
        OAuthCoordinatorConfig {
            max_flows: 8,
            max_invalid_callbacks: 4,
            flow_ttl: ttl,
        },
    )
    .expect("coordinator");
    let (sent, receiver) = mpsc::unbounded_channel();
    let route = OAuthRoute {
        request_id: RequestId::new("start-1"),
        sink: Arc::new(CaptureSink { sent }),
    };
    coordinator
        .try_start(
            "connection-1",
            provider,
            "work-oauth".into(),
            "attempt-1".into(),
            route,
        )
        .expect("start");
    (coordinator, receiver)
}

async fn started_flow(
    receiver: &mut mpsc::UnboundedReceiver<WireFrame>,
) -> (OAuthFlowId, String, u16) {
    let frame = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
        .await
        .expect("start timeout")
        .expect("start response");
    let WireFrame::Response {
        body:
            ResponseBody::AccountOAuthStart {
                flow_id: Some(flow_id),
                authorization_url: Some(url),
                loopback_port: Some(port),
                ..
            },
        ..
    } = frame
    else {
        panic!("unexpected start response: {frame:?}");
    };
    (flow_id, url.expose_authorization_url().to_owned(), port)
}

/// Await the device-flow start frame without ever parking the runtime on a
/// bare `recv`: the paused-clock runner test drives real loopback I/O through
/// [`drive_paused_io`], and an uninhibited park would auto-advance the frozen
/// clock into the production client's connect/request timers while the
/// device-authorization request is still on the wire.
async fn started_device_flow(
    receiver: &mut mpsc::UnboundedReceiver<WireFrame>,
) -> (OAuthFlowId, String) {
    let mut received = None;
    for _ in 0..2_000 {
        match receiver.try_recv() {
            Ok(frame) => {
                received = Some(frame);
                break;
            }
            Err(mpsc::error::TryRecvError::Empty) => drive_paused_io(1).await,
            Err(mpsc::error::TryRecvError::Disconnected) => {
                panic!("device start channel closed")
            }
        }
    }
    let frame = received.expect("device start response");
    let WireFrame::Response {
        body:
            ResponseBody::AccountOAuthStart {
                flow_id: Some(flow_id),
                authorization_url: Some(url),
                loopback_port: None,
                ..
            },
        ..
    } = frame
    else {
        panic!("unexpected device start response: {frame:?}");
    };
    (flow_id, url.expose_authorization_url().to_owned())
}

async fn wait_ready(coordinator: &OAuthCoordinator, flow_id: &OAuthFlowId) -> OAuthFlowStatusWire {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let status = coordinator
                .status("connection-1", flow_id, "attempt-1")
                .expect("owned flow");
            if !matches!(
                status,
                OAuthFlowStatusWire::WaitingBrowser
                    | OAuthFlowStatusWire::WaitingDevice
                    | OAuthFlowStatusWire::Exchanging
            ) {
                break status;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("flow terminal")
}

/// MUTATION CHECK: remove either sanctioned row, alter any owner constant, or
/// make catalog lookup accept an API-key/wildcard provider.
/// Verified by revert on 2026-07-29.
#[test]
fn sanctioned_oauth_table_has_exact_owner_grants_and_precise_reasons() {
    assert_eq!(SANCTIONED_PROVIDER_REGISTRATIONS.len(), 4);
    let openai = SANCTIONED_PROVIDER_REGISTRATIONS
        .iter()
        .find(|registration| registration.provider_id == "openai-oauth")
        .expect("OpenAI OAuth registration");
    assert_eq!(openai.issuer, "https://auth.openai.com");
    assert_eq!(
        openai.authorization_endpoint,
        "https://auth.openai.com/oauth/authorize"
    );
    assert_eq!(openai.token_endpoint, "https://auth.openai.com/oauth/token");
    assert_eq!(openai.client_id, "app_EMoamEEZ73f0CkXaXp7hrann");
    assert_eq!(
        openai.scopes,
        &["openid", "profile", "email", "offline_access"]
    );
    assert_eq!(
        openai.authorize_parameters,
        &[
            OAuthAuthorizeParameter {
                name: "id_token_add_organizations",
                value: "true",
            },
            OAuthAuthorizeParameter {
                name: "codex_cli_simplified_flow",
                value: "true",
            },
        ]
    );
    assert_eq!(
        openai.authorization_code_encoding,
        OAuthTokenRequestEncoding::Form
    );
    assert_eq!(openai.refresh_encoding, OAuthTokenRequestEncoding::Json);
    assert_eq!(
        openai.inference,
        OAuthInferenceRegistration {
            base_url: "https://chatgpt.com/backend-api/codex",
            auth_mode: OAuthInferenceAuthMode::Bearer,
            header_set: OAuthInferenceHeaderSet::OpenAiCodexResponsesLite,
        }
    );

    let anthropic = SANCTIONED_PROVIDER_REGISTRATIONS
        .iter()
        .find(|registration| registration.provider_id == "anthropic-oauth")
        .expect("Anthropic OAuth registration");
    assert_eq!(anthropic.issuer, "https://claude.ai");
    assert_eq!(
        anthropic.authorization_endpoint,
        "https://claude.ai/oauth/authorize"
    );
    assert_eq!(
        anthropic.token_endpoint,
        "https://console.anthropic.com/v1/oauth/token"
    );
    assert_eq!(anthropic.client_id, "9d1c250a-e61b-44d9-88ed-5944d1962f5e");
    // Claude Code 2.1.220 parity (W5g-7): the consent page derives its
    // permission items from these, same set, same order.
    assert_eq!(
        anthropic.scopes,
        &[
            "org:create_api_key",
            "user:profile",
            "user:inference",
            "user:sessions:claude_code",
            "user:mcp_servers",
            "user:file_upload",
        ]
    );
    assert_eq!(
        anthropic.authorization_code_encoding,
        OAuthTokenRequestEncoding::Json
    );
    assert!(anthropic.authorization_code_includes_state);
    assert_eq!(anthropic.refresh_encoding, OAuthTokenRequestEncoding::Json);
    assert_eq!(
        anthropic.inference,
        OAuthInferenceRegistration {
            base_url: "https://api.anthropic.com",
            auth_mode: OAuthInferenceAuthMode::Bearer,
            header_set: OAuthInferenceHeaderSet::AnthropicOAuthBeta,
        }
    );
    assert_eq!(
        haider_provider::ANTHROPIC_OAUTH_BETA_VALUE,
        "oauth-2025-04-20"
    );

    let kimi = SANCTIONED_PROVIDER_REGISTRATIONS
        .iter()
        .find(|registration| registration.provider_id == "kimi-oauth")
        .expect("Kimi OAuth registration");
    assert_eq!(kimi.issuer, "https://auth.kimi.com");
    assert_eq!(
        kimi.authorization_endpoint,
        "https://auth.kimi.com/api/oauth/device_authorization"
    );
    assert_eq!(kimi.token_endpoint, "https://auth.kimi.com/api/oauth/token");
    assert_eq!(kimi.client_id, "17e5f671-d194-4dfb-9706-5516cb48c098");
    assert!(kimi.scopes.is_empty());
    assert_eq!(kimi.flow_mode, OAuthFlowMode::DeviceCode);
    assert_eq!(kimi.auth_header_set, OAuthAuthHeaderSet::KimiMsh);
    assert_eq!(kimi.refresh_policy, OAuthRefreshPolicy::SerializedRotating);
    assert_eq!(kimi.refresh_encoding, OAuthTokenRequestEncoding::Form);
    assert!(!kimi.refresh_includes_binding);
    assert!(!kimi.retain_refresh_on_omission);
    assert_eq!(
        kimi.inference,
        OAuthInferenceRegistration {
            base_url: "https://api.kimi.com/coding/v1",
            auth_mode: OAuthInferenceAuthMode::Bearer,
            header_set: OAuthInferenceHeaderSet::KimiOpenAiChatCompletions,
        }
    );

    // MUTATION CHECK: any endpoint, client-id, scope, encoding, flow, or
    // proxy-origin drift must fail this release-owned Grok registration pin.
    let grok = SANCTIONED_PROVIDER_REGISTRATIONS
        .iter()
        .find(|registration| registration.provider_id == "grok-oauth")
        .expect("Grok OAuth registration");
    assert_eq!(grok.issuer, "https://auth.x.ai");
    assert_eq!(
        grok.authorization_endpoint,
        "https://auth.x.ai/oauth2/device/code"
    );
    assert_eq!(grok.token_endpoint, "https://auth.x.ai/oauth2/token");
    assert_eq!(grok.client_id, "b1a00492-073a-47ea-816f-4c329264a828");
    assert_eq!(
        grok.scopes,
        &[
            "openid",
            "profile",
            "email",
            "offline_access",
            "grok-cli:access",
            "api:access",
            "conversations:read",
            "conversations:write",
        ]
    );
    assert_eq!(grok.flow_mode, OAuthFlowMode::DeviceCode);
    assert_eq!(grok.auth_header_set, OAuthAuthHeaderSet::Standard);
    assert_eq!(grok.refresh_policy, OAuthRefreshPolicy::Conservative);
    assert_eq!(grok.refresh_encoding, OAuthTokenRequestEncoding::Form);
    assert!(!grok.refresh_includes_binding);
    assert!(grok.retain_refresh_on_omission);
    assert_eq!(
        grok.inference,
        OAuthInferenceRegistration {
            base_url: "https://cli-chat-proxy.grok.com/v1",
            auth_mode: OAuthInferenceAuthMode::Bearer,
            header_set: OAuthInferenceHeaderSet::GrokOpenAiChatCompletions,
        }
    );

    let catalog = OAuthProviderCatalog::default();
    for provider in [
        "openai-oauth",
        "anthropic-oauth",
        "kimi-oauth",
        "grok-oauth",
    ] {
        assert_eq!(
            catalog.availability(provider, true),
            OAuthAvailabilityWire {
                available: true,
                reason: None,
            }
        );
    }
    for provider in ["openai", "anthropic", "*", "other-oauth"] {
        let unavailable = catalog.availability(provider, true);
        assert!(!unavailable.available, "{provider} must stay unsanctioned");
        assert!(
            unavailable
                .reason
                .as_deref()
                .is_some_and(|reason| reason.starts_with("Unavailable:")),
            "{provider} needs a precise reason"
        );
    }
    assert_eq!(
        catalog.availability("openai-oauth", false),
        OAuthAvailabilityWire {
            available: false,
            reason: Some(
                "OAuth requires a supported OS credential vault; plaintext token files are not allowed"
                    .into()
            ),
        }
    );
}

/// MUTATION CHECK: make explicit 429/5xx responses terminal, retry an
/// ambiguous transport error, or remove the three-attempt bound. Expected
/// RUNTIME failure: the pure delay policy changes below.
#[test]
fn kimi_refresh_backoff_is_bounded_to_explicit_retryable_statuses() {
    let status = classify_token_error(503, b"bounded fixture body");
    assert_eq!(
        kimi_refresh_retry_delay(&status, 1),
        Some(Duration::from_millis(250))
    );
    assert_eq!(
        kimi_refresh_retry_delay(&status, 2),
        Some(Duration::from_millis(500))
    );
    assert_eq!(kimi_refresh_retry_delay(&status, 3), None);
    assert_eq!(
        kimi_refresh_retry_delay(
            &OAuthPublicError::new("token_endpoint_unavailable", true),
            1,
        ),
        None,
        "an ambiguous transport failure must not replay a rotating token"
    );
    assert_eq!(
        kimi_refresh_retry_delay(
            &classify_token_error(401, br#"{"error":"invalid_grant"}"#),
            1
        ),
        None
    );
}

/// MUTATION CHECK: force a fresh rotation before comparing the access token
/// that actually received the 401 with the under-vault re-read. Expected
/// RUNTIME failure: this unreachable token endpoint is contacted instead of
/// adopting the already-persisted access generation.
#[tokio::test]
async fn forced_401_reread_adopts_already_rotated_access_without_refresh_post() {
    const CURRENT_ACCESS: &[u8] = b"already-rotated-access-generation";

    let registration = OAuthProviderRegistration::new(
        "fake-oauth",
        "http://127.0.0.1:9",
        "http://127.0.0.1:9/device_authorization",
        "http://127.0.0.1:9/token",
        "haider-public-fake",
        ["openid".to_owned()],
        AUDIENCE,
        Some("fake-api-resource".into()),
        true,
        Arc::new(FakeIdentityVerifier),
    )
    .expect("offline registration")
    .with_test_device_flow();
    let descriptor = oauth_descriptor_for_test();
    let vault = Arc::new(MemoryVault::new());
    let bundle = OAuthTokenBundleV1::new(
        "fake-oauth".into(),
        "http://127.0.0.1:9".into(),
        AUDIENCE.into(),
        Some("fake-api-resource".into()),
        "Bearer".into(),
        Zeroizing::new(CURRENT_ACCESS.to_vec()),
        Some(Zeroizing::new(
            b"already-rotated-refresh-generation".to_vec(),
        )),
        now_ms().expect("clock").saturating_add(600_000),
        None,
        Vec::new(),
        OAuthIdentityV1 {
            subject_hash: "subject-hash".into(),
            display_identity: descriptor.identity.clone(),
        },
        2,
    )
    .expect("already-rotated bundle")
    .with_refresh_after(now_ms().expect("clock").saturating_add(300_000));
    vault
        .put(&descriptor.alias, &bundle.encode().expect("encode"))
        .expect("store already-rotated bundle");
    let snapshot = Arc::new(Mutex::new(vec![descriptor.clone()]));
    let broker = CredentialBroker::new(
        vault.clone() as Arc<dyn Vault>,
        OAuthProviderCatalog::with_test_registrations([registration]).expect("catalog"),
        Arc::clone(&snapshot),
        start_status_actor(&snapshot, vault),
    )
    .expect("broker");
    let failed = *blake3::hash(b"superseded-access-generation").as_bytes();
    let adopted = broker
        .refresh_after_auth_failure(&descriptor, Some(failed))
        .await
        .expect("adopt already-rotated access");
    assert_eq!(adopted.expose_secret(), CURRENT_ACCESS);
    assert!(broker.shutdown().await);
}

/// MUTATION CHECK: drop the `!rejection_marker_active` guard from the
/// forced-401 fingerprint-adoption fast path in `resolve_oauth` (or serve the
/// vault bundle directly whenever the failed fingerprint differs). Expected
/// RUNTIME failure: the marked bundle's still-unexpired access token is
/// returned instead of the typed re-login below. An ACTIVE rejection or
/// uncertainty marker means no successful rotation ever replaced this bundle
/// — a real rotation durably clears the marker under the alias lease — so
/// its access token belongs to the rejected/uncertain generation and must
/// only resolve through the serialized under-lease path.
#[tokio::test]
async fn forced_401_reread_never_adopts_an_actively_marked_rotating_bundle() {
    let registration = OAuthProviderRegistration::new(
        "fake-oauth",
        "http://127.0.0.1:9",
        "http://127.0.0.1:9/device_authorization",
        "http://127.0.0.1:9/token",
        "haider-public-fake",
        ["openid".to_owned()],
        AUDIENCE,
        Some("fake-api-resource".into()),
        true,
        Arc::new(FakeIdentityVerifier),
    )
    .expect("offline registration")
    .with_test_device_flow();
    let descriptor = oauth_descriptor_for_test();
    let vault = Arc::new(MemoryVault::new());
    let now = now_ms().expect("clock");
    let bundle = OAuthTokenBundleV1::new(
        "fake-oauth".into(),
        "http://127.0.0.1:9".into(),
        AUDIENCE.into(),
        Some("fake-api-resource".into()),
        "Bearer".into(),
        Zeroizing::new(b"tombstoned-generation-access".to_vec()),
        Some(Zeroizing::new(b"tombstoned-generation-refresh".to_vec())),
        now.saturating_add(600_000),
        None,
        Vec::new(),
        OAuthIdentityV1 {
            subject_hash: "subject-hash".into(),
            display_identity: descriptor.identity.clone(),
        },
        2,
    )
    .expect("marked bundle")
    // Refresh is NOT otherwise due: the active marker alone must force the
    // serialized under-lease path instead of any vault fast path.
    .with_refresh_after(now.saturating_add(300_000))
    .with_refresh_rejected_until(now.saturating_add(300_000));
    vault
        .put(&descriptor.alias, &bundle.encode().expect("encode"))
        .expect("store marked bundle");
    let snapshot = Arc::new(Mutex::new(vec![descriptor.clone()]));
    let broker = CredentialBroker::new(
        vault.clone() as Arc<dyn Vault>,
        OAuthProviderCatalog::with_test_registrations([registration]).expect("catalog"),
        Arc::clone(&snapshot),
        start_status_actor(&snapshot, vault.clone() as Arc<dyn Vault>),
    )
    .expect("broker");
    // The 401 belongs to an older access generation, so the fingerprint
    // DIFFERS from the marked bundle's access token — the exact shape the
    // adoption fast path would otherwise serve straight from the vault.
    let failed = *blake3::hash(b"superseded-access-generation").as_bytes();
    let error = broker
        .refresh_after_auth_failure(&descriptor, Some(failed))
        .await
        .expect_err("an actively marked bundle requires re-login, never direct adoption");
    assert_eq!(error.code, ErrorCode::Unauthorized);
    assert_eq!(
        error
            .details
            .as_ref()
            .and_then(|details| details.get("kind"))
            .and_then(serde_json::Value::as_str),
        Some("oauth_relogin_required")
    );
    // The under-lease path refuses without touching the marked bundle: the
    // tombstone and generation are exactly as seeded, and the unreachable
    // token endpoint proves no rotating request was attempted.
    let stored = vault
        .resolve(&descriptor.alias)
        .and_then(|stored| OAuthTokenBundleV1::decode(stored.expose_secret()))
        .expect("marked bundle remains durable");
    assert_eq!(stored.generation, 2);
    assert_eq!(
        stored.refresh_rejected_until_unix_ms,
        bundle.refresh_rejected_until_unix_ms
    );
    assert!(broker.shutdown().await);
}

/// C2 DECISION: this pinned law is EXTENDED, not superseded. “One-use” is
/// the import-only eager bootstrap marker, not a one-refresh lifetime budget.
/// Durable receipt provenance selects serialized rotation for this first
/// refresh and every later due/401 refresh, while the sanctioned OpenAI
/// registration remains Conservative so ordinary loopback-PKCE is unchanged.
///
/// MUTATION CHECK 1: remove the imported-Codex branch from `resolve_oauth`.
/// Expected runtime failure: the imported bundle returns its old access token
/// without the refresh call asserted below. MUTATION CHECK 2: carry the
/// import's one-use marker into `refreshed_bundle`. Expected runtime failure:
/// the second resolution refreshes again. MUTATION CHECK 3: treat one-use as
/// a lifetime refresh budget or drop receipt-scoped serialized refresh.
/// Expected RUNTIME failure: the later genuinely expired imported generation
/// does not perform the second POST. MUTATION CHECK 4: default ordinary
/// bundles to the import marker. Expected runtime failure: the opaque
/// loopback-PKCE bundle also refreshes instead of retaining the normal
/// 30-second broker skew.
#[tokio::test]
async fn codex_fallback_refresh_is_one_use_and_import_scoped_at_the_broker_call_site() {
    let server = FakeOAuthServer::start(FakeMode::Success, false).await;
    let mut registration = server.registration(Arc::new(FakeIdentityVerifier));
    registration.provider_id = haider_provider::OPENAI_OAUTH_PROVIDER_NAME.to_owned();
    assert_eq!(
        registration.refresh_policy,
        OAuthRefreshPolicy::Conservative,
        "Codex serialization must remain import-scoped, never provider-wide"
    );
    let catalog =
        OAuthProviderCatalog::with_test_registrations([registration.clone()]).expect("catalog");
    let before = now_ms().expect("clock");
    let bundle = codex_import_bundle(
        Path::new("/tmp/fake-codex-auth.json"),
        br#"{"tokens":{"access_token":"fake-access-token-1","refresh_token":"fake-refresh-token-1","account_id":"fake-account-id-1"}}"#,
        &registration,
        1,
    )
    .expect("Codex fallback bundle");
    assert!(
        bundle.expires_at_unix_ms > before.saturating_add(14 * 60 * 1000),
        "fallback remains approximately 15 minutes"
    );
    let imported_descriptor = CredentialDescriptor {
        alias: CredentialAlias::new("openai-oauth"),
        provider: haider_provider::OPENAI_OAUTH_PROVIDER_NAME.to_owned(),
        base_url: None,
        auth_method: AuthMethod::OAuth,
        identity: bundle.identity.display_identity.clone(),
        status: CredentialStatus::Ok,
        active: true,
        label: None,
        account_identity: None,
        created_at_ms: None,
    };
    let vault = Arc::new(MemoryVault::new());
    vault
        .put(
            &imported_descriptor.alias,
            &bundle.encode().expect("encode imported bundle"),
        )
        .expect("seed imported bundle");
    let imported_snapshot = Arc::new(Mutex::new(vec![imported_descriptor.clone()]));
    let imported_broker = CredentialBroker::new_with_fences(
        vault.clone(),
        catalog.clone(),
        Arc::clone(&imported_snapshot),
        start_status_actor_with_import_source(&imported_snapshot, vault.clone(), Some("codex")),
        RefreshFenceRegistry::default(),
    )
    .expect("imported broker");
    let refreshed = imported_broker
        .resolve(&imported_descriptor)
        .await
        .expect("resolve imported fallback");
    assert_eq!(refreshed.expose_secret(), b"ACCESS_ROTATED_SENTINEL_3a19");
    assert_eq!(server.state.refresh_calls.load(Ordering::SeqCst), 1);
    let retained_refresh = imported_broker
        .resolve(&imported_descriptor)
        .await
        .expect("resolve refreshed import");
    assert_eq!(
        retained_refresh.expose_secret(),
        b"ACCESS_ROTATED_SENTINEL_3a19"
    );
    assert_eq!(
        server.state.refresh_calls.load(Ordering::SeqCst),
        1,
        "the import marker must clear after the first durable refresh"
    );

    let stored = vault
        .resolve(&imported_descriptor.alias)
        .and_then(|stored| OAuthTokenBundleV1::decode(stored.expose_secret()))
        .expect("first refreshed import");
    assert!(!stored.refresh_on_first_use());
    let later_due = OAuthTokenBundleV1::new(
        stored.provider_id.clone(),
        stored.issuer.clone(),
        stored.audience.clone(),
        stored.resource.clone(),
        stored.token_type.clone(),
        Zeroizing::new(stored.access_token().to_vec()),
        stored
            .refresh_token()
            .map(|token| Zeroizing::new(token.to_vec())),
        now_ms().expect("clock").saturating_sub(1),
        stored.refresh_expires_at_unix_ms,
        stored.granted_scopes.clone(),
        stored.identity.clone(),
        stored.generation,
    )
    .expect("later-due import");
    vault
        .put(
            &imported_descriptor.alias,
            &later_due.encode().expect("encode later-due import"),
        )
        .expect("age refreshed import");
    let later_refreshed = imported_broker
        .resolve(&imported_descriptor)
        .await
        .expect("later imported lifecycle refresh");
    assert_eq!(
        later_refreshed.expose_secret(),
        b"ACCESS_ROTATED_SENTINEL_3a19"
    );
    assert_eq!(
        server.state.refresh_calls.load(Ordering::SeqCst),
        2,
        "consuming the eager marker must not disable later lifecycle refresh"
    );
    assert!(imported_broker.shutdown().await);

    let pkce_alias = CredentialAlias::new("openai-oauth-2");
    let pkce_descriptor = CredentialDescriptor {
        alias: pkce_alias.clone(),
        provider: haider_provider::OPENAI_OAUTH_PROVIDER_NAME.to_owned(),
        base_url: None,
        auth_method: AuthMethod::OAuth,
        identity: "fake-pkce-person@example.invalid".to_owned(),
        status: CredentialStatus::Ok,
        active: false,
        label: None,
        account_identity: None,
        created_at_ms: None,
    };
    let now = now_ms().expect("clock");
    let pkce_bundle = OAuthTokenBundleV1::new(
        haider_provider::OPENAI_OAUTH_PROVIDER_NAME.to_owned(),
        registration.issuer.clone(),
        registration.audience.clone(),
        registration.resource.clone(),
        "Bearer".to_owned(),
        Zeroizing::new(b"fake-pkce-access-token-1".to_vec()),
        Some(Zeroizing::new(b"fake-pkce-refresh-token-1".to_vec())),
        now.saturating_add(10 * 60 * 1000),
        None,
        registration.scopes.clone(),
        OAuthIdentityV1 {
            subject_hash: "fake-pkce-subject-hash".to_owned(),
            display_identity: pkce_descriptor.identity.clone(),
        },
        1,
    )
    .expect("PKCE bundle");
    vault
        .put(
            &pkce_alias,
            &pkce_bundle.encode().expect("encode PKCE bundle"),
        )
        .expect("seed PKCE bundle");
    let pkce_snapshot = Arc::new(Mutex::new(vec![pkce_descriptor.clone()]));
    let pkce_broker = CredentialBroker::new_with_fences(
        vault.clone(),
        catalog,
        Arc::clone(&pkce_snapshot),
        start_status_actor(&pkce_snapshot, vault),
        RefreshFenceRegistry::default(),
    )
    .expect("PKCE broker");
    let retained = pkce_broker
        .resolve(&pkce_descriptor)
        .await
        .expect("resolve PKCE bundle");
    assert_eq!(retained.expose_secret(), b"fake-pkce-access-token-1");
    assert_eq!(
        server.state.refresh_calls.load(Ordering::SeqCst),
        2,
        "opaque PKCE token must retain the normal refresh skew"
    );
    assert!(pkce_broker.shutdown().await);
}

/// MUTATION CHECK: skip unverified JWT payload parsing during Codex import.
/// Expected runtime failure: the fake access-token expiry, id-token email, or
/// nested ChatGPT account identity below falls back instead of being stored.
#[test]
fn codex_import_leniently_reads_fake_jwt_claims() {
    let registration = OAuthProviderCatalog::default()
        .registration(haider_provider::OPENAI_OAUTH_PROVIDER_NAME)
        .expect("OpenAI OAuth registration");
    let bundle = codex_import_bundle(
        Path::new("/tmp/fake-codex-auth.json"),
        br#"{"tokens":{"id_token":"fake-id-token-header.eyJlbWFpbCI6ImZha2UtcGVyc29uQGV4YW1wbGUuaW52YWxpZCIsImh0dHBzOi8vYXBpLm9wZW5haS5jb20vYXV0aCI6eyJjaGF0Z3B0X2FjY291bnRfaWQiOiJmYWtlLWFjY291bnQtaWQtMSJ9fQ.fake-id-token-signature","access_token":"fake-access-token-header.eyJleHAiOjQxMDI0NDQ4MDB9.fake-access-token-signature","refresh_token":"fake-refresh-token-1"}}"#,
        &registration,
        1,
    )
    .expect("Codex JWT-claim bundle");
    assert_eq!(bundle.expires_at_unix_ms, 4_102_444_800_000);
    assert_eq!(
        bundle.identity.display_identity,
        "fake-person@example.invalid"
    );
    assert_eq!(
        bundle.identity.subject_hash,
        blake3::hash(b"fake-account-id-1").to_hex().to_string()
    );
    let account_identity = bundle
        .account_identity
        .as_ref()
        .expect("provider-generic account identity");
    assert_eq!(
        account_identity.email.as_deref(),
        Some("fake-person@example.invalid")
    );
    assert_eq!(
        account_identity.account_id.as_deref(),
        Some("fake-account-id-1")
    );
    assert!(!account_identity.verified);
    assert!(bundle.id_token().is_some(), "ID token remains vault-only");
}

/// MUTATION CHECK: remove either untagged Grok CLI auth.json arm, lose the
/// refresh token, or trust a mismatched issuer. Each official shape below
/// must yield the same sanctioned provider identity without exposing bytes.
#[test]
fn grok_cli_import_accepts_bare_and_bundle_auth_json_shapes() {
    let registration = OAuthProviderCatalog::default()
        .registration(haider_provider::GROK_OAUTH_PROVIDER_NAME)
        .expect("Grok OAuth registration");
    let bare = grok_import_bundle(
        Path::new("/tmp/fake-grok-auth.json"),
        br#""fake-grok-bare-access""#,
        &registration,
        1,
    )
    .expect("bare Grok token");
    assert_eq!(bare.provider_id, "grok-oauth");
    assert_eq!(bare.access_token(), b"fake-grok-bare-access");
    assert!(bare.refresh_token().is_none());

    let object = grok_import_bundle(
        Path::new("/tmp/fake-grok-auth.json"),
        br#"{
            "access_token":"fake-grok-object-access",
            "refresh_token":"fake-grok-object-refresh",
            "expires_in":3600,
            "issuer":"https://auth.x.ai"
        }"#,
        &registration,
        2,
    )
    .expect("object Grok token bundle");
    assert_eq!(object.provider_id, "grok-oauth");
    assert_eq!(object.access_token(), b"fake-grok-object-access");
    assert_eq!(
        object.refresh_token(),
        Some(b"fake-grok-object-refresh".as_slice())
    );
    assert_eq!(object.generation, 2);
    assert_eq!(object.granted_scopes, registration.scopes);
}

/// MUTATION CHECK: give native-store bytes a separate parser or bypass the
/// shared resolver. Expected runtime failure: the two metadata records or
/// complete Anthropic bundles no longer agree byte-for-byte on token fields
/// and the exact source expiry.
#[test]
fn claude_file_and_native_secret_share_parser_and_fresh_bundle() {
    let fixture_dir = tempfile::tempdir().expect("Claude import fixture directory");
    let file_path = fixture_dir.path().join(".credentials.json");
    std::fs::write(&file_path, CLAUDE_SECURE_STORE_FIXTURE).expect("write Claude fixture");
    let absent_native = OAuthTestClaudeNative::unavailable();
    let file = load_claude_credential_input(
        &file_path,
        &absent_native,
        ClaudeNativeReadEvent::Significant,
    )
    .expect("file input");
    assert_eq!(absent_native.reads.load(Ordering::SeqCst), 1);

    let native = OAuthTestClaudeNative::new(CLAUDE_SECURE_STORE_FIXTURE);
    let missing_path = fixture_dir.path().join("missing-credentials.json");
    let secure =
        load_claude_credential_input(&missing_path, &native, ClaudeNativeReadEvent::Significant)
            .expect("secure-store input");
    assert_eq!(native.reads.load(Ordering::SeqCst), 1);

    let file_metadata =
        parse_claude_credential_metadata(&file.location, &file.bytes).expect("file metadata");
    let secure_metadata = parse_claude_credential_metadata(&secure.location, &secure.bytes)
        .expect("secure-store metadata");
    assert_eq!(file_metadata.expires_at_ms, 4_102_444_800_123);
    assert_eq!(secure_metadata.expires_at_ms, file_metadata.expires_at_ms);
    assert!(file_metadata.has_inference_scope && secure_metadata.has_inference_scope);

    let registration = OAuthProviderCatalog::default()
        .registration(haider_provider::ANTHROPIC_OAUTH_PROVIDER_NAME)
        .expect("Anthropic OAuth registration");
    let file_bundle =
        claude_import_bundle(&file.location, &file.bytes, &registration, 1).expect("file bundle");
    let secure_bundle = claude_import_bundle(&secure.location, &secure.bytes, &registration, 1)
        .expect("secure-store bundle");
    assert_eq!(file_bundle.provider_id, "anthropic-oauth");
    assert_eq!(secure_bundle.provider_id, file_bundle.provider_id);
    assert_eq!(
        secure_bundle.expires_at_unix_ms,
        file_bundle.expires_at_unix_ms
    );
    assert_eq!(secure_bundle.access_token(), file_bundle.access_token());
    assert_eq!(secure_bundle.refresh_token(), file_bundle.refresh_token());
    assert!(secure_bundle.expires_at_unix_ms > now_ms().expect("clock"));
}

/// MUTATION CHECK: force either body builder to ignore the registration's
/// encoding or state/binding flags. Exact content-type and field assertions
/// fail without opening a socket.
/// Verified by revert on 2026-07-29.
#[test]
fn declared_token_encodings_build_exact_provider_payloads() {
    let registration = || {
        OAuthProviderRegistration::new(
            "fake-oauth",
            "http://127.0.0.1:1",
            "http://127.0.0.1:1/authorize",
            "http://127.0.0.1:1/token",
            "fake-client",
            ["scope"].map(str::to_owned),
            "fake-audience",
            Some("fake-resource".into()),
            true,
            Arc::new(FakeIdentityVerifier),
        )
        .expect("registration")
    };

    let mut openai = registration();
    openai.authorization_code_encoding = OAuthTokenRequestEncoding::Form;
    openai.authorization_code_includes_state = false;
    openai.refresh_encoding = OAuthTokenRequestEncoding::Json;
    openai.refresh_includes_binding = false;
    let code = authorization_code_request_body(
        &openai,
        b"secret-code",
        b"secret-state",
        b"secret-verifier",
        "http://127.0.0.1:43210/callback",
    )
    .expect("OpenAI code body");
    assert_eq!(code.content_type(), "application/x-www-form-urlencoded");
    let code_fields = url::form_urlencoded::parse(code.as_ref()).collect::<HashMap<_, _>>();
    assert_eq!(
        code_fields.get("grant_type").map(|value| value.as_ref()),
        Some("authorization_code")
    );
    assert!(!code_fields.contains_key("state"));
    let refresh =
        refresh_token_request_body(&openai, b"secret-refresh").expect("OpenAI refresh body");
    assert_eq!(refresh.content_type(), "application/json");
    let refresh_fields: serde_json::Value =
        serde_json::from_slice(refresh.as_ref()).expect("OpenAI refresh JSON");
    assert_eq!(
        refresh_fields
            .get("grant_type")
            .and_then(serde_json::Value::as_str),
        Some("refresh_token")
    );
    assert!(
        !refresh_fields
            .as_object()
            .expect("refresh object")
            .contains_key("audience")
    );
    assert!(
        !refresh_fields
            .as_object()
            .expect("refresh object")
            .contains_key("resource")
    );

    let mut anthropic = registration();
    anthropic.authorization_code_encoding = OAuthTokenRequestEncoding::Json;
    anthropic.authorization_code_includes_state = true;
    anthropic.refresh_encoding = OAuthTokenRequestEncoding::Json;
    anthropic.refresh_includes_binding = false;
    let code = authorization_code_request_body(
        &anthropic,
        b"secret-code",
        b"secret-state",
        b"secret-verifier",
        "http://127.0.0.1:43210/callback",
    )
    .expect("Anthropic code body");
    assert_eq!(code.content_type(), "application/json");
    let code_fields: serde_json::Value =
        serde_json::from_slice(code.as_ref()).expect("Anthropic code JSON");
    assert_eq!(
        code_fields.get("state").and_then(serde_json::Value::as_str),
        Some("secret-state")
    );
    let refresh =
        refresh_token_request_body(&anthropic, b"secret-refresh").expect("Anthropic refresh body");
    assert_eq!(refresh.content_type(), "application/json");
}

/// MUTATION CHECK: swap either OpenAI token encoding, remove either extra
/// authorize parameter, or weaken S256. The named content-type/URL assertions
/// fail against the fake server.
/// Verified by revert on 2026-07-29.
#[tokio::test]
async fn openai_protocol_uses_extra_authorize_params_form_code_and_json_refresh() {
    let server = FakeOAuthServer::start(FakeMode::Success, false).await;
    server
        .state
        .expect_refresh_binding
        .store(false, Ordering::SeqCst);
    let mut registration = server.registration(Arc::new(FakeIdentityVerifier));
    registration.authorize_parameters = vec![
        ("id_token_add_organizations".into(), "true".into()),
        ("codex_cli_simplified_flow".into(), "true".into()),
    ];
    registration.authorization_code_encoding = OAuthTokenRequestEncoding::Form;
    registration.refresh_encoding = OAuthTokenRequestEncoding::Json;
    registration.refresh_includes_binding = false;
    let refresh_registration = registration.clone();
    let (coordinator, mut receiver) =
        coordinator_for_registration(registration, Duration::from_secs(5)).await;
    let (flow_id, authorization_url, _) = started_flow(&mut receiver).await;
    let parsed = Url::parse(&authorization_url).expect("authorization URL");
    let parameters = parsed.query_pairs().collect::<HashMap<_, _>>();
    assert_eq!(
        parameters
            .get("id_token_add_organizations")
            .map(|v| v.as_ref()),
        Some("true")
    );
    assert_eq!(
        parameters
            .get("codex_cli_simplified_flow")
            .map(|v| v.as_ref()),
        Some("true")
    );
    assert_eq!(
        parameters.get("code_challenge_method").map(|v| v.as_ref()),
        Some("S256")
    );
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("browser")
        .get(authorization_url)
        .send()
        .await
        .expect("browser flow");
    assert!(matches!(
        wait_ready(&coordinator, &flow_id).await,
        OAuthFlowStatusWire::Ready { .. }
    ));

    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("refresh client");
    exchange_refresh_token(
        &client,
        &refresh_registration,
        REFRESH_SENTINEL.as_bytes(),
        None,
    )
    .await
    .expect("JSON refresh");
    assert_eq!(
        *server
            .state
            .token_encodings
            .lock()
            .expect("token encoding lock"),
        vec![
            (
                "authorization_code".into(),
                "application/x-www-form-urlencoded".into()
            ),
            ("refresh_token".into(), "application/json".into()),
        ]
    );
}

/// MUTATION CHECK: drop Anthropic's code-exchange `state` field or change
/// either declared JSON encoding. The fake endpoint rejects the request and
/// this named test fails by assertion.
/// Verified by revert on 2026-07-29.
#[tokio::test]
async fn anthropic_protocol_uses_json_code_with_state_and_json_refresh() {
    let server = FakeOAuthServer::start(FakeMode::Success, false).await;
    server
        .state
        .expect_refresh_binding
        .store(false, Ordering::SeqCst);
    server.state.expect_code_state.store(true, Ordering::SeqCst);
    let mut registration = server.registration(Arc::new(FakeIdentityVerifier));
    registration.authorization_code_encoding = OAuthTokenRequestEncoding::Json;
    registration.authorization_code_includes_state = true;
    registration.refresh_encoding = OAuthTokenRequestEncoding::Json;
    registration.refresh_includes_binding = false;
    let refresh_registration = registration.clone();
    let (coordinator, mut receiver) =
        coordinator_for_registration(registration, Duration::from_secs(5)).await;
    let (flow_id, authorization_url, _) = started_flow(&mut receiver).await;
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("browser")
        .get(authorization_url)
        .send()
        .await
        .expect("browser flow");
    assert!(matches!(
        wait_ready(&coordinator, &flow_id).await,
        OAuthFlowStatusWire::Ready { .. }
    ));

    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("refresh client");
    exchange_refresh_token(
        &client,
        &refresh_registration,
        REFRESH_SENTINEL.as_bytes(),
        None,
    )
    .await
    .expect("JSON refresh");
    assert_eq!(
        *server
            .state
            .token_encodings
            .lock()
            .expect("token encoding lock"),
        vec![
            ("authorization_code".into(), "application/json".into()),
            ("refresh_token".into(), "application/json".into()),
        ]
    );
}

/// The accepted-with-correct-state law, live-shaped (owner bug v0.0.65).
///
/// 91f8156 moved Anthropic's registered redirect to Claude Code parity
/// (`http://localhost:<port>/callback`) but left the listener's Host law at
/// `127.0.0.1:<port>`, so every REAL browser callback — which sends the
/// authority it navigated to, `Host: localhost:<port>` — was served the 400
/// rejection page. This drives the full flow exactly like that browser: an
/// Anthropic-shaped registration, the fake server's redirect followed by
/// reqwest to `http://localhost:<port>/callback`, correct state and code.
/// It MUST be accepted and reach Ready.
///
/// MUTATION CHECK: collapse `compose_redirect`'s authority to
/// `127.0.0.1:<port>` for every provider (the pre-fix listener law) while
/// keeping the localhost redirect. Expected RUNTIME failure: the browser
/// response is the 400 rejection page instead of `SUCCESS_HTML` and the flow
/// never reaches Ready. Verified by revert on 2026-08-04.
#[tokio::test]
async fn anthropic_localhost_browser_callback_is_accepted_with_correct_state() {
    let server = FakeOAuthServer::start(FakeMode::Success, false).await;
    server
        .state
        .expect_refresh_binding
        .store(false, Ordering::SeqCst);
    server.state.expect_code_state.store(true, Ordering::SeqCst);
    let mut registration = server.registration(Arc::new(FakeIdentityVerifier));
    registration.provider_id = haider_provider::ANTHROPIC_OAUTH_PROVIDER_NAME.to_owned();
    registration.authorization_code_encoding = OAuthTokenRequestEncoding::Json;
    registration.authorization_code_includes_state = true;
    registration.refresh_encoding = OAuthTokenRequestEncoding::Json;
    registration.refresh_includes_binding = false;
    let (coordinator, mut receiver) =
        coordinator_for_registration(registration, Duration::from_secs(5)).await;
    let (flow_id, authorization_url, port) = started_flow(&mut receiver).await;
    // The authorize redirect the provider will replay must be the parity
    // shape — the browser lands on localhost, never the numeric authority.
    let authorize = reqwest::Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        .build()
        .expect("authorize probe")
        .get(authorization_url.clone())
        .send()
        .await
        .expect("authorize");
    let location = authorize
        .headers()
        .get(reqwest::header::LOCATION)
        .expect("callback location")
        .to_str()
        .expect("callback text")
        .to_owned();
    assert!(
        location.starts_with(&format!("http://localhost:{port}/callback?")),
        "redirect must be Claude Code parity shaped: {location}"
    );
    // A real browser navigates to that exact URL and therefore sends
    // `Host: localhost:<port>`. reqwest does the same when following it.
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("browser")
        .get(&location)
        .send()
        .await
        .expect("browser callback");
    let status = response.status();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .expect("content type")
        .to_str()
        .expect("content type text")
        .to_owned();
    let body = response.text().await.expect("callback html");
    assert_eq!(
        content_type, "text/html; charset=utf-8",
        "the branded page must declare its type and charset"
    );
    assert_eq!(
        (status.as_u16(), body.as_str()),
        (200, SUCCESS_HTML),
        "the correct-state localhost callback must be accepted"
    );
    assert!(matches!(
        wait_ready(&coordinator, &flow_id).await,
        OAuthFlowStatusWire::Ready { .. }
    ));
    assert_eq!(server.state.token_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn fake_browser_success_proves_s256_exact_redirect_and_one_exchange() {
    let server = FakeOAuthServer::start(FakeMode::Success, false).await;
    let (coordinator, mut receiver) = coordinator_for(&server, Duration::from_secs(5)).await;
    let (flow_id, authorization_url, port) = started_flow(&mut receiver).await;
    assert!(port > 0);
    assert!(!authorization_url.contains("localhost"));
    let browser = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("browser");
    let response = browser
        .get(authorization_url)
        .send()
        .await
        .expect("browser flow");
    assert!(response.status().is_success());
    assert_eq!(response.text().await.expect("html"), SUCCESS_HTML);
    let status = wait_ready(&coordinator, &flow_id).await;
    let OAuthFlowStatusWire::Ready {
        oauth_reference, ..
    } = status
    else {
        panic!("not ready: {status:?}");
    };
    let claim = coordinator
        .claim_ready(
            "connection-1",
            &flow_id,
            "attempt-1",
            "fake-oauth",
            "work-oauth",
            &oauth_reference,
        )
        .expect("claim");
    assert_eq!(claim.bundle.access_token(), ACCESS_SENTINEL.as_bytes());
    assert_eq!(
        claim.bundle.refresh_token(),
        Some(REFRESH_SENTINEL.as_bytes())
    );
    assert_eq!(server.state.token_calls.load(Ordering::SeqCst), 1);
    assert!(!server.state.saw_client_secret.load(Ordering::SeqCst));
}

#[tokio::test]
async fn early_malicious_connection_does_not_consume_valid_callback() {
    let server = FakeOAuthServer::start(FakeMode::Success, false).await;
    let (coordinator, mut receiver) = coordinator_for(&server, Duration::from_secs(5)).await;
    let (flow_id, authorization_url, port) = started_flow(&mut receiver).await;
    let mut attacker = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .expect("attacker");
    attacker
        .write_all(b"GET /wrong HTTP/1.1\r\nHost: 127.0.0.1:1\r\n\r\n")
        .await
        .expect("attack");
    let mut rejected = Vec::new();
    attacker.read_to_end(&mut rejected).await.expect("reject");
    assert!(String::from_utf8_lossy(&rejected).contains("400"));
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("browser")
        .get(authorization_url)
        .send()
        .await
        .expect("browser");
    assert!(matches!(
        wait_ready(&coordinator, &flow_id).await,
        OAuthFlowStatusWire::Ready { .. }
    ));
}

#[test]
fn wrong_missing_duplicate_state_path_host_port_and_non_get_are_rejected() {
    let state = b"STATE_SENTINEL_317b";
    let path = "/oauth/callback/random";
    let authority = "127.0.0.1:43210";
    let valid = |target: &str, host: &str, method: &str| {
        format!("{method} {target} HTTP/1.1\r\nHost: {host}\r\n\r\n")
    };
    assert!(matches!(
        parse_callback(
            valid(
                &format!("{path}?code=x&state=STATE_SENTINEL_317b"),
                "127.0.0.1:43210",
                "GET"
            )
            .as_bytes(),
            path,
            authority,
            state
        ),
        CallbackResult::Code(_)
    ));
    for request in [
        valid(&format!("{path}?code=x"), "127.0.0.1:43210", "GET"),
        valid(
            &format!("{path}?code=x&state=wrong"),
            "127.0.0.1:43210",
            "GET",
        ),
        valid(
            &format!("{path}?code=x&state=STATE_SENTINEL_317b&state=STATE_SENTINEL_317b"),
            "127.0.0.1:43210",
            "GET",
        ),
        valid(
            "/wrong?code=x&state=STATE_SENTINEL_317b",
            "127.0.0.1:43210",
            "GET",
        ),
        // A hardened numeric-loopback flow never registered `localhost`, so
        // that authority stays foreign HERE — while the Anthropic parity
        // flow accepts exactly `localhost:<port>` (see the localhost
        // authority law test below).
        valid(
            &format!("{path}?code=x&state=STATE_SENTINEL_317b"),
            "localhost:43210",
            "GET",
        ),
        valid(
            &format!("{path}?code=x&state=STATE_SENTINEL_317b"),
            "127.0.0.1:43211",
            "GET",
        ),
        valid(
            &format!("{path}?code=x&state=STATE_SENTINEL_317b"),
            "127.0.0.1:43210",
            "POST",
        ),
        format!(
            "GET {path}?code=x&state=STATE_SENTINEL_317b HTTP/1.1\r\nHost: 127.0.0.1:43210\r\nContent-Length: 1\r\n\r\n"
        ),
        format!(
            "GET {path}?code=x&state=STATE_SENTINEL_317b HTTP/1.1\r\nHost: 127.0.0.1:43210\r\nTransfer-Encoding: chunked\r\n\r\n"
        ),
    ] {
        assert!(matches!(
            parse_callback(request.as_bytes(), path, authority, state),
            CallbackResult::Invalid(_)
        ));
    }
}

/// The listener validates the SAME authority the flow registered with the
/// provider. For the Anthropic Claude Code parity shape that authority is
/// `localhost:<port>` — a browser landing on the registered redirect MUST
/// be accepted with the correct state, and the numeric authority (which no
/// browser following that redirect ever sends) stays foreign.
#[test]
fn anthropic_localhost_authority_accepts_correct_state_and_numeric_stays_foreign() {
    let state = b"STATE_SENTINEL_317b";
    let (path, uri, authority) = compose_redirect(
        haider_provider::ANTHROPIC_OAUTH_PROVIDER_NAME,
        43210,
        "SEGMENT",
    );
    assert_eq!(uri, format!("http://{authority}{path}"));
    let request = |host: &str| {
        format!("GET {path}?code=x&state=STATE_SENTINEL_317b HTTP/1.1\r\nHost: {host}\r\n\r\n")
    };
    assert!(matches!(
        parse_callback(
            request("localhost:43210").as_bytes(),
            &path,
            &authority,
            state
        ),
        CallbackResult::Code(_)
    ));
    for foreign in ["127.0.0.1:43210", "localhost:43211", "[::1]:43210"] {
        assert!(matches!(
            parse_callback(request(foreign).as_bytes(), &path, &authority, state),
            CallbackResult::Invalid(CallbackRejection::WrongAddress)
        ));
    }
    assert!(matches!(
        parse_callback(
            format!("GET {path}?code=x&state=wrong HTTP/1.1\r\nHost: localhost:43210\r\n\r\n")
                .as_bytes(),
            &path,
            &authority,
            state
        ),
        CallbackResult::Invalid(CallbackRejection::WrongAttempt)
    ));
}

#[test]
fn duplicate_code_error_and_fragment_are_rejected_but_valid_denial_is_terminal() {
    let path = "/oauth/callback/random";
    let request =
        |query: &str| format!("GET {path}?{query} HTTP/1.1\r\nHost: 127.0.0.1:43210\r\n\r\n");
    for query in [
        "code=a&code=b&state=s",
        "code=a&error=access_denied&state=s",
        "error=access_denied&error=server_error&state=s",
        "code=a&state=s#fragment",
        "error=&state=s",
        "error=private_provider_error&state=s",
    ] {
        assert!(matches!(
            parse_callback(request(query).as_bytes(), path, "127.0.0.1:43210", b"s"),
            CallbackResult::Invalid(_)
        ));
    }
    assert!(matches!(
        parse_callback(
            request("error=access_denied&state=s").as_bytes(),
            path,
            "127.0.0.1:43210",
            b"s"
        ),
        CallbackResult::Denied("access_denied")
    ));
    for error in [
        "invalid_request",
        "unauthorized_client",
        "unsupported_response_type",
        "invalid_scope",
        "server_error",
        "temporarily_unavailable",
    ] {
        assert!(matches!(
            parse_callback(
                request(&format!("error={error}&state=s")).as_bytes(),
                path,
                "127.0.0.1:43210",
                b"s"
            ),
            CallbackResult::Denied("authorization_denied")
        ));
    }
}

#[tokio::test]
async fn callback_timeout_cancel_and_disconnect_cleanup_require_fresh_flow() {
    let server = FakeOAuthServer::start(FakeMode::Success, false).await;
    let (coordinator, mut receiver) = coordinator_for(&server, Duration::from_millis(30)).await;
    let (flow_id, _, _) = started_flow(&mut receiver).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(matches!(
        coordinator.status("connection-1", &flow_id, "attempt-1"),
        Some(OAuthFlowStatusWire::Expired)
    ));

    let (coordinator, mut receiver) = coordinator_for(&server, Duration::from_secs(5)).await;
    let (flow_id, _, _) = started_flow(&mut receiver).await;
    assert!(matches!(
        coordinator.cancel("connection-1", &flow_id, "attempt-1"),
        Some(OAuthFlowStatusWire::Cancelled)
    ));
    coordinator.cancel_connection("connection-1");
    assert_eq!(coordinator.flow_count(), 0);
    assert!(
        coordinator
            .status("connection-1", &flow_id, "attempt-1")
            .is_none()
    );
}

#[tokio::test]
async fn disconnect_before_start_worker_runs_cannot_create_orphan_flows() {
    let server = FakeOAuthServer::start(FakeMode::Success, false).await;
    let registration = server.registration(Arc::new(FakeIdentityVerifier));
    let coordinator = OAuthCoordinator::new(
        "instance".into(),
        OAuthProviderCatalog::with_test_registrations([registration]).expect("catalog"),
        OAuthCoordinatorConfig {
            max_flows: 8,
            ..OAuthCoordinatorConfig::default()
        },
    )
    .expect("coordinator");
    for index in 0..8 {
        let (sent, _receiver) = mpsc::unbounded_channel();
        coordinator
            .try_start(
                "disconnect-before-worker",
                "fake-oauth".into(),
                format!("work-{index}"),
                format!("attempt-{index}"),
                OAuthRoute {
                    request_id: RequestId::new(format!("start-{index}")),
                    sink: Arc::new(CaptureSink { sent }),
                },
            )
            .expect("queued start");
    }
    // No await occurred above, so the spawned start worker has not had an
    // opportunity to insert a flow before cleanup records the disconnect.
    coordinator.cancel_connection("disconnect-before-worker");
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(coordinator.flow_count(), 0);
}

#[tokio::test]
async fn terminal_flows_are_reclaimed_when_bounded_capacity_is_needed() {
    let server = FakeOAuthServer::start(FakeMode::Success, false).await;
    let coordinator = OAuthCoordinator::new(
        "instance".into(),
        OAuthProviderCatalog::with_test_registrations([
            server.registration(Arc::new(FakeIdentityVerifier))
        ])
        .expect("catalog"),
        OAuthCoordinatorConfig {
            max_flows: 2,
            max_invalid_callbacks: 2,
            flow_ttl: Duration::from_secs(5),
        },
    )
    .expect("coordinator");
    for index in 0..6 {
        let (sent, mut receiver) = mpsc::unbounded_channel();
        let connection = format!("connection-{index}");
        let attempt = format!("attempt-{index}");
        coordinator
            .try_start(
                &connection,
                "fake-oauth".into(),
                format!("work-{index}"),
                attempt.clone(),
                OAuthRoute {
                    request_id: RequestId::new(format!("start-{index}")),
                    sink: Arc::new(CaptureSink { sent }),
                },
            )
            .expect("start handoff");
        let (flow_id, _, _) = started_flow(&mut receiver).await;
        assert_eq!(
            coordinator.cancel(&connection, &flow_id, &attempt),
            Some(OAuthFlowStatusWire::Cancelled)
        );
    }
    assert!(coordinator.flow_count() <= 2);
}

#[tokio::test]
async fn shutdown_and_cancel_interrupt_a_slow_accepted_callback_read() {
    let server = FakeOAuthServer::start(FakeMode::Success, false).await;
    let (coordinator, mut receiver) = coordinator_for(&server, Duration::from_secs(5)).await;
    let (flow_id, _, port) = started_flow(&mut receiver).await;
    let mut slow = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .expect("slow callback");
    slow.write_all(b"GET /incomplete HTTP/1.1\r\n")
        .await
        .expect("partial callback");
    assert_eq!(
        coordinator.cancel("connection-1", &flow_id, "attempt-1"),
        Some(OAuthFlowStatusWire::Cancelled)
    );
    let mut response = Vec::new();
    let read_result =
        tokio::time::timeout(Duration::from_millis(250), slow.read_to_end(&mut response))
            .await
            .expect("cancel interrupts read");
    assert!(
        read_result.is_ok()
            || read_result
                .as_ref()
                .is_err_and(|error| error.kind() == std::io::ErrorKind::ConnectionReset),
        "cancel must close the slow callback connection: {read_result:?}"
    );

    let (coordinator, mut receiver) = coordinator_for(&server, Duration::from_secs(5)).await;
    let (_, _, port) = started_flow(&mut receiver).await;
    let mut slow = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .expect("slow shutdown callback");
    slow.write_all(b"GET /incomplete HTTP/1.1\r\n")
        .await
        .expect("partial shutdown callback");
    assert!(
        coordinator.shutdown().await,
        "shutdown joins the callback listener owner"
    );
    let mut response = Vec::new();
    let read_result =
        tokio::time::timeout(Duration::from_millis(250), slow.read_to_end(&mut response))
            .await
            .expect("shutdown interrupts read");
    assert!(
        read_result.is_ok()
            || read_result
                .as_ref()
                .is_err_and(|error| error.kind() == std::io::ErrorKind::ConnectionReset),
        "shutdown must close the slow callback connection: {read_result:?}"
    );

    // Unix listener drop has a synchronous connect-refusal observation. Keep
    // that extra platform oracle; Windows' pending AcceptEx state makes
    // refusal timing a kernel artifact even after the sole owner task joined.
    #[cfg(unix)]
    tokio::time::timeout(Duration::from_millis(250), async {
        loop {
            if TcpStream::connect((Ipv4Addr::LOCALHOST, port))
                .await
                .is_err()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("shutdown closes listener");
}

#[tokio::test]
async fn cancel_during_token_exchange_cannot_publish_a_late_ready_bundle() {
    let server = FakeOAuthServer::start(FakeMode::SlowExchange, false).await;
    let (coordinator, mut receiver) = coordinator_for(&server, Duration::from_secs(5)).await;
    let (flow_id, authorization_url, _) = started_flow(&mut receiver).await;
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("browser")
        .get(authorization_url)
        .send()
        .await
        .expect("browser");
    tokio::time::timeout(Duration::from_secs(2), async {
        while server.state.token_calls.load(Ordering::SeqCst) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("exchange begins");
    assert_eq!(
        coordinator.cancel("connection-1", &flow_id, "attempt-1"),
        Some(OAuthFlowStatusWire::Cancelled)
    );
    server.release_refresh();
    tokio::task::yield_now().await;
    assert_eq!(
        coordinator.status("connection-1", &flow_id, "attempt-1"),
        Some(OAuthFlowStatusWire::Cancelled)
    );
}

#[tokio::test]
async fn state_valid_denial_redirect_malformed_oversized_and_verifier_mismatch_are_public() {
    for (mode, expected) in [
        (FakeMode::Denied, "access_denied"),
        (FakeMode::TokenRedirect, "token_redirect_rejected"),
        (FakeMode::Malformed, "malformed_token_response"),
        (FakeMode::Oversized, "token_response_oversized"),
        (FakeMode::VerifierMismatch, "invalid_grant"),
        (FakeMode::ScopeMismatch, "scope_mismatch"),
    ] {
        let server = FakeOAuthServer::start(mode, false).await;
        let (coordinator, mut receiver) = coordinator_for(&server, Duration::from_secs(5)).await;
        let (flow_id, url, _) = started_flow(&mut receiver).await;
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("browser")
            .get(url)
            .send()
            .await
            .expect("browser");
        assert_eq!(
            wait_ready(&coordinator, &flow_id).await,
            OAuthFlowStatusWire::Failed {
                public_code: expected.into()
            }
        );
        assert_eq!(server.state.redirect_target_calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn independent_flows_never_reuse_state_nonce_callback_path_or_pkce_verifier() {
    let server = FakeOAuthServer::start(FakeMode::Success, false).await;
    let registration = server.registration(Arc::new(FakeIdentityVerifier));
    let catalog = OAuthProviderCatalog::with_test_registrations([registration]).expect("catalog");
    let coordinator = OAuthCoordinator::new(
        "instance".into(),
        catalog,
        OAuthCoordinatorConfig::default(),
    )
    .expect("coordinator");
    let mut urls = Vec::new();
    let mut flows = Vec::new();
    for index in 0..2 {
        let (sent, mut receiver) = mpsc::unbounded_channel();
        coordinator
            .try_start(
                &format!("connection-{index}"),
                "fake-oauth".into(),
                format!("work-{index}"),
                format!("attempt-{index}"),
                OAuthRoute {
                    request_id: RequestId::new(format!("start-{index}")),
                    sink: Arc::new(CaptureSink { sent }),
                },
            )
            .expect("start");
        let (flow, url, _) = started_flow(&mut receiver).await;
        flows.push((flow, index));
        urls.push(url);
    }
    let random_coordinates = urls
        .iter()
        .map(|url| {
            let url = Url::parse(url).expect("authorization URL");
            let query = url.query_pairs().collect::<HashMap<_, _>>();
            let state = query.get("state").expect("state").to_string();
            let nonce = query.get("nonce").expect("nonce").to_string();
            let redirect =
                Url::parse(query.get("redirect_uri").expect("redirect")).expect("redirect URL");
            let callback_path = redirect.path().to_owned();
            assert_ne!(state, nonce, "state and nonce are independent");
            assert!(state.len() >= 43);
            assert!(nonce.len() >= 43);
            assert!(
                callback_path
                    .rsplit('/')
                    .next()
                    .is_some_and(|segment| segment.len() >= 43)
            );
            (state, nonce, callback_path)
        })
        .collect::<Vec<_>>();
    assert_ne!(random_coordinates[0].0, random_coordinates[1].0);
    assert_ne!(random_coordinates[0].1, random_coordinates[1].1);
    assert_ne!(random_coordinates[0].2, random_coordinates[1].2);
    let browser = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("browser");
    for url in urls {
        browser.get(url).send().await.expect("flow");
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while server.state.verifiers.lock().expect("lock").len() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("two exchanges");
    let verifiers = server.state.verifiers.lock().expect("lock");
    assert_ne!(verifiers[0], verifiers[1]);
    assert!(verifiers.iter().all(|verifier| verifier.len() >= 43));
    for (index, verifier) in verifiers.iter().enumerate() {
        let (state, nonce, callback_path) = &random_coordinates[index];
        let callback_segment = callback_path.rsplit('/').next().expect("callback segment");
        assert_ne!(verifier, state, "PKCE verifier must not reuse state");
        assert_ne!(verifier, nonce, "PKCE verifier must not reuse nonce");
        assert_ne!(
            verifier, callback_segment,
            "PKCE verifier must not reuse callback path randomness"
        );
    }
    drop(flows);
}

#[test]
fn callback_success_page_and_provider_config_have_no_secret_material() {
    let rejections = [
        CallbackRejection::MalformedRequest,
        CallbackRejection::WrongAddress,
        CallbackRejection::WrongAttempt,
        CallbackRejection::UnrecognizedProviderError,
    ]
    .map(rejection_html);
    let mut pages = vec![SUCCESS_HTML.to_owned(), DENIED_HTML.to_owned()];
    pages.extend(rejections);
    for html in pages {
        for sentinel in [
            CODE_SENTINEL,
            "STATE_SENTINEL",
            "VERIFIER_SENTINEL",
            ACCESS_SENTINEL,
            REFRESH_SENTINEL,
            ID_SENTINEL,
            RAW_ERROR_SENTINEL,
            RAW_BODY_SENTINEL,
        ] {
            assert!(!html.contains(sentinel));
        }
    }
}

/// The branded callback pages are FULLY self-contained: the Haider wordmark
/// is text, the styling is one inline sheet, and no page may ever reference
/// an external stylesheet, font, image, or script — a loopback page that
/// phones anywhere defeats the point of a loopback page. Also pins the
/// no-JS posture: the pages carry no script at all.
///
/// MUTATION CHECK: brand via `<img src=...>`, an external font `@import`,
/// or a `<script>` — any of the substring bans below fails.
#[test]
fn callback_pages_are_branded_and_fully_self_contained() {
    let rejections = [
        CallbackRejection::MalformedRequest,
        CallbackRejection::WrongAddress,
        CallbackRejection::WrongAttempt,
        CallbackRejection::UnrecognizedProviderError,
    ]
    .map(rejection_html);
    let mut pages = vec![SUCCESS_HTML.to_owned(), DENIED_HTML.to_owned()];
    pages.extend(rejections);
    for html in &pages {
        assert!(
            html.contains(r#"<p class=wordmark>Haider</p>"#),
            "the wordmark is text, present on every page: {html}"
        );
        assert!(
            html.contains("#c9a35c"),
            "the gold accent is part of the shared sheet: {html}"
        );
        assert!(
            html.contains("<style>") && html.contains("color-scheme:dark"),
            "styling is one inline dark sheet: {html}"
        );
        for banned in [
            "http://", "https://", "src=", "@import", "url(", "<script", "<link", "onload",
        ] {
            assert!(
                !html.contains(banned),
                "callback pages must be self-contained (found `{banned}`): {html}"
            );
        }
        assert!(
            html.contains("<meta name=referrer content=no-referrer>"),
            "referrer suppression stays on every page: {html}"
        );
    }
    assert!(
        SUCCESS_HTML.contains("Authorization received")
            && SUCCESS_HTML.contains("You can close this tab and return to Haider."),
        "the success copy is the owner's line: {SUCCESS_HTML}"
    );
    assert!(
        DENIED_HTML.contains("Authorization was not granted"),
        "the cancellation page keeps its meaning: {DENIED_HTML}"
    );
}

/// The rejection page must say WHY and how to retry — never the bare
/// pre-fix "This callback was rejected." with no explanation (owner
/// screenshot, v0.0.65).
#[test]
fn rejection_pages_state_a_reason_and_retry_guidance() {
    for reason in [
        CallbackRejection::MalformedRequest,
        CallbackRejection::WrongAddress,
        CallbackRejection::WrongAttempt,
        CallbackRejection::UnrecognizedProviderError,
    ] {
        let page = rejection_html(reason);
        assert!(
            page.contains(&format!("rejected: {}.", reason.why())),
            "page must state the reason: {page}"
        );
        assert!(
            page.contains("To retry, return to Haider and start the sign-in again"),
            "page must say how to retry: {page}"
        );
        assert!(
            !page.contains("This callback was rejected.</p>"),
            "the bare unexplained rejection sentence must be gone: {page}"
        );
    }
}

#[test]
fn callback_state_comparison_is_constant_time_and_load_bearing() {
    let source = include_str!("oauth.rs");
    let constant_time_call = ["bool::from(expected.", "ct_eq(supplied))"].concat();
    let callback_call = [
        "constant_time_equal(expected_state, ",
        "states[0].as_slice())",
    ]
    .concat();
    assert!(
        source.contains(&constant_time_call) && source.contains(&callback_call),
        "callback state and ready references must retain the constant-time comparator"
    );
}

/// MUTATION CHECK: calling `SharedHttpTransport.client()` in
/// `OAuthCoordinator::new_with_vault` reconstructs rustls during ordinary
/// daemon startup, before any OAuth request exists.
#[test]
fn oauth_coordinator_constructor_retains_only_the_lazy_transport_handle() {
    // `include_str!` preserves checkout-time CRLF on Windows. Normalize only
    // the source view so these semantic mutation pins are platform-neutral.
    let source = include_str!("oauth.rs").replace("\r\n", "\n");
    let constructor_start = source
        .find("pub(crate) fn new_with_vault(")
        .expect("OAuth coordinator constructor");
    let constructor_end = source[constructor_start..]
        .find("    pub(crate) fn availability(")
        .map(|offset| constructor_start + offset)
        .expect("constructor end");
    let constructor = &source[constructor_start..constructor_end];
    assert!(
        constructor.contains("transport: crate::http_transport::SharedHttpTransport"),
        "the coordinator must retain the zero-sized lazy transport handle"
    );
    assert!(
        !constructor.contains(".client()"),
        "coordinator construction must not acquire the shared TLS client"
    );
    assert!(
        source.contains("fn coordinator_http_client(")
            && source.contains("inner\n        .transport\n        .client()"),
        "the first OAuth HTTP operation must acquire the shared client"
    );
}

#[tokio::test]
async fn code_is_consumed_once_and_listener_rejects_replay() {
    let server = FakeOAuthServer::start(FakeMode::Success, false).await;
    let (coordinator, mut receiver) = coordinator_for(&server, Duration::from_secs(5)).await;
    let (flow_id, authorization_url, _) = started_flow(&mut receiver).await;
    let no_redirect = reqwest::Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        .build()
        .expect("browser");
    let redirect = no_redirect
        .get(authorization_url)
        .send()
        .await
        .expect("authorize");
    let callback = redirect
        .headers()
        .get(reqwest::header::LOCATION)
        .expect("callback location")
        .to_str()
        .expect("callback text")
        .to_owned();
    assert!(
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("callback")
            .get(&callback)
            .send()
            .await
            .expect("first callback")
            .status()
            .is_success()
    );
    assert!(matches!(
        wait_ready(&coordinator, &flow_id).await,
        OAuthFlowStatusWire::Ready { .. }
    ));
    assert!(
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("replay")
            .get(callback)
            .send()
            .await
            .is_err(),
        "the consumed listener must be gone"
    );
    assert_eq!(server.state.token_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn simultaneous_callback_replay_allows_exactly_one_code_exchange() {
    let server = FakeOAuthServer::start(FakeMode::Success, false).await;
    let (coordinator, mut receiver) = coordinator_for(&server, Duration::from_secs(5)).await;
    let (flow_id, authorization_url, _) = started_flow(&mut receiver).await;
    let authorize = reqwest::Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        .build()
        .expect("browser")
        .get(authorization_url)
        .send()
        .await
        .expect("authorize");
    let callback = authorize
        .headers()
        .get(reqwest::header::LOCATION)
        .expect("callback")
        .to_str()
        .expect("callback text")
        .to_owned();
    let callback = Url::parse(&callback).expect("callback URL");
    let port = callback.port().expect("callback port");
    let request = format!(
        "GET {}?{} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n",
        callback.path(),
        callback.query().expect("callback query")
    );
    let mut first = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .expect("first replay");
    let mut second = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .expect("second replay");
    let (first_write, second_write) = tokio::join!(
        first.write_all(request.as_bytes()),
        second.write_all(request.as_bytes())
    );
    first_write.expect("first write");
    second_write.expect("second write");
    let (first_read, second_read) = tokio::join!(
        async {
            let mut bytes = Vec::new();
            let result = first.read_to_end(&mut bytes).await;
            (result, bytes)
        },
        async {
            let mut bytes = Vec::new();
            let result = second.read_to_end(&mut bytes).await;
            (result, bytes)
        }
    );
    let successes = [first_read, second_read]
        .into_iter()
        .filter(|(result, bytes)| {
            result.is_ok()
                && String::from_utf8_lossy(bytes).contains("200 OK")
                && String::from_utf8_lossy(bytes).contains(SUCCESS_HTML)
        })
        .count();
    assert_eq!(successes, 1);
    assert!(matches!(
        wait_ready(&coordinator, &flow_id).await,
        OAuthFlowStatusWire::Ready { .. }
    ));
    assert_eq!(server.state.token_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn oversized_callback_is_rejected_without_consuming_flow() {
    let server = FakeOAuthServer::start(FakeMode::Success, false).await;
    let (coordinator, mut receiver) = coordinator_for(&server, Duration::from_secs(5)).await;
    let (flow_id, authorization_url, port) = started_flow(&mut receiver).await;
    let mut attacker = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .expect("attacker");
    let oversized = vec![b'x'; CALLBACK_RESPONSE_LIMIT + 1];
    attacker
        .write_all(&oversized)
        .await
        .expect("oversized write");
    attacker.shutdown().await.expect("attacker shutdown");
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("browser")
        .get(authorization_url)
        .send()
        .await
        .expect("valid browser callback");
    assert!(matches!(
        wait_ready(&coordinator, &flow_id).await,
        OAuthFlowStatusWire::Ready { .. }
    ));
}

#[tokio::test]
async fn issuer_audience_and_nonce_mismatches_fail_verified_identity() {
    for mode in [
        FakeMode::IssuerMismatch,
        FakeMode::AudienceMismatch,
        FakeMode::NonceMismatch,
    ] {
        let server = FakeOAuthServer::start(mode, false).await;
        let (coordinator, mut receiver) = coordinator_for(&server, Duration::from_secs(5)).await;
        let (flow_id, url, _) = started_flow(&mut receiver).await;
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("browser")
            .get(url)
            .send()
            .await
            .expect("browser");
        assert_eq!(
            wait_ready(&coordinator, &flow_id).await,
            OAuthFlowStatusWire::Failed {
                public_code: "identity_claim_mismatch".into()
            }
        );
    }
}

#[tokio::test]
async fn flow_owner_attempt_and_daemon_instance_bind_status_and_ready_reference() {
    let server = FakeOAuthServer::start(FakeMode::Success, false).await;
    let (coordinator, mut receiver) = coordinator_for(&server, Duration::from_secs(5)).await;
    let (flow_id, authorization_url, _) = started_flow(&mut receiver).await;
    assert!(
        coordinator
            .status("other-connection", &flow_id, "attempt-1")
            .is_none()
    );
    assert!(
        coordinator
            .status("connection-1", &flow_id, "other-attempt")
            .is_none()
    );
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("browser")
        .get(authorization_url)
        .send()
        .await
        .expect("browser");
    let OAuthFlowStatusWire::Ready {
        oauth_reference, ..
    } = wait_ready(&coordinator, &flow_id).await
    else {
        panic!("flow not ready");
    };
    assert!(
        coordinator
            .claim_ready(
                "other-connection",
                &flow_id,
                "attempt-1",
                "fake-oauth",
                "work-oauth",
                &oauth_reference,
            )
            .is_none()
    );

    let restarted = OAuthCoordinator::new(
        "different-daemon-instance".into(),
        OAuthProviderCatalog::with_test_registrations([
            server.registration(Arc::new(FakeIdentityVerifier))
        ])
        .expect("catalog"),
        OAuthCoordinatorConfig::default(),
    )
    .expect("restart");
    assert!(
        restarted
            .status("connection-1", &flow_id, "attempt-1")
            .is_none()
    );
}

#[tokio::test]
async fn restoring_ready_claim_preserves_original_deadline() {
    let server = FakeOAuthServer::start(FakeMode::Success, false).await;
    let (coordinator, mut receiver) = coordinator_for(&server, Duration::from_secs(5)).await;
    let (flow_id, authorization_url, _) = started_flow(&mut receiver).await;
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("browser")
        .get(authorization_url)
        .send()
        .await
        .expect("browser");
    let OAuthFlowStatusWire::Ready {
        oauth_reference, ..
    } = wait_ready(&coordinator, &flow_id).await
    else {
        panic!("not ready");
    };
    let mut claim = coordinator
        .claim_ready(
            "connection-1",
            &flow_id,
            "attempt-1",
            "fake-oauth",
            "work-oauth",
            &oauth_reference,
        )
        .expect("claim");
    claim.deadline = Instant::now()
        .checked_add(Duration::from_millis(10))
        .expect("deadline");
    claim.expires_at_ms = now_ms().expect("clock").saturating_add(10);
    coordinator.restore_ready(claim);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(
        coordinator.status("connection-1", &flow_id, "attempt-1"),
        Some(OAuthFlowStatusWire::Expired)
    );
}

struct ControlledVault {
    inner: MemoryVault,
    fail_put: AtomicBool,
    arm_durable: AtomicBool,
    durable: Arc<FakeState>,
}

impl ControlledVault {
    fn new(durable: Arc<FakeState>) -> Self {
        Self {
            inner: MemoryVault::new(),
            fail_put: AtomicBool::new(false),
            arm_durable: AtomicBool::new(false),
            durable,
        }
    }

    fn arm(&self, fail_put: bool) {
        self.fail_put.store(fail_put, Ordering::SeqCst);
        self.arm_durable.store(true, Ordering::SeqCst);
    }
}

impl Vault for ControlledVault {
    fn put(&self, alias: &CredentialAlias, secret: &[u8]) -> AccountsResult<()> {
        if self.fail_put.load(Ordering::SeqCst) {
            return Err(HaiderError::new(
                ErrorCode::ProviderError,
                "injected vault put failure",
                false,
            ));
        }
        self.inner.put(alias, secret)?;
        if self.arm_durable.load(Ordering::SeqCst) {
            self.durable.durable.store(true, Ordering::SeqCst);
        }
        Ok(())
    }

    fn resolve(&self, alias: &CredentialAlias) -> AccountsResult<SecretHandle> {
        self.inner.resolve(alias)
    }

    fn delete(&self, alias: &CredentialAlias) -> AccountsResult<()> {
        self.inner.delete(alias)
    }

    fn list(&self) -> AccountsResult<Vec<CredentialAlias>> {
        self.inner.list()
    }
}

struct StaleReadVault {
    inner: ControlledVault,
    capture_next_resolve: AtomicBool,
    stale_read_captured: Notify,
    stale_read_release: (Mutex<bool>, Condvar),
}

impl StaleReadVault {
    fn new(durable: Arc<FakeState>) -> Self {
        Self {
            inner: ControlledVault::new(durable),
            capture_next_resolve: AtomicBool::new(false),
            stale_read_captured: Notify::new(),
            stale_read_release: (Mutex::new(false), Condvar::new()),
        }
    }

    fn arm(&self) {
        self.inner.arm(false);
        self.capture_next_resolve.store(true, Ordering::SeqCst);
    }

    async fn wait_until_stale_read_is_captured(&self) {
        self.stale_read_captured.notified().await;
    }

    fn release_stale_read(&self) {
        let (released, changed) = &self.stale_read_release;
        *released.lock().expect("stale-read release lock") = true;
        changed.notify_all();
    }
}

impl Vault for StaleReadVault {
    fn put(&self, alias: &CredentialAlias, secret: &[u8]) -> AccountsResult<()> {
        self.inner.put(alias, secret)
    }

    fn resolve(&self, alias: &CredentialAlias) -> AccountsResult<SecretHandle> {
        let stored = self.inner.resolve(alias)?;
        if self.capture_next_resolve.swap(false, Ordering::SeqCst) {
            self.stale_read_captured.notify_one();
            let (released, changed) = &self.stale_read_release;
            let mut released = released.lock().expect("stale-read release lock");
            while !*released {
                released = changed.wait(released).expect("stale-read release wait");
            }
        }
        Ok(stored)
    }

    fn delete(&self, alias: &CredentialAlias) -> AccountsResult<()> {
        self.inner.delete(alias)
    }

    fn list(&self) -> AccountsResult<Vec<CredentialAlias>> {
        self.inner.list()
    }
}

struct ResolveCountingVault {
    inner: MemoryVault,
    resolves: AtomicUsize,
}

impl ResolveCountingVault {
    fn new() -> Self {
        Self {
            inner: MemoryVault::new(),
            resolves: AtomicUsize::new(0),
        }
    }
}

impl Vault for ResolveCountingVault {
    fn put(&self, alias: &CredentialAlias, secret: &[u8]) -> AccountsResult<()> {
        self.inner.put(alias, secret)
    }

    fn resolve(&self, alias: &CredentialAlias) -> AccountsResult<SecretHandle> {
        self.resolves.fetch_add(1, Ordering::SeqCst);
        self.inner.resolve(alias)
    }

    fn delete(&self, alias: &CredentialAlias) -> AccountsResult<()> {
        self.inner.delete(alias)
    }

    fn list(&self) -> AccountsResult<Vec<CredentialAlias>> {
        self.inner.list()
    }
}

fn oauth_descriptor_for_test() -> CredentialDescriptor {
    CredentialDescriptor {
        alias: CredentialAlias::new("work-oauth"),
        provider: "fake-oauth".into(),
        base_url: None,
        auth_method: AuthMethod::OAuth,
        identity: "person@example.invalid".into(),
        status: CredentialStatus::Ok,
        active: true,
        label: None,
        account_identity: None,
        created_at_ms: None,
    }
}

fn oauth_bundle_for_test(server: &FakeOAuthServer, expires_at_unix_ms: u64) -> OAuthTokenBundleV1 {
    OAuthTokenBundleV1::new(
        "fake-oauth".into(),
        server.state.issuer.clone(),
        AUDIENCE.into(),
        Some("fake-api-resource".into()),
        "Bearer".into(),
        Zeroizing::new(b"ACCESS_OLD_SENTINEL_914d".to_vec()),
        Some(Zeroizing::new(REFRESH_SENTINEL.as_bytes().to_vec())),
        expires_at_unix_ms,
        Some(expires_at_unix_ms.saturating_add(3_600_000)),
        SCOPES.split_ascii_whitespace().map(str::to_owned).collect(),
        OAuthIdentityV1 {
            subject_hash: "fake-subject-hash".into(),
            display_identity: "person@example.invalid".into(),
        },
        1,
    )
    .expect("bundle")
}

fn start_status_actor(
    snapshot: &crate::accounts::AccountsSnapshot,
    vault: Arc<dyn Vault>,
) -> mpsc::Sender<crate::accounts::AccountCommand> {
    start_status_actor_with_import_source(snapshot, vault, None)
}

fn start_status_actor_with_import_source(
    snapshot: &crate::accounts::AccountsSnapshot,
    vault: Arc<dyn Vault>,
    import_source: Option<&'static str>,
) -> mpsc::Sender<crate::accounts::AccountCommand> {
    let (sender, mut receiver) = mpsc::channel(16);
    let snapshot = Arc::clone(snapshot);
    tokio::spawn(async move {
        while let Some(command) = receiver.recv().await {
            match command {
                crate::accounts::AccountCommand::BeginOAuthImportHeal { completed, .. } => {
                    let result = import_source.map_or(
                        crate::accounts::OAuthImportHealResult::NotImported,
                        |source| crate::accounts::OAuthImportHealResult::RefreshFallback {
                            source: source.to_owned(),
                        },
                    );
                    let _ = completed.send(Ok(result));
                }
                crate::accounts::AccountCommand::BeginOAuthRefresh {
                    descriptor,
                    expected,
                    completed,
                } => {
                    let current = vault
                        .resolve(&descriptor.alias)
                        .and_then(|stored| OAuthTokenBundleV1::decode(stored.expose_secret()));
                    let current_matches = current.is_ok_and(|current| {
                        current.generation == expected.generation
                            && current.provider_id == descriptor.provider
                            && current.issuer == expected.issuer
                            && current.audience == expected.audience
                            && current.resource == expected.resource
                            && current.identity.subject_hash == expected.subject_hash
                            && current.identity.display_identity == descriptor.identity
                    });
                    let result = snapshot
                        .lock()
                        .map_err(|_| {
                            HaiderError::new(ErrorCode::Internal, "snapshot poisoned", false)
                        })
                        .map(|mut descriptors| {
                            let Some(current) = descriptors.iter_mut().find(|current| {
                                current.alias == descriptor.alias
                                    && current.provider == descriptor.provider
                                    && current.base_url == descriptor.base_url
                                    && current.auth_method == descriptor.auth_method
                                    && current.identity == descriptor.identity
                            }) else {
                                return false;
                            };
                            if !current_matches {
                                return false;
                            }
                            current.status = CredentialStatus::Expired;
                            true
                        });
                    let _ = completed.send(result);
                }
                crate::accounts::AccountCommand::ExpireOAuthRefresh {
                    descriptor,
                    expected,
                    completed,
                } => {
                    let current = vault
                        .resolve(&descriptor.alias)
                        .and_then(|stored| OAuthTokenBundleV1::decode(stored.expose_secret()));
                    let current_matches = current.is_ok_and(|current| {
                        current.generation == expected.generation
                            && current.issuer == expected.issuer
                            && current.audience == expected.audience
                            && current.resource == expected.resource
                            && current.identity.subject_hash == expected.subject_hash
                    });
                    let result = snapshot
                        .lock()
                        .map_err(|_| {
                            HaiderError::new(ErrorCode::Internal, "snapshot poisoned", false)
                        })
                        .map(|mut descriptors| {
                            let Some(current) = descriptors.iter_mut().find(|current| {
                                current.alias == descriptor.alias
                                    && current.provider == descriptor.provider
                                    && current.base_url == descriptor.base_url
                                    && current.auth_method == descriptor.auth_method
                                    && current.identity == descriptor.identity
                            }) else {
                                return false;
                            };
                            if !current_matches {
                                return false;
                            }
                            current.status = CredentialStatus::Expired;
                            true
                        });
                    let _ = completed.send(result);
                }
                crate::accounts::AccountCommand::ApplyOAuthRefresh {
                    descriptor,
                    expected,
                    encoded_bundle,
                    completed,
                } => {
                    let current_descriptor = snapshot.lock().ok().and_then(|descriptors| {
                        descriptors
                            .iter()
                            .find(|current| current.alias == descriptor.alias)
                            .cloned()
                    });
                    let result = if !current_descriptor.as_ref().is_some_and(|current| {
                        current.alias == descriptor.alias
                            && current.provider == descriptor.provider
                            && current.base_url == descriptor.base_url
                            && current.auth_method == descriptor.auth_method
                            && current.identity == descriptor.identity
                    }) {
                        Err(crate::accounts::RefreshApplyError::Stale)
                    } else {
                        vault
                            .resolve(&descriptor.alias)
                            .and_then(|stored| OAuthTokenBundleV1::decode(stored.expose_secret()))
                            .map_err(|_| crate::accounts::RefreshApplyError::Persist)
                            .and_then(|current| {
                                if current.generation != expected.generation
                                    || current.issuer != expected.issuer
                                    || current.audience != expected.audience
                                    || current.resource != expected.resource
                                    || current.identity.subject_hash != expected.subject_hash
                                {
                                    return Err(crate::accounts::RefreshApplyError::Stale);
                                }
                                vault
                                    .put(&descriptor.alias, &encoded_bundle)
                                    .map_err(|_| crate::accounts::RefreshApplyError::Persist)
                            })
                    };
                    if result.is_ok()
                        && let Ok(mut descriptors) = snapshot.lock()
                        && let Some(current) = descriptors
                            .iter_mut()
                            .find(|current| current.alias == descriptor.alias)
                        && matches!(&current.status, CredentialStatus::Expired)
                    {
                        current.status = descriptor.status.clone();
                    }
                    if matches!(result, Err(crate::accounts::RefreshApplyError::Persist))
                        && let Ok(mut descriptors) = snapshot.lock()
                        && let Some(current) = descriptors
                            .iter_mut()
                            .find(|current| current.alias == descriptor.alias)
                    {
                        current.status = CredentialStatus::Expired;
                        let _ = vault.delete(&descriptor.alias);
                    }
                    let _ = completed.send(result);
                }
                _ => {}
            }
        }
    });
    sender
}

fn broker_for(
    server: &FakeOAuthServer,
    vault: Arc<dyn Vault>,
    descriptor: CredentialDescriptor,
) -> (
    CredentialBroker,
    crate::accounts::AccountsSnapshot,
    CredentialDescriptor,
) {
    let snapshot = Arc::new(Mutex::new(vec![descriptor.clone()]));
    let broker = CredentialBroker::new(
        Arc::clone(&vault),
        OAuthProviderCatalog::with_test_registrations([
            server.registration(Arc::new(FakeIdentityVerifier))
        ])
        .expect("catalog"),
        Arc::clone(&snapshot),
        start_status_actor(&snapshot, vault),
    )
    .expect("broker");
    (broker, snapshot, descriptor)
}

fn serialized_registration(server: &FakeOAuthServer) -> OAuthProviderRegistration {
    server
        .registration(Arc::new(FakeIdentityVerifier))
        .with_test_device_flow()
}

fn independent_serialized_broker(
    server: &FakeOAuthServer,
    vault: Arc<dyn Vault>,
    descriptor: &CredentialDescriptor,
) -> (CredentialBroker, crate::accounts::AccountsSnapshot) {
    let snapshot = Arc::new(Mutex::new(vec![descriptor.clone()]));
    let broker = CredentialBroker::new(
        Arc::clone(&vault),
        OAuthProviderCatalog::with_test_registrations([serialized_registration(server)])
            .expect("serialized catalog"),
        Arc::clone(&snapshot),
        start_status_actor(&snapshot, vault),
    )
    .expect("serialized broker");
    (broker, snapshot)
}

fn imported_codex_registration(server: &FakeOAuthServer) -> OAuthProviderRegistration {
    let mut registration = server.registration(Arc::new(FakeIdentityVerifier));
    registration.provider_id = haider_provider::OPENAI_OAUTH_PROVIDER_NAME.to_owned();
    registration.with_test_refresh_shape(OAuthTokenRequestEncoding::Json, false)
}

fn imported_codex_descriptor() -> CredentialDescriptor {
    let mut descriptor = oauth_descriptor_for_test();
    descriptor.provider = haider_provider::OPENAI_OAUTH_PROVIDER_NAME.to_owned();
    descriptor.alias = CredentialAlias::new("imported-codex-rotating");
    descriptor
}

fn imported_codex_bundle_for_test(
    server: &FakeOAuthServer,
    expires_at_unix_ms: u64,
) -> OAuthTokenBundleV1 {
    let registration = imported_codex_registration(server);
    OAuthTokenBundleV1::new(
        registration.provider_id.clone(),
        registration.issuer.clone(),
        registration.audience.clone(),
        registration.resource.clone(),
        "Bearer".into(),
        Zeroizing::new(b"ACCESS_OLD_SENTINEL_914d".to_vec()),
        Some(Zeroizing::new(REFRESH_SENTINEL.as_bytes().to_vec())),
        expires_at_unix_ms,
        Some(expires_at_unix_ms.saturating_add(3_600_000)),
        registration.scopes.clone(),
        OAuthIdentityV1 {
            subject_hash: "fake-subject-hash".into(),
            display_identity: "person@example.invalid".into(),
        },
        1,
    )
    .expect("imported Codex bundle")
}

fn independent_imported_codex_broker(
    server: &FakeOAuthServer,
    vault: Arc<dyn Vault>,
    descriptor: &CredentialDescriptor,
) -> (CredentialBroker, crate::accounts::AccountsSnapshot) {
    let snapshot = Arc::new(Mutex::new(vec![descriptor.clone()]));
    let broker = CredentialBroker::new(
        Arc::clone(&vault),
        OAuthProviderCatalog::with_test_registrations([imported_codex_registration(server)])
            .expect("imported Codex catalog"),
        Arc::clone(&snapshot),
        start_status_actor_with_import_source(&snapshot, vault, Some("codex")),
    )
    .expect("imported Codex broker");
    (broker, snapshot)
}

/// MUTATION CHECK: remove a Kimi MSH header, add PKCE/client-secret fields,
/// stop polling after `authorization_pending`, or omit the persisted refresh
/// threshold. Expected RUNTIME failure: this fixture observes the wire shape,
/// cannot reach tokens, or finds an immediately/late refreshed bundle.
#[tokio::test]
async fn device_flow_polls_to_tokens_with_required_msh_headers() {
    const DEVICE_ID: &str = "123e4567-e89b-42d3-a456-426614174000";

    let server = FakeOAuthServer::start(FakeMode::Success, false).await;
    let mut registration = serialized_registration(&server);
    registration.authorization_endpoint =
        Url::parse(&format!("http://{}/device_authorization", server.address))
            .expect("device authorization endpoint");
    let vault = Arc::new(MemoryVault::new());
    vault
        .put(
            &CredentialAlias::new(KIMI_DEVICE_ALIAS),
            DEVICE_ID.as_bytes(),
        )
        .expect("seed stable device ID");
    let device_id = load_or_create_kimi_device_id(vault as Arc<dyn Vault>)
        .await
        .expect("load stable device ID");
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(2))
        .build()
        .expect("device fixture client");

    let authorization = request_device_authorization(&client, &registration, Some(&device_id))
        .await
        .expect("device authorization");
    assert_eq!(authorization.user_code.0.as_slice(), b"ABCD-EFGH");
    assert!(matches!(
        poll_device_token(
            &client,
            &registration,
            authorization.device_code.0.as_slice(),
            Some(&device_id),
        )
        .await
        .expect("pending poll"),
        DeviceTokenPoll::Pending
    ));
    assert!(matches!(
        poll_device_token(
            &client,
            &registration,
            authorization.device_code.0.as_slice(),
            Some(&device_id),
        )
        .await
        .expect("slow-down poll"),
        DeviceTokenPoll::SlowDown
    ));
    let DeviceTokenPoll::Tokens(bytes) = poll_device_token(
        &client,
        &registration,
        authorization.device_code.0.as_slice(),
        Some(&device_id),
    )
    .await
    .expect("successful poll") else {
        panic!("third device poll must produce tokens");
    };
    let before = now_ms().expect("clock");
    let bundle = token_bundle_from_response(&registration, &bytes, &[], 1, None)
        .await
        .expect("Kimi token bundle");
    assert_eq!(bundle.access_token(), ACCESS_SENTINEL.as_bytes());
    assert_eq!(bundle.refresh_token(), Some(REFRESH_SENTINEL.as_bytes()));
    assert!(bundle.refresh_after_unix_ms.is_some_and(|refresh_after| {
        refresh_after >= before.saturating_add(300_000)
            && refresh_after <= before.saturating_add(301_000)
    }));

    let captured = server.state.msh_headers.lock().expect("MSH headers");
    assert_eq!(
        captured.len(),
        4,
        "authorization and all three polls are captured"
    );
    let expected = [
        ("x-msh-platform", "kimi_cli"),
        ("x-msh-version", env!("CARGO_PKG_VERSION")),
        ("x-msh-device-name", "haider-agent"),
        ("x-msh-device-model", std::env::consts::ARCH),
        ("x-msh-os-version", std::env::consts::OS),
        ("x-msh-device-id", DEVICE_ID),
    ];
    for headers in captured.iter() {
        assert_eq!(
            headers
                .keys()
                .filter(|name| name.starts_with("x-msh-"))
                .count(),
            expected.len()
        );
        for (name, value) in expected {
            assert_eq!(headers.get(name).map(String::as_str), Some(value));
        }
    }
}

/// Drive real loopback I/O under a `start_paused` clock. Awaiting a blocking
/// worker inhibits paused-clock auto-advance (tokio pins the virtual clock
/// while any blocking task is alive), so each iteration parks the runtime for
/// ~1ms of REAL wall time: socket readiness is serviced, already-due virtual
/// timers fire, and the production client's connect/request timers — armed at
/// frozen-now + 5s/15s — can never fire spuriously. A bare park instead
/// auto-advances straight into those timers while a request is on the wire,
/// and a busy `yield_now` loop starves the driver of both I/O and timers.
async fn drive_paused_io(iterations: usize) {
    for _ in 0..iterations {
        tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_millis(1)))
            .await
            .expect("paused-clock I/O driver");
    }
}

async fn wait_for_token_calls(server: &FakeOAuthServer, expected: usize) {
    for _ in 0..2_000 {
        if server.state.token_calls.load(Ordering::SeqCst) == expected {
            return;
        }
        drive_paused_io(1).await;
    }
    assert_eq!(
        server.state.token_calls.load(Ordering::SeqCst),
        expected,
        "device token poll count did not converge"
    );
}

/// MUTATION CHECK: stop the production device runner after
/// `authorization_pending`, ignore `slow_down`, or add less than five seconds
/// to the next interval. Expected RUNTIME failure: the coordinator either
/// never becomes ready or reaches the third poll before the six-second gate.
///
/// `start_paused` (rather than a mid-test `pause()`) is load-bearing twice
/// over: the whole flow runs on one frozen clock so every interval below is
/// exact, and the timer wheel's epoch coincides with the frozen base — a
/// mid-test pause leaves a sub-millisecond fraction between them, and the
/// wheel's round-up tick conversion then puts every whole-millisecond
/// deadline one tick past every whole-millisecond advance, so the runner's
/// interval sleeps never fire at their exact gates.
#[tokio::test(start_paused = true)]
async fn device_flow_runner_continues_and_honors_slow_down_interval() {
    let server = FakeOAuthServer::start(FakeMode::Success, false).await;
    let mut registration = serialized_registration(&server);
    registration.authorization_endpoint =
        Url::parse(&format!("http://{}/device_authorization", server.address))
            .expect("device authorization endpoint");
    let (coordinator, mut receiver) =
        coordinator_for_registration(registration, Duration::from_secs(30)).await;
    let (flow_id, authorization_url) = started_device_flow(&mut receiver).await;
    assert!(authorization_url.ends_with("/verify?user_code=ABCD-EFGH"));
    assert!(matches!(
        coordinator.status("connection-1", &flow_id, "attempt-1"),
        Some(OAuthFlowStatusWire::WaitingDevice)
    ));

    // Let the runner spawn and arm its first interval sleep at the frozen
    // instant, then hold every gate with drive_paused_io so poll responses
    // are fully consumed (including the source-chunk scrub sweeps) and the
    // next interval sleep is re-armed BEFORE the clock advances again.
    drive_paused_io(10).await;
    assert_eq!(server.state.token_calls.load(Ordering::SeqCst), 0);
    tokio::time::advance(Duration::from_millis(999)).await;
    drive_paused_io(10).await;
    assert_eq!(server.state.token_calls.load(Ordering::SeqCst), 0);
    tokio::time::advance(Duration::from_millis(1)).await;
    wait_for_token_calls(&server, 1).await;
    drive_paused_io(20).await;

    tokio::time::advance(Duration::from_secs(1)).await;
    wait_for_token_calls(&server, 2).await;
    drive_paused_io(20).await;
    tokio::time::advance(Duration::from_millis(5_999)).await;
    drive_paused_io(10).await;
    assert_eq!(server.state.token_calls.load(Ordering::SeqCst), 2);
    tokio::time::advance(Duration::from_millis(1)).await;
    wait_for_token_calls(&server, 3).await;

    // wait_ready's bare yield loop would starve the paused-clock driver, so
    // walk the flow to its terminal status with real parks instead.
    let mut ready = false;
    for _ in 0..2_000 {
        let status = coordinator
            .status("connection-1", &flow_id, "attempt-1")
            .expect("owned flow");
        match status {
            OAuthFlowStatusWire::WaitingDevice | OAuthFlowStatusWire::Exchanging => {
                drive_paused_io(1).await;
            }
            OAuthFlowStatusWire::Ready { .. } => {
                ready = true;
                break;
            }
            other => panic!("device flow ended in unexpected status: {other:?}"),
        }
    }
    assert!(ready, "device flow never reached Ready");
    assert!(coordinator.shutdown().await);
}

async fn wait_for_refresh_calls(server: &FakeOAuthServer, expected: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while server.state.refresh_calls.load(Ordering::SeqCst) != expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("refresh call count");
}

/// A distinct file-vault handle that exposes only the contention boundary
/// needed by `concurrent_refreshers_never_destroy_the_rotated_token`.
struct ContentionObservedFileVault {
    inner: haider_accounts::FileVault,
    refresh_contended: Notify,
}

impl ContentionObservedFileVault {
    fn new(root: &Path) -> Self {
        Self {
            inner: haider_accounts::FileVault::new(root),
            refresh_contended: Notify::new(),
        }
    }

    async fn wait_for_refresh_contention(&self) {
        self.refresh_contended.notified().await;
    }
}

impl Vault for ContentionObservedFileVault {
    fn put(&self, alias: &CredentialAlias, secret: &[u8]) -> AccountsResult<()> {
        self.inner.put(alias, secret)
    }

    fn resolve(&self, alias: &CredentialAlias) -> AccountsResult<SecretHandle> {
        self.inner.resolve(alias)
    }

    fn delete(&self, alias: &CredentialAlias) -> AccountsResult<()> {
        self.inner.delete(alias)
    }

    fn list(&self) -> AccountsResult<Vec<CredentialAlias>> {
        self.inner.list()
    }

    fn try_refresh_lock(
        &self,
        alias: &CredentialAlias,
    ) -> AccountsResult<Option<VaultRefreshLock>> {
        let lease = self.inner.try_refresh_lock(alias)?;
        if lease.is_none() {
            self.refresh_contended.notify_one();
        }
        Ok(lease)
    }
}

/// MUTATION CHECK: remove the vault lease, release it before Apply, or remove
/// the under-lease re-read. Expected RUNTIME failure: the fake rotating
/// server observes a second use of generation one's refresh token, or one
/// contender fails instead of adopting the durable generation two bundle.
#[tokio::test]
async fn concurrent_refreshers_never_destroy_the_rotated_token() {
    let server = FakeOAuthServer::start(FakeMode::Success, true).await;
    server
        .state
        .expect_refresh_binding
        .store(false, Ordering::SeqCst);
    let directory = tempfile::tempdir().expect("temp vault");
    let first_vault = Arc::new(haider_accounts::FileVault::new(directory.path()));
    let second_vault = Arc::new(ContentionObservedFileVault::new(directory.path()));
    let descriptor = oauth_descriptor_for_test();
    let expired = oauth_bundle_for_test(&server, now_ms().expect("clock").saturating_sub(1))
        .with_refresh_after(0);
    first_vault
        .put(&descriptor.alias, &expired.encode().expect("encode seed"))
        .expect("seed rotating bundle");
    let (first, _) =
        independent_serialized_broker(&server, first_vault.clone() as Arc<dyn Vault>, &descriptor);
    let (second, _) =
        independent_serialized_broker(&server, second_vault.clone() as Arc<dyn Vault>, &descriptor);
    // Registry #94: endpoint entry and contender admission share one absolute
    // request budget. Arithmetic: start + TOKEN_TIMEOUT; the second wait gets
    // only the remainder after the first wait, never a reset 2 * timeout.
    let refresh_deadline = Instant::now()
        .checked_add(TOKEN_TIMEOUT)
        .expect("token request deadline");
    let first_resolve = {
        let first = first.clone();
        let descriptor = descriptor.clone();
        tokio::spawn(async move { first.resolve(&descriptor).await })
    };
    // CONTRACT WAIT: the fake signals only after parsing generation one's
    // token request. The absolute deadline is the production POST budget.
    tokio::time::timeout_at(refresh_deadline, server.state.refresh_started.notified())
        .await
        .expect("first refresh request must reach the token endpoint");
    assert_eq!(server.state.refresh_calls.load(Ordering::SeqCst), 1);
    let second_resolve = {
        let second = second.clone();
        let descriptor = descriptor.clone();
        tokio::spawn(async move { second.resolve(&descriptor).await })
    };
    // CONTRACT WAIT: release the first request only after the second resolver
    // has observed the same physical alias lease as contended. A scheduler
    // yield does not establish this boundary: if the second resolver starts
    // after generation two is durable, the fake's 120-second token is already
    // refresh-due under the serialized policy's 300-second threshold and a
    // second (generation-two) request is legitimate.
    // The first wait's elapsed time is deducted because both waits reuse the
    // same absolute deadline.
    tokio::time::timeout_at(refresh_deadline, second_vault.wait_for_refresh_contention())
        .await
        .expect("second refresher must contend before the first token POST expires");
    server.release_refresh();
    let first_access = first_resolve
        .await
        .expect("first join")
        .expect("first authorized");
    let second_access = second_resolve
        .await
        .expect("second join")
        .expect("second authorized");
    assert_eq!(
        first_access.expose_secret(),
        b"ACCESS_ROTATED_SENTINEL_3a19"
    );
    assert_eq!(
        second_access.expose_secret(),
        b"ACCESS_ROTATED_SENTINEL_3a19"
    );
    assert_eq!(
        server.state.refresh_calls.load(Ordering::SeqCst),
        1,
        "the superseded rotating token must be submitted exactly once"
    );
    assert_eq!(
        server
            .state
            .refresh_token_fingerprints
            .lock()
            .expect("refresh token fingerprints")
            .as_slice(),
        &[*blake3::hash(REFRESH_SENTINEL.as_bytes()).as_bytes()],
        "the only refresh request must submit generation one's token"
    );
    let stored = second_vault
        .resolve(&descriptor.alias)
        .and_then(|stored| OAuthTokenBundleV1::decode(stored.expose_secret()))
        .expect("durable rotated bundle");
    assert_eq!(stored.generation, 2);
    assert_eq!(
        stored.refresh_token(),
        Some(b"REFRESH_ROTATED_SENTINEL_8c21".as_slice())
    );
    assert!(first.shutdown().await);
    assert!(second.shutdown().await);
}

/// MUTATION CHECK: route a receipt-confirmed Codex import through the
/// conservative refresh path, remove the physical vault-alias lease, move
/// the vault re-read before the lease, or release before durable Apply.
/// Expected RUNTIME failure: the non-degenerate two-FileVault fixture sends
/// generation one's refresh token twice, one contender fails instead of
/// adopting generation two, or the durable rotated bundle differs.
#[tokio::test]
async fn concurrent_imported_refreshers_adopt_not_destroy() {
    let server = FakeOAuthServer::start(FakeMode::Success, true).await;
    server
        .state
        .expect_refresh_binding
        .store(false, Ordering::SeqCst);
    let directory = tempfile::tempdir().expect("temp imported vault");
    let first_vault = Arc::new(haider_accounts::FileVault::new(directory.path()));
    let second_vault = Arc::new(haider_accounts::FileVault::new(directory.path()));
    let descriptor = imported_codex_descriptor();
    let expired =
        imported_codex_bundle_for_test(&server, now_ms().expect("clock").saturating_sub(1));
    first_vault
        .put(&descriptor.alias, &expired.encode().expect("encode seed"))
        .expect("seed imported rotating bundle");
    let (first, _) = independent_imported_codex_broker(
        &server,
        first_vault.clone() as Arc<dyn Vault>,
        &descriptor,
    );
    let (second, _) = independent_imported_codex_broker(
        &server,
        second_vault.clone() as Arc<dyn Vault>,
        &descriptor,
    );

    let first_resolve = {
        let first = first.clone();
        let descriptor = descriptor.clone();
        tokio::spawn(async move { first.resolve(&descriptor).await })
    };
    wait_for_refresh_calls(&server, 1).await;
    let second_resolve = {
        let second = second.clone();
        let descriptor = descriptor.clone();
        tokio::spawn(async move { second.resolve(&descriptor).await })
    };
    tokio::task::yield_now().await;
    server.release_refresh();

    for resolved in [first_resolve, second_resolve] {
        let access = resolved
            .await
            .expect("imported resolve joins")
            .expect("imported resolve authorized");
        assert_eq!(access.expose_secret(), b"ACCESS_ROTATED_SENTINEL_3a19");
    }
    assert_eq!(
        server.state.refresh_calls.load(Ordering::SeqCst),
        1,
        "the rotated Codex refresh token must never be replayed"
    );
    assert_eq!(
        *server
            .state
            .refresh_token_fingerprints
            .lock()
            .expect("refresh token fingerprints"),
        [*blake3::hash(REFRESH_SENTINEL.as_bytes()).as_bytes()],
        "only generation one's token may reach the endpoint, exactly once"
    );
    let stored = second_vault
        .resolve(&descriptor.alias)
        .and_then(|stored| OAuthTokenBundleV1::decode(stored.expose_secret()))
        .expect("durable imported rotation");
    assert_eq!(stored.generation, 2);
    assert_eq!(
        stored.refresh_token(),
        Some(b"REFRESH_ROTATED_SENTINEL_8c21".as_slice())
    );
    assert_eq!(stored.refresh_rejected_until_unix_ms, None);
    assert!(first.shutdown().await);
    assert!(second.shutdown().await);
}

/// MUTATION CHECK: retain generation one's refresh token after a rotating
/// response or build the next provider request from the stale observed
/// bundle. Expected RUNTIME failure: the exact next refresh payload contains
/// R1 instead of the returned R2 token.
#[test]
fn refresh_never_replays_a_rotated_token() {
    const ROTATED_REFRESH: &str = "REFRESH_ROTATED_SENTINEL_8c21";
    let registration = OAuthProviderCatalog::default()
        .registration(haider_provider::OPENAI_OAUTH_PROVIDER_NAME)
        .expect("OpenAI OAuth registration");
    let prior = OAuthTokenBundleV1::new(
        registration.provider_id.clone(),
        registration.issuer.clone(),
        registration.audience.clone(),
        registration.resource.clone(),
        "Bearer".into(),
        Zeroizing::new(ACCESS_SENTINEL.as_bytes().to_vec()),
        Some(Zeroizing::new(REFRESH_SENTINEL.as_bytes().to_vec())),
        now_ms().expect("clock").saturating_sub(1),
        None,
        registration.scopes.clone(),
        OAuthIdentityV1 {
            subject_hash: "fake-subject-hash".into(),
            display_identity: "person@example.invalid".into(),
        },
        1,
    )
    .expect("prior imported bundle");
    let response = serde_json::json!({
        "access_token": "ACCESS_ROTATED_SENTINEL_3a19",
        "refresh_token": ROTATED_REFRESH,
        "token_type": "Bearer",
        "expires_in": 3600,
        "scope": registration.scopes.join(" ")
    })
    .to_string();
    let rotated = refresh_bundle_from_response(&registration, response.as_bytes(), &prior)
        .expect("rotating response");
    assert_eq!(rotated.generation, 2);
    assert_eq!(rotated.refresh_token(), Some(ROTATED_REFRESH.as_bytes()));

    let next_request = refresh_token_request_body(
        &registration,
        rotated.refresh_token().expect("rotated refresh token"),
    )
    .expect("next refresh request");
    let next_request: serde_json::Value =
        serde_json::from_slice(next_request.as_ref()).expect("JSON refresh payload");
    assert_eq!(
        next_request
            .get("refresh_token")
            .and_then(serde_json::Value::as_str),
        Some(ROTATED_REFRESH)
    );
    assert_ne!(
        next_request
            .get("refresh_token")
            .and_then(serde_json::Value::as_str),
        Some(REFRESH_SENTINEL)
    );
}

/// MUTATION CHECK: bypass the serialized runner's retry loop, retry fewer or
/// more than three explicit 429/5xx responses, or leave the permanent
/// uncertainty marker after the declared retry class is exhausted. Expected
/// RUNTIME failure: POST count/fingerprints differ or the original bundle is
/// not restored byte-for-byte before the retryable error escapes.
#[tokio::test]
async fn imported_refresh_retries_only_explicit_statuses_and_restores_bundle() {
    let server = FakeOAuthServer::start(FakeMode::Transient, false).await;
    server
        .state
        .expect_refresh_binding
        .store(false, Ordering::SeqCst);
    let descriptor = imported_codex_descriptor();
    let vault = Arc::new(MemoryVault::new());
    let expired =
        imported_codex_bundle_for_test(&server, now_ms().expect("clock").saturating_sub(1));
    let encoded = expired.encode().expect("encode retry seed");
    vault
        .put(&descriptor.alias, &encoded)
        .expect("seed retryable import");
    let (broker, _) =
        independent_imported_codex_broker(&server, vault.clone() as Arc<dyn Vault>, &descriptor);

    let error = broker
        .resolve(&descriptor)
        .await
        .expect_err("exhausted explicit retry statuses remain retryable");
    assert!(error.retryable);
    assert_eq!(server.state.refresh_calls.load(Ordering::SeqCst), 3);
    assert_eq!(
        *server
            .state
            .refresh_token_fingerprints
            .lock()
            .expect("refresh token fingerprints"),
        [*blake3::hash(REFRESH_SENTINEL.as_bytes()).as_bytes(); 3],
        "only explicit retryable statuses may replay the same request, at the bounded count"
    );
    let restored = vault.resolve(&descriptor.alias).expect("restored import");
    assert_eq!(
        restored.expose_secret(),
        encoded.as_slice(),
        "the exact pre-request bundle must replace uncertainty before retry escapes"
    );
    assert!(broker.shutdown().await);
}

/// MUTATION CHECK: classify a response-less transport close as an explicit
/// retryable status, clear permanent uncertainty, or let a successor daemon
/// replay the possibly-spent token. Expected RUNTIME failure: more than one
/// POST occurs, the marker differs from `u64::MAX`, or either broker omits the
/// typed Codex re-import remedy.
#[tokio::test]
async fn imported_ambiguous_transport_never_replays_uncertain_token() {
    let server = FakeOAuthServer::start(FakeMode::AmbiguousTransport, false).await;
    server
        .state
        .expect_refresh_binding
        .store(false, Ordering::SeqCst);
    let directory = tempfile::tempdir().expect("temp ambiguous imported vault");
    let first_vault = Arc::new(haider_accounts::FileVault::new(directory.path()));
    let second_vault = Arc::new(haider_accounts::FileVault::new(directory.path()));
    let descriptor = imported_codex_descriptor();
    let expired =
        imported_codex_bundle_for_test(&server, now_ms().expect("clock").saturating_sub(1));
    first_vault
        .put(&descriptor.alias, &expired.encode().expect("encode seed"))
        .expect("seed ambiguous import");
    let (first, _) = independent_imported_codex_broker(
        &server,
        first_vault.clone() as Arc<dyn Vault>,
        &descriptor,
    );
    let first_error = first
        .resolve(&descriptor)
        .await
        .expect_err("ambiguous transport requires Codex re-import");
    assert_eq!(server.state.refresh_calls.load(Ordering::SeqCst), 1);
    let uncertain = second_vault
        .resolve(&descriptor.alias)
        .and_then(|stored| OAuthTokenBundleV1::decode(stored.expose_secret()))
        .expect("durable uncertain import");
    assert_eq!(uncertain.refresh_rejected_until_unix_ms, Some(u64::MAX));

    let (second, _) = independent_imported_codex_broker(
        &server,
        second_vault.clone() as Arc<dyn Vault>,
        &descriptor,
    );
    let second_error = second
        .resolve(&descriptor)
        .await
        .expect_err("successor refuses uncertain Codex refresh");
    assert_eq!(
        server.state.refresh_calls.load(Ordering::SeqCst),
        1,
        "a possibly-spent token must not be replayed after restart"
    );
    assert_eq!(
        *server
            .state
            .refresh_token_fingerprints
            .lock()
            .expect("refresh token fingerprints"),
        [*blake3::hash(REFRESH_SENTINEL.as_bytes()).as_bytes()]
    );
    for error in [&first_error, &second_error] {
        assert_eq!(error.code, ErrorCode::Unauthorized);
        assert!(!error.retryable);
        assert_eq!(
            error.message,
            "credential expired — re-run `haider account import codex --confirm` or sign in again"
        );
        assert_eq!(
            error
                .details
                .as_ref()
                .and_then(|details| details.get("reimport_required"))
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
    }
    assert!(first.shutdown().await);
    assert!(second.shutdown().await);
}

/// MUTATION CHECK: omit the imported-Codex rejection tombstone, replay a
/// marked refresh token, return Kimi's generic login taxonomy, or include a
/// token/response body in the error. Expected RUNTIME failure: the second
/// independent broker performs another POST, the typed re-import fields or
/// exact remedy differ, or a sentinel appears in formatted public errors.
#[tokio::test]
async fn terminal_invalid_grant_names_reimport_remedy_typed() {
    let server = FakeOAuthServer::start(FakeMode::InvalidGrant, false).await;
    server
        .state
        .expect_refresh_binding
        .store(false, Ordering::SeqCst);
    let directory = tempfile::tempdir().expect("temp imported vault");
    let first_vault = Arc::new(haider_accounts::FileVault::new(directory.path()));
    let second_vault = Arc::new(haider_accounts::FileVault::new(directory.path()));
    let descriptor = imported_codex_descriptor();
    let expired =
        imported_codex_bundle_for_test(&server, now_ms().expect("clock").saturating_sub(1));
    first_vault
        .put(&descriptor.alias, &expired.encode().expect("encode seed"))
        .expect("seed rejected imported bundle");
    let (first, _) = independent_imported_codex_broker(
        &server,
        first_vault.clone() as Arc<dyn Vault>,
        &descriptor,
    );
    let first_error = first
        .resolve(&descriptor)
        .await
        .expect_err("invalid_grant requires Codex re-import");
    let (second, _) = independent_imported_codex_broker(
        &server,
        second_vault.clone() as Arc<dyn Vault>,
        &descriptor,
    );
    let second_error = second
        .resolve(&descriptor)
        .await
        .expect_err("tombstoned Codex token requires re-import");

    for error in [&first_error, &second_error] {
        assert_eq!(error.code, ErrorCode::Unauthorized);
        assert!(!error.retryable);
        assert_eq!(
            error.message,
            "credential expired — re-run `haider account import codex --confirm` or sign in again"
        );
        assert_eq!(
            error
                .details
                .as_ref()
                .and_then(|details| details.get("kind"))
                .and_then(serde_json::Value::as_str),
            Some("oauth_relogin_required")
        );
        assert_eq!(
            error
                .details
                .as_ref()
                .and_then(|details| details.get("reimport_required"))
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
        assert_eq!(
            error
                .details
                .as_ref()
                .and_then(|details| details.get("import_source"))
                .and_then(serde_json::Value::as_str),
            Some("codex")
        );
    }
    assert_eq!(
        server.state.refresh_calls.load(Ordering::SeqCst),
        1,
        "a rejected imported refresh token must never be replayed"
    );
    let stored = second_vault
        .resolve(&descriptor.alias)
        .and_then(|stored| OAuthTokenBundleV1::decode(stored.expose_secret()))
        .expect("tombstoned imported bundle");
    assert!(
        stored
            .refresh_rejected_until_unix_ms
            .is_some_and(|until| until > now_ms().expect("clock"))
    );
    let formatted = format!("{first_error:?}{second_error:?}");
    for secret in [
        RAW_ERROR_SENTINEL,
        ACCESS_SENTINEL,
        REFRESH_SENTINEL,
        "ACCESS_ROTATED_SENTINEL_3a19",
        "REFRESH_ROTATED_SENTINEL_8c21",
    ] {
        assert!(!formatted.contains(secret), "public error leaked {secret}");
    }
    assert!(first.shutdown().await);
    assert!(second.shutdown().await);
}

/// MUTATION CHECK: omit OpenAI OAuth from stale-401 fingerprint adoption or
/// compare the wrong access generation. Expected RUNTIME failure: the broker
/// contacts the token endpoint instead of returning the already-durable
/// generation-two access token.
#[tokio::test]
async fn imported_codex_401_adopts_new_vault_access_without_refresh_post() {
    let descriptor = imported_codex_descriptor();
    let registration = OAuthProviderRegistration::new(
        haider_provider::OPENAI_OAUTH_PROVIDER_NAME,
        "http://127.0.0.1:9",
        "http://127.0.0.1:9/authorize",
        "http://127.0.0.1:9/token",
        "haider-public-fake",
        ["openid", "inference", "profile"].map(str::to_owned),
        AUDIENCE,
        Some("fake-api-resource".into()),
        true,
        Arc::new(FakeIdentityVerifier),
    )
    .expect("offline imported Codex registration")
    .with_test_refresh_shape(OAuthTokenRequestEncoding::Json, false);
    let vault = Arc::new(MemoryVault::new());
    let current = OAuthTokenBundleV1::new(
        registration.provider_id.clone(),
        registration.issuer.clone(),
        registration.audience.clone(),
        registration.resource.clone(),
        "Bearer".into(),
        Zeroizing::new(b"ACCESS_ROTATED_SENTINEL_3a19".to_vec()),
        Some(Zeroizing::new(b"REFRESH_ROTATED_SENTINEL_8c21".to_vec())),
        now_ms().expect("clock").saturating_add(600_000),
        None,
        registration.scopes.clone(),
        OAuthIdentityV1 {
            subject_hash: "fake-subject-hash".into(),
            display_identity: descriptor.identity.clone(),
        },
        2,
    )
    .expect("already-rotated Codex bundle");
    vault
        .put(
            &descriptor.alias,
            &current.encode().expect("encode current"),
        )
        .expect("seed current Codex bundle");
    let snapshot = Arc::new(Mutex::new(vec![descriptor.clone()]));
    let broker = CredentialBroker::new(
        vault.clone() as Arc<dyn Vault>,
        OAuthProviderCatalog::with_test_registrations([registration])
            .expect("offline imported Codex catalog"),
        Arc::clone(&snapshot),
        start_status_actor_with_import_source(&snapshot, vault as Arc<dyn Vault>, Some("codex")),
    )
    .expect("offline imported Codex broker");
    let failed = *blake3::hash(b"ACCESS_OLD_SENTINEL_914d").as_bytes();
    let adopted = broker
        .refresh_after_auth_failure(&descriptor, Some(failed))
        .await
        .expect("adopt newer Codex access");
    assert_eq!(adopted.expose_secret(), b"ACCESS_ROTATED_SENTINEL_3a19");
    assert!(broker.shutdown().await);
}

/// MUTATION CHECK: require every imported OpenAI refresh response to rotate
/// the refresh token, or clear the stored token when the response omits it.
/// Expected RUNTIME failure: the otherwise-valid non-rotating response is
/// rejected or the durable generation-two bundle loses generation one's
/// refresh token.
#[tokio::test]
async fn imported_refresh_retains_token_when_response_does_not_rotate() {
    let server = FakeOAuthServer::start(FakeMode::RefreshOmitTokenAndExpiry, false).await;
    server
        .state
        .expect_refresh_binding
        .store(false, Ordering::SeqCst);
    let descriptor = imported_codex_descriptor();
    let vault = Arc::new(MemoryVault::new());
    let expired =
        imported_codex_bundle_for_test(&server, now_ms().expect("clock").saturating_sub(1));
    vault
        .put(&descriptor.alias, &expired.encode().expect("encode seed"))
        .expect("seed non-rotating import");
    let (broker, _) =
        independent_imported_codex_broker(&server, vault.clone() as Arc<dyn Vault>, &descriptor);
    let access = broker
        .resolve(&descriptor)
        .await
        .expect("non-rotating imported refresh");
    assert_eq!(access.expose_secret(), b"ACCESS_ROTATED_SENTINEL_3a19");
    let stored = vault
        .resolve(&descriptor.alias)
        .and_then(|stored| OAuthTokenBundleV1::decode(stored.expose_secret()))
        .expect("durable non-rotating response");
    assert_eq!(stored.generation, 2);
    assert_eq!(stored.refresh_token(), Some(REFRESH_SENTINEL.as_bytes()));
    assert_eq!(server.state.refresh_calls.load(Ordering::SeqCst), 1);
    assert!(broker.shutdown().await);
}

/// MUTATION CHECK: omit the persisted rejection marker or check it only in
/// process memory. Expected RUNTIME failure: the second independent broker
/// contacts the endpoint again or does not surface typed re-login details.
#[tokio::test]
async fn rejected_refresh_tombstones_and_surfaces_typed_relogin() {
    let server = FakeOAuthServer::start(FakeMode::InvalidGrant, false).await;
    server
        .state
        .expect_refresh_binding
        .store(false, Ordering::SeqCst);
    let directory = tempfile::tempdir().expect("temp vault");
    let first_vault = Arc::new(haider_accounts::FileVault::new(directory.path()));
    let second_vault = Arc::new(haider_accounts::FileVault::new(directory.path()));
    let descriptor = oauth_descriptor_for_test();
    let expired = oauth_bundle_for_test(&server, now_ms().expect("clock").saturating_sub(1))
        .with_refresh_after(0);
    first_vault
        .put(&descriptor.alias, &expired.encode().expect("encode seed"))
        .expect("seed rejected bundle");
    let (first, _) =
        independent_serialized_broker(&server, first_vault.clone() as Arc<dyn Vault>, &descriptor);
    let first_error = first
        .resolve(&descriptor)
        .await
        .expect_err("terminal rejection requires login");
    assert_eq!(first_error.code, ErrorCode::Unauthorized);
    assert_eq!(
        first_error
            .details
            .as_ref()
            .and_then(|details| details.get("kind"))
            .and_then(serde_json::Value::as_str),
        Some("oauth_relogin_required")
    );
    let (second, _) =
        independent_serialized_broker(&server, second_vault.clone() as Arc<dyn Vault>, &descriptor);
    let second_error = second
        .resolve(&descriptor)
        .await
        .expect_err("tombstoned token requires login");
    assert_eq!(second_error.code, ErrorCode::Unauthorized);
    assert_eq!(
        server.state.refresh_calls.load(Ordering::SeqCst),
        1,
        "a tombstoned refresh token must not be replayed"
    );
    let stored = second_vault
        .resolve(&descriptor.alias)
        .and_then(|stored| OAuthTokenBundleV1::decode(stored.expose_secret()))
        .expect("tombstoned bundle");
    assert!(
        stored
            .refresh_rejected_until_unix_ms
            .is_some_and(|until| { until > now_ms().expect("clock") })
    );
    let errors = format!("{first_error:?}{second_error:?}");
    assert!(!errors.contains(REFRESH_SENTINEL));
    assert!(first.shutdown().await);
    assert!(second.shutdown().await);
}

/// MUTATION CHECK: send the rotating request before persisting uncertainty or
/// clear uncertainty when a success response is malformed. Expected RUNTIME
/// failure: a successor replays the potentially spent refresh token instead
/// of observing the permanent fail-closed marker.
#[tokio::test]
async fn malformed_rotating_success_never_leaves_the_old_token_replayable() {
    let server = FakeOAuthServer::start(FakeMode::Malformed, false).await;
    server
        .state
        .expect_refresh_binding
        .store(false, Ordering::SeqCst);
    let directory = tempfile::tempdir().expect("temp vault");
    let first_vault = Arc::new(haider_accounts::FileVault::new(directory.path()));
    let second_vault = Arc::new(haider_accounts::FileVault::new(directory.path()));
    let descriptor = oauth_descriptor_for_test();
    let expired = oauth_bundle_for_test(&server, now_ms().expect("clock").saturating_sub(1))
        .with_refresh_after(0);
    first_vault
        .put(&descriptor.alias, &expired.encode().expect("encode seed"))
        .expect("seed malformed-response bundle");
    let (first, _) =
        independent_serialized_broker(&server, first_vault.clone() as Arc<dyn Vault>, &descriptor);
    let error = first
        .resolve(&descriptor)
        .await
        .expect_err("malformed rotating success requires login");
    assert_eq!(
        error
            .details
            .as_ref()
            .and_then(|details| details.get("kind"))
            .and_then(serde_json::Value::as_str),
        Some("oauth_relogin_required")
    );
    let stored = second_vault
        .resolve(&descriptor.alias)
        .and_then(|stored| OAuthTokenBundleV1::decode(stored.expose_secret()))
        .expect("uncertain bundle remains durable");
    assert_eq!(stored.refresh_rejected_until_unix_ms, Some(u64::MAX));

    let (second, _) =
        independent_serialized_broker(&server, second_vault as Arc<dyn Vault>, &descriptor);
    second
        .resolve(&descriptor)
        .await
        .expect_err("successor refuses the uncertain token");
    assert_eq!(
        server.state.refresh_calls.load(Ordering::SeqCst),
        1,
        "a potentially spent token is never replayed"
    );
    assert!(first.shutdown().await);
    assert!(second.shutdown().await);
}

#[tokio::test]
async fn concurrent_refresh_is_single_flight_rotates_and_persists_before_return() {
    let server = FakeOAuthServer::start(FakeMode::Success, true).await;
    let vault = Arc::new(ControlledVault::new(Arc::clone(&server.state)));
    let descriptor = oauth_descriptor_for_test();
    vault
        .put(
            &descriptor.alias,
            &oauth_bundle_for_test(&server, now_ms().expect("clock").saturating_sub(1))
                .encode()
                .expect("encode"),
        )
        .expect("seed");
    vault.arm(false);
    let (broker, _, descriptor) = broker_for(&server, vault.clone(), descriptor);
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let broker = broker.clone();
        let descriptor = descriptor.clone();
        tasks.push(tokio::spawn(
            async move { broker.resolve(&descriptor).await },
        ));
    }
    wait_for_refresh_calls(&server, 1).await;
    assert_eq!(server.state.refresh_calls.load(Ordering::SeqCst), 1);
    server.release_refresh();
    for task in tasks {
        let access = task.await.expect("join").expect("resolve");
        assert_eq!(access.expose_secret(), b"ACCESS_ROTATED_SENTINEL_3a19");
    }
    assert_eq!(server.state.refresh_calls.load(Ordering::SeqCst), 1);
    let stored = vault.resolve(&descriptor.alias).expect("stored");
    let rotated = OAuthTokenBundleV1::decode(stored.expose_secret()).expect("decode");
    assert_eq!(rotated.generation, 2);
    assert_eq!(
        rotated.refresh_token(),
        Some(b"REFRESH_ROTATED_SENTINEL_8c21".as_slice())
    );
    let resource = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("resource client")
        .get(format!("http://{}/resource", server.address))
        .bearer_auth("ACCESS_ROTATED_SENTINEL_3a19")
        .send()
        .await
        .expect("resource");
    assert!(resource.status().is_success());
    assert_eq!(
        server.state.resource_before_durable.load(Ordering::SeqCst),
        0
    );
    assert!(
        !server.state.saw_client_secret.load(Ordering::SeqCst),
        "a public OAuth refresh request must never send client_secret"
    );
}

// MUTATION CHECK (W5b.1b P1-2): restore `resolve`'s old
// `snapshot_allows_oauth || has_active_flight` admission and/or return a
// long-lived vault bundle immediately from `resolve_oauth`. The direct
// `resolve_oauth` contender represents a resolver that crossed the outer
// admission check immediately before Begin made the snapshot Expired.
// Expected failure: either contender finishes with the rotated access token
// while descriptor commit is still blocked, before the fail-closed delete.
#[tokio::test]
async fn concurrent_resolve_waits_for_rotated_descriptor_commit_or_fail_closed_delete() {
    let server = FakeOAuthServer::start(FakeMode::Success, true).await;
    let vault = Arc::new(MemoryVault::new());
    let descriptor = oauth_descriptor_for_test();
    vault
        .put(
            &descriptor.alias,
            &oauth_bundle_for_test(&server, now_ms().expect("clock").saturating_sub(1))
                .encode()
                .expect("encode"),
        )
        .expect("seed");
    let snapshot = Arc::new(Mutex::new(vec![descriptor.clone()]));
    let (commands, mut receiver) = mpsc::channel(8);
    let published = Arc::new(Semaphore::new(0));
    let release_commit = Arc::new(Semaphore::new(0));
    tokio::spawn({
        let snapshot = Arc::clone(&snapshot);
        let vault = Arc::clone(&vault);
        let published = Arc::clone(&published);
        let release_commit = Arc::clone(&release_commit);
        async move {
            while let Some(command) = receiver.recv().await {
                match command {
                    crate::accounts::AccountCommand::BeginOAuthImportHeal { completed, .. } => {
                        let _ =
                            completed.send(Ok(crate::accounts::OAuthImportHealResult::NotImported));
                    }
                    crate::accounts::AccountCommand::BeginOAuthRefresh {
                        descriptor,
                        completed,
                        ..
                    } => {
                        if let Ok(mut descriptors) = snapshot.lock()
                            && let Some(current) = descriptors
                                .iter_mut()
                                .find(|current| current.alias == descriptor.alias)
                        {
                            current.status = CredentialStatus::Expired;
                        }
                        let _ = completed.send(Ok(true));
                    }
                    crate::accounts::AccountCommand::ApplyOAuthRefresh {
                        descriptor,
                        encoded_bundle,
                        completed,
                        ..
                    } => {
                        let result = vault
                            .put(&descriptor.alias, &encoded_bundle)
                            .map_err(|_| crate::accounts::RefreshApplyError::Persist);
                        published.add_permits(1);
                        release_commit
                            .acquire()
                            .await
                            .expect("release descriptor commit")
                            .forget();
                        let _ = vault.delete(&descriptor.alias);
                        let _ = completed.send(match result {
                            Ok(()) => Err(crate::accounts::RefreshApplyError::Persist),
                            Err(error) => Err(error),
                        });
                    }
                    _ => {}
                }
            }
        }
    });
    let broker = CredentialBroker::new(
        vault.clone() as Arc<dyn Vault>,
        OAuthProviderCatalog::with_test_registrations([
            server.registration(Arc::new(FakeIdentityVerifier))
        ])
        .expect("catalog"),
        Arc::clone(&snapshot),
        commands,
    )
    .expect("broker");
    let leader = tokio::spawn({
        let broker = broker.clone();
        let descriptor = descriptor.clone();
        async move { broker.resolve(&descriptor).await }
    });
    wait_for_refresh_calls(&server, 1).await;
    server.release_refresh();
    published
        .acquire()
        .await
        .expect("rotated vault publish")
        .forget();

    let mut preadmitted_contender = tokio::spawn({
        let broker = broker.clone();
        let descriptor = descriptor.clone();
        async move { broker.resolve_oauth(&descriptor, false, None).await }
    });
    let mut public_contender = tokio::spawn({
        let broker = broker.clone();
        let descriptor = descriptor.clone();
        async move { broker.resolve(&descriptor).await }
    });
    let preadmitted_early =
        tokio::time::timeout(Duration::from_millis(100), &mut preadmitted_contender).await;
    let public_early =
        tokio::time::timeout(Duration::from_millis(100), &mut public_contender).await;
    let both_stopped_at_commit = preadmitted_early.is_err() && public_early.is_err();
    release_commit.add_permits(1);
    assert!(leader.await.expect("leader joins").is_err());
    let preadmitted_result = match preadmitted_early {
        Ok(joined) => joined,
        Err(_) => preadmitted_contender.await,
    }
    .expect("preadmitted contender joins");
    let public_result = match public_early {
        Ok(joined) => joined,
        Err(_) => public_contender.await,
    }
    .expect("public contender joins");
    assert!(
        both_stopped_at_commit,
        "neither a preadmitted nor public resolver may publish a physically written rotated bundle before descriptor commit"
    );
    assert!(
        preadmitted_result.is_err() && public_result.is_err(),
        "the failed descriptor commit must fail preadmitted and public waiters closed"
    );
    assert!(
        vault.resolve(&descriptor.alias).is_err(),
        "fail-closed completion tombstones the unpublished rotated bundle"
    );
    assert!(broker.shutdown().await);
}

// W5g-8 deliberately supersedes the old tombstone admission law: Expired is
// now a healing hint, while removal/replacement remains fenced separately.
// MUTATION CHECK: restore the old `snapshot_allows_oauth` fail-fast in
// `resolve`. Expected runtime failure: the expired manual OAuth credential is
// rejected before its existing refresh fallback can rotate the token.
#[tokio::test]
async fn expired_oauth_admission_self_heals_through_refresh() {
    let server = FakeOAuthServer::start(FakeMode::Success, false).await;
    let vault = Arc::new(ResolveCountingVault::new());
    let descriptor = oauth_descriptor_for_test();
    // Natural expiry with a CLEAN snapshot: the refresh fallback is legal
    // here. A snapshot-EXPIRED mark instead forbids replaying the
    // rotating token (W5g-8 safety split; see
    // forced_shutdown_never_retries_an_uncertain_refresh_on_successor).
    vault
        .put(
            &descriptor.alias,
            &oauth_bundle_for_test(&server, now_ms().expect("clock").saturating_sub(1))
                .encode()
                .expect("encode"),
        )
        .expect("seed");
    let (broker, _, descriptor) = broker_for(&server, vault.clone(), descriptor);

    let access = broker
        .resolve(&descriptor)
        .await
        .expect("expired descriptor refreshes");
    assert_eq!(access.expose_secret(), b"ACCESS_ROTATED_SENTINEL_3a19");
    assert!(vault.resolves.load(Ordering::SeqCst) > 0);
    assert_eq!(server.state.refresh_calls.load(Ordering::SeqCst), 1);
    assert!(broker.shutdown().await);
}

// MUTATION CHECK (W5b.1c P2): remove `RefreshFlightRegistration`, restoring
// publication of a flight before cancellable `OwnedTaskSet::spawn`
// registration. Expected failure: aborting the leader while the task lock is
// held leaves the flight mapped, so this assertion fails (and a successor
// would otherwise time out waiting for work that never became live).
#[tokio::test]
async fn cancelled_task_registration_poison_removes_the_unowned_refresh_flight() {
    let server = FakeOAuthServer::start(FakeMode::Success, true).await;
    let vault = Arc::new(MemoryVault::new());
    let descriptor = oauth_descriptor_for_test();
    vault
        .put(
            &descriptor.alias,
            &oauth_bundle_for_test(&server, now_ms().expect("clock").saturating_sub(1))
                .encode()
                .expect("encode"),
        )
        .expect("seed");
    let (broker, _, descriptor) = broker_for(&server, vault, descriptor);
    let task_registration_lock = broker.tasks.tasks.lock().await;
    let leader = tokio::spawn({
        let broker = broker.clone();
        let descriptor = descriptor.clone();
        async move { broker.resolve(&descriptor).await }
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if !broker.inner.flights.lock().expect("flights").is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("flight is published before task registration");

    leader.abort();
    let _ = leader.await;
    assert!(
        broker.inner.flights.lock().expect("flights").is_empty(),
        "cancellation before task registration must poison and remove the unowned flight"
    );
    drop(task_registration_lock);

    let successor = tokio::spawn({
        let broker = broker.clone();
        let descriptor = descriptor.clone();
        async move { broker.resolve(&descriptor).await }
    });
    wait_for_refresh_calls(&server, 1).await;
    server.release_refresh();
    let access = tokio::time::timeout(Duration::from_secs(2), successor)
        .await
        .expect("successor must not wait on the cancelled flight")
        .expect("successor join")
        .expect("successor resolve");
    assert_eq!(access.expose_secret(), b"ACCESS_ROTATED_SENTINEL_3a19");
    assert!(broker.shutdown().await);
}

// MUTATION CHECK (W5b.1b P2-3): discard `JoinError` values in
// `OwnedTaskSet::join_all`. Expected failure: the flight guard still wakes
// the resolver closed, but shutdown incorrectly returns `true` (graceful).
// MUTATION CHECK (W5b.1c P2): remove the admission seal from
// `RefreshWorkerCompletion::drop`. Expected failure: the waiter is woken
// before admission is sealed, so the seal assertion below fails and a
// replacement refresh can enter during panic unwind.
#[tokio::test]
async fn refresh_worker_panic_wakes_waiters_and_makes_graceful_shutdown_unclean() {
    let server = FakeOAuthServer::start(FakeMode::Success, false).await;
    let vault = Arc::new(MemoryVault::new());
    let descriptor = oauth_descriptor_for_test();
    vault
        .put(
            &descriptor.alias,
            &oauth_bundle_for_test(&server, now_ms().expect("clock").saturating_sub(1))
                .encode()
                .expect("encode"),
        )
        .expect("seed");
    let (broker, _, descriptor) = broker_for(&server, vault, descriptor);
    broker.panic_next_refresh_worker();
    let result = tokio::time::timeout(Duration::from_secs(2), broker.resolve(&descriptor))
        .await
        .expect("panic must not strand the single-flight waiter");
    assert!(result.is_err(), "a panicked refresh worker fails closed");
    assert!(
        broker.tasks.sealed.load(Ordering::Acquire),
        "panic cleanup must seal admission before waking the failed flight"
    );
    let replacement = tokio::time::timeout(Duration::from_secs(2), broker.resolve(&descriptor))
        .await
        .expect("sealed admission fails replacement work closed");
    assert!(
        replacement.is_err(),
        "a panic must not admit a replacement refresh worker"
    );
    assert_eq!(
        server.state.refresh_calls.load(Ordering::SeqCst),
        0,
        "neither the panicked worker nor a replacement may reach the token endpoint"
    );
    assert!(
        !broker.shutdown().await,
        "a child panic must prevent a clean graceful shutdown report"
    );
}

#[tokio::test]
async fn cancelled_resolver_does_not_abandon_or_duplicate_refresh_flight() {
    let server = FakeOAuthServer::start(FakeMode::Success, true).await;
    let vault = Arc::new(ControlledVault::new(Arc::clone(&server.state)));
    let descriptor = oauth_descriptor_for_test();
    vault
        .put(
            &descriptor.alias,
            &oauth_bundle_for_test(&server, now_ms().expect("clock").saturating_sub(1))
                .encode()
                .expect("encode"),
        )
        .expect("seed");
    vault.arm(false);
    let (broker, _, descriptor) = broker_for(&server, vault.clone(), descriptor);
    let first = {
        let broker = broker.clone();
        let descriptor = descriptor.clone();
        tokio::spawn(async move { broker.resolve(&descriptor).await })
    };
    wait_for_refresh_calls(&server, 1).await;
    first.abort();
    let waiter = {
        let broker = broker.clone();
        let descriptor = descriptor.clone();
        tokio::spawn(async move { broker.resolve(&descriptor).await })
    };
    tokio::task::yield_now().await;
    assert_eq!(
        server.state.refresh_calls.load(Ordering::SeqCst),
        1,
        "caller cancellation must not start a replacement refresh"
    );
    server.release_refresh();
    let access = waiter.await.expect("waiter join").expect("waiter resolve");
    assert_eq!(access.expose_secret(), b"ACCESS_ROTATED_SENTINEL_3a19");
    let stored = vault.resolve(&descriptor.alias).expect("stored");
    assert_eq!(
        OAuthTokenBundleV1::decode(stored.expose_secret())
            .expect("decode")
            .generation,
        2
    );
}

// MUTATION CHECK: map a lost BeginOAuthRefresh comparison directly to
// stale_refresh. Expected runtime failure: the waiter resumes with generation
// one after generation two is durable and receives the removal/replacement
// fence instead of adopting the completed refresh.
#[tokio::test]
async fn resolver_with_stale_vault_read_adopts_completed_refresh_generation() {
    let server = FakeOAuthServer::start(FakeMode::Success, true).await;
    let vault = Arc::new(StaleReadVault::new(Arc::clone(&server.state)));
    let descriptor = oauth_descriptor_for_test();
    vault
        .put(
            &descriptor.alias,
            &oauth_bundle_for_test(&server, now_ms().expect("clock").saturating_sub(1))
                .encode()
                .expect("encode"),
        )
        .expect("seed");
    let (broker, _, descriptor) = broker_for(&server, vault.clone(), descriptor);
    let first = {
        let broker = broker.clone();
        let descriptor = descriptor.clone();
        tokio::spawn(async move { broker.resolve(&descriptor).await })
    };
    wait_for_refresh_calls(&server, 1).await;
    first.abort();
    assert!(
        first
            .await
            .expect_err("first resolver is cancelled")
            .is_cancelled(),
        "the caller, not the daemon-owned refresh worker, is cancelled"
    );

    vault.arm();
    let waiter = {
        let broker = broker.clone();
        let descriptor = descriptor.clone();
        tokio::spawn(async move { broker.resolve(&descriptor).await })
    };
    tokio::time::timeout(
        Duration::from_secs(2),
        vault.wait_until_stale_read_is_captured(),
    )
    .await
    .expect("waiter captures generation one");
    assert_eq!(
        server.state.refresh_calls.load(Ordering::SeqCst),
        1,
        "the successor must not duplicate the in-flight refresh"
    );

    server.release_refresh();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !server.state.durable.load(Ordering::SeqCst)
            || !broker.inner.flights.lock().expect("flights").is_empty()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("generation two is durable and its flight is retired");
    vault.release_stale_read();

    let access = waiter.await.expect("waiter join").expect("waiter resolve");
    assert_eq!(access.expose_secret(), b"ACCESS_ROTATED_SENTINEL_3a19");
    assert_eq!(server.state.refresh_calls.load(Ordering::SeqCst), 1);
    let stored = vault.resolve(&descriptor.alias).expect("stored");
    assert_eq!(
        OAuthTokenBundleV1::decode(stored.expose_secret())
            .expect("decode")
            .generation,
        2
    );
}

#[tokio::test]
async fn shutdown_joins_an_open_callback_and_inflight_refresh_before_successor_work() {
    let server = FakeOAuthServer::start(FakeMode::Success, true).await;
    let (coordinator, mut receiver) = coordinator_for(&server, Duration::from_secs(5)).await;
    let (_, _, callback_port) = started_flow(&mut receiver).await;
    let mut slow_callback = TcpStream::connect((Ipv4Addr::LOCALHOST, callback_port))
        .await
        .expect("callback connect");
    slow_callback
        .write_all(b"GET /partial HTTP/1.1\r\n")
        .await
        .expect("partial callback");

    let vault = Arc::new(ControlledVault::new(Arc::clone(&server.state)));
    let descriptor = oauth_descriptor_for_test();
    vault
        .put(
            &descriptor.alias,
            &oauth_bundle_for_test(&server, now_ms().expect("clock").saturating_sub(1))
                .encode()
                .expect("encode"),
        )
        .expect("seed");
    vault.arm(false);
    let (broker, _, descriptor) = broker_for(&server, vault.clone(), descriptor);
    let resolver = {
        let broker = broker.clone();
        let descriptor = descriptor.clone();
        tokio::spawn(async move { broker.resolve(&descriptor).await })
    };
    wait_for_refresh_calls(&server, 1).await;
    let shutdown = {
        let coordinator = coordinator.clone();
        let broker = broker.clone();
        tokio::spawn(async move {
            tokio::join!(coordinator.shutdown(), broker.shutdown());
        })
    };
    tokio::task::yield_now().await;
    assert!(
        !shutdown.is_finished(),
        "graceful shutdown must wait for an accepted refresh to settle"
    );
    assert_eq!(
        server.state.refresh_calls.load(Ordering::SeqCst),
        1,
        "shutdown must not abandon the accepted refresh"
    );
    let mut callback_response = Vec::new();
    let callback_read = tokio::time::timeout(
        Duration::from_millis(250),
        slow_callback.read_to_end(&mut callback_response),
    )
    .await
    .expect("callback closes");
    assert!(
        callback_read.is_ok()
            || callback_read
                .as_ref()
                .is_err_and(|error| error.kind() == std::io::ErrorKind::ConnectionReset)
    );
    server.release_refresh();
    tokio::time::timeout(Duration::from_secs(1), shutdown)
        .await
        .expect("owned OAuth tasks join")
        .expect("shutdown task");
    assert!(
        coordinator.tasks.sealed.load(Ordering::Acquire),
        "coordinator task owner is sealed"
    );
    assert!(
        broker.tasks.sealed.load(Ordering::Acquire),
        "broker task owner is sealed"
    );
    let access = resolver
        .await
        .expect("resolver join")
        .expect("accepted refresh settles before shutdown");
    assert_eq!(access.expose_secret(), b"ACCESS_ROTATED_SENTINEL_3a19");

    let (successor, _, successor_descriptor) =
        broker_for(&server, vault.clone(), descriptor.clone());
    let access = successor
        .resolve(&successor_descriptor)
        .await
        .expect("successor uses the settled bundle");
    assert_eq!(access.expose_secret(), b"ACCESS_ROTATED_SENTINEL_3a19");
    assert_eq!(
        server.state.refresh_calls.load(Ordering::SeqCst),
        1,
        "Ready-of-next must not overlap the previous refresh"
    );
    successor.shutdown().await;
}

#[tokio::test]
async fn forced_task_owner_drop_aborts_a_blocked_refresh_at_the_deadline() {
    struct DropSignal(Arc<AtomicBool>);
    impl Drop for DropSignal {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    let (status_commands, _status_receiver) = mpsc::channel(1);
    let broker = CredentialBroker::new(
        Arc::new(MemoryVault::new()),
        OAuthProviderCatalog::default(),
        Arc::new(Mutex::new(Vec::new())),
        status_commands,
    )
    .expect("broker");
    let dropped = Arc::new(AtomicBool::new(false));
    let signal = DropSignal(Arc::clone(&dropped));
    assert!(
        broker
            .tasks
            .spawn(async move {
                let _signal = signal;
                std::future::pending::<()>().await;
            })
            .await
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(10), broker.shutdown())
            .await
            .is_err(),
        "the synthetic worker holds graceful join until the barrier deadline"
    );
    broker.abort_and_join().await;
    assert!(
        dropped.load(Ordering::SeqCst),
        "forced barrier completion aborts and joins the blocked worker"
    );
    assert!(broker.tasks.sealed.load(Ordering::Acquire));
}

#[tokio::test]
async fn forced_shutdown_never_retries_an_uncertain_refresh_on_successor() {
    let server = FakeOAuthServer::start(FakeMode::Success, true).await;
    let vault = Arc::new(ControlledVault::new(Arc::clone(&server.state)));
    let descriptor = oauth_descriptor_for_test();
    vault
        .put(
            &descriptor.alias,
            &oauth_bundle_for_test(&server, now_ms().expect("clock").saturating_sub(1))
                .encode()
                .expect("encode"),
        )
        .expect("seed");
    vault.arm(false);
    let (broker, snapshot, descriptor) = broker_for(&server, vault.clone(), descriptor);
    let resolver = {
        let broker = broker.clone();
        let descriptor = descriptor.clone();
        tokio::spawn(async move { broker.resolve(&descriptor).await })
    };
    wait_for_refresh_calls(&server, 1).await;
    assert!(matches!(
        snapshot.lock().expect("snapshot")[0].status,
        CredentialStatus::Expired
    ));

    broker.abort_and_join().await;
    resolver
        .await
        .expect("resolver join")
        .expect_err("forced barrier wakes the refresh waiter");
    server.release_refresh();

    let successor = CredentialBroker::new(
        vault.clone() as Arc<dyn Vault>,
        OAuthProviderCatalog::with_test_registrations([
            server.registration(Arc::new(FakeIdentityVerifier))
        ])
        .expect("successor catalog"),
        Arc::clone(&snapshot),
        start_status_actor(&snapshot, vault),
    )
    .expect("successor broker");
    successor
        .resolve(&descriptor)
        .await
        .expect_err("restart refuses the uncertain old refresh token");
    assert_eq!(
        server.state.refresh_calls.load(Ordering::SeqCst),
        1,
        "successor must not replay the pre-barrier refresh token"
    );
    successor.shutdown().await;
}

#[tokio::test]
async fn invalid_grant_marks_expired_and_reports_auth_rotation_without_fake_deadline() {
    let server = FakeOAuthServer::start(FakeMode::InvalidGrant, false).await;
    let vault = Arc::new(ControlledVault::new(Arc::clone(&server.state)));
    let descriptor = oauth_descriptor_for_test();
    vault
        .put(
            &descriptor.alias,
            &oauth_bundle_for_test(&server, now_ms().expect("clock").saturating_sub(1))
                .encode()
                .expect("encode"),
        )
        .expect("seed");
    vault.arm(false);
    let (broker, snapshot, descriptor) = broker_for(&server, vault.clone(), descriptor);
    let error = broker
        .resolve(&descriptor)
        .await
        .expect_err("invalid grant");
    assert_eq!(
        error
            .details
            .as_ref()
            .and_then(|details| details.get("rotation_trigger"))
            .and_then(serde_json::Value::as_str),
        Some("auth_expired")
    );
    assert_eq!(
        error
            .details
            .as_ref()
            .and_then(|details| details.get("rotation_cause"))
            .and_then(serde_json::Value::as_str),
        Some("error")
    );
    assert!(
        error
            .details
            .as_ref()
            .is_none_or(|details| details.get("until_ms").is_none())
    );
    assert!(matches!(
        snapshot.lock().expect("snapshot")[0].status,
        CredentialStatus::Expired
    ));
    assert!(!format!("{error:?}").contains(RAW_ERROR_SENTINEL));
}

#[tokio::test]
async fn refresh_wrong_issuer_is_terminal_and_never_overwrites_the_stored_bundle() {
    let server = FakeOAuthServer::start(FakeMode::RefreshIssuerMismatch, false).await;
    let vault = Arc::new(ControlledVault::new(Arc::clone(&server.state)));
    let descriptor = oauth_descriptor_for_test();
    let original = oauth_bundle_for_test(&server, now_ms().expect("clock").saturating_sub(1));
    vault
        .put(&descriptor.alias, &original.encode().expect("encode"))
        .expect("seed");
    vault.arm(false);
    let (broker, snapshot, descriptor) = broker_for(&server, vault.clone(), descriptor);
    let error = broker
        .resolve(&descriptor)
        .await
        .expect_err("issuer mismatch must fail");
    assert!(!error.retryable);
    let stored = vault.resolve(&descriptor.alias).expect("original remains");
    let stored = OAuthTokenBundleV1::decode(stored.expose_secret()).expect("decode original");
    assert_eq!(stored.generation, 1);
    assert_eq!(stored.access_token(), b"ACCESS_OLD_SENTINEL_914d");
    assert!(matches!(
        snapshot.lock().expect("snapshot")[0].status,
        CredentialStatus::Expired
    ));
}

#[tokio::test]
async fn refresh_audience_and_resource_mismatches_are_terminal() {
    for mode in [
        FakeMode::RefreshAudienceMismatch,
        FakeMode::RefreshResourceMismatch,
    ] {
        let server = FakeOAuthServer::start(mode, false).await;
        let vault = Arc::new(ControlledVault::new(Arc::clone(&server.state)));
        let descriptor = oauth_descriptor_for_test();
        vault
            .put(
                &descriptor.alias,
                &oauth_bundle_for_test(&server, now_ms().expect("clock").saturating_sub(1))
                    .encode()
                    .expect("encode"),
            )
            .expect("seed");
        vault.arm(false);
        let (broker, snapshot, descriptor) = broker_for(&server, vault.clone(), descriptor);
        broker
            .resolve(&descriptor)
            .await
            .expect_err("binding mismatch must fail");
        let stored = vault.resolve(&descriptor.alias).expect("original remains");
        assert_eq!(
            OAuthTokenBundleV1::decode(stored.expose_secret())
                .expect("decode")
                .generation,
            1
        );
        assert!(matches!(
            snapshot.lock().expect("snapshot")[0].status,
            CredentialStatus::Expired
        ));
    }
}

#[tokio::test]
async fn refresh_rejects_registration_binding_drift_from_the_original_bundle() {
    let server = FakeOAuthServer::start(FakeMode::Success, false).await;
    let registration = server.registration(Arc::new(FakeIdentityVerifier));
    let response = serde_json::json!({
        "access_token": "ROTATED_BINDING_ACCESS",
        "refresh_token": "ROTATED_BINDING_REFRESH",
        "token_type": "Bearer",
        "expires_in": 120,
        "refresh_expires_in": 3600,
        "scope": SCOPES
    })
    .to_string();

    let mut audience_drift =
        oauth_bundle_for_test(&server, now_ms().expect("clock").saturating_sub(1));
    audience_drift.audience = "previous-audience".into();
    assert_eq!(
        refresh_bundle_from_response(&registration, response.as_bytes(), &audience_drift)
            .expect_err("current metadata cannot rebind an old audience")
            .code,
        "invalid_token_response"
    );

    let mut resource_drift =
        oauth_bundle_for_test(&server, now_ms().expect("clock").saturating_sub(1));
    resource_drift.resource = Some("previous-resource".into());
    assert_eq!(
        refresh_bundle_from_response(&registration, response.as_bytes(), &resource_drift)
            .expect_err("current metadata cannot rebind an old resource")
            .code,
        "invalid_token_response"
    );
}

#[tokio::test]
async fn refresh_omission_retains_the_token_and_known_finite_expiry() {
    let server = FakeOAuthServer::start(FakeMode::RefreshOmitTokenAndExpiry, false).await;
    let registration = server.registration(Arc::new(FakeIdentityVerifier));
    let prior = oauth_bundle_for_test(&server, now_ms().expect("clock").saturating_sub(1));
    let prior_expiry = prior.refresh_expires_at_unix_ms;
    let response = serde_json::json!({
        "access_token": "ROTATED_ACCESS",
        "token_type": "Bearer",
        "expires_in": 120,
        "scope": SCOPES
    })
    .to_string();
    let refreshed = refresh_bundle_from_response(&registration, response.as_bytes(), &prior)
        .expect("retained refresh");
    assert_eq!(refreshed.refresh_token(), prior.refresh_token());
    assert_eq!(refreshed.refresh_expires_at_unix_ms, prior_expiry);
    assert_eq!(refreshed.generation, 2);
}

#[tokio::test]
async fn refresh_omission_reject_policy_never_erases_refreshability() {
    let server = FakeOAuthServer::start(FakeMode::Success, false).await;
    let mut registration = server.registration(Arc::new(FakeIdentityVerifier));
    registration.retain_refresh_on_omission = false;
    let prior = oauth_bundle_for_test(&server, now_ms().expect("clock").saturating_sub(1));
    let response = serde_json::json!({
        "access_token": "ROTATED_ACCESS",
        "token_type": "Bearer",
        "expires_in": 120,
        "scope": SCOPES
    })
    .to_string();
    assert_eq!(
        refresh_bundle_from_response(&registration, response.as_bytes(), &prior)
            .expect_err("missing refresh token must reject")
            .code,
        "missing_refresh_token"
    );
}

#[tokio::test]
async fn transient_refresh_fails_closed_for_leader_and_waiter_after_uncertain_request() {
    let server = FakeOAuthServer::start(FakeMode::Transient, false).await;
    let vault = Arc::new(ControlledVault::new(Arc::clone(&server.state)));
    let descriptor = oauth_descriptor_for_test();
    vault
        .put(
            &descriptor.alias,
            &oauth_bundle_for_test(&server, now_ms().expect("clock").saturating_add(10_000))
                .encode()
                .expect("encode"),
        )
        .expect("seed");
    vault.arm(false);
    let (broker, snapshot, descriptor) = broker_for(&server, vault, descriptor);
    let (first, second) = tokio::join!(broker.resolve(&descriptor), broker.resolve(&descriptor));
    for error in [
        first.expect_err("leader fails closed"),
        second.expect_err("waiter fails closed"),
    ] {
        assert!(!error.retryable);
    }
    assert_eq!(server.state.refresh_calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        snapshot.lock().expect("snapshot")[0].status,
        CredentialStatus::Expired
    ));
}

/// MUTATION CHECK: propagate the bounded endpoint body, access token, or
/// refresh token into the broker error. Expected RUNTIME failure: formatting
/// the only externally observable failure below reveals a sentinel (OAuth
/// refresh does not write a journal or log record).
#[tokio::test]
async fn no_secret_bytes_in_errors_journal_or_logs() {
    let server = FakeOAuthServer::start(FakeMode::Transient, false).await;
    let vault = Arc::new(ControlledVault::new(Arc::clone(&server.state)));
    let descriptor = oauth_descriptor_for_test();
    vault
        .put(
            &descriptor.alias,
            &oauth_bundle_for_test(&server, now_ms().expect("clock").saturating_sub(1))
                .encode()
                .expect("encode"),
        )
        .expect("seed");
    vault.arm(false);
    let (broker, _, descriptor) = broker_for(&server, vault, descriptor);
    let error = broker
        .resolve(&descriptor)
        .await
        .expect_err("expired access cannot fall back");
    assert!(error.retryable);
    let formatted = format!("{error:?}");
    assert!(!formatted.contains(RAW_BODY_SENTINEL));
    assert!(!formatted.contains(ACCESS_SENTINEL));
    assert!(!formatted.contains(REFRESH_SENTINEL));
}

#[tokio::test]
async fn expired_refresh_token_fails_without_contacting_token_endpoint() {
    let server = FakeOAuthServer::start(FakeMode::Success, false).await;
    let vault = Arc::new(ControlledVault::new(Arc::clone(&server.state)));
    let descriptor = oauth_descriptor_for_test();
    let now = now_ms().expect("clock");
    let mut bundle = oauth_bundle_for_test(&server, now.saturating_sub(1));
    bundle.refresh_expires_at_unix_ms = Some(now.saturating_sub(1));
    vault
        .put(&descriptor.alias, &bundle.encode().expect("encode"))
        .expect("seed");
    vault.arm(false);
    let (broker, snapshot, descriptor) = broker_for(&server, vault.clone(), descriptor);
    let error = broker
        .resolve(&descriptor)
        .await
        .expect_err("expired refresh");
    assert_eq!(server.state.token_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        error
            .details
            .as_ref()
            .and_then(|details| details.get("rotation_trigger"))
            .and_then(serde_json::Value::as_str),
        Some("auth_expired")
    );
    assert!(matches!(
        snapshot.lock().expect("snapshot")[0].status,
        CredentialStatus::Expired
    ));
}

#[tokio::test]
async fn refresh_redirect_is_not_followed_and_is_terminal() {
    let server = FakeOAuthServer::start(FakeMode::TokenRedirect, false).await;
    let vault = Arc::new(ControlledVault::new(Arc::clone(&server.state)));
    let descriptor = oauth_descriptor_for_test();
    vault
        .put(
            &descriptor.alias,
            &oauth_bundle_for_test(&server, now_ms().expect("clock").saturating_sub(1))
                .encode()
                .expect("encode"),
        )
        .expect("seed");
    vault.arm(false);
    let (broker, snapshot, descriptor) = broker_for(&server, vault, descriptor);
    let error = broker
        .resolve(&descriptor)
        .await
        .expect_err("redirect must fail");
    assert!(!error.retryable);
    assert_eq!(server.state.redirect_target_calls.load(Ordering::SeqCst), 0);
    assert!(matches!(
        snapshot.lock().expect("snapshot")[0].status,
        CredentialStatus::Expired
    ));
}

#[tokio::test]
async fn refresh_malformed_and_oversized_responses_fail_closed() {
    for mode in [FakeMode::Malformed, FakeMode::Oversized] {
        let server = FakeOAuthServer::start(mode, false).await;
        let vault = Arc::new(ControlledVault::new(Arc::clone(&server.state)));
        let descriptor = oauth_descriptor_for_test();
        vault
            .put(
                &descriptor.alias,
                &oauth_bundle_for_test(&server, now_ms().expect("clock").saturating_sub(1))
                    .encode()
                    .expect("encode"),
            )
            .expect("seed");
        vault.arm(false);
        let (broker, snapshot, descriptor) = broker_for(&server, vault.clone(), descriptor);
        broker
            .resolve(&descriptor)
            .await
            .expect_err("malformed refresh response must fail");
        assert!(matches!(
            snapshot.lock().expect("snapshot")[0].status,
            CredentialStatus::Expired
        ));
        let stored = vault
            .resolve(&descriptor.alias)
            .expect("old bundle remains");
        assert_eq!(
            OAuthTokenBundleV1::decode(stored.expose_secret())
                .expect("decode")
                .generation,
            1
        );
    }
}

#[test]
fn token_response_source_chunks_are_exclusively_owned_and_scrubbed() {
    let source = include_str!("oauth.rs");
    assert!(
        source.contains(".try_into_mut()")
            && source.contains("drop(response);")
            && source
                .matches("reqwest::header::CONNECTION, \"close\"")
                .count()
                // Five key-bearing transports must each close: authorization-code
                // exchange, refresh, the W5b.2a JWKS fetch, and the B6k Kimi
                // device-authorization and device-token-poll requests (both feed
                // bounded_response, so their source chunks join the same scrub).
                == 5
            && source.contains("chunk.as_mut().zeroize();"),
        "token and JWKS transports must close before every source chunk is mutable-owned and scrubbed"
    );
}

/// MUTATION CHECK: restore abort-on-shared in `scrub_source_chunks`
/// (`Err(_) => std::process::abort()`). Expected runtime failure: this test
/// binary dies SIGABRT on the held-clone segment below.
#[tokio::test]
async fn shared_source_chunk_is_scrubbed_late_or_left_bounded_never_process_death() {
    // Exclusive chunks scrub on the first sweep.
    let exclusive = vec![
        bytes::Bytes::from(b"CHUNK_SECRET_A".to_vec()),
        bytes::Bytes::from(b"CHUNK_SECRET_B".to_vec()),
    ];
    assert_eq!(scrub_source_chunks(exclusive).await, 0);
    // A transiently shared chunk (production: hyper's connection task still
    // holds its read-buffer reference) scrubs once the sibling drops.
    let chunk = bytes::Bytes::from(b"CHUNK_SECRET_C".to_vec());
    let sibling = chunk.clone();
    let scrub = tokio::spawn(scrub_source_chunks(vec![chunk]));
    tokio::task::yield_now().await;
    drop(sibling);
    assert_eq!(scrub.await.expect("scrub task"), 0);
    // A chunk that never becomes exclusive is a bounded residual — the
    // daemon survives (previously: std::process::abort).
    let chunk = bytes::Bytes::from(b"CHUNK_SECRET_D".to_vec());
    let _held = chunk.clone();
    assert_eq!(scrub_source_chunks(vec![chunk]).await, 1);
}

#[tokio::test]
async fn auth_aware_broker_keeps_api_keys_raw_and_never_treats_bundle_as_bearer() {
    let server = FakeOAuthServer::start(FakeMode::Success, false).await;
    let vault = Arc::new(MemoryVault::new());
    let descriptor = CredentialDescriptor {
        alias: CredentialAlias::new("fake-api"),
        provider: "fake-oauth".into(),
        base_url: None,
        auth_method: AuthMethod::ApiKey,
        identity: "api fixture".into(),
        status: CredentialStatus::Ok,
        active: true,
        label: None,
        account_identity: None,
        created_at_ms: None,
    };
    vault
        .put(&descriptor.alias, b"API_KEY_BROKER_SENTINEL_20ac")
        .expect("seed API key");
    let (broker, _, descriptor) = broker_for(&server, vault, descriptor);
    let resolved = broker.resolve(&descriptor).await.expect("resolve API key");
    assert_eq!(resolved.expose_secret(), b"API_KEY_BROKER_SENTINEL_20ac");
}

#[tokio::test]
async fn refresh_vault_failure_never_returns_rotated_access() {
    let server = FakeOAuthServer::start(FakeMode::Success, false).await;
    let vault = Arc::new(ControlledVault::new(Arc::clone(&server.state)));
    let descriptor = oauth_descriptor_for_test();
    vault
        .put(
            &descriptor.alias,
            &oauth_bundle_for_test(&server, now_ms().expect("clock").saturating_sub(1))
                .encode()
                .expect("encode"),
        )
        .expect("seed");
    vault.arm(true);
    let (broker, snapshot, descriptor) = broker_for(&server, vault.clone(), descriptor);
    let error = broker
        .resolve(&descriptor)
        .await
        .expect_err("vault failure must fail closed");
    assert_eq!(
        error
            .details
            .as_ref()
            .and_then(|details| details.get("rotation_trigger"))
            .and_then(serde_json::Value::as_str),
        Some("refresh_failed")
    );
    assert!(!server.state.durable.load(Ordering::SeqCst));
    assert!(matches!(
        snapshot.lock().expect("snapshot")[0].status,
        CredentialStatus::Expired
    ));
    assert!(
        vault.resolve(&descriptor.alias).is_err(),
        "the server-invalidated refresh token must be tombstoned"
    );
}

#[tokio::test]
async fn late_refresh_completion_after_remove_is_generation_fenced() {
    let server = FakeOAuthServer::start(FakeMode::Success, true).await;
    let vault = Arc::new(ControlledVault::new(Arc::clone(&server.state)));
    let descriptor = oauth_descriptor_for_test();
    vault
        .put(
            &descriptor.alias,
            &oauth_bundle_for_test(&server, now_ms().expect("clock").saturating_sub(1))
                .encode()
                .expect("encode"),
        )
        .expect("seed");
    vault.arm(false);
    let (broker, snapshot, descriptor) = broker_for(&server, vault.clone(), descriptor);
    let running = {
        let broker = broker.clone();
        let descriptor = descriptor.clone();
        tokio::spawn(async move { broker.resolve(&descriptor).await })
    };
    wait_for_refresh_calls(&server, 1).await;
    broker.invalidate(&descriptor.alias);
    snapshot.lock().expect("snapshot").clear();
    vault.delete(&descriptor.alias).expect("remove vault value");
    server.release_refresh();
    let error = running
        .await
        .expect("join")
        .expect_err("late completion must be fenced");
    assert_eq!(error.code, ErrorCode::Unauthorized);
    assert!(vault.resolve(&descriptor.alias).is_err());
    assert!(!server.state.durable.load(Ordering::SeqCst));
}

#[tokio::test]
async fn late_refresh_completion_cannot_overwrite_a_newer_bundle_generation() {
    let server = FakeOAuthServer::start(FakeMode::Success, true).await;
    let vault = Arc::new(ControlledVault::new(Arc::clone(&server.state)));
    let descriptor = oauth_descriptor_for_test();
    vault
        .put(
            &descriptor.alias,
            &oauth_bundle_for_test(&server, now_ms().expect("clock").saturating_sub(1))
                .encode()
                .expect("encode"),
        )
        .expect("seed");
    vault.arm(false);
    let (broker, _, descriptor) = broker_for(&server, vault.clone(), descriptor);
    let running = {
        let broker = broker.clone();
        let descriptor = descriptor.clone();
        tokio::spawn(async move { broker.resolve(&descriptor).await })
    };
    wait_for_refresh_calls(&server, 1).await;

    let replacement = OAuthTokenBundleV1::new(
        "fake-oauth".into(),
        server.state.issuer.clone(),
        AUDIENCE.into(),
        Some("fake-api-resource".into()),
        "Bearer".into(),
        Zeroizing::new(b"NEWER_ACCESS_SENTINEL_88c0".to_vec()),
        Some(Zeroizing::new(b"NEWER_REFRESH_SENTINEL_91c2".to_vec())),
        now_ms().expect("clock").saturating_add(3_600_000),
        None,
        SCOPES.split_ascii_whitespace().map(str::to_owned).collect(),
        OAuthIdentityV1 {
            subject_hash: "replacement-subject-hash".into(),
            display_identity: "person@example.invalid".into(),
        },
        9,
    )
    .expect("replacement bundle");
    vault
        .put(
            &descriptor.alias,
            &replacement.encode().expect("encode replacement"),
        )
        .expect("store replacement");

    server.release_refresh();
    let error = running
        .await
        .expect("join")
        .expect_err("late completion must be fenced");
    assert_eq!(error.code, ErrorCode::Unauthorized);
    let stored = vault
        .resolve(&descriptor.alias)
        .expect("replacement retained");
    let stored = OAuthTokenBundleV1::decode(stored.expose_secret()).expect("decode replacement");
    assert_eq!(stored.generation, 9);
    assert_eq!(stored.access_token(), b"NEWER_ACCESS_SENTINEL_88c0");
}

#[tokio::test]
async fn late_refresh_failure_after_remove_readd_cannot_expire_the_replacement() {
    let server = FakeOAuthServer::start(FakeMode::InvalidGrant, true).await;
    let vault = Arc::new(ControlledVault::new(Arc::clone(&server.state)));
    let descriptor = oauth_descriptor_for_test();
    vault
        .put(
            &descriptor.alias,
            &oauth_bundle_for_test(&server, now_ms().expect("clock").saturating_sub(1))
                .encode()
                .expect("encode"),
        )
        .expect("seed");
    vault.arm(false);
    let (broker, snapshot, descriptor) = broker_for(&server, vault.clone(), descriptor);
    let running = {
        let broker = broker.clone();
        let descriptor = descriptor.clone();
        tokio::spawn(async move { broker.resolve(&descriptor).await })
    };
    wait_for_refresh_calls(&server, 1).await;

    broker.invalidate(&descriptor.alias);
    let replacement = CredentialDescriptor {
        alias: descriptor.alias.clone(),
        provider: "replacement-provider".into(),
        base_url: Some("https://replacement.invalid".into()),
        auth_method: AuthMethod::ApiKey,
        identity: "replacement identity".into(),
        status: CredentialStatus::Ok,
        active: true,
        label: None,
        account_identity: None,
        created_at_ms: None,
    };
    *snapshot.lock().expect("snapshot") = vec![replacement.clone()];
    vault
        .put(&descriptor.alias, b"REPLACEMENT_API_KEY_SENTINEL_a42f")
        .expect("replacement vault");
    server.release_refresh();
    running
        .await
        .expect("join")
        .expect_err("old refresh must fail");
    assert_eq!(snapshot.lock().expect("snapshot")[0], replacement);
    assert_eq!(
        vault
            .resolve(&descriptor.alias)
            .expect("replacement retained")
            .expose_secret(),
        b"REPLACEMENT_API_KEY_SENTINEL_a42f"
    );
}

#[tokio::test]
async fn late_refresh_failure_cannot_expire_a_newer_same_alias_generation() {
    let server = FakeOAuthServer::start(FakeMode::InvalidGrant, true).await;
    let vault = Arc::new(ControlledVault::new(Arc::clone(&server.state)));
    let descriptor = oauth_descriptor_for_test();
    vault
        .put(
            &descriptor.alias,
            &oauth_bundle_for_test(&server, now_ms().expect("clock").saturating_sub(1))
                .encode()
                .expect("encode"),
        )
        .expect("seed");
    vault.arm(false);
    let (broker, snapshot, descriptor) = broker_for(&server, vault.clone(), descriptor);
    let running = {
        let broker = broker.clone();
        let descriptor = descriptor.clone();
        tokio::spawn(async move { broker.resolve(&descriptor).await })
    };
    wait_for_refresh_calls(&server, 1).await;
    let replacement = OAuthTokenBundleV1::new(
        "fake-oauth".into(),
        server.state.issuer.clone(),
        AUDIENCE.into(),
        Some("fake-api-resource".into()),
        "Bearer".into(),
        Zeroizing::new(b"NEWER_ACCESS_AFTER_FAILURE_12a9".to_vec()),
        Some(Zeroizing::new(b"NEWER_REFRESH_AFTER_FAILURE_b718".to_vec())),
        now_ms().expect("clock").saturating_add(3_600_000),
        Some(now_ms().expect("clock").saturating_add(7_200_000)),
        SCOPES.split_ascii_whitespace().map(str::to_owned).collect(),
        OAuthIdentityV1 {
            subject_hash: "fake-subject-hash".into(),
            display_identity: descriptor.identity.clone(),
        },
        9,
    )
    .expect("replacement");
    vault
        .put(
            &descriptor.alias,
            &replacement.encode().expect("encode replacement"),
        )
        .expect("store replacement");
    snapshot.lock().expect("snapshot")[0].status = CredentialStatus::Ok;
    server.release_refresh();
    running
        .await
        .expect("join")
        .expect_err("old refresh must fail");
    assert!(matches!(
        snapshot.lock().expect("snapshot")[0].status,
        CredentialStatus::Ok
    ));
    let stored = vault
        .resolve(&descriptor.alias)
        .expect("replacement retained");
    let stored = OAuthTokenBundleV1::decode(stored.expose_secret()).expect("decode replacement");
    assert_eq!(stored.generation, 9);
    assert_eq!(stored.access_token(), b"NEWER_ACCESS_AFTER_FAILURE_12a9");
}

/// MUTATION CHECK: collapse the per-provider redirect branch (every
/// provider hardened, or every provider parity), or decouple the listener
/// authority from the registered redirect authority. Expected RUNTIME
/// failure: one of the exact shapes below, or an authority that is not the
/// exact `host:port` of its own `uri`.
#[test]
fn anthropic_redirect_is_claude_code_parity_and_others_stay_hardened() {
    let (path, uri, authority) = compose_redirect(
        haider_provider::ANTHROPIC_OAUTH_PROVIDER_NAME,
        58820,
        "SEGMENT",
    );
    assert_eq!(path, "/callback");
    assert_eq!(uri, "http://localhost:58820/callback");
    assert_eq!(authority, "localhost:58820");
    let (path, uri, authority) = compose_redirect("openai-oauth", 58820, "SEGMENT");
    assert_eq!(path, "/oauth/callback/SEGMENT");
    assert_eq!(uri, "http://127.0.0.1:58820/oauth/callback/SEGMENT");
    assert_eq!(authority, "127.0.0.1:58820");
}

/// The flow TTL is generous by design: the user is off reading the
/// provider's consent page (often logging in and completing 2FA first).
/// MUTATION CHECK: re-tie `flow_ttl` to the 5-minute staged-secret TTL.
/// Expected RUNTIME failure: the ten-minute floor below.
#[test]
fn default_flow_ttl_is_at_least_ten_minutes() {
    assert!(OAuthCoordinatorConfig::default().flow_ttl >= Duration::from_secs(600));
}
