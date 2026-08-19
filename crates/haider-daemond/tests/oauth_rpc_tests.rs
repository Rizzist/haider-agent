//! W5b OAuth acceptance over the real production UDS and real loopback TCP.
//!
//! The only enabled provider is an injected, public-client fake. No live
//! provider metadata or endpoint is used by this suite.

#![allow(clippy::expect_used)]

mod support;

use std::collections::HashMap;
use std::fmt::Write as _;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use haider_accounts::{MemoryVault, OAuthIdentityV1, OAuthTokenBundleV1, Vault};
use haider_daemon::{
    AccountProviderBuilder, AccountsDependencies, DaemonConfig, DaemonDependencies, DaemonState,
    DaemonTask, OAuthCoordinatorConfig, OAuthIdentityExpectation, OAuthIdentityVerifier,
    OAuthProviderCatalog, OAuthProviderRegistration, OAuthPublicError, ProviderFactoryConfig,
    VaultProvision,
};
use haider_protocol::EventPayload;
use haider_protocol::credential::{AuthMethod, CredentialStatus};
use haider_provider::{FakeProvider, FakeStep, Provider};
use haider_rpc::{
    AccountAddMethod, AttachMode, Capability, CapabilitySet, ClientKind, CommandId,
    DEFAULT_FRAME_LIMIT, ERROR_CODE_CAPABILITY_DENIED, ERROR_CODE_OAUTH_FLOW_NOT_FOUND,
    OAuthFlowId, OAuthFlowStatusWire, OAuthReadyRefWire, RequestBody, RequestId, ResponseBody,
    WireFrame,
};
use haider_tui::app::{AppEvent, AppModel};
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use sha2::{Digest, Sha256};
use support::{DEADLINE, UdsClient, ready_with_dependencies, test_root};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use url::Url;
use zeroize::Zeroizing;

const CODE: &str = "OAUTH_CODE_SWEEP_812f";
const ACCESS: &str = "OAUTH_ACCESS_SWEEP_17a3";
const REFRESH: &str = "OAUTH_REFRESH_SWEEP_6d51";
const ID_MARKER: &str = "OAUTH_ID_SWEEP_9bc4";
const RAW_ERROR: &str = "OAUTH_RAW_ERROR_SWEEP_40e8";
const ROTATED_ACCESS: &str = "OAUTH_ROTATED_BARRIER_ACCESS_f1a7";
const ROTATED_REFRESH: &str = "OAUTH_ROTATED_BARRIER_REFRESH_0c42";
const SCOPES: &str = "openid inference profile";
const LIMIT: usize = DEFAULT_FRAME_LIMIT;

#[derive(Clone)]
struct TraceCapture {
    output: Arc<Mutex<String>>,
    next_span: Arc<AtomicU64>,
}

struct TraceFields<'a>(&'a Arc<Mutex<String>>);

impl tracing::field::Visit for TraceFields<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if let Ok(mut output) = self.0.lock() {
            let _ = write!(output, "{}={value:?};", field.name());
        }
    }
}

impl tracing::Subscriber for TraceCapture {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, attributes: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        attributes.record(&mut TraceFields(&self.output));
        tracing::span::Id::from_u64(self.next_span.fetch_add(1, Ordering::Relaxed).max(1))
    }

    fn record(&self, _span: &tracing::span::Id, values: &tracing::span::Record<'_>) {
        values.record(&mut TraceFields(&self.output));
    }

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        if let Ok(mut output) = self.output.lock() {
            let _ = write!(
                output,
                "target={};name={};",
                event.metadata().target(),
                event.metadata().name()
            );
        }
        event.record(&mut TraceFields(&self.output));
    }

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}
}

fn assert_tree_secret_free(
    root: &std::path::Path,
    _profile_lock: &std::path::Path,
    sentinels: &[&str],
) -> (usize, bool) {
    let mut scanned = 0_usize;
    let mut saw_wal = false;
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        for entry in std::fs::read_dir(&directory).expect("read store") {
            let entry = entry.expect("entry");
            let path = entry.path();
            let file_type = entry.file_type().expect("entry type");
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            #[cfg(windows)]
            if path == _profile_lock {
                // The singleton authority is a deliberately empty lock target;
                // diagnostics live in `lock.owner`. Windows mandatory locking
                // forbids reading it while the daemon is Ready, so exclude only
                // this exact sentinel and keep every readable artifact in the
                // secret-hygiene sweep.
                assert_eq!(
                    entry.metadata().expect("profile lock metadata").len(),
                    0,
                    "profile lock must remain empty during secret sweep"
                );
                continue;
            }
            saw_wal |= path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with("-wal"));
            let bytes = std::fs::read(&path).expect("read file");
            for sentinel in sentinels {
                assert!(
                    !bytes
                        .windows(sentinel.len())
                        .any(|window| window == sentinel.as_bytes()),
                    "OAuth sentinel leaked into {}",
                    path.display()
                );
            }
            scanned += 1;
        }
    }
    (scanned, saw_wal)
}

fn render_real_tui_login_surface(secret: &str) -> String {
    let mut model = AppModel::new();
    model.handle(AppEvent::Envelope(Box::new(
        haider_protocol::EventPayload::HarnessStatus(haider_protocol::state::HarnessStatus::Ready),
    )));
    for character in "/login fake-oauth api work".chars() {
        model.handle(AppEvent::Key(KeyEvent::new(
            KeyCode::Char(character),
            KeyModifiers::NONE,
        )));
    }
    model.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
    assert!(model.login.is_some(), "the real login card must be open");
    for character in secret.chars() {
        model.handle(AppEvent::Key(KeyEvent::new(
            KeyCode::Char(character),
            KeyModifiers::NONE,
        )));
    }
    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).expect("TUI test terminal");
    terminal
        .draw(|frame| {
            render(&model, frame);
        })
        .expect("render real TUI surface");
    let buffer = terminal.backend().buffer();
    let mut output = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            output.push_str(buffer[(x, y)].symbol());
        }
        output.push('\n');
    }
    output
}

