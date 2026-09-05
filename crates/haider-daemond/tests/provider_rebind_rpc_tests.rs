//! Per-session rebind through production RPC, registry and HTTP adapters.
#![allow(clippy::expect_used)]

mod support;

use haider_accounts::{MemoryVault, Vault as _};
use haider_daemon::{AccountsDependencies, DaemonConfig, DaemonDependencies, VaultProvision};
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::ids::{RunId, SessionId};
use haider_protocol::session::{SessionInteractionModeV1, SessionProviderRebound};
use haider_protocol::state::RunState;
use haider_protocol::{DeliveryMode, EventPayload};
use haider_rpc::{
    AttachMode, ClientKind, CommandId, ProviderApiFamilyWire, ProviderAuthRequirementWire,
    RequestBody, RequestId, ResponseBody, WireFrame,
};
use std::net::Ipv4Addr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::{UdsClient, ready_with_dependencies, test_root};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};

const PROVIDER: &str = "rebind-proxy";
const MODEL: &str = "rebind-model";
const OPEN_TIMEOUT_MS: u64 = 5_000;
// Registry #94: one configured HTTP-open budget plus the shared local RPC /
// journal publication budget. Every wait keeps the negotiated IPC serviced.
const OBSERVER_BOUND: Duration =
    Duration::from_millis(OPEN_TIMEOUT_MS).saturating_add(support::DEADLINE);

struct Proxy {
    origin: String,
    posts: Arc<Mutex<Vec<String>>>,
    arrived: mpsc::UnboundedReceiver<()>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Proxy {
    async fn start(hold_first: Option<oneshot::Receiver<()>>) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("proxy listener");
        let origin = format!(
            "http://{}/v1",
            listener.local_addr().expect("proxy address")
        );
        let posts = Arc::new(Mutex::new(Vec::new()));
        let observed = posts.clone();
        let (arrived_tx, arrived) = mpsc::unbounded_channel();
        let mut hold_first = hold_first;
        let task = tokio::spawn(async move {
            let mut handlers = tokio::task::JoinSet::new();
            loop {
                let (mut stream, _) = listener.accept().await.expect("proxy accept");
                let request = read_request(&mut stream).await;
                if request.starts_with("GET ") {
                    respond(
                        &mut stream,
                        "application/json",
                        &serde_json::json!({
                            "object": "list", "data": [{"id": MODEL, "object": "model"}]
                        })
                        .to_string(),
                    )
                    .await;
                    continue;
                }
                assert!(
                    request.starts_with("POST /v1/chat/completions "),
                    "unexpected request: {request}"
                );
                observed.lock().expect("ledger").push(request);
                arrived_tx.send(()).expect("ledger receiver");
                let hold = hold_first.take();
                handlers.spawn(async move {
                    let tool = hold.is_some();
                    if let Some(release) = hold { release.await.expect("release held A response"); }
                    let delta = if tool {
                        serde_json::json!({"tool_calls": [{"index": 0, "id": "rebind-tool", "type": "function", "function": {"name": "fs_glob", "arguments": "{\"pattern\":\"*\"}"}}]})
                    } else { serde_json::json!({"content": "routing complete"}) };
                    let chunk = serde_json::json!({"id":"rebind-completion", "choices":[{"index":0,"delta":delta,"finish_reason":null}]});
                    let stop = serde_json::json!({"id":"rebind-completion", "choices":[{"index":0,"delta":{},"finish_reason": if tool {"tool_calls"} else {"stop"}}]});
                    let usage = serde_json::json!({"id":"rebind-completion", "choices":[],"usage":{"prompt_tokens":16,"completion_tokens":2,"total_tokens":18}});
                    let body = format!("data: {chunk}\n\ndata: {stop}\n\ndata: {usage}\n\ndata: [DONE]\n\n");
                    respond(&mut stream, "text/event-stream", &body).await;
                });
            }
        });
        Self {
            origin,
            posts,
            arrived,
            task,
        }
    }

    fn ledger(&self) -> Vec<String> {
        self.posts.lock().expect("ledger").clone()
    }
}

async fn read_request(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    loop {
        let mut chunk = [0; 8192];
        let count = stream.read(&mut chunk).await.expect("read request");
        assert_ne!(count, 0, "HTTP request ended early");
        bytes.extend_from_slice(&chunk[..count]);
        assert!(bytes.len() <= 1024 * 1024, "fixture request limit");
        if let Some(end) = bytes.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..end]);
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().expect("content length"))
                })
                .unwrap_or(0);
            if bytes.len() >= end + 4 + length {
                return String::from_utf8(bytes).expect("UTF-8 request");
            }
        }
    }
}

