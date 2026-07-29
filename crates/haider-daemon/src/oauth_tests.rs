#![allow(clippy::expect_used)]

use std::sync::atomic::{AtomicBool, AtomicUsize};

use haider_accounts::{AccountsResult, MemoryVault};
use haider_protocol::credential::CredentialStatus;
use tokio::sync::Semaphore;

use super::*;
use crate::session_hub::{FrameSendError, FrameSink};

const CODE_SENTINEL: &str = "AUTH_CODE_SENTINEL_51d2";
const ACCESS_SENTINEL: &str = "ACCESS_TOKEN_SENTINEL_834a";
const REFRESH_SENTINEL: &str = "REFRESH_TOKEN_SENTINEL_1c72";
const ID_SENTINEL: &str = "ID_TOKEN_SENTINEL_97e1";
const RAW_ERROR_SENTINEL: &str = "RAW_TOKEN_ERROR_SENTINEL_29af";
const RAW_BODY_SENTINEL: &str = "RAW_TOKEN_BODY_SENTINEL_a83c";
const SCOPES: &str = "openid inference profile";
const AUDIENCE: &str = "fake-resource";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FakeMode {
    Success,
    Denied,
    TokenRedirect,
    Malformed,
    Oversized,
    InvalidGrant,
    Transient,
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
    verifiers: Mutex<Vec<String>>,
    refresh_gate: Option<Arc<Semaphore>>,
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
            verifiers: Mutex::new(Vec::new()),
            refresh_gate: (gated_refresh || mode == FakeMode::SlowExchange)
                .then(|| Arc::new(Semaphore::new(0))),
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
    let Some((method, target, authorization, body)) = read_http_request(&mut stream).await else {
        return;
    };
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
    state.token_calls.fetch_add(1, Ordering::SeqCst);
    let form = url::form_urlencoded::parse(body.as_bytes())
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<HashMap<_, _>>();
    let grant = form.get("grant_type").map(String::as_str).unwrap_or("");
    if grant == "refresh_token" {
        state.refresh_calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(form.get("audience").map(String::as_str), Some(AUDIENCE));
        assert_eq!(
            form.get("resource").map(String::as_str),
            Some("fake-api-resource"),
            "refresh requests must bind the configured resource"
        );
        if let Some(gate) = &state.refresh_gate {
            let permit = gate.acquire().await.expect("gate");
            permit.forget();
        }
    }
    if form.contains_key("client_secret") {
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
    if grant == "authorization_code" {
        let seen = state
            .auth
            .lock()
            .expect("auth lock")
            .clone()
            .expect("authorize first");
        assert_eq!(form.get("code").map(String::as_str), Some(CODE_SENTINEL));
        assert_eq!(
            form.get("redirect_uri").map(String::as_str),
            Some(seen.redirect_uri.as_str())
        );
        let verifier = form.get("code_verifier").cloned().unwrap_or_default();
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
) -> Option<(String, String, Option<String>, String)> {
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
    while bytes.len() < header_end.saturating_add(content_length) {
        let count = stream.read(&mut chunk).await.ok()?;
        if count == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    let body = String::from_utf8(bytes[header_end..header_end + content_length].to_vec()).ok()?;
    Some((method, target, authorization, body))
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

struct FakeIdentityVerifier;

impl OAuthIdentityVerifier for FakeIdentityVerifier {
    fn verify(
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

async fn coordinator_for(
    server: &FakeOAuthServer,
    ttl: Duration,
) -> (OAuthCoordinator, mpsc::UnboundedReceiver<WireFrame>) {
    let registration = server.registration(Arc::new(FakeIdentityVerifier));
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
            "fake-oauth".into(),
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

async fn wait_ready(coordinator: &OAuthCoordinator, flow_id: &OAuthFlowId) -> OAuthFlowStatusWire {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let status = coordinator
                .status("connection-1", flow_id, "attempt-1")
                .expect("owned flow");
            if !matches!(
                status,
                OAuthFlowStatusWire::WaitingBrowser | OAuthFlowStatusWire::Exchanging
            ) {
                break status;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("flow terminal")
}

#[test]
fn sanctioned_oauth_table_is_empty_and_reports_precise_reasons() {
    assert!(SANCTIONED_PROVIDER_REGISTRATIONS.is_empty());
    let catalog = OAuthProviderCatalog::default();
    let openai = catalog.availability("openai", true);
    assert!(!openai.available);
    assert!(
        openai
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("no sanctioned"))
    );
    let anthropic = catalog.availability("anthropic", true);
    assert!(!anthropic.available);
    assert!(
        anthropic
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("forbids"))
    );
    assert!(
        catalog
            .availability("openai", false)
            .reason
            .unwrap_or_default()
            .contains("plaintext")
    );
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
            43210,
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
            parse_callback(request.as_bytes(), path, 43210, state),
            CallbackResult::Invalid
        ));
    }
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
            parse_callback(request(query).as_bytes(), path, 43210, b"s"),
            CallbackResult::Invalid
        ));
    }
    assert!(matches!(
        parse_callback(
            request("error=access_denied&state=s").as_bytes(),
            path,
            43210,
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
                43210,
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
    coordinator.shutdown().await;
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
    for html in [SUCCESS_HTML, DENIED_HTML, INVALID_HTML] {
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

fn oauth_descriptor_for_test() -> CredentialDescriptor {
    CredentialDescriptor {
        alias: CredentialAlias::new("work-oauth"),
        provider: "fake-oauth".into(),
        base_url: None,
        auth_method: AuthMethod::OAuth,
        identity: "person@example.invalid".into(),
        status: CredentialStatus::Ok,
        active: true,
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
    let (sender, mut receiver) = mpsc::channel(16);
    let snapshot = Arc::clone(snapshot);
    tokio::spawn(async move {
        while let Some(command) = receiver.recv().await {
            match command {
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

async fn wait_for_refresh_calls(server: &FakeOAuthServer, expected: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while server.state.refresh_calls.load(Ordering::SeqCst) != expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("refresh call count");
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
        async move { broker.resolve_oauth(&descriptor).await }
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

// MUTATION CHECK (W5b.1b P2-3): discard `JoinError` values in
// `OwnedTaskSet::join_all`. Expected failure: the flight guard still wakes
// the resolver closed, but shutdown incorrectly returns `true` (graceful).
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

#[tokio::test]
async fn expired_access_with_transient_refresh_returns_sanitized_retryable_error() {
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
                == 2
            && source.contains("chunk.as_mut().zeroize();"),
        "token transports must close before every source chunk is mutable-owned and scrubbed"
    );
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
