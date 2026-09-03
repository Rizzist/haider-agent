//! v0.0.969 upstream-retry regression: the production daemon and real
//! OpenAI-compatible HTTP adapter must terminalize a bounded 429 ladder before
//! the caller's absolute deadline.

#![allow(clippy::expect_used)]

mod support;

use async_trait::async_trait;
use haider_accounts::{MemoryVault, Vault as _};
use haider_core::retry_jittered_backoff_ms;
use haider_daemon::{
    DaemonConfig, DaemonDependencies, ProviderFactory, ProviderFactoryConfig, ResolvedTurnProvider,
};
use haider_protocol::EventPayload;
use haider_protocol::error::{ErrorAction, ErrorCode, ErrorPresentation};
use haider_protocol::headless::{HeadlessRunSpecV1, RunBudgetV1};
use haider_protocol::ids::{CredentialAlias, RunId, SessionId};
use haider_protocol::provider::CapabilityDoc;
use haider_protocol::session::{
    SessionInteractionModeV1, SessionMetadataV1, SessionPermissionOverridesV1,
};
use haider_protocol::state::{RunState, WaitReason};
use haider_provider::{
    OpenAiCompatibleProvider, PreparedTurn, Provider, ProviderError, ProviderStream,
    ToolDefinition, TurnRequest,
};
use haider_rpc::{
    AttachMode, CancelStatus, ClientKind, CommandId, RequestBody, RequestId, ResponseBody,
    WireFrame,
};
use std::collections::{BTreeMap, BTreeSet};
use std::net::Ipv4Addr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use support::{UdsClient, ready_with_dependencies, test_root};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

const UPSTREAM_429_COUNT: usize = 6;
const CALLER_DEADLINE: Duration = Duration::from_millis(32_400);
const TERMINAL_PUBLICATION_GRACE: Duration = Duration::from_secs(2);
const TERMINAL_OBSERVER_DEADLINE: Duration = Duration::from_millis(34_400);
// Registry #94: once the terminal append is durable, replay has no provider
// wait; reuse the two-second local publication/store grace rather than adding
// an unrelated wall-clock constant.
const REPLAY_OBSERVER_DEADLINE: Duration = TERMINAL_PUBLICATION_GRACE;
// The first five lower-half jittered delays can total 15.5--31 seconds. Pick
// a real run coordinate whose deterministic ladder leaves 10.9 seconds for
// worker startup plus six loopback/store cycles under the explicit 32.4-second
// deadline. Attempt six has a minimum 15-second jitter plus both 1s margins,
// so the global 15.5-second minimum through attempt five needs at least 32.5
// seconds to admit a seventh request. The 100ms strict gap forbids it.
const SIXTH_OPEN_LADDER_CEILING: u64 = 21_500;
const RUN_SELECTION_ATTEMPTS: usize = 32;
// Registry #94: one-second cancellation settlement + two copies of the
// two-second local store/publication grace (request and reply) = five seconds.
const SELECTION_CANCEL_DEADLINE: Duration = Duration::from_secs(5);

#[derive(Clone)]
struct RoutingFactory {
    providers: Arc<BTreeMap<String, Arc<dyn Provider>>>,
}

#[async_trait]
impl ProviderFactory for RoutingFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, haider_protocol::error::HaiderError> {
        let provider = self.providers.get(&metadata.provider).ok_or_else(|| {
            haider_protocol::error::HaiderError::new(
                ErrorCode::Internal,
                format!("fixture provider `{}` is not routed", metadata.provider),
                false,
            )
        })?;
        Ok(ResolvedTurnProvider {
            provider: Arc::clone(provider),
            provider_name: metadata.provider.clone(),
            model: metadata.model.clone(),
            context_window: None,
            account_alias: Some("upstretry-fixture-account".into()),
            active_no_auth: true,
            initial_rotation: None,
            rotation_budget_consumed: false,
            attempt_resolver: None,
            compaction_promotion: None,
        })
    }
}