async fn respond(stream: &mut TcpStream, content_type: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .expect("HTTP response");
    stream.shutdown().await.expect("HTTP close");
}

struct Client {
    wire: UdsClient,
    events: Vec<RawEnvelope>,
    caught_up: Vec<haider_rpc::AttachmentId>,
    config: DaemonConfig,
}

impl Client {
    async fn connect(config: &DaemonConfig) -> Self {
        Self {
            wire: UdsClient::connect_control(
                &config.endpoint_path(),
                config.frame_limit,
                "provider-rebind",
                "rebind-observer",
                ClientKind::Headless,
            )
            .await,
            events: Vec::new(),
            caught_up: Vec::new(),
            config: config.clone(),
        }
    }

    fn record(&mut self, frame: WireFrame) {
        match frame {
            WireFrame::Event { envelope, .. } => self.events.push(envelope),
            WireFrame::AttachCaughtUp { attachment_id, .. } => self.caught_up.push(attachment_id),
            _ => {}
        }
    }

    async fn request(&mut self, id: &str, body: RequestBody) -> ResponseBody {
        self.wire
            .send(
                &WireFrame::Request {
                    request_id: RequestId::new(id),
                    body,
                },
                self.config.frame_limit,
            )
            .await;
        loop {
            let frame = self.wire.next().await;
            match frame {
                WireFrame::Response { request_id, body } if request_id.as_str() == id => {
                    return body;
                }
                other => self.record(other),
            }
        }
    }

    async fn attach(&mut self, session: &SessionId) -> u64 {
        let response = self
            .request(
                "attach",
                RequestBody::SessionAttach {
                    session_id: session.clone(),
                    after_seq: 0,
                    mode: AttachMode::Control,
                    sealed_replay: false,
                },
            )
            .await;
        let (generation, attachment) = match response {
            ResponseBody::SessionAttach {
                attach_state,
                attachment_id,
            } => (attach_state.worker_generation, attachment_id),
            other => panic!("attach: {other:?}"),
        };
        while !self.caught_up.contains(&attachment) {
            let frame = self.wire.next().await;
            self.record(frame);
        }
        generation
    }

    async fn create(&mut self, workspace: &Path, suffix: &str) -> (SessionId, u64) {
        let response = self
            .request(
                "create",
                RequestBody::SessionCreateWithPermissionOverrides {
                    command_id: CommandId::new(format!("create-{suffix}")),
                    cwd: workspace.display().to_string(),
                    provider: PROVIDER.into(),
                    model: MODEL.into(),
                    max_tokens: 4096,
                    permission_overrides: None,
                    cache_policy: None,
                    interaction_mode: SessionInteractionModeV1::Autonomous,
                    ssh_scope: None,
                    account_alias: None,
                    resolve_provider: false,
                    resolve_model: false,
                    effort: None,
                    fast: None,
                },
            )
            .await;
        let session = match response {
            ResponseBody::SessionCreate { session_id, .. } => session_id,
            other => panic!("create: {other:?}"),
        };
        let generation = self.attach(&session).await;
        (session, generation)
    }

    async fn submit(&mut self, session: &SessionId, generation: u64, suffix: &str) -> RunId {
        match self
            .request(
                "submit",
                RequestBody::TurnSubmit {
                    command_id: CommandId::new(format!("submit-{suffix}")),
                    session_id: session.clone(),
                    worker_generation: generation,
                    text: format!("routing fixture {suffix}"),
                    attachments: Vec::new(),
                    mode: DeliveryMode::Queue,
                },
            )
            .await
        {
            ResponseBody::TurnSubmit { run_id, .. } => run_id,
            other => panic!("submit: {other:?}"),
        }
    }

    async fn await_post(&mut self, proxy: &mut Proxy) {
        tokio::time::timeout(OBSERVER_BOUND, async {
            loop {
                tokio::select! {
                    arrival = proxy.arrived.recv() => { arrival.expect("proxy arrival"); return; },
                    frame = self.wire.next() => self.record(frame),
                }
            }
        })
        .await
        .expect("configured HTTP-open budget plus local publication budget");
    }

