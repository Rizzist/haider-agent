//! W5b OAuth acceptance over the real production UDS and real loopback TCP.
//!
//! The only enabled provider is an injected, public-client fake. No live
//! provider metadata or endpoint is used by this suite.

#![allow(clippy::expect_used)]

mod support;

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use haider_accounts::{MemoryVault, OAuthIdentityV1, OAuthTokenBundleV1, Vault};
use haider_daemon::{
    AccountsDependencies, DaemonConfig, DaemonDependencies, OAuthCoordinatorConfig,
    OAuthIdentityExpectation, OAuthIdentityVerifier, OAuthProviderCatalog,
    OAuthProviderRegistration, OAuthPublicError, VaultProvision,
};
use haider_protocol::credential::{AuthMethod, CredentialStatus};
use haider_rpc::{
    AccountAddMethod, Capability, CapabilitySet, ClientKind, CommandId, DEFAULT_FRAME_LIMIT,
    ERROR_CODE_CAPABILITY_DENIED, ERROR_CODE_OAUTH_FLOW_NOT_FOUND, OAuthFlowId,
    OAuthFlowStatusWire, OAuthReadyRefWire, RequestBody, RequestId, ResponseBody, WireFrame,
};
use sha2::{Digest, Sha256};
use support::{DEADLINE, UdsClient, ready_with_dependencies, test_root};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use url::Url;

const CODE: &str = "OAUTH_CODE_SWEEP_812f";
const ACCESS: &str = "OAUTH_ACCESS_SWEEP_17a3";
const REFRESH: &str = "OAUTH_REFRESH_SWEEP_6d51";
const ID_MARKER: &str = "OAUTH_ID_SWEEP_9bc4";
const RAW_ERROR: &str = "OAUTH_RAW_ERROR_SWEEP_40e8";
const SCOPES: &str = "openid inference profile";
const LIMIT: usize = DEFAULT_FRAME_LIMIT;

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
        let seen_for_task = Arc::clone(&seen);
        let verifier_for_task = Arc::clone(&verifier);
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let seen = Arc::clone(&seen_for_task);
                let verifier = Arc::clone(&verifier_for_task);
                tokio::spawn(async move {
                    serve(stream, address, seen, verifier).await;
                });
            }
        });
        Self {
            address,
            seen,
            verifier,
            task,
        }
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
    assert!(!fields.contains_key("client_secret"));
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

impl OAuthIdentityVerifier for FakeVerifier {
    fn verify(
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
    let root = test_root("haoW");
    let store_dir = root.path().join("store");
    let config = DaemonConfig::new("oauth-wire", store_dir.clone(), root.path());
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
                "GET {}?{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
                callback.path(),
                callback.query().expect("callback query"),
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
    let stored = vault.resolve(&descriptor.alias).expect("vault bundle");
    let bundle = OAuthTokenBundleV1::decode(stored.expose_secret()).expect("bundle");
    assert_eq!(bundle.access_token(), ACCESS.as_bytes());
    assert_eq!(bundle.refresh_token(), Some(REFRESH.as_bytes()));

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

    task.shutdown_handle().request("OAuth secret sweep");
    let _ = task.join().await;
    let captured = server
        .seen
        .lock()
        .expect("seen")
        .clone()
        .expect("seen auth");
    let verifier = server
        .verifier
        .lock()
        .expect("verifier")
        .clone()
        .expect("captured verifier");
    let sentinels = [
        CODE,
        ACCESS,
        REFRESH,
        ID_MARKER,
        RAW_ERROR,
        captured.state.as_str(),
        verifier.as_str(),
    ];
    let mut scanned = 0_usize;
    // Cover the full disposable profile/runtime root, including SQLite/WAL,
    // accounts.json, provider/runtime configuration, and temporary files.
    let mut stack = vec![root.path().to_path_buf()];
    while let Some(directory) = stack.pop() {
        for entry in std::fs::read_dir(&directory).expect("read store") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
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
    assert!(scanned >= 2, "must scan SQLite and accounts.json");

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