fn dependencies(provider: Arc<dyn Provider>) -> DaemonDependencies {
    let providers = BTreeMap::from([("upstretry-http".to_owned(), provider)]);
    let creatable = providers.keys().cloned().collect::<BTreeSet<_>>();
    DaemonDependencies {
        provider_factory: ProviderFactoryConfig::Injected {
            factory: Arc::new(RoutingFactory {
                providers: Arc::new(providers),
            }),
            providers: creatable,
        },
        ..DaemonDependencies::default()
    }
}

/// Holds only the first physical provider open while the test reads the
/// daemon-minted run id and verifies its deterministic jitter ladder. Once
/// released, every retry crosses the unmodified HTTP adapter directly.
struct FirstOpenBarrierProvider {
    inner: OpenAiCompatibleProvider,
    armed: AtomicBool,
    entered: Semaphore,
    release: Semaphore,
}

impl FirstOpenBarrierProvider {
    fn new(inner: OpenAiCompatibleProvider) -> Self {
        Self {
            inner,
            armed: AtomicBool::new(true),
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
        }
    }

    fn arm(&self) {
        assert!(
            !self.armed.swap(true, Ordering::AcqRel),
            "the prior first-open barrier must be consumed before rearming"
        );
    }

    async fn wait_until_entered(&self) {
        self.entered
            .acquire()
            .await
            .expect("first-open barrier remains live")
            .forget();
    }

    fn release(&self) {
        self.release.add_permits(1);
    }
}

#[async_trait]
impl Provider for FirstOpenBarrierProvider {
    fn prepare_turn(&self, request: &TurnRequest) -> Option<PreparedTurn> {
        self.inner.prepare_turn(request)
    }

    fn prepare_turn_with_tools(
        &self,
        request: &TurnRequest,
        tools: &[ToolDefinition],
    ) -> Option<PreparedTurn> {
        self.inner.prepare_turn_with_tools(request, tools)
    }

    async fn capabilities(&self) -> CapabilityDoc {
        self.inner.capabilities().await
    }

    async fn stream_turn(&self, request: TurnRequest) -> Result<ProviderStream, ProviderError> {
        if self.armed.swap(false, Ordering::AcqRel) {
            self.entered.add_permits(1);
            self.release
                .acquire()
                .await
                .expect("first-open release remains live")
                .forget();
        }
        self.inner.stream_turn(request).await
    }
}

struct Scripted429Server {
    origin: String,
    requests: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Scripted429Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Scripted429Server {
    async fn spawn() -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind scripted 429 provider");
        let address = listener.local_addr().expect("scripted provider address");
        let requests = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.expect("accept provider request");
                read_complete_http_request(&mut stream).await;
                let ordinal = observed.fetch_add(1, Ordering::AcqRel) + 1;
                let diagnostic_terminal_sixth =
                    std::env::var_os("HAIDER_UPSTRETRY_DIAGNOSTIC_TERMINAL_SIXTH").is_some();
                if diagnostic_terminal_sixth && ordinal == UPSTREAM_429_COUNT {
                    write_http_response(
                        &mut stream,
                        "400 Bad Request",
                        "application/json",
                        br#"{"error":"terminal"}"#,
                    )
                    .await;
                } else if ordinal <= UPSTREAM_429_COUNT {
                    write_http_response(
                        &mut stream,
                        "429 Too Many Requests",
                        "application/json",
                        br#"{"error":"transient"}"#,
                    )
                    .await;
                } else {
                    let sse = concat!(
                        "data: {\"id\":\"chatcmpl-upstretry\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
                        "data: {\"id\":\"chatcmpl-upstretry\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                        "data: {\"id\":\"chatcmpl-upstretry\",\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n",
                        "data: [DONE]\n\n"
                    );
                    write_http_response(&mut stream, "200 OK", "text/event-stream", sse.as_bytes())
                        .await;
                }
            }
        });
        Self {
            origin: format!("http://{address}/v1"),
            requests,
            task,
        }
    }

    fn request_count(&self) -> usize {
        self.requests.load(Ordering::Acquire)
    }
}