    async fn terminal(&mut self, run: &RunId) {
        tokio::time::timeout(OBSERVER_BOUND, async {
            loop {
                if let Some(state) = self
                    .events
                    .iter()
                    .filter(|envelope| envelope.run_id.as_ref() == Some(run))
                    .find_map(|envelope| match envelope.payload.decode_event().ok()? {
                        EventPayload::RunState(state) if state.is_terminal() => Some(state),
                        _ => None,
                    })
                {
                    assert_eq!(
                        state,
                        RunState::Done,
                        "run must complete after routing; events: {:?}",
                        self.events
                    );
                    return;
                }
                let frame = self.wire.next().await;
                self.record(frame);
            }
        })
        .await
        .expect("terminal within configured HTTP-open plus publication budget");
    }
}

fn dependencies() -> DaemonDependencies {
    DaemonDependencies {
        accounts: AccountsDependencies {
            vault: VaultProvision::Available(Arc::new(MemoryVault::default())),
            ..AccountsDependencies::default()
        },
        ..DaemonDependencies::default()
    }
}

fn rebind(
    session: &SessionId,
    generation: u64,
    command: &str,
    provider: &str,
    url: Option<&str>,
    account: Option<&str>,
) -> RequestBody {
    RequestBody::SessionProviderRebind {
        command_id: CommandId::new(command),
        session_id: session.clone(),
        worker_generation: generation,
        provider: provider.into(),
        base_url: url.map(str::to_owned),
        account: account.map(str::to_owned),
    }
}

async fn configure(client: &mut Client, origin: &str) {
    configure_with_auth(client, origin, ProviderAuthRequirementWire::None).await;
}

async fn configure_with_auth(client: &mut Client, origin: &str, auth: ProviderAuthRequirementWire) {
    let revision = match client
        .request("providers", RequestBody::ProviderList { provider: None })
        .await
    {
        ResponseBody::ProviderList { revision, .. } => revision,
        other => panic!("provider.list: {other:?}"),
    };
    let response = client
        .request(
            "configure",
            RequestBody::ProviderConfigure {
                command_id: CommandId::new(format!("configure-rebind-proxy-{revision}")),
                provider: PROVIDER.into(),
                api_family: Some(ProviderApiFamilyWire::OpenAiChatCompletions),
                origin: Some(origin.into()),
                auth_requirement: Some(auth),
                enabled: true,
                models: vec![MODEL.into()],
                default_model: Some(MODEL.into()),
                response_open_timeout_ms: Some(OPEN_TIMEOUT_MS),
                chunk_idle_timeout_ms: None,
                semantic_progress_timeout_ms: None,
                probe_vault_reference: None,
                trust: None,
                expected_revision: revision,
            },
        )
        .await;
    assert!(
        matches!(response, ResponseBody::ProviderConfigure { .. }),
        "configure: {response:?}"
    );
}

/// The override tuple is unchanged (`provider`, `None`, `None`), but a new
/// rebind fact must resolve the registry's current endpoint at the next POST.
#[tokio::test]
async fn provider_rebind_without_overrides_refreshes_changed_registry_endpoint() {
    let (release_a, held_a) = oneshot::channel();
    let mut a = Proxy::start(Some(held_a)).await;
    let mut b = Proxy::start(None).await;
    let root = test_root("rebind-current-registry-");
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "rebind-current-registry",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let task = ready_with_dependencies(&config, dependencies()).await;
    let mut client = Client::connect(&config).await;
    configure(&mut client, &a.origin).await;
    let (session, generation) = client.create(&workspace, "current-registry").await;
    let run = client
        .submit(&session, generation, "current-registry")
        .await;
    client.await_post(&mut a).await;

    configure(&mut client, &b.origin).await;
    let response = client
        .request(
            "rebind-current-registry",
            rebind(
                &session,
                generation,
                "rebind-current-registry",
                PROVIDER,
                None,
                None,
            ),
        )
        .await;
    assert!(
        matches!(
            response,
            ResponseBody::SessionProviderRebind {
                base_url: None,
                account: None,
                ..
            }
        ),
        "omitted overrides remain omitted: {response:?}"
    );
    assert_eq!(
        a.ledger().len(),
        1,
        "registry update/rebind leaves A in flight"
    );
    assert!(b.ledger().is_empty());
    release_a.send(()).expect("release old endpoint response");
    client.await_post(&mut b).await;
    client.terminal(&run).await;
    assert_eq!(
        a.ledger().len(),
        1,
        "unchanged override coordinates still re-resolve"
    );
    assert_eq!(b.ledger().len(), 1);
    assert!(b.ledger()[0].contains("rebind-tool"));
    drop(client);
    task.shutdown_handle()
        .request("registry refresh rebind complete");
    task.join().await.expect("daemon stops");
}