struct BlockingRotationVault {
    inner: MemoryVault,
    armed: AtomicBool,
    entered: AtomicBool,
    cleartext_live: AtomicBool,
    release: (Mutex<bool>, Condvar),
}

impl BlockingRotationVault {
    fn new() -> Self {
        Self {
            inner: MemoryVault::new(),
            armed: AtomicBool::new(false),
            entered: AtomicBool::new(false),
            cleartext_live: AtomicBool::new(false),
            release: (Mutex::new(false), Condvar::new()),
        }
    }

    fn arm(&self) {
        if let Ok(mut released) = self.release.0.lock() {
            *released = false;
        }
        self.entered.store(false, Ordering::Release);
        self.armed.store(true, Ordering::Release);
    }

    fn release(&self) {
        if let Ok(mut released) = self.release.0.lock() {
            *released = true;
            self.release.1.notify_all();
        }
    }
}

impl Vault for BlockingRotationVault {
    fn put(
        &self,
        alias: &haider_protocol::ids::CredentialAlias,
        secret: &[u8],
    ) -> haider_accounts::AccountsResult<()> {
        let rotated = self.armed.load(Ordering::Acquire)
            && OAuthTokenBundleV1::decode(secret).is_ok_and(|bundle| bundle.generation > 1);
        self.inner.put(alias, secret)?;
        if rotated {
            // The physical write has happened, and the closure still owns the
            // encoded cleartext bytes. This is the exact non-cancellable
            // `spawn_blocking` boundary the runtime must join transitively.
            self.cleartext_live.store(true, Ordering::Release);
            self.entered.store(true, Ordering::Release);
            let mut released = self
                .release
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while !*released {
                released = self
                    .release
                    .1
                    .wait(released)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            self.cleartext_live.store(false, Ordering::Release);
        }
        Ok(())
    }

    fn resolve(
        &self,
        alias: &haider_protocol::ids::CredentialAlias,
    ) -> haider_accounts::AccountsResult<haider_accounts::SecretHandle> {
        self.inner.resolve(alias)
    }

    fn delete(
        &self,
        alias: &haider_protocol::ids::CredentialAlias,
    ) -> haider_accounts::AccountsResult<()> {
        self.inner.delete(alias)
    }

    fn list(&self) -> haider_accounts::AccountsResult<Vec<haider_protocol::ids::CredentialAlias>> {
        self.inner.list()
    }
}

struct OAuthTurnBuilder {
    fake: Arc<FakeProvider>,
    builds: AtomicU64,
}

impl AccountProviderBuilder for OAuthTurnBuilder {
    fn providers(&self) -> std::collections::BTreeSet<String> {
        std::collections::BTreeSet::from(["fake-oauth".to_owned()])
    }

    fn build(
        &self,
        _provider: &str,
        credential: haider_accounts::SecretHandle,
        _model: &str,
        _alias: &haider_protocol::ids::CredentialAlias,
    ) -> Result<Arc<dyn Provider>, haider_protocol::error::HaiderError> {
        assert_eq!(
            credential.expose_secret(),
            ROTATED_ACCESS.as_bytes(),
            "only a durably committed rotated access token may reach a provider"
        );
        self.builds.fetch_add(1, Ordering::SeqCst);
        Ok(self.fake.clone())
    }
}

#[derive(Clone)]
struct SeenAuthorization {
    redirect_uri: String,
    state: String,
    challenge: String,
    nonce: String,
}

struct FakeServer {
    address: SocketAddr,
    seen: Arc<Mutex<Option<SeenAuthorization>>>,
    verifier: Arc<Mutex<Option<String>>>,
    fail_next_token: Arc<AtomicBool>,
    refresh_calls: Arc<AtomicU64>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeServer {
    async fn start() -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("fake bind");
        let address = listener.local_addr().expect("fake address");
        let seen = Arc::new(Mutex::new(None));
        let verifier = Arc::new(Mutex::new(None));
        let fail_next_token = Arc::new(AtomicBool::new(false));
        let refresh_calls = Arc::new(AtomicU64::new(0));
        let seen_for_task = Arc::clone(&seen);
        let verifier_for_task = Arc::clone(&verifier);
        let fail_next_for_task = Arc::clone(&fail_next_token);
        let refresh_calls_for_task = Arc::clone(&refresh_calls);
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let seen = Arc::clone(&seen_for_task);
                let verifier = Arc::clone(&verifier_for_task);
                let fail_next_token = Arc::clone(&fail_next_for_task);
                let refresh_calls = Arc::clone(&refresh_calls_for_task);
                tokio::spawn(async move {
                    serve(
                        stream,
                        address,
                        seen,
                        verifier,
                        fail_next_token,
                        refresh_calls,
                    )
                    .await;
                });
            }
        });
        Self {
            address,
            seen,
            verifier,
            fail_next_token,
            refresh_calls,
            task,
        }
    }

    fn fail_next_token(&self) {
        self.fail_next_token.store(true, Ordering::SeqCst);
    }

    fn catalog(&self) -> OAuthProviderCatalog {
        OAuthProviderCatalog::with_test_registrations([OAuthProviderRegistration::new(
            "fake-oauth",
            format!("http://{}", self.address),
            format!("http://{}/authorize", self.address),
            format!("http://{}/token", self.address),
            "haider-public-fake",
            ["openid", "inference", "profile"].map(str::to_owned),
            "fake-resource",
            Some("fake-api-resource".into()),
            true,
            Arc::new(FakeVerifier),
        )
        .expect("fake registration")])
        .expect("fake catalog")
    }
}