async fn read_complete_http_request(stream: &mut tokio::net::TcpStream) {
    const MAX_REQUEST_BYTES: usize = 1024 * 1024;
    let mut bytes = Vec::new();
    let mut expected = None;
    loop {
        let mut chunk = [0_u8; 8 * 1024];
        let read = stream
            .read(&mut chunk)
            .await
            .expect("read provider request");
        assert!(read > 0, "provider request ended before its complete body");
        bytes.extend_from_slice(&chunk[..read]);
        assert!(
            bytes.len() <= MAX_REQUEST_BYTES,
            "provider request exceeded the fixture byte bound"
        );
        if expected.is_none()
            && let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n")
        {
            let body_start = header_end + 4;
            let headers = String::from_utf8_lossy(&bytes[..body_start]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().expect("content length"))
                })
                .unwrap_or(0);
            expected = Some(body_start.saturating_add(content_length));
        }
        if expected.is_some_and(|total| bytes.len() >= total) {
            return;
        }
    }
}

async fn write_http_response(
    stream: &mut tokio::net::TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
) {
    let headers = format!(
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(headers.as_bytes())
        .await
        .expect("write provider response headers");
    stream
        .write_all(body)
        .await
        .expect("write complete provider response body");
    stream.flush().await.expect("flush provider response");
}

async fn send_request(
    client: &mut UdsClient,
    config: &DaemonConfig,
    request_id: &str,
    body: RequestBody,
) {
    client
        .send(
            &WireFrame::Request {
                request_id: RequestId::new(request_id),
                body,
            },
            config.frame_limit,
        )
        .await;
}

async fn create_and_attach(
    client: &mut UdsClient,
    config: &DaemonConfig,
    workspace: &Path,
) -> (SessionId, u64) {
    send_request(
        client,
        config,
        "create",
        RequestBody::SessionCreateWithPermissionOverrides {
            command_id: CommandId::new("upstretry-create"),
            cwd: workspace.to_string_lossy().into_owned(),
            provider: "upstretry-http".into(),
            model: "upstretry-model".into(),
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
    let (session_id, generation) = match client.next_reply().await {
        WireFrame::Response {
            body:
                ResponseBody::SessionCreate {
                    session_id,
                    worker_generation,
                    ..
                },
            ..
        } => (session_id, worker_generation),
        other => panic!("expected session.create response, got {other:?}"),
    };
    send_request(
        client,
        config,
        "attach",
        RequestBody::SessionAttach {
            session_id: session_id.clone(),
            after_seq: 0,
            mode: AttachMode::Control,
            sealed_replay: false,
        },
    )
    .await;
    let mut response = false;
    let mut caught_up = false;
    while !(response && caught_up) {
        match client.next().await {
            WireFrame::Response {
                body: ResponseBody::SessionAttach { .. },
                ..
            } => response = true,
            WireFrame::AttachCaughtUp { .. } => caught_up = true,
            _ => {}
        }
    }
    (session_id, generation)
}

fn unix_time_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("wall clock follows Unix epoch")
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

async fn start_bounded_run(
    client: &mut UdsClient,
    config: &DaemonConfig,
    workspace: &Path,
    session_id: &SessionId,
    generation: u64,
    ordinal: usize,
) -> (RunId, Instant) {
    let started = Instant::now();
    let request_deadline_unix_ms = unix_time_ms().saturating_add(
        u64::try_from(CALLER_DEADLINE.as_millis()).expect("caller deadline fits u64"),
    );
    send_request(
        client,
        config,
        &format!("headless-start-{ordinal}"),
        RequestBody::HeadlessRunStart {
            command_id: CommandId::new(format!("upstretry-run-{ordinal}")),
            session_id: session_id.clone(),
            worker_generation: generation,
            text: "exercise bounded upstream retry".into(),
            attachments: Vec::new(),
            spec: HeadlessRunSpecV1 {
                cwd: workspace.to_string_lossy().into_owned(),
                provider: "upstretry-http".into(),
                model: "upstretry-model".into(),
                max_output_tokens: 4096,
                effort: None,
                fast: false,
                seed: Some(969),
                permission_overrides: SessionPermissionOverridesV1::default(),
                trust_hooks: false,
                budget: RunBudgetV1::default(),
                request_deadline_unix_ms: Some(request_deadline_unix_ms),
                replay_of: None,
            },
            trust_hooks: false,
        },
    )
    .await;
    loop {
        match client.next().await {
            WireFrame::Response {
                body: ResponseBody::HeadlessRunStart { run_id, .. },
                ..
            } => return (run_id, started),
            WireFrame::Response {
                body: ResponseBody::Error { code, message, .. },
                ..
            } => panic!("headless start failed ({code}): {message}"),
            _ => {}
        }
    }
}

async fn cancel_selection_run(
    client: &mut UdsClient,
    config: &DaemonConfig,
    session_id: &SessionId,
    generation: u64,
    run_id: &RunId,
    ordinal: usize,
) {
    send_request(
        client,
        config,
        &format!("cancel-selection-{ordinal}"),
        RequestBody::TurnCancel {
            command_id: CommandId::new(format!("cancel-selection-{ordinal}")),
            session_id: session_id.clone(),
            worker_generation: generation,
            run_id: run_id.clone(),
        },
    )
    .await;
    tokio::time::timeout(SELECTION_CANCEL_DEADLINE, async {
        loop {
            if let WireFrame::Response {
                body:
                    ResponseBody::TurnCancel {
                        status: CancelStatus::Accepted | CancelStatus::AlreadyTerminal,
                        ..
                    },
                ..
            } = client.next().await
            {
                return;
            }
        }
    })
    .await
    .expect("selection cancellation settles within its derived deadline");
}

#[derive(Default)]
struct TerminalObservation {
    events: Vec<EventPayload>,
    failure: Option<(ErrorCode, String, bool, Option<ErrorPresentation>)>,
    terminal: Option<RunState>,
}

async fn observe_terminal(client: &mut UdsClient, run_id: &RunId) -> TerminalObservation {
    let mut observation = TerminalObservation::default();
    tokio::time::timeout(TERMINAL_OBSERVER_DEADLINE, async {
        loop {
            let Some(frame) = client.try_next().await else {
                panic!("daemon connection closed before run {run_id} terminalized")
            };
            let WireFrame::Event { envelope, .. } = frame else {
                continue;
            };
            if envelope.run_id.as_ref() != Some(run_id) {
                continue;
            }
            let Ok(payload) = envelope.payload.decode_event() else {
                continue;
            };
            match &payload {
                EventPayload::RunFailed {
                    code,
                    message,
                    retryable,
                    presentation,
                } => {
                    observation.failure =
                        Some((*code, message.clone(), *retryable, presentation.clone()));
                }
                EventPayload::RunState(state) if state.is_terminal() => {
                    observation.terminal = Some(state.clone());
                }
                _ => {}
            }
            observation.events.push(payload);
            if observation.terminal.is_some() {
                return;
            }
        }
    })
    .await
    .expect("32.4s caller deadline plus 2s terminal-publication grace");
    observation
}

async fn replay_run_events(
    config: &DaemonConfig,
    session_id: &SessionId,
    run_id: &RunId,
) -> Vec<EventPayload> {
    let mut replay = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "upstretry-bounded-429-replay",
        "upstretry-replay-client",
        ClientKind::Headless,
    )
    .await;
    send_request(
        &mut replay,
        config,
        "replay-attach",
        RequestBody::SessionAttach {
            session_id: session_id.clone(),
            after_seq: 0,
            mode: AttachMode::View,
            sealed_replay: true,
        },
    )
    .await;
    tokio::time::timeout(REPLAY_OBSERVER_DEADLINE, async {
        let mut response = false;
        let mut caught_up = false;
        let mut events = Vec::new();
        while !(response && caught_up) {
            match replay.next().await {
                WireFrame::Response {
                    body: ResponseBody::SessionAttach { .. },
                    ..
                } => response = true,
                WireFrame::AttachCaughtUp { .. } => caught_up = true,
                WireFrame::Event { envelope, .. } if envelope.run_id.as_ref() == Some(run_id) => {
                    if let Ok(payload) = envelope.payload.decode_event() {
                        events.push(payload);
                    }
                }
                _ => {}
            }
        }
        events
    })
    .await
    .expect("durable terminal replays within the local publication grace")
}

fn retry_delays(events: &[EventPayload]) -> Vec<u64> {
    events
        .iter()
        .filter_map(|payload| match payload {
            EventPayload::RunState(RunState::Retrying { delay_ms, .. }) => Some(*delay_ms),
            _ => None,
        })
        .collect()
}

fn retry_sequence(events: &[EventPayload]) -> Vec<(u32, u32, u64, WaitReason)> {
    events
        .iter()
        .filter_map(|payload| match payload {
            EventPayload::RunState(RunState::Retrying {
                attempt,
                max,
                delay_ms,
                reason,
            }) => Some((*attempt, *max, *delay_ms, reason.clone())),
            _ => None,
        })
        .collect()
}

/// Six complete HTTP 429 bodies must not enter a backoff which the absolute
/// caller budget cannot contain. The run fails as the bounded provider error
/// after six real HTTP opens; it never relies on the caller's timeout.
#[tokio::test]
async fn bounded_429_ladder_terminalizes_before_caller_deadline() {
    assert_eq!(
        TERMINAL_OBSERVER_DEADLINE,
        CALLER_DEADLINE + TERMINAL_PUBLICATION_GRACE,
        "registry #94 observer arithmetic stays explicit"
    );
    let server = Scripted429Server::spawn().await;
    let vault = MemoryVault::default();
    let alias = CredentialAlias::new("upstretry-construction-token");
    vault
        .put(&alias, b"fixture-secret-never-sent")
        .expect("stage provider construction token");
    let provider = Arc::new(FirstOpenBarrierProvider::new(
        OpenAiCompatibleProvider::new_custom_no_auth(
            vault.resolve(&alias).expect("resolve construction token"),
            "upstretry-model",
            &server.origin,
        )
        .expect("construct OpenAI-compatible provider"),
    ));
    let routed: Arc<dyn Provider> = provider.clone();

    let root = test_root("upstretry-bounded-429-");
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("create fixture workspace");
    let config = DaemonConfig::new(
        "upstretry-bounded-429",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let _task = ready_with_dependencies(&config, dependencies(routed)).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "upstretry-bounded-429",
        "upstretry-client",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut client, &config, &workspace).await;

    let mut selected = None;
    for ordinal in 1..=RUN_SELECTION_ATTEMPTS {
        if ordinal > 1 {
            provider.arm();
        }
        let (run_id, started) = start_bounded_run(
            &mut client,
            &config,
            &workspace,
            &session_id,
            generation,
            ordinal,
        )
        .await;
        provider.wait_until_entered().await;
        let first_five = (1..UPSTREAM_429_COUNT)
            .map(|attempt| retry_jittered_backoff_ms(&run_id, attempt))
            .collect::<Vec<_>>();
        let sum = first_five.iter().copied().sum::<u64>();
        if sum <= SIXTH_OPEN_LADDER_CEILING {
            selected = Some((run_id, started, first_five));
            provider.release();
            break;
        }
        cancel_selection_run(
            &mut client,
            &config,
            &session_id,
            generation,
            &run_id,
            ordinal,
        )
        .await;
    }
    let (run_id, started, expected_delays) = selected.expect(
        "a deterministic run id with <=21.5s through five backoffs is found within 32 candidates",
    );

    let observation = observe_terminal(&mut client, &run_id).await;
    let wall = started.elapsed();
    eprintln!(
        "upstretry run={run_id} wall_ms={} requests={} delays={:?}",
        wall.as_millis(),
        server.request_count(),
        retry_delays(&observation.events),
    );

    assert!(
        wall < CALLER_DEADLINE,
        "the provider failure must self-terminalize before the 32.4s caller deadline; wall={wall:?}"
    );
    assert_eq!(server.request_count(), UPSTREAM_429_COUNT);
    assert_eq!(observation.terminal, Some(RunState::Errored));
    assert_eq!(
        observation.failure.as_ref().map(|(code, _, _, _)| *code),
        Some(ErrorCode::ProviderError),
        "the bounded rate limit remains a provider failure, not a timeout"
    );
    let (_, failure_message, retryable, presentation) = observation
        .failure
        .as_ref()
        .expect("bounded ladder commits run_failed");
    assert!(failure_message.contains("RateLimited"));
    assert!(!retryable, "deadline-bounded retry exhaustion is terminal");
    assert_eq!(
        presentation
            .as_ref()
            .expect("provider failure has a typed presentation")
            .allowed_actions,
        vec![ErrorAction::None]
    );
    let delays = retry_delays(&observation.events);
    assert_eq!(
        delays, expected_delays,
        "no retry wait was inserted or lost"
    );
    let expected_retry_sequence = expected_delays
        .iter()
        .enumerate()
        .map(|(index, delay_ms)| {
            (
                u32::try_from(index + 2).expect("retry attempt fits u32"),
                10,
                *delay_ms,
                WaitReason::RateLimit,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        retry_sequence(&observation.events),
        expected_retry_sequence,
        "typed attempt/max/delay/reason telemetry is the exact jittered ladder"
    );
    let waiting = observation
        .events
        .iter()
        .filter_map(|payload| match payload {
            EventPayload::RunState(RunState::Waiting { reason }) => Some(reason),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        waiting,
        vec![&WaitReason::RateLimit; expected_delays.len()],
        "each jittered retry has one rate-limit wait and no route wait"
    );
    let thinking_cycles = observation
        .events
        .iter()
        .filter(|payload| matches!(payload, EventPayload::RunState(RunState::Thinking)))
        .count();
    assert_eq!(thinking_cycles, UPSTREAM_429_COUNT);
    let terminal_count = observation
        .events
        .iter()
        .filter(|payload| matches!(payload, EventPayload::RunState(state) if state.is_terminal()))
        .count();
    assert_eq!(terminal_count, 1, "the live stream has one typed terminal");
    assert_eq!(
        observation
            .events
            .iter()
            .filter(|payload| matches!(payload, EventPayload::RunFailed { .. }))
            .count(),
        1,
        "the live stream has one adjacent failure detail"
    );
    assert!(
        matches!(
            observation.events.as_slice(),
            [
                ..,
                EventPayload::RunFailed { .. },
                EventPayload::RunState(RunState::Errored)
            ]
        ),
        "run_failed immediately precedes the typed terminal"
    );
    let replay = replay_run_events(&config, &session_id, &run_id).await;
    assert_eq!(
        replay.last().zip(replay.iter().rev().nth(1)),
        observation
            .events
            .last()
            .zip(observation.events.iter().rev().nth(1)),
        "sealed replay preserves the same adjacent terminal pair"
    );
    assert_eq!(
        replay
            .iter()
            .filter(
                |payload| matches!(payload, EventPayload::RunState(state) if state.is_terminal())
            )
            .count(),
        1,
        "sealed replay also has exactly one typed terminal"
    );
    let ladder_wall = Duration::from_millis(expected_delays.iter().copied().sum());
    assert!(
        wall <= ladder_wall + TERMINAL_PUBLICATION_GRACE,
        "loopback/store overhead exceeded the deadline-derived publication grace: wall={wall:?}, ladder={ladder_wall:?}"
    );
}