/// A selected inactive account supplies the next request's credentials, while
/// an account belonging to another provider is refused before mutation.
#[tokio::test]
async fn provider_rebind_selects_named_account_and_rejects_provider_mismatch() {
    use haider_protocol::credential::{AuthMethod, CredentialDescriptor, CredentialStatus};
    use haider_protocol::ids::CredentialAlias;
    let mut a = Proxy::start(None).await;
    let mut b = Proxy::start(None).await;
    let root = test_root("rebind-account-");
    let workspace = root.path().join("workspace");
    let store = root.path().join("store");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&store).expect("store");
    let descriptors = [
        ("default-account", PROVIDER, true),
        ("selected-account", PROVIDER, false),
        ("other-account", "different-proxy", true),
    ]
    .map(|(alias, provider, active)| CredentialDescriptor {
        alias: CredentialAlias::new(alias),
        provider: provider.into(),
        base_url: None,
        auth_method: AuthMethod::ApiKey,
        identity: alias.into(),
        status: CredentialStatus::Ok,
        active,
        label: None,
        account_identity: None,
        created_at_ms: None,
    });
    std::fs::write(
        store.join("accounts.json"),
        serde_json::to_vec(&descriptors).expect("account JSON"),
    )
    .expect("seed descriptors");
    let vault = Arc::new(MemoryVault::default());
    vault
        .put(
            &CredentialAlias::new("default-account"),
            b"rebind-default-fixture-key",
        )
        .expect("default key");
    vault
        .put(
            &CredentialAlias::new("selected-account"),
            b"rebind-selected-fixture-key",
        )
        .expect("selected key");
    let config = DaemonConfig::new("rebind-account", store, root.path().join("runtime"));
    let task = ready_with_dependencies(
        &config,
        DaemonDependencies {
            accounts: AccountsDependencies {
                vault: VaultProvision::Available(vault),
                ..AccountsDependencies::default()
            },
            ..DaemonDependencies::default()
        },
    )
    .await;
    let mut client = Client::connect(&config).await;
    configure_with_auth(&mut client, &a.origin, ProviderAuthRequirementWire::ApiKey).await;
    let (session, generation) = client.create(&workspace, "account").await;
    let run = client.submit(&session, generation, "default-account").await;
    client.await_post(&mut a).await;
    client.terminal(&run).await;
    assert!(
        a.ledger()[0]
            .to_ascii_lowercase()
            .contains("authorization: bearer rebind-default-fixture-key")
    );

    let refused = client
        .request(
            "wrong-provider-account",
            rebind(
                &session,
                generation,
                "wrong-provider-account",
                PROVIDER,
                Some(&b.origin),
                Some("other-account"),
            ),
        )
        .await;
    assert!(
        matches!(refused, ResponseBody::Error { ref code, retryable: false, .. }
        if code == "account_provider_mismatch"),
        "wrong-provider account: {refused:?}"
    );
    let response = client
        .request(
            "selected-account",
            rebind(
                &session,
                generation,
                "selected-account",
                PROVIDER,
                Some(&b.origin),
                Some("selected-account"),
            ),
        )
        .await;
    assert!(
        matches!(response, ResponseBody::SessionProviderRebind { account: Some(ref alias), .. }
        if alias == "selected-account"),
        "selected account: {response:?}"
    );
    let run = client
        .submit(&session, generation, "selected-account")
        .await;
    client.await_post(&mut b).await;
    client.terminal(&run).await;
    assert_eq!(a.ledger().len(), 1);
    assert_eq!(b.ledger().len(), 1);
    assert!(
        b.ledger()[0]
            .to_ascii_lowercase()
            .contains("authorization: bearer rebind-selected-fixture-key")
    );
    assert!(!b.ledger()[0].contains("rebind-default-fixture-key"));
    drop(client);
    task.shutdown_handle().request("account rebind complete");
    task.join().await.expect("daemon stops");
}