async fn serve(
    mut stream: TcpStream,
    address: SocketAddr,
    seen: Arc<Mutex<Option<SeenAuthorization>>>,
    captured_verifier: Arc<Mutex<Option<String>>>,
    fail_next_token: Arc<AtomicBool>,
    refresh_calls: Arc<AtomicU64>,
) {
    let Some((method, target, body)) = read_request(&mut stream).await else {
        return;
    };
    if target.starts_with("/authorize") {
        assert_eq!(method, "GET");
        let url = Url::parse(&format!("http://fake{target}")).expect("authorize URL");
        let params = url
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect::<HashMap<_, _>>();
        assert_eq!(
            params.get("client_id").map(String::as_str),
            Some("haider-public-fake")
        );
        assert_eq!(
            params.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        let authorization = SeenAuthorization {
            redirect_uri: params["redirect_uri"].clone(),
            state: params["state"].clone(),
            challenge: params["code_challenge"].clone(),
            nonce: params["nonce"].clone(),
        };
        *seen.lock().expect("seen") = Some(authorization.clone());
        write_response(
            &mut stream,
            302,
            Some(&format!(
                "{}?code={CODE}&state={}",
                authorization.redirect_uri, authorization.state
            )),
            b"",
        )
        .await;
        return;
    }
    assert_eq!(target, "/token");
    assert_eq!(method, "POST");
    let fields = url::form_urlencoded::parse(body.as_bytes())
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<HashMap<_, _>>();
    if fail_next_token.swap(false, Ordering::SeqCst) {
        write_response(
            &mut stream,
            400,
            None,
            format!(r#"{{"error":"invalid_grant","detail":"{RAW_ERROR}"}}"#).as_bytes(),
        )
        .await;
        return;
    }
    assert!(!fields.contains_key("client_secret"));
    if fields.get("grant_type").map(String::as_str) == Some("refresh_token") {
        refresh_calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(
            fields.get("refresh_token").map(String::as_str),
            Some(REFRESH)
        );
        assert_eq!(
            fields.get("audience").map(String::as_str),
            Some("fake-resource")
        );
        assert_eq!(
            fields.get("resource").map(String::as_str),
            Some("fake-api-resource")
        );
        let token = serde_json::json!({
            "access_token": ROTATED_ACCESS,
            "refresh_token": ROTATED_REFRESH,
            "token_type": "Bearer",
            "expires_in": 3600,
            "refresh_expires_in": 7200,
            "scope": SCOPES
        })
        .to_string();
        write_response(&mut stream, 200, None, token.as_bytes()).await;
        return;
    }
    assert_eq!(
        fields.get("grant_type").map(String::as_str),
        Some("authorization_code")
    );
    assert_eq!(fields.get("code").map(String::as_str), Some(CODE));
    let authorization = seen.lock().expect("seen").clone().expect("authorize first");
    assert_eq!(
        fields.get("redirect_uri").map(String::as_str),
        Some(authorization.redirect_uri.as_str())
    );
    let verifier = fields.get("code_verifier").expect("verifier");
    *captured_verifier.lock().expect("verifier") = Some(verifier.clone());
    assert_eq!(
        URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
        authorization.challenge
    );
    let identity = serde_json::json!({
        "marker": ID_MARKER,
        "issuer": format!("http://{address}"),
        "audience": "fake-resource",
        "nonce": authorization.nonce,
        "subject": "fake-subject",
        "display": "person@example.invalid"
    });
    let token = serde_json::json!({
        "access_token": ACCESS,
        "refresh_token": REFRESH,
        "token_type": "Bearer",
        "expires_in": 3600,
        "refresh_expires_in": 7200,
        "scope": SCOPES,
        "id_token": identity.to_string()
    })
    .to_string();
    write_response(&mut stream, 200, None, token.as_bytes()).await;
}

async fn read_request(stream: &mut TcpStream) -> Option<(String, String, String)> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 2048];
    let header_end = loop {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..read]);
        let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        break position + 4;
    };
    let headers = std::str::from_utf8(&bytes[..header_end]).ok()?;
    let mut first = headers.lines().next()?.split_ascii_whitespace();
    let method = first.next()?.to_owned();
    let target = first.next()?.to_owned();
    let length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    while bytes.len() < header_end + length {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    Some((
        method,
        target,
        String::from_utf8(bytes[header_end..header_end + length].to_vec()).ok()?,
    ))
}

async fn write_response(stream: &mut TcpStream, status: u16, location: Option<&str>, body: &[u8]) {
    let reason = if status == 302 { "Found" } else { "OK" };
    let location = location
        .map(|value| format!("Location: {value}\r\n"))
        .unwrap_or_default();
    let headers = format!(
        "HTTP/1.1 {status} {reason}\r\n{location}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(headers.as_bytes()).await.expect("headers");
    stream.write_all(body).await.expect("body");
    let _ = stream.shutdown().await;
}

struct FakeVerifier;

#[async_trait::async_trait]
impl OAuthIdentityVerifier for FakeVerifier {
    async fn verify(
        &self,
        id_token: &[u8],
        expected: OAuthIdentityExpectation<'_>,
    ) -> Result<OAuthIdentityV1, OAuthPublicError> {
        let claims: serde_json::Value = serde_json::from_slice(id_token)
            .map_err(|_| OAuthPublicError::new("id_token_malformed", false))?;
        if claims.get("marker").and_then(serde_json::Value::as_str) != Some(ID_MARKER)
            || claims.get("issuer").and_then(serde_json::Value::as_str) != Some(expected.issuer)
            || claims.get("audience").and_then(serde_json::Value::as_str) != Some(expected.audience)
            || claims
                .get("nonce")
                .and_then(serde_json::Value::as_str)
                .map(str::as_bytes)
                != Some(expected.nonce)
        {
            return Err(OAuthPublicError::new("identity_claim_mismatch", false));
        }
        Ok(OAuthIdentityV1 {
            subject_hash: "verified-subject-hash".into(),
            display_identity: "person@example.invalid".into(),
        })
    }
}

async fn request(client: &mut UdsClient, id: &str, body: RequestBody) -> ResponseBody {
    client
        .send(
            &WireFrame::Request {
                request_id: RequestId::new(id),
                body,
            },
            LIMIT,
        )
        .await;
    tokio::time::timeout(DEADLINE, async {
        loop {
            if let WireFrame::Response { request_id, body } = client.receive().await
                && request_id.as_str() == id
            {
                return body;
            }
        }
    })
    .await
    .expect("response deadline")
}

async fn control(config: &DaemonConfig, instance: &str) -> UdsClient {
    UdsClient::connect_control(
        &config.endpoint_path(),
        LIMIT,
        "oauth-integration",
        instance,
        ClientKind::Cli,
    )
    .await
}

async fn ready_successor_after_forced_close(
    config: &DaemonConfig,
    dependencies: DaemonDependencies,
) -> DaemonTask {
    tokio::time::timeout(DEADLINE, async {
        loop {
            let task = haider_daemon::spawn_with_dependencies(config.clone(), dependencies.clone());
            let mut readiness = task.readiness();
            loop {
                match readiness.current() {
                    DaemonState::Ready => return task,
                    DaemonState::Failed { .. } | DaemonState::Stopped => {
                        let _ = task.join().await;
                        break;
                    }
                    _ => {
                        readiness.changed().await.expect("readiness remains open");
                    }
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("successor acquires profile after forced close")
}

fn dependencies(server: &FakeServer, vault: Arc<MemoryVault>) -> DaemonDependencies {
    DaemonDependencies {
        accounts: AccountsDependencies {
            vault: VaultProvision::Available(vault as Arc<dyn Vault>),
            oauth_catalog: server.catalog(),
            oauth_coordinator: OAuthCoordinatorConfig {
                max_flows: 8,
                max_invalid_callbacks: 4,
                flow_ttl: Duration::from_secs(5),
            },
            ..AccountsDependencies::default()
        },
        ..DaemonDependencies::default()
    }
}

async fn start_flow(
    client: &mut UdsClient,
    request_id: &str,
    attempt_id: &str,
    desired_alias: &str,
) -> (OAuthFlowId, String) {
    match request(
        client,
        request_id,
        RequestBody::AccountOAuthStart {
            provider: "fake-oauth".into(),
            desired_alias: desired_alias.into(),
            attempt_id: attempt_id.into(),
        },
    )
    .await
    {
        ResponseBody::AccountOAuthStart {
            availability,
            flow_id: Some(flow_id),
            authorization_url: Some(url),
            loopback_port: Some(port),
            ..
        } => {
            assert!(availability.available);
            assert_ne!(port, 0);
            (flow_id, url.expose_authorization_url().to_owned())
        }
        other => panic!("unexpected OAuth start: {other:?}"),
    }
}

async fn ready_reference(
    client: &mut UdsClient,
    flow_id: &OAuthFlowId,
    attempt_id: &str,
) -> OAuthReadyRefWire {
    tokio::time::timeout(DEADLINE, async {
        loop {
            match request(
                client,
                "oauth-status",
                RequestBody::AccountOAuthStatus {
                    flow_id: flow_id.clone(),
                    attempt_id: attempt_id.into(),
                },
            )
            .await
            {
                ResponseBody::AccountOAuthStatus {
                    status:
                        OAuthFlowStatusWire::Ready {
                            oauth_reference, ..
                        },
                    ..
                } => return oauth_reference,
                ResponseBody::AccountOAuthStatus {
                    status: OAuthFlowStatusWire::WaitingBrowser | OAuthFlowStatusWire::Exchanging,
                    ..
                } => tokio::task::yield_now().await,
                other => panic!("unexpected OAuth status: {other:?}"),
            }
        }
    })
    .await
    .expect("ready deadline")
}

#[tokio::test]
async fn real_uds_oauth_add_is_capability_and_connection_bound_durable_and_secret_clean() {
    let tracing_output = Arc::new(Mutex::new(String::new()));
    tracing::subscriber::set_global_default(TraceCapture {
        output: Arc::clone(&tracing_output),
        next_span: Arc::new(AtomicU64::new(1)),
    })
    .expect("install tracing capture");
    let root = test_root("haoW");
    let store_dir = root.path().join("store");
    let mut config = DaemonConfig::new("oauth-wire", store_dir.clone(), root.path());
    config.discovery_disabled = true; // hermetic: direct spawns bypass support::ready
    let server = FakeServer::start().await;
    let vault = Arc::new(MemoryVault::new());
    let deps = dependencies(&server, Arc::clone(&vault));
    let task = ready_with_dependencies(&config, deps.clone()).await;

    let mut viewer = UdsClient::connect_with_capabilities(
        &config.endpoint_path(),
        LIMIT,
        "oauth-viewer",
        "viewer-1",
        ClientKind::Cli,
        CapabilitySet::from([Capability::View]),
    )
    .await;
    let denied = request(
        &mut viewer,
        "viewer-start",
        RequestBody::AccountOAuthStart {
            provider: "fake-oauth".into(),
            desired_alias: "work-oauth".into(),
            attempt_id: "attempt-view".into(),
        },
    )
    .await;
    assert!(matches!(
        denied,
        ResponseBody::Error { code, .. } if code == ERROR_CODE_CAPABILITY_DENIED
    ));

    let mut owner = control(&config, "owner-1").await;
    let (flow_id, authorization_url) =
        start_flow(&mut owner, "oauth-start", "attempt-1", "work-oauth").await;
    let formatted_start = format!("{authorization_url:?}");
    for sentinel in [CODE, ACCESS, REFRESH, ID_MARKER, RAW_ERROR] {
        assert!(!formatted_start.contains(sentinel));
    }

    let mut other = control(&config, "other-1").await;
    let cross_connection = request(
        &mut other,
        "cross-status",
        RequestBody::AccountOAuthStatus {
            flow_id: flow_id.clone(),
            attempt_id: "attempt-1".into(),
        },
    )
    .await;
    assert!(matches!(
        cross_connection,
        ResponseBody::Error { code, .. } if code == ERROR_CODE_OAUTH_FLOW_NOT_FOUND
    ));

    // Use a minimal raw browser request so this integration suite exercises
    // redirects without adding a second HTTP client to the binary crate.
    let authorization = Url::parse(&authorization_url).expect("authorization URL");
    let mut browser_socket = TcpStream::connect((
        Ipv4Addr::LOCALHOST,
        authorization.port().expect("authorization port"),
    ))
    .await
    .expect("browser connect");
    browser_socket
        .write_all(
            format!(
                "GET {}?{} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
                authorization.path(),
                authorization.query().expect("authorization query"),
                authorization.host_str().expect("authorization host")
            )
            .as_bytes(),
        )
        .await
        .expect("browser request");
    let mut first_response = Vec::new();
    browser_socket
        .read_to_end(&mut first_response)
        .await
        .expect("authorize response");
    let response_text = String::from_utf8(first_response).expect("authorize response text");
    let location = response_text
        .lines()
        .find_map(|line| line.strip_prefix("Location: "))
        .expect("callback location")
        .trim()
        .to_owned();
    let callback = Url::parse(&location).expect("callback URL");
    let mut callback_socket =
        TcpStream::connect((Ipv4Addr::LOCALHOST, callback.port().expect("callback port")))
            .await
            .expect("callback connect");
    callback_socket
        .write_all(
            format!(
                // A browser sends the authority it navigated to — derive the
                // Host from the redirect URL itself, never a hand-recomputed
                // shape (the v0.0.65 owner bug hid behind a hand-written
                // `127.0.0.1` here while the Anthropic redirect said
                // `localhost`).
                "GET {}?{} HTTP/1.1\r\nHost: {}:{}\r\nConnection: close\r\n\r\n",
                callback.path(),
                callback.query().expect("callback query"),
                callback.host_str().expect("callback host"),
                callback.port().expect("callback port")
            )
            .as_bytes(),
        )
        .await
        .expect("callback request");
    let mut success_html = Vec::new();
    callback_socket
        .read_to_end(&mut success_html)
        .await
        .expect("callback response");
    for sentinel in [CODE, ACCESS, REFRESH, ID_MARKER, RAW_ERROR] {
        assert!(
            !success_html
                .windows(sentinel.len())
                .any(|window| window == sentinel.as_bytes())
        );
    }
    let oauth_reference = ready_reference(&mut owner, &flow_id, "attempt-1").await;
    let successful_authorization = server
        .seen
        .lock()
        .expect("seen")
        .clone()
        .expect("seen successful authorization");
    let successful_verifier = server
        .verifier
        .lock()
        .expect("verifier")
        .clone()
        .expect("captured successful verifier");
    for sentinel in [
        successful_authorization.state.as_str(),
        successful_authorization.nonce.as_str(),
        successful_verifier.as_str(),
        callback.path(),
    ] {
        assert!(
            !success_html
                .windows(sentinel.len())
                .any(|window| window == sentinel.as_bytes()),
            "live success HTML must not echo state, nonce, verifier, or callback path"
        );
    }
    let add = RequestBody::AccountAdd {
        command_id: CommandId::new("oauth-command-1"),
        provider: "fake-oauth".into(),
        alias: "work-oauth".into(),
        auth_method: AccountAddMethod::OAuth,
        flow_id: flow_id.clone(),
        attempt_id: "attempt-1".into(),
        oauth_reference: oauth_reference.clone(),
    };
    let descriptor = match request(&mut owner, "oauth-add", add.clone()).await {
        ResponseBody::AccountAdd { descriptor } => descriptor,
        other => panic!("unexpected account.add: {other:?}"),
    };
    assert_eq!(descriptor.auth_method, AuthMethod::OAuth);
    assert_eq!(descriptor.status, CredentialStatus::Ok);
    assert_eq!(descriptor.identity, "person@example.invalid");

    // The ready reference is excluded from the durable identity: a lost
    // response retry replays the receipt even though the reference was used.
    assert!(matches!(
        request(&mut owner, "oauth-add-replay", add).await,
        ResponseBody::AccountAdd { descriptor: replay } if replay == descriptor
    ));
    let stored = vault
        .resolve(&haider_daemon::scoped_vault_alias(
            "oauth-wire",
            &descriptor.alias,
        ))
        .expect("vault bundle");
    let bundle = OAuthTokenBundleV1::decode(stored.expose_secret()).expect("bundle");
    assert_eq!(bundle.access_token(), ACCESS.as_bytes());
    assert_eq!(bundle.refresh_token(), Some(REFRESH.as_bytes()));

    // Exercise the raw token-error sentinel through the live endpoint and
    // retain only the sanitized public failure.
    server.fail_next_token();
    let (failed_flow, failed_authorization_url) = start_flow(
        &mut owner,
        "oauth-fail-start",
        "attempt-fail",
        "failed-oauth",
    )
    .await;
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("failure browser")
        .get(failed_authorization_url)
        .send()
        .await
        .expect("failure browser callback");
    let failed_status = tokio::time::timeout(DEADLINE, async {
        loop {
            match request(
                &mut owner,
                "oauth-fail-status",
                RequestBody::AccountOAuthStatus {
                    flow_id: failed_flow.clone(),
                    attempt_id: "attempt-fail".into(),
                },
            )
            .await
            {
                ResponseBody::AccountOAuthStatus {
                    status: status @ OAuthFlowStatusWire::Failed { public_code: _ },
                    ..
                } => return status,
                ResponseBody::AccountOAuthStatus {
                    status: OAuthFlowStatusWire::WaitingBrowser | OAuthFlowStatusWire::Exchanging,
                    ..
                } => tokio::task::yield_now().await,
                other => panic!("unexpected failed OAuth status: {other:?}"),
            }
        }
    })
    .await
    .expect("failed flow deadline");
    assert_eq!(
        failed_status,
        OAuthFlowStatusWire::Failed {
            public_code: "invalid_grant".into()
        }
    );

    // Dropping the owning card/connection cancels its still-waiting listener
    // and wipes the unclaimed flow; no later connection can drive it.
    let (_, disconnected_url) = start_flow(
        &mut owner,
        "disconnect-start",
        "disconnect-attempt",
        "discarded",
    )
    .await;
    let disconnected = Url::parse(&disconnected_url).expect("disconnect URL");
    let callback = disconnected
        .query_pairs()
        .find(|(name, _)| name == "redirect_uri")
        .map(|(_, value)| value.into_owned())
        .expect("disconnect redirect");
    let callback = Url::parse(&callback).expect("disconnect callback");
    let callback_port = callback.port().expect("disconnect callback port");
    drop(owner);
    tokio::time::timeout(DEADLINE, async {
        loop {
            if TcpStream::connect((Ipv4Addr::LOCALHOST, callback_port))
                .await
                .is_err()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("disconnect must close loopback listener");

    let callback_segment = Url::parse(&successful_authorization.redirect_uri)
        .expect("successful redirect")
        .path()
        .rsplit('/')
        .next()
        .expect("callback segment")
        .to_owned();
    let sentinels = vec![
        CODE,
        ACCESS,
        REFRESH,
        ID_MARKER,
        RAW_ERROR,
        successful_authorization.state.as_str(),
        successful_authorization.nonce.as_str(),
        successful_verifier.as_str(),
        callback_segment.as_str(),
    ];
    let tui_secret = sentinels.join("|");
    let public_tui_output = render_real_tui_login_surface(&tui_secret);
    let trace_snapshot = tracing_output.lock().expect("tracing output").clone();
    for output in [&public_tui_output, &trace_snapshot] {
        for sentinel in &sentinels {
            assert!(
                !output.contains(sentinel),
                "OAuth sentinel leaked into captured tracing/TUI output"
            );
        }
    }
    // Sweep WHILE Ready, before orderly checkpoint/WAL cleanup.
    let (live_scanned, saw_live_wal) =
        assert_tree_secret_free(root.path(), &store_dir.join("lock"), &sentinels);
    assert!(
        live_scanned >= 3,
        "must scan SQLite, WAL, and accounts.json"
    );
    assert!(
        saw_live_wal,
        "the live secret sweep must include SQLite WAL"
    );

    task.shutdown_handle().request("OAuth secret sweep");
    let _ = task.join().await;
    let (stopped_scanned, _) =
        assert_tree_secret_free(root.path(), &store_dir.join("lock"), &sentinels);
    assert!(stopped_scanned >= 2, "post-shutdown sweep remains clean");

    // A daemon instance never adopts an old flow id/reference.
    let restarted = ready_with_dependencies(&config, deps).await;
    let mut after_restart = control(&config, "after-restart").await;
    let stale = request(
        &mut after_restart,
        "stale-after-restart",
        RequestBody::AccountOAuthStatus {
            flow_id,
            attempt_id: "attempt-1".into(),
        },
    )
    .await;
    assert!(matches!(
        stale,
        ResponseBody::Error { code, .. } if code == ERROR_CODE_OAUTH_FLOW_NOT_FOUND
    ));
    restarted.shutdown_handle().request("complete");
    let _ = restarted.join().await;
}

// MUTATION CHECK (W5b.1b P1-1): remove `AccountActorHandle::force_and_join`
// from the forced runtime path. Expected failure: the predecessor publishes
// `Stopped` while `cleartext_live` is still true (and its already-started
// rotated vault write can race the successor) instead of blocking here until
// the actor tombstones and joins.
#[tokio::test]
async fn forced_runtime_joins_blocking_refresh_persistence_before_successor_ready() {
    blocking_refresh_shutdown_barrier(false).await;
}

// MUTATION CHECK (W5b.1c P1): restore the early
// `Some(Err(error)) => return Err(error.into())` from worker-manager shutdown.
// Expected failure: the injected worker error drops the account actor before
// its blocking refresh persistence joins, so `stopped_withheld` and successor
// lease exclusion below fail while the vault write is still live.
#[tokio::test]
async fn worker_shutdown_error_still_joins_refresh_persistence_and_withholds_lease() {
    blocking_refresh_shutdown_barrier(true).await;
}

async fn blocking_refresh_shutdown_barrier(inject_worker_shutdown_error: bool) {
    let root = test_root("haoB");
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    // Windows derives named-pipe names solely from singleton profile identity,
    // while Unix also gets isolation from each fixture's runtime directory. Keep the two
    // parameterizations distinct on Windows so the parallel test runner does
    // not manufacture a live-profile collision; each case still restarts its
    // own exact profile below.
    let profile_id = if cfg!(windows) && inject_worker_shutdown_error {
        "oauth-barrier-worker-error"
    } else {
        "oauth-barrier"
    };
    let mut config = DaemonConfig::new(profile_id, root.path().join("store"), root.path());
    config.discovery_disabled = true; // hermetic: direct spawns bypass support::ready
    config.drain_timeout = Duration::from_millis(100);
    config.inject_worker_manager_shutdown_error = inject_worker_shutdown_error;
    let server = FakeServer::start().await;
    let vault = Arc::new(BlockingRotationVault::new());
    let builder = Arc::new(OAuthTurnBuilder {
        fake: Arc::new(FakeProvider::new(vec![FakeStep::Finish {
            reason: haider_protocol::provider::FinishReason::EndTurn,
        }])),
        builds: AtomicU64::new(0),
    });
    let dependencies = DaemonDependencies {
        provider_factory: ProviderFactoryConfig::AccountsWith(builder.clone()),
        accounts: AccountsDependencies {
            vault: VaultProvision::Available(vault.clone() as Arc<dyn Vault>),
            oauth_catalog: server.catalog(),
            oauth_coordinator: OAuthCoordinatorConfig {
                max_flows: 8,
                max_invalid_callbacks: 4,
                flow_ttl: Duration::from_secs(5),
            },
            ..AccountsDependencies::default()
        },
        ..DaemonDependencies::default()
    };
    let task = ready_with_dependencies(&config, dependencies.clone()).await;
    // Only the predecessor receives the deterministic manager fault. Every
    // successor below exercises the normal daemon with the same profile,
    // vault, and account dependencies.
    config.inject_worker_manager_shutdown_error = false;
    let mut client = control(&config, "barrier-owner").await;
    let (flow_id, authorization_url) = start_flow(
        &mut client,
        "barrier-oauth-start",
        "barrier-attempt",
        "work",
    )
    .await;
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("browser")
        .get(authorization_url)
        .send()
        .await
        .expect("browser flow");
    assert!(response.status().is_success());
    let oauth_reference = ready_reference(&mut client, &flow_id, "barrier-attempt").await;
    let descriptor = match request(
        &mut client,
        "barrier-account-add",
        RequestBody::AccountAdd {
            command_id: CommandId::new("barrier-account-command"),
            provider: "fake-oauth".into(),
            alias: "work".into(),
            auth_method: AccountAddMethod::OAuth,
            flow_id,
            attempt_id: "barrier-attempt".into(),
            oauth_reference,
        },
    )
    .await
    {
        ResponseBody::AccountAdd { descriptor } => descriptor,
        other => panic!("unexpected account.add response: {other:?}"),
    };
    let stored = vault
        .resolve(&haider_daemon::scoped_vault_alias(
            profile_id,
            &descriptor.alias,
        ))
        .expect("initial bundle");
    let initial = OAuthTokenBundleV1::decode(stored.expose_secret()).expect("decode initial");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis()
        .try_into()
        .expect("millisecond clock");
    let expiring = OAuthTokenBundleV1::new(
        initial.provider_id.clone(),
        initial.issuer.clone(),
        initial.audience.clone(),
        initial.resource.clone(),
        initial.token_type.clone(),
        Zeroizing::new(initial.access_token().to_vec()),
        initial
            .refresh_token()
            .map(|token| Zeroizing::new(token.to_vec())),
        now,
        initial.refresh_expires_at_unix_ms,
        initial.granted_scopes.clone(),
        initial.identity.clone(),
        initial.generation,
    )
    .expect("expiring bundle");
    vault
        .put(
            &haider_daemon::scoped_vault_alias(profile_id, &descriptor.alias),
            &expiring.encode().expect("encode expiring"),
        )
        .expect("seed expiring");
    vault.arm();

    let created = request(
        &mut client,
        "barrier-session-create",
        RequestBody::SessionCreate {
            command_id: CommandId::new("barrier-session-command"),
            cwd: workspace.to_string_lossy().into_owned(),
            provider: "fake-oauth".into(),
            model: "fake-model".into(),
            max_tokens: 64,
        },
    )
    .await;
    let (session_id, worker_generation) = match created {
        ResponseBody::SessionCreate {
            session_id,
            worker_generation,
            ..
        } => (session_id, worker_generation),
        other => panic!("unexpected session.create response: {other:?}"),
    };
    let _ = request(
        &mut client,
        "barrier-session-attach",
        RequestBody::SessionAttach {
            session_id: session_id.clone(),
            after_seq: 0,
            mode: AttachMode::Control,
            sealed_replay: false,
        },
    )
    .await;
    client
        .send(
            &WireFrame::Request {
                request_id: RequestId::new("barrier-turn-submit"),
                body: RequestBody::TurnSubmit {
                    command_id: CommandId::new("barrier-turn-command"),
                    session_id: session_id.clone(),
                    worker_generation,
                    text: "drive refresh persistence".into(),
                    attachments: Vec::new(),
                    mode: haider_protocol::DeliveryMode::Queue,
                },
            },
            LIMIT,
        )
        .await;
    tokio::time::timeout(DEADLINE, async {
        while !vault.entered.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("rotated spawn_blocking vault write entered");
    assert_eq!(server.refresh_calls.load(Ordering::SeqCst), 1);

    task.shutdown_handle()
        .request("force blocking OAuth persistence");
    let predecessor = tokio::spawn(async move { task.join().await });
    tokio::time::sleep(config.drain_timeout + Duration::from_millis(100)).await;
    let stopped_withheld = !predecessor.is_finished();
    let cleartext_was_live = vault.cleartext_live.load(Ordering::Acquire);

    // A successor that races the in-flight predecessor write must lose the
    // profile lease before it can initialize accounts or resolve the vault.
    let premature_successor =
        haider_daemon::spawn_with_dependencies(config.clone(), dependencies.clone());
    let mut premature_readiness = premature_successor.readiness();
    let successor_was_excluded = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match premature_readiness.current() {
                DaemonState::Ready => break false,
                DaemonState::Failed { .. } | DaemonState::Stopped => break true,
                _ => {
                    if premature_readiness.changed().await.is_none() {
                        break !matches!(premature_readiness.current(), DaemonState::Ready);
                    }
                }
            }
        }
    })
    .await
    .unwrap_or(true);
    premature_successor
        .shutdown_handle()
        .request("finish blocked successor probe");
    let _ = tokio::time::timeout(DEADLINE, premature_successor.join())
        .await
        .expect("blocked successor probe joins");

    // Always unblock the non-cancellable test worker before asserting, so the
    // deliberate no-join mutation fails without stranding the test runtime.
    vault.release();
    let predecessor_result = tokio::time::timeout(DEADLINE, predecessor)
        .await
        .expect("predecessor join deadline")
        .expect("predecessor owner join");
    assert!(
        stopped_withheld,
        "Stopped must remain withheld while blocking persistence owns cleartext"
    );
    assert!(
        cleartext_was_live,
        "the controlled blocking task still owns token bytes at the barrier"
    );
    assert!(
        successor_was_excluded,
        "a successor must not reach Ready while predecessor persistence is in flight"
    );
    if inject_worker_shutdown_error {
        let error = predecessor_result.expect_err("injected worker shutdown error propagates");
        assert!(
            error
                .to_string()
                .contains("injected worker manager shutdown failure"),
            "the captured worker error propagates only after barrier cleanup: {error}"
        );
    } else {
        assert_eq!(
            predecessor_result.expect("predecessor daemon outcome"),
            haider_daemon::ShutdownOutcome::Forced
        );
    }
    assert!(
        !vault.cleartext_live.load(Ordering::Acquire),
        "joined blocking persistence no longer owns cleartext bytes"
    );
    assert!(
        vault
            .resolve(&haider_daemon::scoped_vault_alias(
                profile_id,
                &descriptor.alias
            ))
            .is_err(),
        "forced actor completion tombstones the predecessor's rotated write"
    );

    let successor = ready_successor_after_forced_close(&config, dependencies).await;
    let mut after_restart = control(&config, "barrier-successor").await;
    match request(
        &mut after_restart,
        "barrier-list-after-restart",
        RequestBody::AccountList { provider: None },
    )
    .await
    {
        ResponseBody::AccountList { descriptors, .. } => assert!(
            descriptors.iter().any(|current| {
                current.alias == descriptor.alias
                    && matches!(current.status, CredentialStatus::Expired)
            }),
            "successor sees only the durable fail-closed descriptor"
        ),
        other => panic!("unexpected account.list response: {other:?}"),
    }

    let successor_created = request(
        &mut after_restart,
        "barrier-successor-create",
        RequestBody::SessionCreate {
            command_id: CommandId::new("barrier-successor-session-command"),
            cwd: workspace.to_string_lossy().into_owned(),
            provider: "fake-oauth".into(),
            model: "fake-model".into(),
            max_tokens: 64,
        },
    )
    .await;
    let (successor_session, successor_generation) = match successor_created {
        ResponseBody::SessionCreate {
            session_id,
            worker_generation,
            ..
        } => (session_id, worker_generation),
        other => panic!("unexpected successor session.create: {other:?}"),
    };
    let _ = request(
        &mut after_restart,
        "barrier-successor-attach",
        RequestBody::SessionAttach {
            session_id: successor_session.clone(),
            after_seq: 0,
            mode: AttachMode::Control,
            sealed_replay: false,
        },
    )
    .await;
    after_restart
        .send(
            &WireFrame::Request {
                request_id: RequestId::new("barrier-successor-submit"),
                body: RequestBody::TurnSubmit {
                    command_id: CommandId::new("barrier-successor-turn-command"),
                    session_id: successor_session,
                    worker_generation: successor_generation,
                    text: "must fail without retrying predecessor token".into(),
                    attachments: Vec::new(),
                    mode: haider_protocol::DeliveryMode::Queue,
                },
            },
            LIMIT,
        )
        .await;
    tokio::time::timeout(DEADLINE, async {
        loop {
            if let WireFrame::Event { envelope, .. } = after_restart.receive().await
                && let Ok(EventPayload::RunState(state)) =
                    serde_json::from_value::<EventPayload>(envelope.payload)
                && state.is_terminal()
            {
                break;
            }
        }
    })
    .await
    .expect("successor turn terminal");
    assert_eq!(
        server.refresh_calls.load(Ordering::SeqCst),
        1,
        "the successor must not retry the predecessor's refresh token"
    );
    assert_eq!(
        builder.builds.load(Ordering::SeqCst),
        0,
        "no predecessor or successor turn may observe the uncommitted rotation"
    );
    successor
        .shutdown_handle()
        .request("barrier successor complete");
    let _ = successor.join().await;
}

#[tokio::test]
async fn unavailable_live_provider_returns_reason_without_flow_allocation() {
    let root = test_root("haoU");
    let config = DaemonConfig::new("oauth-unavailable", root.path().join("store"), root.path());
    let task = ready_with_dependencies(
        &config,
        DaemonDependencies {
            accounts: AccountsDependencies {
                vault: VaultProvision::Available(Arc::new(MemoryVault::new()) as Arc<dyn Vault>),
                ..AccountsDependencies::default()
            },
            ..DaemonDependencies::default()
        },
    )
    .await;
    let mut client = control(&config, "unavailable").await;
    match request(
        &mut client,
        "unavailable-openai",
        RequestBody::AccountOAuthStart {
            provider: "openai".into(),
            desired_alias: "work".into(),
            attempt_id: "attempt".into(),
        },
    )
    .await
    {
        ResponseBody::AccountOAuthStart {
            availability,
            flow_id,
            authorization_url,
            loopback_port,
            ..
        } => {
            assert!(!availability.available);
            assert!(
                availability
                    .reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("no sanctioned"))
            );
            assert!(flow_id.is_none());
            assert!(authorization_url.is_none());
            assert!(loopback_port.is_none());
        }
        other => panic!("unexpected unavailable response: {other:?}"),
    }
    task.shutdown_handle().request("complete");
    let _ = task.join().await;
}