/// A request already received by A finishes there; the tool-loop continuation
/// and post-restart turn reach B, while a second session still resolves A.
#[tokio::test]
async fn provider_rebind_routes_next_request_preserves_inflight_and_replays() {
    let (release_a, held_a) = oneshot::channel();
    let mut a = Proxy::start(Some(held_a)).await;
    let mut b = Proxy::start(None).await;
    let root = test_root("provider-rebind-");
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "provider-rebind",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let task = ready_with_dependencies(&config, dependencies()).await;
    let mut client = Client::connect(&config).await;
    configure(&mut client, &a.origin).await;
    let (session, generation) = client.create(&workspace, "first").await;
    let run = client.submit(&session, generation, "first").await;
    client.await_post(&mut a).await;
    assert_eq!(a.ledger().len(), 1);
    assert!(b.ledger().is_empty());

    let response = client
        .request(
            "rebind",
            rebind(
                &session,
                generation,
                "rebind-first",
                PROVIDER,
                Some(&b.origin),
                None,
            ),
        )
        .await;
    let repeated = client
        .request(
            "rebind-replay",
            rebind(
                &session,
                generation,
                "rebind-first",
                PROVIDER,
                Some(&b.origin),
                None,
            ),
        )
        .await;
    assert_eq!(
        response, repeated,
        "lost-response retry returns the original receipt"
    );
    let selected_seq = match response {
        ResponseBody::SessionProviderRebind {
            session_id,
            provider,
            base_url,
            account,
            selected_seq,
            worker_generation,
        } => {
            assert_eq!(session_id, session);
            assert_eq!(provider, PROVIDER);
            assert_eq!(base_url.as_deref(), Some(b.origin.as_str()));
            assert!(account.is_none());
            assert_eq!(worker_generation, generation);
            selected_seq
        }
        other => panic!("rebind: {other:?}"),
    };
    assert_eq!(
        a.ledger().len(),
        1,
        "rebind must not redispatch the in-flight request"
    );
    assert!(
        b.ledger().is_empty(),
        "no replacement POST before held A returns"
    );
    release_a.send(()).expect("release response on A");
    client.await_post(&mut b).await;
    client.terminal(&run).await;
    assert_eq!(a.ledger().len(), 1);
    assert_eq!(b.ledger().len(), 1);
    assert!(
        b.ledger()[0].contains("rebind-tool"),
        "B receives A's tool continuation"
    );
    assert!(b.ledger()[0].contains("routing fixture first"));

    for (suffix, provider, url, account, code) in [
        (
            "provider",
            "missing-provider",
            None,
            None,
            "provider_unknown",
        ),
        (
            "account",
            PROVIDER,
            None,
            Some("missing-account"),
            "account_unknown",
        ),
        (
            "builtin",
            "openai",
            Some(b.origin.as_str()),
            None,
            "invalid_argument",
        ),
        ("url", PROVIDER, Some("not-a-url"), None, "invalid_argument"),
    ] {
        let response = client
            .request(
                "invalid",
                rebind(
                    &session,
                    generation,
                    &format!("invalid-{suffix}"),
                    provider,
                    url,
                    account,
                ),
            )
            .await;
        assert!(
            matches!(response, ResponseBody::Error { code: ref actual, retryable: false, .. } if actual == code),
            "{suffix}: {response:?}"
        );
    }

    let (other, other_generation) = client.create(&workspace, "other").await;
    let other_run = client.submit(&other, other_generation, "other").await;
    client.await_post(&mut a).await;
    client.terminal(&other_run).await;
    assert_eq!(a.ledger().len(), 2);
    assert!(a.ledger()[1].contains("routing fixture other"));
    assert_eq!(
        b.ledger().len(),
        1,
        "the second session retains registry route A"
    );
    let live = client
        .events
        .iter()
        .find(|event| event.session_id == session && event.seq == selected_seq)
        .expect("live rebind fact")
        .clone();
    let fact = SessionProviderRebound::from_payload_value(
        &serde_json::to_value(&live.payload).expect("payload JSON"),
    )
    .expect("typed rebind fact");
    assert_eq!(fact.base_url.as_deref(), Some(b.origin.as_str()));
    drop(client);
    task.shutdown_handle().request("rebind replay test restart");
    task.join().await.expect("first daemon stops");

    let restarted = ready_with_dependencies(&config, dependencies()).await;
    let mut replay = Client::connect(&config).await;
    let generation = replay.attach(&session).await;
    let replayed = replay
        .events
        .iter()
        .find(|event| event.seq == selected_seq)
        .expect("replayed rebind fact");
    assert_eq!(
        replayed, &live,
        "same durable envelope after daemon restart"
    );
    let run = replay.submit(&session, generation, "restarted").await;
    replay.await_post(&mut b).await;
    replay.terminal(&run).await;
    assert_eq!(a.ledger().len(), 2);
    assert_eq!(b.ledger().len(), 2, "replayed route targets B");
    drop(replay);
    restarted.shutdown_handle().request("rebind test complete");
    restarted.join().await.expect("restarted daemon stops");
}
