//! W3c1 primary gate: production daemon/runtime over a real UnixStream.
//!
//! All thirteen numbered report-§6.1 scenarios live here, each headed
//! `Scenario N` and driven by an injected fake provider factory; no test in
//! this file may use a live API. File order: scenario 1, scenarios 3-8, the
//! worker-aware-drain satellite, scenarios 9-12, scenario 2 (the M2 prefix)
//! with its two session-create satellites, then scenario 13 — the
//! mutation-seam sweep manifest that names every load-bearing seam and the
//! focused test observing it.

#![allow(clippy::expect_used)]

mod support;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use haider_core::{CancelToken, StoreHandle, ToolDispatchResult, ToolDispatcher};
use haider_daemon::ProviderFactoryConfig;
use haider_daemon::{
    DaemonConfig, DaemonDependencies, ProviderFactory, ResolvedTurnProvider, TurnToolFactory,
    WorkerToolContext,
};
use haider_protocol::DeliveryMode;
use haider_protocol::EventPayload;
use haider_protocol::context::ContextFootprintTruth;
use haider_protocol::effect::{
    AuthorizationSource, AuthorizationVerdict, EffectClass, EffectIntent, EffectOutcome,
    EffectPhase, FileFreshness,
};
use haider_protocol::envelope::{
    EventEnvelope, PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION,
};
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::hook::HookEventPayload;
#[cfg(windows)]
use haider_protocol::ids::ItemId;
use haider_protocol::ids::{ArtifactRef, DeviceId, EffectId, EventId, MenuId, RunId, SessionId};
use haider_protocol::item::{ItemDelta, ItemEvent, OutputStream, TurnItem, UserCommandOriginV1};
use haider_protocol::menu::{Menu, MenuAnswer};
use haider_protocol::provider::{
    Block, CacheStatAvailability, CapabilityDoc, FinishReason, RequestUsage, Usage, UsageSource,
};
use haider_protocol::session::{SessionMetadataV1, SessionPermissionOverridesV1};
use haider_protocol::state::{RunState, SessionState};
use haider_protocol::tool::{AttachmentBlock, ToolPermissionDefault};
use haider_protocol::workspace::WorkspaceEventPayload;
use haider_provider::{
    FakeInputKind, FakeInputOption, FakeProvider, FakeStep, Provider, ProviderError,
    ProviderStream, ToolDefinition, TurnRequest,
};
use haider_rpc::{
    AttachMode, CancelStatus, Capability, CapabilitySet, ClientKind, CommandId,
    ERROR_CODE_ALREADY_RESOLVED, ERROR_CODE_BUSY, ERROR_CODE_CAPABILITY_DENIED,
    ERROR_CODE_INVALID_ARGUMENT, ERROR_CODE_UNSUPPORTED_SHELL_BUILTIN, FEATURE_SESSION_MUTATION_V1,
    FEATURE_SESSION_PERMISSION_OVERRIDES_V1, FEATURE_TURN_CONTROL_V1, RequestBody, RequestId,
    ResponseBody, SeqRange, SessionSummary, WireFrame,
};
use haider_store::{EventStore, Store};
use haider_tools::{
    ChangeLedgerSink, EffectBroker, FsEdit, FsWriteRecord, JournalSink, PermissionPolicy,
    ToolError, ToolResult, TurnAttribution,
};
use std::fs;
use std::future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex as StdMutex};
use support::{UdsClient, ready, ready_with_dependencies, test_root};
use tokio::sync::Semaphore;

// Spawning powershell.exe + cmd.exe + ping can be starved well past the
// suite-wide 60s bound on a loaded Windows runner. The in-command descendant
// readiness check remains independently bounded below the process wall limit.
#[cfg(windows)]
const WINDOWS_PROCESS_START_DEADLINE: std::time::Duration = std::time::Duration::from_secs(240);

// PowerShell, cmd.exe, and the Job-Object descendant probes are deliberately
// real. Running several of them at once on a hosted Windows runner can starve
// process creation long enough for every test to enter its bounded retry path,
// whose combined wall time then outlives the crate runner. Serialize only this
// integration binary's real-process tests; protocol-only tests stay parallel.
#[cfg(windows)]
static WINDOWS_REAL_PROCESS_TEST_GATE: Semaphore = Semaphore::const_new(1);

#[cfg(windows)]
async fn windows_real_process_test_guard(
    test_name: &'static str,
) -> tokio::sync::SemaphorePermit<'static> {
    eprintln!("haider-daemond windows-process test={test_name} phase=waiting-for-gate");
    let permit = WINDOWS_REAL_PROCESS_TEST_GATE
        .acquire()
        .await
        .expect("Windows real-process test gate remains open");
    eprintln!("haider-daemond windows-process test={test_name} phase=running");
    permit
}

#[derive(Clone)]
struct FakeFactory {
    fake: Arc<FakeProvider>,
}

trait FakeRequestCounter {
    fn request_count(&self) -> usize;
}

impl<T> FakeRequestCounter for Arc<T>
where
    T: FakeRequestCounter + ?Sized,
{
    fn request_count(&self) -> usize {
        self.as_ref().request_count()
    }
}

impl FakeRequestCounter for FakeProvider {
    fn request_count(&self) -> usize {
        self.requests().len()
    }
}

#[cfg(windows)]
#[derive(Clone)]
struct WindowsExecFakeFactory {
    active: Arc<StdMutex<Arc<FakeProvider>>>,
    first_attempt: Arc<FakeProvider>,
    retry: Arc<FakeProvider>,
}

#[cfg(windows)]
impl WindowsExecFakeFactory {
    fn arm_retry(&self) {
        match self.active.lock() {
            Ok(mut active) => *active = Arc::clone(&self.retry),
            Err(poisoned) => *poisoned.into_inner() = Arc::clone(&self.retry),
        }
    }

    fn first_attempt_request_count(&self) -> usize {
        self.first_attempt.requests().len()
    }

    fn retry_request_count(&self) -> usize {
        self.retry.requests().len()
    }
}

#[cfg(windows)]
impl FakeRequestCounter for WindowsExecFakeFactory {
    fn request_count(&self) -> usize {
        self.first_attempt_request_count()
            .saturating_add(self.retry_request_count())
    }
}

fn expected_request_usage(ordinal: u64, usage: &Usage) -> RequestUsage {
    RequestUsage {
        ordinal,
        input: usage.input,
        output: usage.output,
        reasoning: (usage.reasoning > 0).then_some(usage.reasoning),
        cached: (usage.cached > 0
            || usage.normalized.as_ref().is_some_and(|normalized| {
                normalized.cache_status == CacheStatAvailability::Present
            }))
        .then_some(usage.cached),
        source: usage.source,
        account: usage.account.clone(),
        normalized: usage.normalized.clone(),
        cache_cost: usage.cache_cost,
        // The daemon supplies the request diagnostic from the rendered
        // provider call; these fixtures only own the response-local usage.
        cache: None,
    }
}

#[async_trait]
impl ProviderFactory for FakeFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, haider_protocol::error::HaiderError> {
        Ok(ResolvedTurnProvider {
            provider: self.fake.clone(),
            provider_name: metadata.provider.clone(),
            model: metadata.model.clone(),
            context_window: None,
            account_alias: None,
            active_no_auth: false,
            initial_rotation: None,
            rotation_budget_consumed: false,
            attempt_resolver: None,
            compaction_promotion: None,
        })
    }
}

#[cfg(windows)]
#[async_trait]
impl ProviderFactory for WindowsExecFakeFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, haider_protocol::error::HaiderError> {
        let provider = match self.active.lock() {
            Ok(active) => Arc::clone(&active),
            Err(poisoned) => Arc::clone(&poisoned.into_inner()),
        };
        Ok(ResolvedTurnProvider {
            provider,
            provider_name: metadata.provider.clone(),
            model: metadata.model.clone(),
            context_window: None,
            account_alias: None,
            active_no_auth: false,
            initial_rotation: None,
            rotation_budget_consumed: false,
            attempt_resolver: None,
            compaction_promotion: None,
        })
    }
}

fn fake_dependencies(script: Vec<FakeStep>) -> (DaemonDependencies, Arc<FakeProvider>) {
    fake_dependencies_for_provider(script, "fake")
}

fn fake_dependencies_for_provider(
    script: Vec<FakeStep>,
    provider_name: &str,
) -> (DaemonDependencies, Arc<FakeProvider>) {
    let fake = Arc::new(FakeProvider::new(script));
    let dependencies = DaemonDependencies {
        provider_factory: ProviderFactoryConfig::Injected {
            factory: Arc::new(FakeFactory { fake: fake.clone() }),
            providers: std::collections::BTreeSet::from([provider_name.to_owned()]),
        },
        ..DaemonDependencies::default()
    };
    (dependencies, fake)
}

#[cfg(windows)]
fn windows_exec_fake_dependencies(
    first_attempt_script: Vec<FakeStep>,
    retry_script: Vec<FakeStep>,
) -> (DaemonDependencies, Arc<WindowsExecFakeFactory>) {
    let first_attempt = Arc::new(FakeProvider::new(first_attempt_script));
    let retry = Arc::new(FakeProvider::new(retry_script));
    let fake = Arc::new(WindowsExecFakeFactory {
        active: Arc::new(StdMutex::new(Arc::clone(&first_attempt))),
        first_attempt,
        retry,
    });
    let dependencies = DaemonDependencies {
        provider_factory: ProviderFactoryConfig::Injected {
            factory: fake.clone(),
            providers: std::collections::BTreeSet::from(["fake".to_owned()]),
        },
        ..DaemonDependencies::default()
    };
    (dependencies, fake)
}

#[derive(Clone)]
struct DurableEntryFactory {
    fake: Arc<FakeProvider>,
    database_path: std::path::PathBuf,
    inspections: Arc<AtomicUsize>,
}

#[async_trait]
impl ProviderFactory for DurableEntryFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, HaiderError> {
        Ok(ResolvedTurnProvider {
            provider: Arc::new(DurableEntryProvider {
                fake: self.fake.clone(),
                database_path: self.database_path.clone(),
                inspections: self.inspections.clone(),
            }),
            provider_name: "fake".into(),
            model: metadata.model.clone(),
            context_window: None,
            account_alias: None,
            active_no_auth: false,
            initial_rotation: None,
            rotation_budget_consumed: false,
            attempt_resolver: None,
            compaction_promotion: None,
        })
    }
}

struct DurableEntryProvider {
    fake: Arc<FakeProvider>,
    database_path: std::path::PathBuf,
    inspections: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for DurableEntryProvider {
    async fn capabilities(&self) -> CapabilityDoc {
        self.fake.capabilities().await
    }

    async fn stream_turn(&self, request: TurnRequest) -> Result<ProviderStream, ProviderError> {
        {
            let connection = rusqlite::Connection::open_with_flags(
                &self.database_path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .expect("provider-entry store opens read-only");
            let mut statement = connection
                .prepare("SELECT envelope_json FROM events ORDER BY seq ASC")
                .expect("provider-entry query");
            let mut rows = statement.query([]).expect("provider-entry rows");
            let mut envelopes = Vec::<RawEnvelope>::new();
            while let Some(row) = rows.next().expect("provider-entry row") {
                let envelope = match row.get_ref(0).expect("stored envelope") {
                    rusqlite::types::ValueRef::Text(bytes) => {
                        serde_json::from_slice(bytes).expect("typed legacy JSON envelope")
                    }
                    rusqlite::types::ValueRef::Blob(bytes) => {
                        rmp_serde::from_slice(bytes).expect("typed MessagePack envelope")
                    }
                    value => panic!("unexpected envelope storage class: {value:?}"),
                };
                envelopes.push(envelope);
            }
            let run = envelopes.iter().find_map(|envelope| {
                let payload =
                    serde_json::from_value::<EventPayload>(envelope.payload.clone().into()).ok()?;
                matches!(
                    payload,
                    EventPayload::UserMessage { ref text, .. } if text == "say hello"
                )
                .then(|| envelope.run_id.clone())
                .flatten()
            });
            let run = run.expect("UserMessage is durable before provider entry");
            assert!(envelopes.iter().any(|envelope| {
                envelope.run_id.as_ref() == Some(&run)
                    && serde_json::from_value::<EventPayload>(envelope.payload.clone().into())
                        .is_ok_and(|payload| payload == EventPayload::RunState(RunState::Queued))
            }));
            self.inspections.fetch_add(1, Ordering::SeqCst);
        }
        self.fake.stream_turn(request).await
    }
}

fn recovery_fixture_envelope(
    session_id: &SessionId,
    run_id: &RunId,
    generation: u64,
    event_id: &str,
    payload: EventPayload,
    prompt: PromptRender,
) -> RawEnvelope {
    EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(event_id),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: Some(run_id.clone()),
        agent_id: None,
        device_id: DeviceId::new("recovery-fixture"),
        authority_epoch: 0,
        worker_generation: generation,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt,
        },
        payload: serde_json::to_value(payload)
            .expect("recovery payload")
            .into(),
    }
}

fn create_body(command_id: &str, cwd: String) -> RequestBody {
    create_body_for_provider(command_id, cwd, "fake", "fake-v1")
}

fn create_body_for_provider(
    command_id: &str,
    cwd: String,
    provider: &str,
    model: &str,
) -> RequestBody {
    RequestBody::SessionCreate {
        command_id: CommandId::new(command_id),
        cwd,
        provider: provider.into(),
        model: model.into(),
        max_tokens: 4096,
    }
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

fn created_response(frame: WireFrame) -> (haider_protocol::ids::SessionId, SessionMetadataV1) {
    match frame {
        WireFrame::Response {
            body:
                ResponseBody::SessionCreate {
                    session_id,
                    created_seq,
                    metadata,
                    ..
                },
            ..
        } => {
            assert_eq!(created_seq, 1);
            (session_id, metadata)
        }
        other => panic!("expected session.create response, got {other:?}"),
    }
}

async fn create_and_attach(
    client: &mut UdsClient,
    config: &DaemonConfig,
    workspace: &std::path::Path,
) -> (haider_protocol::ids::SessionId, u64) {
    create_and_attach_for_provider(client, config, workspace, "fake", "fake-v1").await
}

async fn create_and_attach_for_provider(
    client: &mut UdsClient,
    config: &DaemonConfig,
    workspace: &std::path::Path,
    provider: &str,
    model: &str,
) -> (haider_protocol::ids::SessionId, u64) {
    send_request(
        client,
        config,
        "create",
        create_body_for_provider(
            "create-command",
            workspace.to_string_lossy().into_owned(),
            provider,
            model,
        ),
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
        other => panic!("expected create response, got {other:?}"),
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
    assert!(matches!(
        client.next_reply().await,
        WireFrame::Response {
            body: ResponseBody::SessionAttach { .. },
            ..
        }
    ));
    loop {
        if matches!(client.next().await, WireFrame::AttachCaughtUp { .. }) {
            break;
        }
    }
    (session_id, generation)
}

fn submit_body(
    command_id: &str,
    session_id: haider_protocol::ids::SessionId,
    generation: u64,
    text: &str,
) -> RequestBody {
    RequestBody::TurnSubmit {
        command_id: CommandId::new(command_id),
        session_id,
        worker_generation: generation,
        text: text.into(),
        attachments: Vec::new(),
        mode: DeliveryMode::Queue,
    }
}

async fn upload_artifact(
    client: &mut UdsClient,
    config: &DaemonConfig,
    request_id: &str,
    bytes: &[u8],
) -> ArtifactRef {
    send_request(
        client,
        config,
        request_id,
        RequestBody::ArtifactPut {
            data_base64: BASE64.encode(bytes),
        },
    )
    .await;
    match next_response(client).await {
        WireFrame::Response {
            body:
                ResponseBody::ArtifactPut {
                    artifact,
                    bytes: stored,
                },
            ..
        } => {
            assert_eq!(stored, u64::try_from(bytes.len()).expect("fixture size"));
            artifact
        }
        other => panic!("expected artifact.put response, got {other:?}"),
    }
}

async fn next_submit_response(client: &mut UdsClient) -> (haider_protocol::ids::RunId, u64) {
    loop {
        if let WireFrame::Response {
            body:
                ResponseBody::TurnSubmit {
                    run_id,
                    accepted_seq,
                    ..
                },
            ..
        } = client.next().await
        {
            return (run_id, accepted_seq);
        }
    }
}

async fn next_response(client: &mut UdsClient) -> WireFrame {
    loop {
        let frame = client.next().await;
        if matches!(frame, WireFrame::Response { .. }) {
            return frame;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn answer_menu(
    client: &mut UdsClient,
    config: &DaemonConfig,
    request_id: &str,
    command_id: &str,
    session_id: SessionId,
    menu_id: haider_protocol::ids::MenuId,
    request_seq: u64,
    worker_generation: u64,
    option_key: &str,
    option_index: u32,
) {
    client
        .send(
            &WireFrame::MenuAnswer {
                request_id: Some(RequestId::new(request_id)),
                command_id: CommandId::new(command_id),
                session_id,
                menu_id,
                request_seq,
                worker_generation,
                option_key: option_key.into(),
                option_index,
                input: None,
            },
            config.frame_limit,
        )
        .await;
    let response = next_response(client).await;
    assert!(
        matches!(
            response,
            WireFrame::Response {
                body: ResponseBody::MenuAnswer { .. },
                ..
            }
        ),
        "expected menu success, got {response:?}"
    );
}

async fn next_permission_menu(client: &mut UdsClient) -> (haider_protocol::menu::Menu, u64, u64) {
    loop {
        if let WireFrame::Event { envelope, .. } = client.next().await
            && let Ok(EventPayload::MenuOpened(menu)) =
                serde_json::from_value::<EventPayload>(envelope.payload.into())
            && matches!(
                menu.kind,
                haider_protocol::menu::MenuKind::Permission { .. }
            )
        {
            return (menu, envelope.seq, envelope.worker_generation);
        }
    }
}

async fn next_permission_menu_before_create(
    client: &mut UdsClient,
    target: &std::path::Path,
) -> (haider_protocol::menu::Menu, u64, u64) {
    loop {
        assert!(
            !target.exists(),
            "approval-bypass sentinel: file mutated before a committed approval"
        );
        let frame = tokio::select! {
            frame = client.next() => frame,
            () = tokio::time::sleep(std::time::Duration::from_millis(1)) => continue,
        };
        if let WireFrame::Event { envelope, .. } = frame
            && let Ok(EventPayload::MenuOpened(menu)) =
                serde_json::from_value::<EventPayload>(envelope.payload.into())
            && matches!(
                menu.kind,
                haider_protocol::menu::MenuKind::Permission { .. }
            )
        {
            assert!(
                !target.exists(),
                "approval-bypass sentinel: file mutated while opening approval"
            );
            return (menu, envelope.seq, envelope.worker_generation);
        }
    }
}

async fn attach_existing(
    client: &mut UdsClient,
    config: &DaemonConfig,
    session_id: haider_protocol::ids::SessionId,
    after_seq: u64,
    request_id: &str,
) -> Vec<RawEnvelope> {
    send_request(
        client,
        config,
        request_id,
        RequestBody::SessionAttach {
            session_id: session_id.clone(),
            after_seq,
            mode: AttachMode::Control,
            sealed_replay: false,
        },
    )
    .await;
    let mut response = false;
    let mut caught_up = false;
    let mut replay = Vec::new();
    while !(response && caught_up) {
        match client.next().await {
            WireFrame::Response {
                body: ResponseBody::SessionAttach { .. },
                ..
            } => response = true,
            WireFrame::AttachCaughtUp { .. } => caught_up = true,
            WireFrame::Event { envelope, .. } => replay.push(envelope),
            _ => {}
        }
    }
    replay
}

async fn events_until_terminal(
    client: &mut UdsClient,
    run_id: &haider_protocol::ids::RunId,
) -> Vec<(u64, EventPayload)> {
    let mut events = Vec::new();
    loop {
        if let WireFrame::Event { envelope, .. } = client.next().await {
            if envelope.run_id.as_ref() != Some(run_id) {
                continue;
            }
            // Hook-engine facts are documented additive journal extensions,
            // not turn-state payloads. Ignore only that named family while
            // retaining strict decoding for every other run-scoped frame.
            if HookEventPayload::is_engine_fact(&envelope.payload) {
                continue;
            }
            let payload = serde_json::from_value::<EventPayload>(envelope.payload.into())
                .expect("typed event");
            let terminal = matches!(
                payload,
                EventPayload::RunState(RunState::Done | RunState::Errored | RunState::Cancelled)
            );
            events.push((envelope.seq, payload));
            if terminal {
                return events;
            }
        }
    }
}

async fn next_idle(client: &mut UdsClient) -> bool {
    loop {
        if let WireFrame::Event { envelope, .. } = client.next().await
            && let Ok(EventPayload::SessionState(SessionState::Idle { interrupted })) =
                serde_json::from_value::<EventPayload>(envelope.payload.into())
        {
            return interrupted;
        }
    }
}

fn process_start_deadline() -> std::time::Duration {
    #[cfg(windows)]
    {
        WINDOWS_PROCESS_START_DEADLINE
    }
    #[cfg(not(windows))]
    {
        support::DEADLINE
    }
}

#[cfg(windows)]
const OBSERVED_FRAME_TRACE_LIMIT: usize = 200;

#[cfg(windows)]
struct WindowsProcessStartTrace {
    test_name: &'static str,
    run_id: RunId,
    started_at: tokio::time::Instant,
    observed_frames: usize,
    last_progress: Option<(bool, u64)>,
    enabled: bool,
}

#[cfg(windows)]
impl WindowsProcessStartTrace {
    fn new(test_name: &'static str, run_id: &RunId, heartbeat: &std::path::Path) -> Self {
        let enabled = windows_test_process_trace_enabled();
        if enabled {
            eprintln!(
                "haider-daemond windows-process test={test_name} phase=first-start-wait-begin run_id={run_id} heartbeat={} exists={}",
                heartbeat.display(),
                heartbeat.exists(),
            );
        }
        Self {
            test_name,
            run_id: run_id.clone(),
            started_at: tokio::time::Instant::now(),
            observed_frames: 0,
            last_progress: None,
            enabled,
        }
    }

    fn heartbeat_started(
        &mut self,
        heartbeat: &std::path::Path,
        workspace: &std::path::Path,
    ) -> bool {
        let bytes = fs::metadata(heartbeat).map_or(0, |metadata| metadata.len());
        let ps_alive = workspace.join("ps-alive.log").exists();
        if self.enabled && self.last_progress != Some((ps_alive, bytes)) {
            eprintln!(
                "haider-daemond windows-process test={} phase=ps-alive exists={ps_alive} heartbeat_bytes={bytes} elapsed_ms={}",
                self.test_name,
                self.started_at.elapsed().as_millis(),
            );
            self.last_progress = Some((ps_alive, bytes));
        }
        bytes > 1
    }

    fn observed_frame(&mut self, frame: &WireFrame) {
        if !self.enabled {
            return;
        }
        self.trace_run_event(frame);
        if self.observed_frames < OBSERVED_FRAME_TRACE_LIMIT {
            eprintln!(
                "haider-daemond windows-process test={} phase=observed-frame kind={} elapsed_ms={}",
                self.test_name,
                observed_frame_kind(frame),
                self.started_at.elapsed().as_millis(),
            );
        } else if self.observed_frames == OBSERVED_FRAME_TRACE_LIMIT {
            eprintln!(
                "haider-daemond windows-process test={} phase=observed-frame-cap elapsed_ms={}",
                self.test_name,
                self.started_at.elapsed().as_millis(),
            );
        }
        self.observed_frames = self.observed_frames.saturating_add(1);
    }

    fn trace_run_event(&self, frame: &WireFrame) {
        let WireFrame::Event { envelope, .. } = frame else {
            return;
        };
        if envelope.run_id.as_ref() != Some(&self.run_id) {
            return;
        }
        let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload.clone().into())
        else {
            return;
        };
        match payload {
            EventPayload::RunState(state) => eprintln!(
                "haider-daemond windows-process test={} phase=run-state seq={} state={state:?}",
                self.test_name, envelope.seq,
            ),
            EventPayload::MenuOpened(menu) => eprintln!(
                "haider-daemond windows-process test={} phase=menu-opened seq={} menu_id={} origin={}",
                self.test_name, envelope.seq, menu.id, menu.origin,
            ),
            EventPayload::Item(ItemEvent::Delta {
                delta: ItemDelta::CommandOutput { stream, chunk_b64 },
                ..
            }) => match BASE64.decode(chunk_b64) {
                Ok(bytes) => {
                    let capped_len = bytes.len().min(512);
                    let decoded = String::from_utf8_lossy(&bytes[..capped_len]);
                    eprintln!(
                        "haider-daemond windows-process test={} phase=command-output seq={} stream={stream:?} bytes={} truncated={} decoded={decoded:?}",
                        self.test_name,
                        envelope.seq,
                        bytes.len(),
                        bytes.len() > capped_len,
                    );
                }
                Err(error) => eprintln!(
                    "haider-daemond windows-process test={} phase=command-output seq={} stream={stream:?} decode_error={error}",
                    self.test_name, envelope.seq,
                ),
            },
            _ => {}
        }
    }

    fn failure_diagnostics(
        &self,
        workspace: &std::path::Path,
        fake: &dyn FakeRequestCounter,
        result: &Result<ProcessStartObservation, tokio::time::error::Elapsed>,
        observer: Option<&WindowsExecStartObserver>,
    ) {
        if !self.enabled {
            return;
        }
        match result {
            Err(error) => eprintln!(
                "haider-daemond windows-process test={} phase=start-observation-failed result=Err({error:?})",
                self.test_name,
            ),
            Ok(ProcessStartObservation::Failed { reason }) => eprintln!(
                "haider-daemond windows-process test={} phase=start-observation-failed result=Failed reason={reason:?}",
                self.test_name,
            ),
            Ok(ProcessStartObservation::Terminal) => eprintln!(
                "haider-daemond windows-process test={} phase=start-observation-failed result=Terminal",
                self.test_name,
            ),
            Ok(ProcessStartObservation::Started) => return,
        }
        eprintln!(
            "haider-daemond windows-process test={} phase=workspace-listing entries={:?}",
            self.test_name,
            workspace_listing(workspace),
        );
        eprintln!(
            "haider-daemond windows-process test={} phase=fake-requests count={}",
            self.test_name,
            fake.request_count(),
        );
        if let Some(observer) = observer {
            eprintln!(
                "haider-daemond windows-process test={} phase=observer-state {}",
                self.test_name,
                observer.diagnostic_summary(),
            );
        }
    }
}

#[cfg(windows)]
fn windows_test_process_trace_enabled() -> bool {
    std::env::var("HAIDER_TEST_PROCESS_TRACE").is_ok_and(|value| value == "1")
}

#[cfg(windows)]
fn workspace_listing(workspace: &std::path::Path) -> Vec<String> {
    let entries = match fs::read_dir(workspace) {
        Ok(entries) => entries,
        Err(error) => return vec![format!("<read_dir failed: {error}>")],
    };
    let mut listing = entries
        .map(|entry| match entry {
            Ok(entry) => {
                let name = entry.file_name().to_string_lossy().into_owned();
                match entry.metadata() {
                    Ok(metadata) => format!("{name}:{}", metadata.len()),
                    Err(error) => format!("{name}:<metadata failed: {error}>"),
                }
            }
            Err(error) => format!("<entry failed: {error}>"),
        })
        .collect::<Vec<_>>();
    listing.sort();
    listing
}

#[cfg(windows)]
struct WindowsExecTreeFailureDiagnostics {
    started_at: std::time::Instant,
    phase: &'static str,
    workspace: std::path::PathBuf,
    store_dir: std::path::PathBuf,
    runtime_dir: std::path::PathBuf,
    endpoint_path: std::path::PathBuf,
    live_tree_observations: Vec<(String, String)>,
    process_start_observations: Vec<(String, String)>,
}

#[cfg(windows)]
impl WindowsExecTreeFailureDiagnostics {
    fn new(config: &DaemonConfig, workspace: &std::path::Path) -> Self {
        Self {
            started_at: std::time::Instant::now(),
            phase: "fixture-created",
            workspace: workspace.to_path_buf(),
            store_dir: config.store_dir.clone(),
            runtime_dir: config.runtime_dir.clone(),
            endpoint_path: config.endpoint_path(),
            live_tree_observations: Vec::new(),
            process_start_observations: Vec::new(),
        }
    }

    fn set_phase(&mut self, phase: &'static str) {
        self.phase = phase;
    }

    fn observe_live_tree(&mut self) {
        self.live_tree_observations = windows_exec_process_snapshots(&self.workspace)
            .into_iter()
            .map(|(role, state)| (format!("{role}-before-cancel"), state))
            .collect();
    }

    fn observe_process_start(
        &mut self,
        attempt: &str,
        result: &Result<ProcessStartObservation, tokio::time::error::Elapsed>,
        observer: Option<String>,
    ) {
        self.process_start_observations
            .push((format!("{attempt}-result"), format!("{result:?}")));
        if let Some(observer) = observer {
            self.process_start_observations
                .push((format!("{attempt}-observer"), observer));
        }
    }

    fn snapshot(&self) -> support::BoundarySnapshot {
        let mut snapshot = support::BoundarySnapshot::default()
            .observation("test_phase", self.phase)
            .observation(
                "workspace_listing",
                format!("{:?}", workspace_listing(&self.workspace)),
            );
        for name in [
            "powershell-parent.pid",
            "ps-alive.log",
            "descendant.pid",
            "descendant-started.log",
            "heartbeat.log",
            "descendant-survived.log",
        ] {
            snapshot = snapshot.observation(
                format!("workspace_file[{name}]"),
                windows_failure_file_state(&self.workspace.join(name)),
            );
        }
        for (role, state) in &self.live_tree_observations {
            snapshot = snapshot.process(role, state);
        }
        for (name, state) in &self.process_start_observations {
            snapshot = snapshot.observation(name, state);
        }
        for (role, state) in windows_exec_process_snapshots(&self.workspace) {
            snapshot = snapshot.process(format!("{role}-at-failure"), state);
        }
        snapshot
    }
}

#[cfg(windows)]
impl Drop for WindowsExecTreeFailureDiagnostics {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        let context = support::BoundaryContext {
            store_dir: &self.store_dir,
            runtime_dir: &self.runtime_dir,
            endpoint_path: &self.endpoint_path,
            captured_daemon_stderr: &[],
        };
        support::report_boundary_failure(
            "w4a2_cancelled_exec_child_process_group_dies",
            self.started_at.elapsed(),
            &context,
            self.snapshot(),
        );
    }
}

#[cfg(windows)]
fn windows_failure_file_state(path: &std::path::Path) -> String {
    use std::io::Read as _;

    const CAPTURE_BYTES: u64 = 4 * 1024;
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return "exists=false".into();
        }
        Err(error) => return format!("metadata_error={error}"),
    };
    let mut bytes = Vec::new();
    let read = fs::File::open(path).and_then(|file| {
        file.take(CAPTURE_BYTES).read_to_end(&mut bytes)?;
        Ok(())
    });
    match read {
        Ok(()) => format!(
            "exists=true size={} captured={} truncated={} contents={:?}",
            metadata.len(),
            bytes.len(),
            metadata.len() > CAPTURE_BYTES,
            String::from_utf8_lossy(&bytes),
        ),
        Err(error) => format!("exists=true size={} read_error={error}", metadata.len(),),
    }
}

#[cfg(windows)]
fn windows_exec_process_snapshots(workspace: &std::path::Path) -> Vec<(String, String)> {
    let parent_pid = windows_fixture_pid(&workspace.join("powershell-parent.pid"));
    let descendant_pid = windows_fixture_pid(&workspace.join("descendant.pid"));
    let parent_group = parent_pid
        .as_ref()
        .ok()
        .and_then(|pid| haider_platform::process_group(Some(*pid)));
    let mut snapshots = Vec::new();
    match parent_pid {
        Ok(pid) => snapshots.push((
            "powershell-parent".into(),
            windows_process_state_text(pid, parent_group),
        )),
        Err(state) => snapshots.push(("powershell-parent".into(), state)),
    }
    match descendant_pid {
        Ok(pid) => snapshots.push((
            "cmd-descendant".into(),
            windows_process_state_text(pid, parent_group),
        )),
        Err(state) => snapshots.push(("cmd-descendant".into(), state)),
    }
    snapshots
}

#[cfg(windows)]
fn windows_fixture_pid(path: &std::path::Path) -> Result<u32, String> {
    let value = fs::read_to_string(path).map_err(|error| format!("pid_unavailable={error}"))?;
    value
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|pid| *pid != 0)
        .ok_or_else(|| format!("invalid_pid_contents={value:?}"))
}

#[cfg(windows)]
fn windows_process_state_text(pid: u32, group: Option<haider_platform::ProcessGroup>) -> String {
    let state = match haider_platform::windows_process_state(pid) {
        Ok(Some(state)) => format!(
            "pid={pid} alive={} exit_code={:?} in_any_job={}",
            state.alive, state.exit_code, state.in_any_job,
        ),
        Ok(None) => format!("pid={pid} alive=false exit_code=unavailable in_any_job=unavailable"),
        Err(error) => format!("pid={pid} state_error={error}"),
    };
    let registered_membership = match group {
        Some(group) => match haider_platform::windows_process_in_group(group, pid) {
            Ok(in_group) => in_group.to_string(),
            Err(error) => format!("unavailable({error})"),
        },
        None => "unavailable(parent Job Object is not registered)".into(),
    };
    format!("{state} in_parent_registered_job={registered_membership}")
}

#[cfg(windows)]
fn observed_frame_kind(frame: &WireFrame) -> String {
    match frame {
        WireFrame::Hello(_) => "Hello".into(),
        WireFrame::Welcome(_) => "Welcome".into(),
        WireFrame::Request { .. } => "Request".into(),
        WireFrame::Response { .. } => "Response".into(),
        WireFrame::Event { envelope, .. } => observed_event_kind(envelope),
        WireFrame::AttachCaughtUp { .. } => "AttachCaughtUp".into(),
        WireFrame::SessionRosterDelta { .. } => "SessionRosterDelta".into(),
        WireFrame::AccountsChanged { .. } => "AccountsChanged".into(),
        WireFrame::HaiderCodePlanStatus { .. } => "HaiderCodePlanStatus".into(),
        WireFrame::ResidentSessionBinding { .. } => "ResidentSessionBinding".into(),
        WireFrame::SessionSurfaceDelta { .. } => "SessionSurfaceDelta".into(),
        WireFrame::SessionInputInjected { .. } => "SessionInputInjected".into(),
        WireFrame::MenuAnswer { .. } => "MenuAnswer".into(),
        WireFrame::Lagged { .. } => "Lagged".into(),
        WireFrame::ServerDraining { .. } => "ServerDraining".into(),
        WireFrame::Ping { .. } => "Ping".into(),
        WireFrame::Pong { .. } => "Pong".into(),
        WireFrame::ProtocolError(_) => "ProtocolError".into(),
        WireFrame::SessionDescendantStream { .. } => "SessionDescendantStream".into(),
        WireFrame::SessionDescendantRepairRequired { .. } => {
            "SessionDescendantRepairRequired".into()
        }
        WireFrame::MonitorDelivery { .. } => "MonitorDelivery".into(),
        WireFrame::MonitorDeliveryCaughtUp { .. } => "MonitorDeliveryCaughtUp".into(),
        WireFrame::LoomRegistryDelta { .. } => "LoomRegistryDelta".into(),
        WireFrame::LoomRegistryCaughtUp { .. } => "LoomRegistryCaughtUp".into(),
        WireFrame::Unknown => "Unknown".into(),
        _ => "Other".into(),
    }
}

#[cfg(windows)]
fn observed_event_kind(envelope: &RawEnvelope) -> String {
    let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload.clone().into())
    else {
        return "Event/Unparsed".into();
    };
    match payload {
        EventPayload::RunState(state) => format!("Event/RunState/{state:?}"),
        EventPayload::Item(ItemEvent::Started { item, .. }) => observed_item_kind("Started", &item),
        EventPayload::Item(ItemEvent::Delta { delta, .. }) => match delta {
            ItemDelta::Text { .. } => "Event/Item/Delta/Text".into(),
            ItemDelta::Reasoning { .. } => "Event/Item/Delta/Reasoning".into(),
            ItemDelta::ToolArgs { .. } => "Event/Item/Delta/ToolArgs".into(),
            ItemDelta::CommandOutput { .. } => "Event/Item/Delta/CommandOutput".into(),
        },
        EventPayload::Item(ItemEvent::Completed { item, .. }) => {
            observed_item_kind("Completed", &item)
        }
        _ => format!(
            "Event/{}",
            envelope
                .payload
                .get("type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("UnknownPayload")
        ),
    }
}

#[cfg(windows)]
fn observed_item_kind(event: &str, item: &TurnItem) -> String {
    match item {
        TurnItem::ToolCall { status, .. } => {
            format!("Event/Item/{event}/ToolCall:{status:?}")
        }
        TurnItem::CommandExecution { status, .. } => {
            format!("Event/Item/{event}/CommandExecution:{status:?}")
        }
        _ => format!("Event/Item/{event}"),
    }
}

#[cfg(windows)]
const PROCESS_START_FAILURE_OUTPUT_TAIL: usize = 4 * 1024;

#[cfg(windows)]
struct WindowsExecStartObserver {
    approved_menu: MenuId,
    exec_item: Option<ItemId>,
    exec_call_id: Option<String>,
    tool_result: Option<(String, String)>,
    output_tail: Vec<u8>,
    stdout_tail: Vec<u8>,
    stderr_tail: Vec<u8>,
}

#[cfg(windows)]
impl WindowsExecStartObserver {
    fn new(approved_menu: &MenuId) -> Self {
        Self {
            approved_menu: approved_menu.clone(),
            exec_item: None,
            exec_call_id: None,
            tool_result: None,
            output_tail: Vec::new(),
            stdout_tail: Vec::new(),
            stderr_tail: Vec::new(),
        }
    }

    fn observe(
        &mut self,
        envelope: &RawEnvelope,
        run_id: &RunId,
    ) -> Option<ProcessStartObservation> {
        if envelope.run_id.as_ref() != Some(run_id) {
            return None;
        }
        let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload.clone().into())
        else {
            return None;
        };
        match payload {
            EventPayload::MenuOpened(menu) if menu.id != self.approved_menu => {
                Some(self.failed(format!(
                    "second menu {} opened at seq {} before the exec start signal",
                    menu.id, envelope.seq
                )))
            }
            EventPayload::RunState(RunState::PermissionRequired { menu })
                if menu != self.approved_menu =>
            {
                Some(self.failed(format!(
                    "run re-entered PermissionRequired for second menu {menu} at seq {} before the exec start signal",
                    envelope.seq
                )))
            }
            EventPayload::RunState(state) if state.is_terminal() => {
                Some(ProcessStartObservation::Terminal)
            }
            EventPayload::Item(ItemEvent::Started {
                item_id,
                item:
                    TurnItem::ToolCall {
                        call_id, name, ..
                    },
            }) if name == "exec" => {
                self.exec_item = Some(item_id);
                self.exec_call_id = Some(call_id);
                None
            }
            EventPayload::Item(ItemEvent::Delta {
                item_id,
                delta: ItemDelta::CommandOutput { stream, chunk_b64 },
            }) if self.exec_item.as_ref() == Some(&item_id) => {
                let bytes = match BASE64.decode(chunk_b64) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        return Some(self.failed(format!(
                            "exec command output at seq {} was not valid base64: {error}",
                            envelope.seq
                        )));
                    }
                };
                self.output_tail.extend_from_slice(&bytes);
                match stream {
                    OutputStream::Stdout => self.stdout_tail.extend_from_slice(&bytes),
                    OutputStream::Stderr => self.stderr_tail.extend_from_slice(&bytes),
                }
                let started = self
                    .stdout_tail
                    .windows(b"started".len())
                    .any(|window| window == b"started");
                retain_tail(&mut self.output_tail, PROCESS_START_FAILURE_OUTPUT_TAIL);
                retain_tail(&mut self.stdout_tail, PROCESS_START_FAILURE_OUTPUT_TAIL);
                retain_tail(&mut self.stderr_tail, PROCESS_START_FAILURE_OUTPUT_TAIL);
                started.then_some(ProcessStartObservation::Started)
            }
            EventPayload::ToolResult { call_id, result }
                if self.exec_call_id.as_deref() == Some(call_id.as_str()) =>
            {
                self.tool_result = Some((
                    call_id,
                    format!(
                        "status={:?} reason={:?} preview={}",
                        result.status, result.reason, result.preview
                    ),
                ));
                None
            }
            // `ItemEvent` has one terminal variant; success versus failure is
            // carried by the completed tool item's `ToolStatus`.
            EventPayload::Item(ItemEvent::Completed {
                item_id,
                item:
                    TurnItem::ToolCall {
                        call_id,
                        name,
                        status,
                        ..
                    },
            }) if name == "exec"
                && self
                    .exec_item
                    .as_ref()
                    .is_none_or(|exec_item| exec_item == &item_id) =>
            {
                Some(self.failed(format!(
                    "exec tool-call {call_id} item {item_id} completed before the start signal with status {status:?}"
                )))
            }
            _ => None,
        }
    }

    fn failed(&self, reason: String) -> ProcessStartObservation {
        let tool_result = self
            .tool_result
            .as_ref()
            .map_or("<not observed>", |(_, result)| result.as_str());
        let output = String::from_utf8_lossy(&self.output_tail);
        let stderr = String::from_utf8_lossy(&self.stderr_tail);
        ProcessStartObservation::Failed {
            reason: format!(
                "{reason}; tool_result={tool_result}; decoded_output={output:?}; decoded_stderr={stderr:?}"
            ),
        }
    }

    fn diagnostic_summary(&self) -> String {
        let tool_result = self
            .tool_result
            .as_ref()
            .map_or("<not observed>", |(_, result)| result.as_str());
        format!(
            "exec_item={:?} exec_call_id={:?} tool_result={tool_result} output_bytes={} stdout_bytes={} stderr_bytes={} decoded_output={:?} decoded_stderr={:?}",
            self.exec_item,
            self.exec_call_id,
            self.output_tail.len(),
            self.stdout_tail.len(),
            self.stderr_tail.len(),
            String::from_utf8_lossy(&self.output_tail),
            String::from_utf8_lossy(&self.stderr_tail),
        )
    }
}

#[cfg(windows)]
fn retain_tail(bytes: &mut Vec<u8>, max_len: usize) {
    if bytes.len() > max_len {
        let tail = bytes.split_off(bytes.len() - max_len);
        *bytes = tail;
    }
}

#[cfg(windows)]
fn process_start_observer_event(run_id: &RunId, seq: u64, payload: EventPayload) -> RawEnvelope {
    let mut envelope = recovery_fixture_envelope(
        &SessionId::new("process-start-observer-session"),
        run_id,
        1,
        &format!("process-start-observer-event-{seq}"),
        payload,
        PromptRender::Omit,
    );
    envelope.seq = seq;
    envelope
}

#[cfg(windows)]
fn process_start_observer_menu(id: MenuId) -> Menu {
    Menu {
        id,
        kind: haider_protocol::menu::MenuKind::Permission {
            effect_summary: "run fixture command".into(),
        },
        title: "Allow fixture?".into(),
        body: Vec::new(),
        options: Vec::new(),
        blocking: true,
        scope: haider_protocol::menu::MenuScope::Session,
        origin: "process-start-observer-test".into(),
        ttl_ms: None,
        timeout_option: None,
    }
}

#[cfg(windows)]
#[test]
fn process_start_observer_surfaces_second_permission_and_terminal_exec_text() {
    let run_id = RunId::new("process-start-observer-run");
    let approved_menu = MenuId::new("process-start-observer-approved");
    let second_menu = MenuId::new("process-start-observer-second");
    let mut permissions = WindowsExecStartObserver::new(&approved_menu);
    assert_eq!(
        permissions.observe(
            &process_start_observer_event(
                &run_id,
                1,
                EventPayload::MenuOpened(process_start_observer_menu(approved_menu.clone())),
            ),
            &run_id,
        ),
        None,
        "replay of the approved menu is not a second permission"
    );
    assert!(matches!(
        permissions.observe(
            &process_start_observer_event(
                &run_id,
                2,
                EventPayload::MenuOpened(process_start_observer_menu(second_menu.clone())),
            ),
            &run_id,
        ),
        Some(ProcessStartObservation::Failed { reason })
            if reason.contains("second menu")
    ));
    assert!(matches!(
        permissions.observe(
            &process_start_observer_event(
                &run_id,
                3,
                EventPayload::RunState(RunState::PermissionRequired {
                    menu: second_menu,
                }),
            ),
            &run_id,
        ),
        Some(ProcessStartObservation::Failed { reason })
            if reason.contains("PermissionRequired")
    ));

    let item_id = ItemId::new("process-start-observer-exec-item");
    let call_id = "process-start-observer-exec";
    let mut terminal = WindowsExecStartObserver::new(&approved_menu);
    assert_eq!(
        terminal.observe(
            &process_start_observer_event(
                &run_id,
                4,
                EventPayload::Item(ItemEvent::Started {
                    item_id: item_id.clone(),
                    item: TurnItem::ToolCall {
                        call_id: call_id.into(),
                        name: "exec".into(),
                        args: serde_json::json!({}),
                        status: haider_protocol::item::ToolStatus::InProgress,
                    },
                }),
            ),
            &run_id,
        ),
        None
    );
    assert_eq!(
        terminal.observe(
            &process_start_observer_event(
                &run_id,
                5,
                EventPayload::Item(ItemEvent::Delta {
                    item_id: item_id.clone(),
                    delta: ItemDelta::CommandOutput {
                        stream: OutputStream::Stderr,
                        chunk_b64: BASE64.encode(b"fixture stderr echoed started source"),
                    },
                }),
            ),
            &run_id,
        ),
        None
    );
    assert_eq!(
        terminal.observe(
            &process_start_observer_event(
                &run_id,
                6,
                EventPayload::ToolResult {
                    call_id: call_id.into(),
                    result: haider_protocol::tool::BoundedResult {
                        preview: "fixture tool result".into(),
                        truncated: false,
                        truncation: None,
                        effects: Vec::new(),
                        artifact: None,
                        images: Vec::new(),
                        cursor: None,
                        status: haider_protocol::tool::ToolResultStatus::Failed,
                        reason: Some("fixture failed".into()),
                        presentation: None,
                        data: None,
                    },
                },
            ),
            &run_id,
        ),
        None
    );
    assert!(matches!(
        terminal.observe(
            &process_start_observer_event(
                &run_id,
                7,
                EventPayload::Item(ItemEvent::Completed {
                    item_id,
                    item: TurnItem::ToolCall {
                        call_id: call_id.into(),
                        name: "exec".into(),
                        args: serde_json::json!({}),
                        status: haider_protocol::item::ToolStatus::Failed,
                    },
                }),
            ),
            &run_id,
        ),
        Some(ProcessStartObservation::Failed { reason })
            if reason.contains("fixture tool result")
                && reason.contains("fixture stderr echoed started source")
    ));
}

async fn wait_for_direct_shell_process_tree(
    client: &mut UdsClient,
    config: &DaemonConfig,
    _run_id: &RunId,
    heartbeat: &std::path::Path,
    _workspace: &std::path::Path,
    _fake: &dyn FakeRequestCounter,
) -> Result<ProcessStartObservation, tokio::time::error::Elapsed> {
    #[cfg(not(windows))]
    {
        tokio::time::timeout(process_start_deadline(), async {
            let mut keepalive = tokio::time::interval(support::KEEPALIVE_INTERVAL);
            keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                if fs::metadata(heartbeat).is_ok_and(|metadata| metadata.len() > 1) {
                    return ProcessStartObservation::Started;
                }
                tokio::select! {
                    _ = keepalive.tick() => {
                        client
                            .send(&WireFrame::Ping { nonce: u64::MAX - 1 }, config.frame_limit)
                            .await;
                    }
                    () = tokio::task::yield_now() => {}
                }
            }
        })
        .await
    }
    #[cfg(windows)]
    {
        let mut trace = WindowsProcessStartTrace::new(
            "w8a_shell_exec_cancel_kills_the_process_tree",
            _run_id,
            heartbeat,
        );
        let result = tokio::time::timeout(process_start_deadline(), async {
            let mut poll = tokio::time::interval(std::time::Duration::from_millis(10));
            poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                if trace.heartbeat_started(heartbeat, _workspace) {
                    return ProcessStartObservation::Started;
                }
                tokio::select! {
                    _ = poll.tick() => {}
                    frame = client.try_next_with_keepalive(config.frame_limit) => {
                        let Some(frame) = frame else {
                            client.report_connection_failure(
                                "direct-shell process-start observer reached EOF",
                            );
                            panic!("direct-shell process-start observer reached EOF");
                        };
                        trace.observed_frame(&frame);
                        if let WireFrame::Event { envelope, .. } = frame
                            && run_terminal(&envelope, _run_id)
                        {
                            return ProcessStartObservation::Terminal;
                        }
                    }
                }
            }
        })
        .await;
        trace.failure_diagnostics(_workspace, _fake, &result, None);
        result
    }
}

#[allow(clippy::too_many_arguments)]
async fn wait_for_exec_child_started(
    client: &mut UdsClient,
    config: &DaemonConfig,
    _session_id: &SessionId,
    run_id: &RunId,
    _heartbeat: &std::path::Path,
    _workspace: &std::path::Path,
    _fake: &dyn FakeRequestCounter,
    _approved_menu: &MenuId,
) -> (
    Result<ProcessStartObservation, tokio::time::error::Elapsed>,
    Option<String>,
) {
    #[cfg(windows)]
    let mut trace = WindowsProcessStartTrace::new(
        "w4a2_cancelled_exec_child_process_group_dies",
        run_id,
        _heartbeat,
    );
    #[cfg(windows)]
    let mut observer = WindowsExecStartObserver::new(_approved_menu);
    let result = tokio::time::timeout(process_start_deadline(), async {
        #[cfg(windows)]
        let mut reconnects = 0_u64;
        #[cfg(windows)]
        if let Some(observation) = reconnect_exec_start_observer(
            client,
            config,
            _session_id,
            run_id,
            _workspace,
            _heartbeat,
            "initial-replay",
            &mut reconnects,
            &mut trace,
            &mut observer,
            false,
        )
        .await
        {
            return observation;
        }
        #[cfg(not(windows))]
        let mut output_tail = Vec::new();
        loop {
            #[cfg(windows)]
            if trace.heartbeat_started(_heartbeat, _workspace) {
                return ProcessStartObservation::Started;
            }
            #[cfg(not(windows))]
            let frame = client.next_with_keepalive(config.frame_limit).await;
            #[cfg(windows)]
            let frame = loop {
                if let Some(frame) = client.try_next_with_keepalive(config.frame_limit).await {
                    trace.observed_frame(&frame);
                    break frame;
                }
                // Reattach from the durable beginning so no terminal or start
                // observation written before an exceptional EOF is lost.
                if let Some(observation) = reconnect_exec_start_observer(
                    client,
                    config,
                    _session_id,
                    run_id,
                    _workspace,
                    _heartbeat,
                    "wait",
                    &mut reconnects,
                    &mut trace,
                    &mut observer,
                    true,
                )
                .await
                {
                    return observation;
                }
            };
            if let WireFrame::Event { envelope, .. } = frame {
                #[cfg(windows)]
                if let Some(observation) = observer.observe(&envelope, run_id) {
                    return observation;
                }
                #[cfg(not(windows))]
                if exec_child_started(&envelope, run_id, &mut output_tail) {
                    return ProcessStartObservation::Started;
                }
            }
        }
    })
    .await;
    #[cfg(windows)]
    trace.failure_diagnostics(_workspace, _fake, &result, Some(&observer));
    #[cfg(windows)]
    let observer_diagnostics = Some(observer.diagnostic_summary());
    #[cfg(not(windows))]
    let observer_diagnostics = None;
    (result, observer_diagnostics)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProcessStartObservation {
    Started,
    #[cfg(windows)]
    Terminal,
    #[cfg(windows)]
    Failed {
        reason: String,
    },
}

#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
async fn reconnect_exec_start_observer(
    client: &mut UdsClient,
    config: &DaemonConfig,
    session_id: &SessionId,
    run_id: &RunId,
    workspace: &std::path::Path,
    heartbeat: &std::path::Path,
    phase: &str,
    reconnects: &mut u64,
    trace: &mut WindowsProcessStartTrace,
    observer: &mut WindowsExecStartObserver,
    heartbeat_can_short_circuit: bool,
) -> Option<ProcessStartObservation> {
    loop {
        *reconnects += 1;
        let instance_id = format!("exec-start-observer-{phase}-{run_id}-{reconnects}");
        let Some(mut connected) = UdsClient::try_connect_control_with_keepalive(
            &config.endpoint_path(),
            config.frame_limit,
            "w4a2-test",
            &instance_id,
            ClientKind::Headless,
        )
        .await
        else {
            tokio::task::yield_now().await;
            continue;
        };
        connected.inherit_diagnostics_from(client);
        *client = connected;
        let request_id = format!("exec-start-observer-attach-{phase}-{run_id}-{reconnects}");
        let attached = client
            .try_send(
                &WireFrame::Request {
                    request_id: RequestId::new(request_id),
                    body: RequestBody::SessionAttach {
                        session_id: session_id.clone(),
                        after_seq: 0,
                        mode: AttachMode::Control,
                        sealed_replay: false,
                    },
                },
                config.frame_limit,
            )
            .await;
        if !attached {
            continue;
        }

        let mut response_seen = false;
        let mut caught_up_seen = false;
        loop {
            if heartbeat_can_short_circuit && trace.heartbeat_started(heartbeat, workspace) {
                return Some(ProcessStartObservation::Started);
            }
            let Some(frame) = client.try_next_with_keepalive(config.frame_limit).await else {
                break;
            };
            trace.observed_frame(&frame);
            match frame {
                WireFrame::Response {
                    body: ResponseBody::SessionAttach { .. },
                    ..
                } => response_seen = true,
                WireFrame::AttachCaughtUp { .. } => caught_up_seen = true,
                WireFrame::Event { envelope, .. } => {
                    if let Some(observation) = observer.observe(&envelope, run_id) {
                        return Some(observation);
                    }
                }
                _ => {}
            }
            if response_seen && caught_up_seen {
                return None;
            }
        }
    }
}

#[cfg(not(windows))]
fn exec_child_started(envelope: &RawEnvelope, run_id: &RunId, _output_tail: &mut Vec<u8>) -> bool {
    envelope.run_id.as_ref() == Some(run_id)
        && serde_json::from_value::<EventPayload>(envelope.payload.clone().into()).is_ok_and(
            |payload| {
                let EventPayload::Item(ItemEvent::Delta {
                    delta: ItemDelta::CommandOutput { chunk_b64, .. },
                    ..
                }) = payload
                else {
                    return false;
                };
                BASE64
                    .decode(chunk_b64)
                    .expect("command output base64")
                    .windows(b"started".len())
                    .any(|window| window == b"started")
            },
        )
}

fn run_terminal(envelope: &RawEnvelope, run_id: &RunId) -> bool {
    envelope.run_id.as_ref() == Some(run_id)
        && serde_json::from_value::<EventPayload>(envelope.payload.clone().into()).is_ok_and(
            |payload| matches!(payload, EventPayload::RunState(state) if state.is_terminal()),
        )
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryPreconditionDisposition {
    Cancelled,
    AlreadyTerminal,
}

#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
async fn settle_process_start_attempt_before_retry(
    client: &mut UdsClient,
    config: &DaemonConfig,
    session_id: &SessionId,
    generation: u64,
    run_id: &RunId,
    deadline: tokio::time::Instant,
    request_id: &str,
    command_id: &str,
) -> RetryPreconditionDisposition {
    tokio::time::timeout_at(deadline, async {
        // The write belongs to the same continuous deadline as its reply and
        // terminal observations. In particular, a wedged Windows named-pipe
        // writer must not escape the retry-precondition bound.
        send_request(
            client,
            config,
            request_id,
            RequestBody::TurnCancel {
                command_id: CommandId::new(command_id),
                session_id: session_id.clone(),
                worker_generation: generation,
                run_id: run_id.clone(),
            },
        )
        .await;
        let mut cancellation_accepted = false;
        let mut terminal_seen = false;
        let mut interrupted_idle_seen = false;
        while !(cancellation_accepted && terminal_seen && interrupted_idle_seen) {
            match client.next_with_keepalive(config.frame_limit).await {
                WireFrame::Response {
                    body:
                        ResponseBody::TurnCancel {
                            run_id: cancelled,
                            status,
                            terminal_seq,
                            ..
                        },
                    ..
                } => {
                    assert_eq!(&cancelled, run_id);
                    match status {
                        CancelStatus::Accepted => {
                            assert_eq!(terminal_seq, None);
                            cancellation_accepted = true;
                        }
                        CancelStatus::AlreadyTerminal => {
                            assert!(
                                terminal_seq.is_some_and(|seq| seq > 0),
                                "an already-terminal retry precondition carries its terminal sequence"
                            );
                            // This attempt was not cancelled. Its only valid
                            // use is as a typed reason to submit a fresh run;
                            // no kill/status assertion may target it.
                            return RetryPreconditionDisposition::AlreadyTerminal;
                        }
                        other => panic!(
                            "a timed-out process start may be retried only after active cancellation or authoritative terminal observation, got {other:?}"
                        ),
                    }
                }
                WireFrame::Event { envelope, .. } => {
                    let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload.into())
                    else {
                        continue;
                    };
                    if envelope.run_id.as_ref() == Some(run_id)
                        && payload == EventPayload::RunState(RunState::Cancelled)
                    {
                        terminal_seen = true;
                    }
                    if payload
                        == EventPayload::SessionState(SessionState::Idle { interrupted: true })
                    {
                        interrupted_idle_seen = true;
                    }
                }
                _ => {}
            }
        }
        RetryPreconditionDisposition::Cancelled
    })
    .await
    .expect("timed-out Windows process start settles before retry")
}

#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
async fn cancel_live_windows_attempt(
    client: &mut UdsClient,
    config: &DaemonConfig,
    session_id: &SessionId,
    generation: u64,
    run_id: &RunId,
    request_id: &str,
    command_id: &str,
) -> Vec<(u64, EventPayload)> {
    send_request(
        client,
        config,
        request_id,
        RequestBody::TurnCancel {
            command_id: CommandId::new(command_id),
            session_id: session_id.clone(),
            worker_generation: generation,
            run_id: run_id.clone(),
        },
    )
    .await;
    tokio::time::timeout(support::DEADLINE, async {
        let mut accepted = false;
        let mut cancelled_seq = None;
        let mut interrupted_idle_after_cancel = false;
        let mut events = Vec::new();
        while !(accepted && cancelled_seq.is_some() && interrupted_idle_after_cancel) {
            match client.next_with_keepalive(config.frame_limit).await {
                WireFrame::Response {
                    body:
                        ResponseBody::TurnCancel {
                            run_id: cancelled,
                            status,
                            terminal_seq,
                            ..
                        },
                    ..
                } if &cancelled == run_id => match status {
                    CancelStatus::Accepted => {
                        assert_eq!(terminal_seq, None);
                        accepted = true;
                    }
                    CancelStatus::AlreadyTerminal => panic!(
                        "attempt {run_id} terminalized before the cancellation used by kill/status assertions (terminal_seq={terminal_seq:?})"
                    ),
                    other => panic!(
                        "attempt {run_id} cancellation was not accepted: {other:?}"
                    ),
                },
                WireFrame::Event { envelope, .. } => {
                    let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload.into())
                    else {
                        continue;
                    };
                    if envelope.run_id.as_ref() == Some(run_id) {
                        if payload == EventPayload::RunState(RunState::Cancelled) {
                            cancelled_seq = Some(envelope.seq);
                        }
                        if cancelled_seq.is_none()
                            || payload == EventPayload::RunState(RunState::Cancelled)
                        {
                            events.push((envelope.seq, payload));
                        }
                    } else if payload
                        == EventPayload::SessionState(SessionState::Idle { interrupted: true })
                        && cancelled_seq.is_some_and(|seq| envelope.seq > seq)
                    {
                        interrupted_idle_after_cancel = true;
                    }
                }
                _ => {}
            }
        }
        events
    })
    .await
    .expect("live Windows cancellation reaches Accepted, Cancelled, and later interrupted Idle")
}

#[cfg(windows)]
async fn clean_timed_out_process_start_files(
    workspace: &std::path::Path,
    heartbeat: &std::path::Path,
) {
    let stopped_size = fs::metadata(heartbeat).ok().map(|metadata| metadata.len());
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    match stopped_size {
        Some(stopped_size) => assert_eq!(
            fs::metadata(heartbeat)
                .expect("timed-out start heartbeat metadata")
                .len(),
            stopped_size,
            "timed-out start attempt kept running after cancellation"
        ),
        None => assert!(
            !heartbeat.exists(),
            "timed-out start attempt began running after cancellation"
        ),
    }
    tokio::time::sleep(std::time::Duration::from_millis(1_200)).await;
    assert!(
        !workspace.join("descendant-survived.log").exists(),
        "timed-out start attempt left a surviving descendant"
    );
    for path in [
        heartbeat.to_path_buf(),
        workspace.join("powershell-parent.pid"),
        workspace.join("ps-alive.log"),
        workspace.join("descendant.pid"),
        workspace.join("descendant-started.log"),
    ] {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("clear timed-out start marker before retry: {error}"),
        }
    }
}

async fn read_session(
    client: &mut UdsClient,
    config: &DaemonConfig,
    session_id: SessionId,
    request_id: &str,
) -> Vec<RawEnvelope> {
    send_request(
        client,
        config,
        request_id,
        RequestBody::SessionRead {
            session_id,
            range: SeqRange {
                start_seq: 1,
                end_seq: 1_024,
            },
        },
    )
    .await;
    // On Windows `next` intentionally returns reserved keepalive Pongs so an
    // independently bounded long operation stays connected (ee2b1ce). Own one
    // continuous RPC deadline here; a per-frame deadline would restart after
    // every Pong and could leave a missing SessionRead response unbounded.
    tokio::time::timeout(support::DEADLINE, async {
        loop {
            if let WireFrame::Response {
                body: ResponseBody::SessionRead { result },
                ..
            } = client.next().await
            {
                return result.envelopes;
            }
        }
    })
    .await
    .expect("session read response deadline")
}

async fn current_generation(
    client: &mut UdsClient,
    config: &DaemonConfig,
    session_id: &SessionId,
    request_id: &str,
) -> u64 {
    send_request(
        client,
        config,
        request_id,
        RequestBody::SessionList {
            cursor: None,
            limit: 128,
            order: Default::default(),
        },
    )
    .await;
    loop {
        if let WireFrame::Response {
            body: ResponseBody::SessionList { sessions, .. },
            ..
        } = client.next().await
        {
            return sessions
                .into_iter()
                .find(|session| &session.session_id == session_id)
                .expect("session is listed")
                .worker_generation;
        }
    }
}

fn payloads_for_run<'a>(
    envelopes: &'a [RawEnvelope],
    run_id: &'a RunId,
) -> impl Iterator<Item = EventPayload> + 'a {
    envelopes
        .iter()
        .filter(move |envelope| envelope.run_id.as_ref() == Some(run_id))
        .filter_map(|envelope| serde_json::from_value(envelope.payload.clone().into()).ok())
}

#[cfg(unix)]
fn short_live_test_root(prefix: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in("/tmp")
        .expect("short temporary root")
}

#[cfg(windows)]
fn short_live_test_root(prefix: &str) -> tempfile::TempDir {
    let temporary_base = fs::canonicalize(std::env::temp_dir()).expect("canonical temporary base");
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(temporary_base)
        .expect("canonical native temporary root")
}

#[cfg(unix)]
const EXACT_ONCE_SHELL_COMMAND: &str = "printf 'once\\n' >> shell-count.txt; printf 'héllo\\n'";

#[cfg(unix)]
fn exact_once_shell_command() -> String {
    EXACT_ONCE_SHELL_COMMAND.into()
}

#[cfg(windows)]
fn exact_once_shell_command() -> String {
    windows_powershell_command(concat!(
        "[IO.File]::AppendAllText('shell-count.txt',('once'+[char]10),",
        "[Text.Encoding]::ASCII);",
        "$b=[Text.Encoding]::UTF8.GetBytes(('héllo'+[char]10));",
        "$s=[Console]::OpenStandardOutput();$s.Write($b,0,$b.Length)"
    ))
}

#[cfg(windows)]
fn windows_powershell_command(script: &str) -> String {
    script.into()
}

#[cfg(unix)]
fn denied_exec_command() -> String {
    "printf denied > denied.log".into()
}

#[cfg(windows)]
fn denied_exec_command() -> String {
    "[IO.File]::WriteAllText('denied.log','denied',[Text.Encoding]::ASCII)".into()
}

#[cfg(unix)]
fn approved_exec_command() -> String {
    "printf 'run\\n' >> runs.log; printf stdout-ok; printf stderr-ok >&2".into()
}

#[cfg(windows)]
fn approved_exec_command() -> String {
    windows_powershell_command(concat!(
        "[IO.File]::AppendAllText('runs.log',('run'+[char]10),",
        "[Text.Encoding]::ASCII);",
        "[Console]::Out.Write('stdout-ok');[Console]::Error.Write('stderr-ok')"
    ))
}

#[cfg(unix)]
fn different_exec_command() -> String {
    "printf different > different.log".into()
}

#[cfg(windows)]
fn different_exec_command() -> String {
    "[IO.File]::WriteAllText('different.log','different',[Text.Encoding]::ASCII)".into()
}

#[cfg(unix)]
fn restart_exec_command() -> String {
    "printf 'attempt\\n' >> attempts.log; printf started; sleep 1".into()
}

#[cfg(windows)]
fn restart_exec_command() -> String {
    windows_powershell_command(concat!(
        "[IO.File]::AppendAllText('attempts.log',('attempt'+[char]10),",
        "[Text.Encoding]::ASCII);",
        "[Console]::Out.Write('started');[Console]::Out.Flush();",
        "Start-Sleep -Seconds 1"
    ))
}

#[cfg(unix)]
fn cancellable_exec_command() -> String {
    concat!(
        "(sleep 0.35; printf survived > descendant-survived.log) & ",
        "printf x >> heartbeat.log; printf started; ",
        "while :; do printf x >> heartbeat.log; printf y; sleep 0.01; done"
    )
    .into()
}

#[cfg(windows)]
fn cancellable_exec_command() -> String {
    // Windows PowerShell 5.1 `Start-Process` defaults to shell activation.
    // This fixture needs a direct child so it inherits the daemon-owned Job
    // Object; the ready marker proves that child ran in the workspace. Keep
    // PowerShell's location and .NET's relative-path base aligned: they are
    // distinct process state on Windows and can otherwise name different dirs.
    windows_powershell_command(concat!(
        "[IO.File]::WriteAllText((Join-Path (Get-Location).Path 'powershell-parent.pid'),",
        "$PID.ToString([Globalization.CultureInfo]::InvariantCulture),",
        "[Text.Encoding]::ASCII);",
        "[IO.File]::WriteAllText((Join-Path (Get-Location).Path 'ps-alive.log'),",
        "'x',[Text.Encoding]::ASCII);",
        "$workspace=(Get-Location).Path;Write-Error ('resolved-workspace='+$workspace);",
        "[Environment]::CurrentDirectory=$workspace;",
        "$ready=Join-Path $workspace 'descendant-started.log';",
        "$heartbeat=Join-Path $workspace 'heartbeat.log';",
        "$cmd=Join-Path ([Environment]::SystemDirectory) 'cmd.exe';",
        "$start=[Diagnostics.ProcessStartInfo]::new();$start.FileName=$cmd;",
        "$start.Arguments='/D /S /C \"echo ready>descendant-started.log & ",
        // Do not let the short-lived descendant outrun a starved parent before
        // PowerShell returns from Process.Start and publishes the two-byte
        // heartbeat. That same file releases the descendant, which then retains
        // the original one-second escape oracle used by the post-cancel check.
        "for /L %i in (1,1,55) do @if not exist heartbeat.log ",
        "ping -n 2 127.0.0.1 >nul & ping -n 2 127.0.0.1 >nul & ",
        "echo survived>descendant-survived.log\"';",
        "$start.WorkingDirectory=$workspace;$start.UseShellExecute=$false;",
        "$start.CreateNoWindow=$true;$child=[Diagnostics.Process]::Start($start);",
        "if($null -eq $child){throw 'descendant process did not start'};",
        "[IO.File]::WriteAllText((Join-Path $workspace 'descendant.pid'),",
        "$child.Id.ToString([Globalization.CultureInfo]::InvariantCulture),",
        "[Text.Encoding]::ASCII);",
        "$readyWait=[Diagnostics.Stopwatch]::StartNew();",
        "while(-not [IO.File]::Exists($ready)){",
        "if($child.HasExited){$child.Dispose();",
        "throw 'descendant exited before its ready marker'};",
        // Leave headroom below the production 60-second process wall limit,
        // while tolerating the heavily starved Windows child-spawn observed in
        // the full CI suite. The descendant must still prove readiness before
        // the parent emits `started` or the test sends cancellation.
        "if($readyWait.ElapsedMilliseconds -ge 45000){",
        "try{$child.Kill()}catch{};$child.Dispose();",
        "throw 'descendant did not create its ready marker within 45 seconds'};",
        "Start-Sleep -Milliseconds 10};$readyWait.Stop();",
        "if($child.HasExited){$child.Dispose();",
        "throw 'descendant exited immediately after its ready marker'};",
        "$child.Dispose();",
        "[IO.File]::AppendAllText($heartbeat,'xx',[Text.Encoding]::ASCII);",
        "[Console]::Out.Write('started');[Console]::Out.Flush();",
        "while($true){[IO.File]::AppendAllText($heartbeat,'x',[Text.Encoding]::ASCII);",
        "[Console]::Out.Write('y');[Console]::Out.Flush();Start-Sleep -Milliseconds 10}"
    ))
}

#[cfg(windows)]
fn cancellable_exec_fixture_command(workspace: &std::path::Path) -> String {
    // Keep the real PowerShell -> cmd.exe -> cmd.exe process tree, but move
    // the fixture program out of PowerShell's command-line parser. Hosted
    // Windows runners have taken nearly the entire production process budget
    // before the former large inline script executed its first statement.
    // These batch files exercise the same inherited Job membership while
    // making process readiness depend only on actual child scheduling.
    fs::write(
        workspace.join("cancel-descendant.cmd"),
        concat!(
            "@echo off\r\n",
            ">descendant-started.log <nul set /p \"=ready\"\r\n",
            ":wait_for_heartbeat\r\n",
            "if not exist heartbeat.log (\r\n",
            "  \"%SystemRoot%\\System32\\ping.exe\" -n 2 127.0.0.1 >nul\r\n",
            "  goto wait_for_heartbeat\r\n",
            ")\r\n",
            "\"%SystemRoot%\\System32\\ping.exe\" -n 2 127.0.0.1 >nul\r\n",
            ">descendant-survived.log <nul set /p \"=survived\"\r\n",
        ),
    )
    .expect("write cancellable descendant fixture");
    fs::write(
        workspace.join("cancel-parent.cmd"),
        concat!(
            "@echo off\r\n",
            ">ps-alive.log <nul set /p \"=x\"\r\n",
            "start \"\" /b \"%SystemRoot%\\System32\\cmd.exe\" /d /s /c \"\"cancel-descendant.cmd\"\"\r\n",
            ":wait_for_descendant\r\n",
            "if not exist descendant-started.log (\r\n",
            "  \"%SystemRoot%\\System32\\ping.exe\" -n 2 127.0.0.1 >nul\r\n",
            "  goto wait_for_descendant\r\n",
            ")\r\n",
            ">heartbeat.log <nul set /p \"=xx\"\r\n",
            "<nul set /p \"=started\"\r\n",
            ":heartbeat\r\n",
            ">>heartbeat.log <nul set /p \"=x\"\r\n",
            "<nul set /p \"=y\"\r\n",
            "\"%SystemRoot%\\System32\\ping.exe\" -n 2 127.0.0.1 >nul\r\n",
            "goto heartbeat\r\n",
        ),
    )
    .expect("write cancellable parent fixture");
    windows_powershell_command(concat!(
        "[IO.File]::WriteAllText('powershell-parent.pid',",
        "$PID.ToString([Globalization.CultureInfo]::InvariantCulture),",
        "[Text.Encoding]::ASCII);",
        "& '.\\cancel-parent.cmd'"
    ))
}

#[cfg(windows)]
fn cancellable_exec_attempt_script(command: String) -> Vec<FakeStep> {
    vec![
        // `Finish` seals this run's only tool-call segment. Its sole possible
        // continuation is `Hang`, so an unexpectedly self-terminating exec can
        // never consume another tool-call segment and open a second menu. The
        // retry uses a different FakeProvider, explicitly armed by the test.
        FakeStep::EmitToolCall {
            call_id: "cancel-exec".into(),
            name: "exec".into(),
            args: serde_json::json!({"command": command}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::Hang,
    ]
}

/// Scenario 1: the production runtime is constructed with an injected,
/// deterministic provider factory; no live provider is reachable.
///
/// MUTATION CHECK: make `spawn_with_dependencies` (haider-daemon
/// `runtime.rs`) ignore its `dependencies` argument and construct
/// `DaemonDependencies::default()`. Expected failure: this boot still
/// passes (the pinned law here is only that injection is accepted), but
/// every turn scenario in this file fails — scenario 3 first, with a
/// `credential_missing` RunFailed instead of a streamed turn — which is why
/// the scenario-13 manifest lists scenario 3 as this seam's observer.
#[tokio::test]
async fn scenario_1_production_runtime_accepts_an_injected_fake_provider_factory() {
    let root = test_root("w3c-live-");
    let config = DaemonConfig::new(
        "injected-factory",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (dependencies, fake) = fake_dependencies(vec![FakeStep::Finish {
        reason: FinishReason::EndTurn,
    }]);
    let task = ready_with_dependencies(&config, dependencies).await;
    assert!(fake.requests().is_empty());
    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// MUTATION CHECK: defer vision support checks until `stream_turn`, silently
/// drop the image, or downgrade the error to provider_error. Expected RUNTIME
/// failure: the fake records a request, or the durable failure is not the
/// typed local `vision_unsupported` refusal naming the selected provider.
#[tokio::test]
async fn vision_unsupported_provider_refuses_locally_with_typed_error() {
    let root = short_live_test_root("b4av-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "b4a-vision-refusal",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (dependencies, fake) = fake_dependencies(Vec::new());
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "b4a-vision-client",
        "b4a-vision-instance",
        ClientKind::Cli,
    )
    .await;
    let artifact = upload_artifact(
        &mut client,
        &config,
        "put-vision-image",
        &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a],
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut client, &config, &workspace).await;
    send_request(
        &mut client,
        &config,
        "submit-image",
        RequestBody::TurnSubmit {
            command_id: CommandId::new("submit-image-command"),
            session_id,
            worker_generation: generation,
            text: "describe this image".into(),
            attachments: vec![AttachmentBlock::Image {
                artifact,
                mime: "image/png".into(),
                width: None,
                height: None,
            }],
            mode: DeliveryMode::Queue,
        },
    )
    .await;
    let (run_id, _) = next_submit_response(&mut client).await;
    let events = events_until_terminal(&mut client, &run_id).await;
    assert!(events.iter().any(|(_, payload)| {
        matches!(
            payload,
            EventPayload::RunFailed {
                code: ErrorCode::VisionUnsupported,
                message,
                retryable: false,
                ..
            } if message.contains("provider `fake`")
        )
    }));
    assert!(
        fake.requests().is_empty(),
        "unsupported vision must be refused before provider spend"
    );

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// MUTATION CHECK: resolve image bytes into a summarization request, or let
/// compaction erase the original attachment-bearing user fact. Expected
/// RUNTIME failure: request three contains an image/base64 payload, or the
/// durable post-compaction journal no longer carries the original CAS ref.
#[tokio::test]
async fn compaction_summary_request_carries_no_image_attachments() {
    let root = short_live_test_root("b4ac-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "b4a-compaction-images",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let fake = Arc::new(
        FakeProvider::new(vec![
            FakeStep::EmitText {
                text: "older answer".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
            FakeStep::EmitText {
                text: "image answer".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
            FakeStep::EmitText {
                text: "summary without image bytes".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ])
        .with_vision_native(),
    );
    let dependencies = DaemonDependencies {
        provider_factory: ProviderFactoryConfig::Injected {
            factory: Arc::new(FakeFactory { fake: fake.clone() }),
            providers: std::collections::BTreeSet::from(["fake".to_owned()]),
        },
        ..DaemonDependencies::default()
    };
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "b4a-compaction-client",
        "b4a-compaction-instance",
        ClientKind::Cli,
    )
    .await;
    let artifact = upload_artifact(
        &mut client,
        &config,
        "put-compaction-image",
        &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a],
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut client, &config, &workspace).await;
    send_request(
        &mut client,
        &config,
        "submit-older-clean-turn",
        RequestBody::TurnSubmit {
            command_id: CommandId::new("submit-older-clean-turn-command"),
            session_id: session_id.clone(),
            worker_generation: generation,
            text: "older clean fact".into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Queue,
        },
    )
    .await;
    let (run_id, _) = next_submit_response(&mut client).await;
    let _ = events_until_terminal(&mut client, &run_id).await;
    let _ = next_idle(&mut client).await;

    let image = AttachmentBlock::Image {
        artifact: artifact.clone(),
        mime: "image/png".into(),
        width: None,
        height: None,
    };
    send_request(
        &mut client,
        &config,
        "submit-before-compaction",
        RequestBody::TurnSubmit {
            command_id: CommandId::new("submit-before-compaction-command"),
            session_id: session_id.clone(),
            worker_generation: generation,
            text: "remember this image".into(),
            attachments: vec![image.clone()],
            mode: DeliveryMode::Queue,
        },
    )
    .await;
    let (run_id, _) = next_submit_response(&mut client).await;
    let _ = events_until_terminal(&mut client, &run_id).await;
    let _ = next_idle(&mut client).await;

    send_request(
        &mut client,
        &config,
        "compact-image-history",
        RequestBody::SessionCompact {
            command_id: CommandId::new("compact-image-history-command"),
            session_id: session_id.clone(),
            worker_generation: generation,
        },
    )
    .await;
    loop {
        if matches!(
            client.next().await,
            WireFrame::Response {
                body: ResponseBody::SessionCompact { .. },
                ..
            }
        ) {
            break;
        }
    }

    let requests = fake.requests();
    assert_eq!(requests.len(), 3);
    let summary_request = &requests[2];
    assert!(summary_request.attachments.is_empty());
    assert!(summary_request.messages.iter().all(|message| {
        message
            .blocks
            .iter()
            .all(|block| !matches!(block, Block::Attachment(AttachmentBlock::Image { .. })))
    }));

    let journal = read_session(
        &mut client,
        &config,
        session_id,
        "read-after-image-compaction",
    )
    .await;
    assert!(journal.iter().any(|envelope| {
        serde_json::from_value::<EventPayload>(envelope.payload.clone().into()).is_ok_and(
            |payload| {
                matches!(
                    payload,
                    EventPayload::UserMessage { attachments, .. }
                        if attachments == vec![image.clone()]
                )
            },
        )
    }));

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// LAW (LA1, G2 end-to-end text attach): a `turn.submit` carrying a File
/// attachment journals the File block in `UserMessage` (CAS ref, never
/// bytes), `resolve_prompt_attachments` REPLACES it in place with a
/// `Block::Text` wearing the `<file name=… lines=…>` header, and the
/// provider request carries the file text with ZERO attachment blocks and
/// zero resolved image payloads. The compaction summarization request gets
/// the SAME inlined text (compaction parity).
///
/// MUTATION CHECK: drop the File arm from `resolve_prompt_attachments` (the
/// block reaches the adapter and the run fails), or drop the header from
/// `file_attachment_text`. Expected RUNTIME failure: the provider request
/// assertions below.
#[tokio::test]
async fn file_attachment_is_inlined_with_header_and_never_reaches_the_provider() {
    let root = short_live_test_root("g2af-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "g2a-file-inline",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (dependencies, fake) = fake_dependencies(vec![
        FakeStep::EmitText {
            text: "file answer".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::EmitText {
            text: "summary without file blocks".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "g2a-file-client",
        "g2a-file-instance",
        ClientKind::Cli,
    )
    .await;
    let artifact = upload_artifact(
        &mut client,
        &config,
        "put-file-text",
        b"alpha line\nbeta line\ngamma line",
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut client, &config, &workspace).await;
    let file = AttachmentBlock::File {
        artifact: artifact.clone(),
        name: "notes.md".into(),
        lines: 3,
    };
    send_request(
        &mut client,
        &config,
        "submit-file",
        RequestBody::TurnSubmit {
            command_id: CommandId::new("submit-file-command"),
            session_id: session_id.clone(),
            worker_generation: generation,
            text: "summarize the attached notes".into(),
            attachments: vec![file.clone()],
            mode: DeliveryMode::Queue,
        },
    )
    .await;
    let (run_id, _) = next_submit_response(&mut client).await;
    let _ = events_until_terminal(&mut client, &run_id).await;
    let _ = next_idle(&mut client).await;

    let expected_inline =
        "<file name=\"notes.md\" lines=\"3\">\nalpha line\nbeta line\ngamma line\n</file>";
    let requests = fake.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert!(
        request.attachments.is_empty(),
        "no resolved base64 payloads ride a text-file turn"
    );
    assert!(
        request.messages.iter().all(|message| {
            message
                .blocks
                .iter()
                .all(|block| !matches!(block, Block::Attachment(_)))
        }),
        "the provider must never see an attachment block"
    );
    assert!(
        request.messages.iter().any(|message| {
            message
                .blocks
                .iter()
                .any(|block| matches!(block, Block::Text { text } if text == expected_inline))
        }),
        "the file text is inlined with its header"
    );

    // Compaction parity: the summarization request sees the same inline.
    send_request(
        &mut client,
        &config,
        "compact-file-history",
        RequestBody::SessionCompact {
            command_id: CommandId::new("compact-file-history-command"),
            session_id: session_id.clone(),
            worker_generation: generation,
        },
    )
    .await;
    loop {
        if matches!(
            client.next().await,
            WireFrame::Response {
                body: ResponseBody::SessionCompact { .. },
                ..
            }
        ) {
            break;
        }
    }
    let requests = fake.requests();
    assert_eq!(requests.len(), 2);
    let summary_request = &requests[1];
    assert!(summary_request.attachments.is_empty());
    assert!(summary_request.messages.iter().all(|message| {
        message
            .blocks
            .iter()
            .all(|block| !matches!(block, Block::Attachment(_)))
    }));
    assert!(
        summary_request.messages.iter().any(|message| {
            message.blocks.iter().any(
                |block| matches!(block, Block::Text { text } if text.contains(expected_inline)),
            )
        }),
        "compaction inlines the same `<file>` text"
    );

    // The DURABLE journal keeps the File block by ref — inlining is a
    // prompt-compile concern, never a journal rewrite.
    let journal = read_session(&mut client, &config, session_id, "read-after-file-turn").await;
    assert!(journal.iter().any(|envelope| {
        serde_json::from_value::<EventPayload>(envelope.payload.clone().into()).is_ok_and(
            |payload| {
                matches!(
                    payload,
                    EventPayload::UserMessage { attachments, .. }
                        if attachments == vec![file.clone()]
                )
            },
        )
    }));

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

#[tokio::test]
async fn injected_fake_turn_runs_through_the_openai_provider_name() {
    let root = test_root("w5a-openai-live-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "openai-fake-turn",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (dependencies, fake) = fake_dependencies_for_provider(
        vec![
            FakeStep::EmitText {
                text: "openai-shaped route".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ],
        haider_provider::OPENAI_PROVIDER_NAME,
    );
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w5a-openai-live-test",
        "openai-turn-client",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach_for_provider(
        &mut client,
        &config,
        &workspace,
        haider_provider::OPENAI_PROVIDER_NAME,
        "gpt-5-test",
    )
    .await;
    send_request(
        &mut client,
        &config,
        "submit-openai",
        submit_body(
            "submit-openai-command",
            session_id,
            generation,
            "route through openai",
        ),
    )
    .await;
    let (run_id, _) = next_submit_response(&mut client).await;
    let events = events_until_terminal(&mut client, &run_id).await;

    assert!(
        events
            .iter()
            .any(|(_, payload)| { *payload == EventPayload::RunState(RunState::Done) })
    );
    assert_eq!(fake.requests().len(), 1);
    assert_eq!(fake.requests()[0].model, "gpt-5-test");

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// Scenario 3.
///
/// MUTATION CHECK (three seams, one per revert):
/// - move `worker_manager()?.submit(..)` before `hub.accept_turn(..)` in
///   `turn_submit` (session_hub/rpc.rs) — expected failure: the provider
///   request races the durable prefix and the Queued-before-UserMessage-
///   before-Thinking position assertions below fail;
/// - hand the worker the raw store instead of its lease-fenced
///   `HubStoreHandle` in `start_turn` (worker.rs) — expected failure: worker
///   envelopes bypass the actor and the contiguous-sequence window check
///   fails on interleaved publication;
/// - publish before append in the actor's `WorkerAppend` arm (actor.rs) —
///   expected failure: a delivered event precedes its durable seq and the
///   contiguity/durable-read checks disagree.
#[tokio::test]
async fn scenario_3_submit_streams_one_contiguous_durable_turn_over_real_uds() {
    let root = test_root("w3c-live-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "live-turn",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let usage = Usage {
        input: 11,
        output: 7,
        reasoning: 0,
        cached: 3,
        source: UsageSource::ProviderReported,
        account: None,
        accounts: Vec::new(),
        normalized: None,
        scope: None,
        cache_cost: None,
        request: None,
    };
    let fake = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitText {
            text: "hello".into(),
        },
        FakeStep::EmitUsage {
            usage: usage.clone(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let inspections = Arc::new(AtomicUsize::new(0));
    let dependencies = DaemonDependencies {
        provider_factory: ProviderFactoryConfig::injected(Arc::new(DurableEntryFactory {
            fake: fake.clone(),
            database_path: config.store_dir.join("store.sqlite"),
            inspections: inspections.clone(),
        })),
        ..DaemonDependencies::default()
    };
    let task = ready_with_dependencies(&config, dependencies.clone()).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "turn-client",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut client, &config, &workspace).await;
    send_request(
        &mut client,
        &config,
        "submit",
        submit_body(
            "submit-command",
            session_id.clone(),
            generation,
            "say hello",
        ),
    )
    .await;

    let mut events = Vec::new();
    let mut accepted = None;
    loop {
        match client.next().await {
            WireFrame::Response {
                body:
                    ResponseBody::TurnSubmit {
                        run_id,
                        accepted_seq,
                        ..
                    },
                ..
            } => accepted = Some((run_id, accepted_seq)),
            WireFrame::Event { envelope, .. } => {
                let seq = envelope.seq;
                // G2: the first accept interleaves ONE additive
                // session-config fact (the auto-title `session_renamed`),
                // which is NOT core-EventPayload vocabulary by design.
                // Everything else must still decode strictly typed.
                let payload = match serde_json::from_value::<EventPayload>(
                    envelope.payload.clone().into(),
                ) {
                    Ok(payload) => Some(payload),
                    Err(_) => {
                        assert!(
                                haider_protocol::session::SessionConfigEventPayload::session_renamed_from_value(
                                    &envelope.payload
                                )
                                .is_some(),
                                "only the additive session-config fact may be non-core: {:?}",
                                envelope.payload
                            );
                        None
                    }
                };
                let terminal = payload == Some(EventPayload::RunState(RunState::Done));
                events.push((seq, payload));
                if terminal {
                    break;
                }
            }
            _ => {}
        }
    }
    let (run_id, accepted_seq) = accepted.expect("correlated submit response");
    assert_eq!(accepted_seq, 3);
    assert_eq!(fake.requests().len(), 1);
    assert_eq!(
        inspections.load(Ordering::SeqCst),
        1,
        "provider entry inspected the already-durable acceptance prefix"
    );
    assert_eq!(
        fake.requests()[0]
            .system_prompt
            .as_deref()
            .map(|prompt| { prompt.starts_with(haider_daemon::SystemPromptBuilder::VERSION) }),
        Some(true)
    );
    for pair in events.windows(2) {
        assert_eq!(
            pair[1].0,
            pair[0].0 + 1,
            "event sequence must be contiguous"
        );
    }
    let payloads = events
        .iter()
        .filter_map(|(_, payload)| payload.as_ref())
        .collect::<Vec<_>>();
    // Position, not just presence: the acceptance transaction commits Queued
    // then UserMessage, and only then may the worker commit Thinking and
    // stream (R3's durable-before-provider order).
    let position = |predicate: &dyn Fn(&EventPayload) -> bool| {
        payloads
            .iter()
            .position(|payload| predicate(payload))
            .expect("expected payload present")
    };
    let queued = position(&|payload| *payload == EventPayload::RunState(RunState::Queued));
    let user = position(
        &|payload| matches!(payload, EventPayload::UserMessage { text, .. } if text == "say hello"),
    );
    let thinking = position(&|payload| *payload == EventPayload::RunState(RunState::Thinking));
    let streaming = position(&|payload| *payload == EventPayload::RunState(RunState::Streaming));
    assert!(queued < user, "Queued must precede UserMessage");
    assert!(user < thinking, "UserMessage must precede Thinking");
    assert!(thinking < streaming, "Thinking must precede Streaming");
    assert!(payloads.iter().any(|payload| matches!(
        payload,
        EventPayload::Item(haider_protocol::item::ItemEvent::Started { .. })
    )));
    assert!(payloads.iter().any(|payload| matches!(
        payload,
        EventPayload::Item(haider_protocol::item::ItemEvent::Delta { .. })
    )));
    assert!(payloads.iter().any(|payload| matches!(
        payload,
        EventPayload::Item(haider_protocol::item::ItemEvent::Completed { .. })
    )));
    // CM1 enriches the journaled Usage with a measurement `scope` and
    // request-local cache diagnostic the fake provider does not emit. This
    // test pins billing durability and request identity, so normalize only
    // those daemon-owned measurements.
    let mut expected_usage = usage.clone();
    expected_usage.request = Some(expected_request_usage(1, &usage));
    assert!(payloads.iter().any(|payload| matches!(
        payload,
        EventPayload::Usage(actual) if {
            let mut actual = actual.clone();
            actual.scope = None;
            if let Some(request) = &mut actual.request {
                request.cache = None;
            }
            actual == expected_usage
        }
    )));
    assert!(payloads.contains(&&EventPayload::RunState(RunState::Done)));
    assert!(!run_id.as_str().is_empty());
    assert!(
        !next_idle(&mut client).await,
        "natural completion settles non-interrupted Idle"
    );
    let durable = read_session(&mut client, &config, session_id, "full-turn-read").await;
    assert!(durable.iter().enumerate().all(|(index, envelope)| {
        envelope.seq == u64::try_from(index).expect("test index") + 1
    }));
    assert_eq!(
        serde_json::from_value::<EventPayload>(durable[0].payload.clone().into())
            .expect("created payload"),
        EventPayload::SessionState(SessionState::Created)
    );

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// A stored root is a capability, not a prerequisite for conversation. The
/// daemon rechecks it at attach/turn boundaries without canonicalizing, emits
/// one typed fact, and still completes the provider turn. The same test pins
/// receipt-first re-root replay after the newly selected directory vanishes.
#[tokio::test]
async fn vanished_workspace_degrades_plain_turn_and_workspace_set_replays() {
    let root = test_root("wsroot-vanished-");
    let workspace = root.path().join("workspace");
    let recovery = root.path().join("recovery");
    let other = root.path().join("other");
    fs::create_dir(&workspace).expect("workspace");
    fs::create_dir(&recovery).expect("recovery workspace");
    fs::create_dir(&other).expect("alternate workspace");
    let config = DaemonConfig::new(
        "workspace-vanished",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (dependencies, fake) = fake_dependencies(vec![
        FakeStep::EmitText {
            text: "first".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::EmitText {
            text: "still here".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::Delay { ms: 250 },
        FakeStep::EmitText {
            text: "recovered".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "workspace-test",
        "workspace-client",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut client, &config, &workspace).await;

    send_request(
        &mut client,
        &config,
        "first-submit",
        submit_body(
            "first-command",
            session_id.clone(),
            generation,
            "first turn",
        ),
    )
    .await;
    let (first_run, _) = next_submit_response(&mut client).await;
    let first = events_until_terminal(&mut client, &first_run).await;
    assert!(matches!(
        first.last(),
        Some((_, EventPayload::RunState(RunState::Done)))
    ));
    let first_head = first
        .last()
        .map(|(seq, _)| *seq)
        .expect("terminal event has a sequence");

    fs::remove_dir(&workspace).expect("delete stored workspace between turns");
    let mut attach_client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "workspace-attach-test",
        "workspace-attach-client",
        ClientKind::Headless,
    )
    .await;
    send_request(
        &mut attach_client,
        &config,
        "attach-after-delete",
        RequestBody::SessionAttach {
            session_id: session_id.clone(),
            after_seq: first_head,
            mode: AttachMode::Control,
            sealed_replay: false,
        },
    )
    .await;
    assert!(matches!(
        next_response(&mut attach_client).await,
        WireFrame::Response {
            body: ResponseBody::SessionAttach { .. },
            ..
        }
    ));
    drop(attach_client);
    send_request(
        &mut client,
        &config,
        "second-submit",
        submit_body(
            "second-command",
            session_id.clone(),
            generation,
            "plain chat still works",
        ),
    )
    .await;
    let (second_run, _) = next_submit_response(&mut client).await;
    let mut notices = Vec::new();
    let mut core = Vec::new();
    loop {
        let WireFrame::Event { envelope, .. } = client.next().await else {
            continue;
        };
        if envelope.run_id.as_ref() != Some(&second_run) {
            continue;
        }
        if let Some(workspace) = WorkspaceEventPayload::from_payload_value(&envelope.payload) {
            notices.push((envelope.clone(), workspace));
            continue;
        }
        assert_ne!(
            envelope
                .payload
                .get("type")
                .and_then(serde_json::Value::as_str),
            Some("project_instructions_loaded"),
            "unavailable workspace must not journal an empty instruction transition"
        );
        let payload = serde_json::from_value::<EventPayload>(envelope.payload.into())
            .expect("non-workspace turn event remains core typed");
        let done = payload == EventPayload::RunState(RunState::Done);
        core.push(payload);
        if done {
            break;
        }
    }
    assert_eq!(notices.len(), 1, "exactly one workspace notice per turn");
    let (notice_envelope, WorkspaceEventPayload::WorkspaceUnavailable(unavailable)) = &notices[0]
    else {
        panic!("expected workspace_unavailable fact")
    };
    assert_eq!(unavailable.path, workspace.to_string_lossy());
    assert_eq!(unavailable.reason.as_str(), "missing");
    assert!(notice_envelope.render.ui && notice_envelope.render.durable);
    assert_eq!(notice_envelope.render.prompt, PromptRender::Omit);
    assert!(core.iter().all(|payload| !matches!(
        payload,
        EventPayload::RunFailed {
            code: ErrorCode::ProviderError,
            ..
        }
    )));
    assert!(
        core.iter()
            .all(|payload| !matches!(payload, EventPayload::Effect(_))),
        "a degraded plain turn must not create workspace effect receipts"
    );
    assert!(matches!(
        core.last(),
        Some(EventPayload::RunState(RunState::Done))
    ));
    assert_eq!(fake.requests().len(), 2, "degraded turn reaches provider");

    let set = RequestBody::SessionWorkspaceSet {
        command_id: CommandId::new("workspace-set-command"),
        session_id: session_id.clone(),
        worker_generation: generation,
        path: recovery.to_string_lossy().into_owned(),
    };
    send_request(&mut client, &config, "workspace-set", set.clone()).await;
    let first_receipt = next_response(&mut client).await;
    assert!(matches!(
        first_receipt,
        WireFrame::Response {
            body: ResponseBody::SessionWorkspaceSet { ref path, .. },
            ..
        } if path == &recovery.to_string_lossy()
    ));
    let selected_seq = match &first_receipt {
        WireFrame::Response {
            body: ResponseBody::SessionWorkspaceSet { selected_seq, .. },
            ..
        } => *selected_seq,
        _ => unreachable!("workspace receipt shape asserted above"),
    };
    let mut mutation_client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "workspace-mutation-test",
        "workspace-mutation-client",
        ClientKind::Headless,
    )
    .await;
    send_request(
        &mut mutation_client,
        &config,
        "attach-for-busy-set",
        RequestBody::SessionAttach {
            session_id: session_id.clone(),
            after_seq: selected_seq,
            mode: AttachMode::Control,
            sealed_replay: false,
        },
    )
    .await;
    assert!(matches!(
        next_response(&mut mutation_client).await,
        WireFrame::Response {
            body: ResponseBody::SessionAttach { .. },
            ..
        }
    ));

    send_request(
        &mut client,
        &config,
        "recovered-submit",
        submit_body(
            "recovered-command",
            session_id.clone(),
            generation,
            "workspace is back",
        ),
    )
    .await;
    let (recovered_run, _) = next_submit_response(&mut client).await;
    send_request(
        &mut mutation_client,
        &config,
        "workspace-set-while-active",
        RequestBody::SessionWorkspaceSet {
            command_id: CommandId::new("workspace-set-active-command"),
            session_id: session_id.clone(),
            worker_generation: generation,
            path: other.to_string_lossy().into_owned(),
        },
    )
    .await;
    assert!(matches!(
        next_response(&mut mutation_client).await,
        WireFrame::Response {
            body: ResponseBody::Error { ref code, .. },
            ..
        } if code == ERROR_CODE_BUSY
    ));
    drop(mutation_client);
    let recovered_events = events_until_terminal(&mut client, &recovered_run).await;
    assert!(matches!(
        recovered_events.last(),
        Some((_, EventPayload::RunState(RunState::Done)))
    ));
    assert_eq!(
        fake.requests().len(),
        3,
        "a turn after re-root uses the selected workspace"
    );

    fs::remove_dir(&recovery).expect("remove selected root after commit");
    send_request(&mut client, &config, "workspace-replay", set).await;
    let replayed = next_response(&mut client).await;
    let response_body = |frame: WireFrame| match frame {
        WireFrame::Response { body, .. } => body,
        other => panic!("expected response, got {other:?}"),
    };
    assert_eq!(response_body(first_receipt), response_body(replayed));

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// Scenario 4.
///
/// MUTATION CHECK: skip the `turn_accept_receipt` preflight in `turn_submit`
/// (session_hub/rpc.rs), or remove `admit_pending`'s active-run compare and
/// in-queue run-id scan (worker.rs). Expected failure: the same-command
/// retry takes the provider slot reserved for the positive fence turn or
/// commits a second user message.
#[tokio::test]
async fn scenario_4_lost_submit_response_replays_one_run_and_one_provider_request() {
    let root = test_root("w3c-live-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "submit-idempotency",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (dependencies, fake) = fake_dependencies(vec![
        FakeStep::EmitText {
            text: "only once".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::EmitText {
            text: "fence".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let task = ready_with_dependencies(&config, dependencies.clone()).await;
    let mut first = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "lost-submit",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut first, &config, &workspace).await;
    send_request(
        &mut first,
        &config,
        "lost",
        submit_body(
            "same-submit-command",
            session_id.clone(),
            generation,
            "one turn",
        ),
    )
    .await;
    tokio::time::timeout(support::DEADLINE, async {
        while fake.requests().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("provider request begins");
    drop(first);

    let mut retry = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "retry-submit",
        ClientKind::Headless,
    )
    .await;
    attach_existing(&mut retry, &config, session_id.clone(), 0, "retry-attach").await;
    let session_for_read = session_id.clone();
    send_request(
        &mut retry,
        &config,
        "retry",
        submit_body(
            "same-submit-command",
            session_id.clone(),
            generation,
            "one turn",
        ),
    )
    .await;
    let (run_id, _) = next_submit_response(&mut retry).await;

    let events = tokio::time::timeout(support::DEADLINE, async {
        'until_terminal: loop {
            send_request(
                &mut retry,
                &config,
                "read-until-terminal",
                RequestBody::SessionRead {
                    session_id: session_for_read.clone(),
                    range: SeqRange {
                        start_seq: 1,
                        end_seq: 64,
                    },
                },
            )
            .await;
            loop {
                if let WireFrame::Response {
                    body: ResponseBody::SessionRead { result },
                    ..
                } = retry.next().await
                {
                    if result.envelopes.iter().any(|envelope| {
                        envelope.run_id.as_ref() == Some(&run_id)
                            && serde_json::from_value::<EventPayload>(envelope.payload.clone().into())
                                .is_ok_and(|payload| {
                                    matches!(payload, EventPayload::RunState(ref state) if state.is_terminal())
                                })
                    }) {
                        break 'until_terminal result.envelopes;
                    }
                    break;
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("original run becomes durably terminal");
    assert_eq!(
        events
            .iter()
            .filter(|envelope| {
                envelope.run_id.as_ref() == Some(&run_id)
                    && serde_json::from_value::<EventPayload>(envelope.payload.clone().into())
                        .is_ok_and(|payload| matches!(payload, EventPayload::UserMessage { .. }))
            })
            .count(),
        1
    );

    // Positive quiescence fence: this distinct turn entered the manager
    // after the replay hint. Supervisor FIFO plus serial execution means the
    // second provider request must be this turn; a duplicate queued from the
    // replay would necessarily take that slot first.
    send_request(
        &mut retry,
        &config,
        "fence",
        submit_body("fence-command", session_id, generation, "fence turn"),
    )
    .await;
    let _ = next_submit_response(&mut retry).await;
    tokio::time::timeout(support::DEADLINE, async {
        while fake.requests().len() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("fence provider request begins");
    let requests = fake.requests();
    assert!(requests[1].messages.iter().any(|message| {
        message.blocks.iter().any(
            |block| matches!(block, haider_protocol::provider::Block::Text { text } if text == "fence turn"),
        )
    }));

    // This scenario tests receipt replay across a graceful restart, not the
    // forced-drain contract. Release the no-longer-used client so shutdown can
    // close the store and its profile lease before the successor starts.
    drop(retry);
    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
    assert_eq!(fake.requests().len(), 2);

    let restarted = ready_with_dependencies(&config, dependencies).await;
    let mut after_restart = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "retry-submit-after-restart",
        ClientKind::Headless,
    )
    .await;
    attach_existing(
        &mut after_restart,
        &config,
        session_for_read.clone(),
        0,
        "retry-restart-attach",
    )
    .await;
    send_request(
        &mut after_restart,
        &config,
        "retry-after-restart",
        submit_body(
            "same-submit-command",
            session_for_read,
            generation,
            "one turn",
        ),
    )
    .await;
    let (replayed_run, _) = next_submit_response(&mut after_restart).await;
    assert_eq!(replayed_run, run_id);
    restarted.shutdown_handle().request("test complete");
    restarted.join().await.expect("restarted daemon joins");
    assert_eq!(
        fake.requests().len(),
        2,
        "old-generation receipt replay is response-only"
    );
}

/// Scenario 5.
///
/// MUTATION CHECK: make `PromptHistoryCompiler::compile`
/// (haider-core/src/prompt_history.rs) return only the current user message,
/// or drop its Done-runs-only terminal filter. Expected failure: request two
/// lacks the completed first exchange, or includes non-terminal content.
/// This live scenario runs one head-turn identity; the negative
/// branch/agent and nonterminal exclusions are pinned separately in the
/// `haider-core` MemoryStore prompt-history test.
#[tokio::test]
async fn scenario_5_second_turn_contains_prior_completed_conversation() {
    let root = test_root("w3c-live-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "conversation",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (dependencies, fake) = fake_dependencies(vec![
        FakeStep::EmitText {
            text: "first answer".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::EmitText {
            text: "second answer".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "history-client",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut client, &config, &workspace).await;
    send_request(
        &mut client,
        &config,
        "first",
        submit_body(
            "first-command",
            session_id.clone(),
            generation,
            "first question",
        ),
    )
    .await;
    let (first_run, _) = next_submit_response(&mut client).await;
    let _ = events_until_terminal(&mut client, &first_run).await;

    send_request(
        &mut client,
        &config,
        "second",
        submit_body("second-command", session_id, generation, "second question"),
    )
    .await;
    let (second_run, _) = next_submit_response(&mut client).await;
    let _ = events_until_terminal(&mut client, &second_run).await;
    let requests = fake.requests();
    assert_eq!(requests.len(), 2);
    let second = &requests[1].messages;
    assert!(second.iter().any(|message| {
        message.blocks.iter().any(|block| {
            matches!(
                block,
                haider_protocol::provider::Block::Text { text } if text == "first question"
            )
        })
    }));
    assert!(second.iter().any(|message| {
        message.blocks.iter().any(|block| {
            matches!(
                block,
                haider_protocol::provider::Block::Text { text } if text == "first answer"
            )
        })
    }));
    assert!(second.iter().any(|message| {
        message.blocks.iter().any(|block| {
            matches!(
                block,
                haider_protocol::provider::Block::Text { text } if text == "second question"
            )
        })
    }));

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// Scenario 6.
///
/// MUTATION CHECK: wake the harness before the menu CAS commits, append the
/// answer again in core, or issue the next provider request without the tool
/// result. Expected failure: duplicate MenuAnswered/ToolResult or request two
/// lacks the selected value.
#[tokio::test]
async fn scenario_6_request_input_round_trip_uses_second_control_attachment() {
    let root = test_root("w3c-live-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "menu-round-trip",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (dependencies, fake) = fake_dependencies(vec![
        FakeStep::EmitRequestInput {
            call_id: "choose-1".into(),
            kind: FakeInputKind::Choice,
            title: "Choose".into(),
            body: vec!["Pick one".into()],
            options: vec![FakeInputOption {
                key: "yes".into(),
                label: "Yes".into(),
                detail: None,
            }],
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "choose-1".into(),
        },
        FakeStep::EmitText {
            text: "continued".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut submitter = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "menu-submitter",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut submitter, &config, &workspace).await;
    send_request(
        &mut submitter,
        &config,
        "submit",
        submit_body("menu-submit", session_id.clone(), generation, "ask me"),
    )
    .await;
    let (run_id, _) = next_submit_response(&mut submitter).await;
    let (menu_id, request_seq, opening_generation) = loop {
        if let WireFrame::Event { envelope, .. } = submitter.next().await
            && let Ok(EventPayload::MenuOpened(menu)) =
                serde_json::from_value::<EventPayload>(envelope.payload.into())
        {
            break (menu.id, envelope.seq, envelope.worker_generation);
        }
    };

    let mut answerer = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "menu-answerer",
        ClientKind::Headless,
    )
    .await;
    attach_existing(
        &mut answerer,
        &config,
        session_id.clone(),
        request_seq,
        "answer-attach",
    )
    .await;
    answerer
        .send(
            &WireFrame::MenuAnswer {
                request_id: Some(RequestId::new("answer")),
                command_id: CommandId::new("answer-command"),
                session_id: session_id.clone(),
                menu_id: menu_id.clone(),
                request_seq,
                worker_generation: opening_generation,
                option_key: "yes".into(),
                option_index: 0,
                input: None,
            },
            config.frame_limit,
        )
        .await;
    assert!(matches!(
        next_response(&mut answerer).await,
        WireFrame::Response {
            body: ResponseBody::MenuAnswer { .. },
            ..
        }
    ));
    let events = events_until_terminal(&mut submitter, &run_id).await;
    assert!(matches!(
        events.last(),
        Some((_, EventPayload::RunState(RunState::Done)))
    ));
    assert_eq!(
        events
            .iter()
            .filter(|(_, payload)| matches!(payload, EventPayload::MenuAnswered(_)))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|(_, payload)| matches!(payload, EventPayload::ToolResult { .. }))
            .count(),
        1
    );
    let requests = fake.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| message.tool_result_for("choose-1").is_some())
    );

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// Scenario 7.
///
/// MUTATION CHECK: decide the winner in memory or wake both callers before
/// SQLite's first-committed-wins CAS. Expected failure: two successful
/// responses, two durable answers, or two follow-up provider requests.
#[tokio::test]
async fn scenario_7_two_menu_answers_race_and_only_first_commit_wins() {
    let root = test_root("w3c-live-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "menu-race",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (dependencies, fake) = fake_dependencies(vec![
        FakeStep::EmitRequestInput {
            call_id: "race-1".into(),
            kind: FakeInputKind::Choice,
            title: "Race".into(),
            body: Vec::new(),
            options: vec![
                FakeInputOption {
                    key: "a".into(),
                    label: "A".into(),
                    detail: None,
                },
                FakeInputOption {
                    key: "b".into(),
                    label: "B".into(),
                    detail: None,
                },
            ],
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "race-1".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut submitter = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "race-submitter",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut submitter, &config, &workspace).await;
    send_request(
        &mut submitter,
        &config,
        "submit",
        submit_body("race-submit", session_id.clone(), generation, "race"),
    )
    .await;
    let (run_id, _) = next_submit_response(&mut submitter).await;
    let (menu_id, request_seq, opening_generation) = loop {
        if let WireFrame::Event { envelope, .. } = submitter.next().await
            && let Ok(EventPayload::MenuOpened(menu)) =
                serde_json::from_value::<EventPayload>(envelope.payload.into())
        {
            break (menu.id, envelope.seq, envelope.worker_generation);
        }
    };
    let mut first = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "race-a",
        ClientKind::Headless,
    )
    .await;
    let mut second = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "race-b",
        ClientKind::Headless,
    )
    .await;
    attach_existing(
        &mut first,
        &config,
        session_id.clone(),
        request_seq,
        "attach-a",
    )
    .await;
    attach_existing(
        &mut second,
        &config,
        session_id.clone(),
        request_seq,
        "attach-b",
    )
    .await;
    let answer_a = WireFrame::MenuAnswer {
        request_id: Some(RequestId::new("answer-a")),
        command_id: CommandId::new("answer-command-a"),
        session_id: session_id.clone(),
        menu_id: menu_id.clone(),
        request_seq,
        worker_generation: opening_generation,
        option_key: "a".into(),
        option_index: 0,
        input: None,
    };
    let answer_b = WireFrame::MenuAnswer {
        request_id: Some(RequestId::new("answer-b")),
        command_id: CommandId::new("answer-command-b"),
        session_id,
        menu_id,
        request_seq,
        worker_generation: opening_generation,
        option_key: "b".into(),
        option_index: 1,
        input: None,
    };
    tokio::join!(
        first.send(&answer_a, config.frame_limit),
        second.send(&answer_b, config.frame_limit)
    );
    let responses = [
        next_response(&mut first).await,
        next_response(&mut second).await,
    ];
    assert_eq!(
        responses
            .iter()
            .filter(|frame| matches!(
                frame,
                WireFrame::Response {
                    body: ResponseBody::MenuAnswer { .. },
                    ..
                }
            ))
            .count(),
        1
    );
    assert_eq!(
        responses
            .iter()
            .filter(|frame| matches!(
                frame,
                WireFrame::Response {
                    body: ResponseBody::Error { code, .. },
                    ..
                } if code == ERROR_CODE_ALREADY_RESOLVED
            ))
            .count(),
        1
    );
    let events = events_until_terminal(&mut submitter, &run_id).await;
    assert_eq!(
        events
            .iter()
            .filter(|(_, payload)| matches!(payload, EventPayload::MenuAnswered(_)))
            .count(),
        1
    );
    assert_eq!(fake.requests().len(), 2);

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// Scenario 8.
///
/// MUTATION CHECK: signal cancellation before committing Cancelling, let the
/// provider stream win a buffered tie, or leave an open item uncompleted.
/// Expected failure: ordering/item-lifecycle assertions fail or a run event
/// appears after Cancelled.
#[tokio::test]
async fn scenario_8_wire_cancel_closes_open_items_and_cancelled_is_run_terminal() {
    let root = test_root("w3c-live-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "turn-cancel",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (dependencies, fake) = fake_dependencies(vec![
        FakeStep::EmitText {
            text: "partial".into(),
        },
        FakeStep::Hang,
    ]);
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "cancel-client",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut client, &config, &workspace).await;
    send_request(
        &mut client,
        &config,
        "submit",
        submit_body("cancel-submit", session_id.clone(), generation, "hang"),
    )
    .await;
    let (run_id, _) = next_submit_response(&mut client).await;
    let mut events = Vec::new();
    loop {
        if let WireFrame::Event { envelope, .. } = client.next().await
            && envelope.run_id.as_ref() == Some(&run_id)
        {
            let payload = serde_json::from_value::<EventPayload>(envelope.payload.into())
                .expect("typed event");
            let has_delta = matches!(
                payload,
                EventPayload::Item(haider_protocol::item::ItemEvent::Delta { .. })
            );
            events.push((envelope.seq, payload));
            if has_delta {
                break;
            }
        }
    }
    send_request(
        &mut client,
        &config,
        "cancel",
        RequestBody::TurnCancel {
            command_id: CommandId::new("cancel-command"),
            session_id: session_id.clone(),
            worker_generation: generation,
            run_id: run_id.clone(),
        },
    )
    .await;
    let mut response_seen = false;
    let cancelled_seq = loop {
        match client.next().await {
            WireFrame::Response {
                body:
                    ResponseBody::TurnCancel {
                        status: CancelStatus::Accepted,
                        ..
                    },
                ..
            } => response_seen = true,
            WireFrame::Event { envelope, .. } if envelope.run_id.as_ref() == Some(&run_id) => {
                let payload = serde_json::from_value::<EventPayload>(envelope.payload.into())
                    .expect("typed event");
                let terminal = payload == EventPayload::RunState(RunState::Cancelled);
                let seq = envelope.seq;
                events.push((seq, payload));
                if terminal {
                    break seq;
                }
            }
            _ => {}
        }
    };
    assert!(response_seen);
    let cancelling = events
        .iter()
        .position(|(_, payload)| *payload == EventPayload::RunState(RunState::Cancelling))
        .expect("durable cancelling");
    let cancelled = events
        .iter()
        .position(|(_, payload)| *payload == EventPayload::RunState(RunState::Cancelled))
        .expect("durable cancelled");
    assert!(cancelling < cancelled);
    assert!(events.iter().any(|(_, payload)| matches!(
        payload,
        EventPayload::Item(haider_protocol::item::ItemEvent::Completed { .. })
    )));
    let durable = read_session(&mut client, &config, session_id, "cancel-durable-read").await;
    let run_envelopes = durable
        .iter()
        .filter(|envelope| envelope.run_id.as_ref() == Some(&run_id))
        .collect::<Vec<_>>();
    let cancelled_index = run_envelopes
        .iter()
        .position(|envelope| envelope.seq == cancelled_seq)
        .expect("durable Cancelled");
    assert_eq!(
        cancelled_index + 1,
        run_envelopes.len(),
        "no durable run event may follow Cancelled"
    );
    let payloads = run_envelopes
        .iter()
        .filter_map(|envelope| {
            serde_json::from_value::<EventPayload>(envelope.payload.clone().into()).ok()
        })
        .collect::<Vec<_>>();
    for started in payloads.iter().filter_map(|payload| match payload {
        EventPayload::Item(haider_protocol::item::ItemEvent::Started { item_id, .. }) => {
            Some(item_id)
        }
        _ => None,
    }) {
        assert!(payloads.iter().any(|payload| matches!(
            payload,
            EventPayload::Item(haider_protocol::item::ItemEvent::Completed { item_id, .. })
                if item_id == started
        )));
    }
    assert_eq!(fake.requests().len(), 1);

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

#[derive(Clone)]
struct BlockingProviderFactory {
    entered: Arc<Semaphore>,
    release: Arc<Semaphore>,
    fake: Arc<FakeProvider>,
}

#[async_trait]
impl ProviderFactory for BlockingProviderFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, HaiderError> {
        self.entered.add_permits(1);
        self.release
            .acquire()
            .await
            .expect("test release semaphore")
            .forget();
        Ok(ResolvedTurnProvider {
            provider: self.fake.clone(),
            provider_name: "fake".into(),
            model: metadata.model.clone(),
            context_window: None,
            account_alias: None,
            active_no_auth: false,
            initial_rotation: None,
            rotation_budget_consumed: false,
            attempt_resolver: None,
            compaction_promotion: None,
        })
    }
}

/// Exact P1-2 schedule: provider resolution is blocked, wire cancellation
/// durably commits, then resolution is released. No provider request may
/// begin after that durable fence.
///
/// MUTATION CHECK: make `cancellation_fences_start` return false (its focused
/// law test fails); this controlled schedule separately proves the live call
/// site reaches `Cancelled` with zero provider requests. Verified by revert
/// on 2026-07-27.
#[tokio::test]
async fn cancelling_while_provider_factory_is_blocked_never_starts_provider() {
    let root = test_root("w3c-live-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "cancel-start-fence",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let entered = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let fake = Arc::new(FakeProvider::new(vec![FakeStep::Finish {
        reason: FinishReason::EndTurn,
    }]));
    let task = ready_with_dependencies(
        &config,
        DaemonDependencies {
            provider_factory: ProviderFactoryConfig::injected(Arc::new(BlockingProviderFactory {
                entered: entered.clone(),
                release: release.clone(),
                fake: fake.clone(),
            })),
            ..DaemonDependencies::default()
        },
    )
    .await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "cancel-start-client",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut client, &config, &workspace).await;
    send_request(
        &mut client,
        &config,
        "start-fence-submit",
        submit_body(
            "start-fence-submit-command",
            session_id.clone(),
            generation,
            "do not start after cancel",
        ),
    )
    .await;
    let (run_id, _) = next_submit_response(&mut client).await;
    entered.acquire().await.expect("factory entry").forget();
    send_request(
        &mut client,
        &config,
        "start-fence-cancel",
        RequestBody::TurnCancel {
            command_id: CommandId::new("start-fence-cancel-command"),
            session_id,
            worker_generation: generation,
            run_id: run_id.clone(),
        },
    )
    .await;
    loop {
        if matches!(
            client.next().await,
            WireFrame::Response {
                body: ResponseBody::TurnCancel {
                    status: CancelStatus::Accepted,
                    ..
                },
                ..
            }
        ) {
            break;
        }
    }
    release.add_permits(1);
    let events = events_until_terminal(&mut client, &run_id).await;
    assert!(matches!(
        events.last(),
        Some((_, EventPayload::RunState(RunState::Cancelled)))
    ));
    assert!(
        next_idle(&mut client).await,
        "user cancellation settles interrupted Idle"
    );
    assert!(fake.requests().is_empty());
    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

#[derive(Clone)]
struct ClosingHeldEffectFactory {
    effect: EffectId,
    dispatched: Arc<Semaphore>,
}

#[async_trait]
impl TurnToolFactory for ClosingHeldEffectFactory {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "held_effect".into(),
            description: "Hold after durable dispatch until cancellation".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }]
    }

    async fn create(
        &self,
        context: WorkerToolContext,
    ) -> Result<Option<Arc<dyn ToolDispatcher>>, HaiderError> {
        Ok(Some(Arc::new(ClosingHeldEffectDispatcher {
            context,
            effect: self.effect.clone(),
            dispatched: self.dispatched.clone(),
        })))
    }
}

struct ClosingHeldEffectDispatcher {
    context: WorkerToolContext,
    effect: EffectId,
    dispatched: Arc<Semaphore>,
}

impl ClosingHeldEffectDispatcher {
    async fn append(&self, suffix: &str, payload: EventPayload) -> Result<(), HaiderError> {
        let mut envelopes = [EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: EventId::new(format!("live-held-{}-{suffix}", self.effect)),
            seq: 0,
            session_id: self.context.store.session_id().clone(),
            branch_id: None,
            run_id: Some(self.context.run_id.clone()),
            agent_id: None,
            device_id: self.context.device_id.clone(),
            authority_epoch: 0,
            worker_generation: self.context.store.worker_generation(),
            causation_id: None,
            correlation_id: None,
            committed_at_ms: 0,
            render: RenderTargets {
                ui: true,
                durable: true,
                prompt: PromptRender::Omit,
            },
            payload: serde_json::to_value(payload)
                .expect("effect payload")
                .into(),
        }];
        StoreHandle::append(&self.context.store, &mut envelopes).await?;
        Ok(())
    }
}

#[async_trait]
impl ToolDispatcher for ClosingHeldEffectDispatcher {
    async fn execute(
        &self,
        _run_id: &RunId,
        _item_id: &haider_protocol::ids::ItemId,
        _call_id: &str,
        _name: &str,
        _args: serde_json::Value,
        _cancel: &CancelToken,
    ) -> Result<ToolDispatchResult, HaiderError> {
        self.append(
            "dispatched",
            EventPayload::Effect(EffectPhase::Dispatched {
                effect: self.effect.clone(),
            }),
        )
        .await?;
        self.dispatched.add_permits(1);
        future::pending().await
    }

    async fn close(&self) -> Result<(), HaiderError> {
        // Exact close-failure schedule: the dispatcher reports failure before
        // writing an outcome. The supervisor must reduce durable truth and
        // terminalize the abandoned dispatch itself before it may commit
        // Cancelled.
        Err(HaiderError::new(
            ErrorCode::EffectUnknownOutcome,
            "injected dispatcher close failure",
            true,
        ))
    }
}

/// Exact P1-1 schedule: cancellation drops a held dispatched execution and
/// dispatcher close fails without recording an outcome. Orderly cancellation
/// reconciles the abandoned dispatch to a terminal Cancelled outcome — never
/// an Unknown crash window — and only then may the run commit Cancelled.
///
/// MUTATION CHECK: remove `reconcile_unknown_effects` or restore the terminal
/// commit before it; or make the cancel path reconcile with
/// `UnknownReconcile::EvidenceOnly`/`Park` instead of `Cancel`. Expected
/// failure: the Cancelled outcome is absent (or Unknown appears) or the
/// outcome follows the run terminal. Verified by revert in W3c1.1.
#[tokio::test]
async fn held_effect_reconciles_unknown_before_cancelled() {
    let root = test_root("w3c-live-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "live-held-cancel",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let dispatched = Arc::new(Semaphore::new(0));
    let effect = EffectId::new("live-held-cancel-effect");
    let (dependencies, _fake) = fake_dependencies(vec![FakeStep::EmitToolCall {
        call_id: "held-call".into(),
        name: "held_effect".into(),
        args: serde_json::json!({}),
    }]);
    let task = ready_with_dependencies(
        &config,
        DaemonDependencies {
            tool_factory: Arc::new(ClosingHeldEffectFactory {
                effect: effect.clone(),
                dispatched: dispatched.clone(),
            }),
            ..dependencies
        },
    )
    .await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "held-cancel-client",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut client, &config, &workspace).await;
    send_request(
        &mut client,
        &config,
        "held-submit",
        submit_body(
            "held-submit-command",
            session_id.clone(),
            generation,
            "dispatch and hold",
        ),
    )
    .await;
    let (run_id, _) = next_submit_response(&mut client).await;
    dispatched
        .acquire()
        .await
        .expect("dispatch commits")
        .forget();
    send_request(
        &mut client,
        &config,
        "held-cancel",
        RequestBody::TurnCancel {
            command_id: CommandId::new("held-cancel-command"),
            session_id: session_id.clone(),
            worker_generation: generation,
            run_id: run_id.clone(),
        },
    )
    .await;
    let _events = events_until_terminal(&mut client, &run_id).await;
    assert!(
        next_idle(&mut client).await,
        "user cancellation settles interrupted Idle"
    );
    let durable = read_session(&mut client, &config, session_id, "held-cancel-read").await;
    let events = durable
        .into_iter()
        .filter(|envelope| envelope.run_id.as_ref() == Some(&run_id))
        .filter_map(|envelope| {
            serde_json::from_value::<EventPayload>(envelope.payload.into())
                .ok()
                .map(|payload| (envelope.seq, payload))
        })
        .collect::<Vec<_>>();
    let position = |predicate: &dyn Fn(&EventPayload) -> bool| {
        events
            .iter()
            .position(|(_, payload)| predicate(payload))
            .expect("expected event")
    };
    let dispatched = position(&|payload| {
        matches!(
            payload,
            EventPayload::Effect(EffectPhase::Dispatched { effect: candidate })
                if *candidate == effect
        )
    });
    let outcome = position(&|payload| {
        matches!(
            payload,
            EventPayload::Effect(EffectPhase::Outcome {
                effect: candidate,
                outcome: EffectOutcome::Cancelled,
                ..
            }) if *candidate == effect
        )
    });
    assert!(
        !events.iter().any(|(_, payload)| matches!(
            payload,
            EventPayload::Effect(EffectPhase::Outcome {
                outcome: EffectOutcome::Unknown,
                ..
            })
        )),
        "an orderly cancellation never records an Unknown crash window"
    );
    let cancelled = position(&|payload| *payload == EventPayload::RunState(RunState::Cancelled));
    assert!(dispatched < outcome && outcome < cancelled);
    assert!(matches!(
        events.last(),
        Some((_, EventPayload::RunState(RunState::Cancelled)))
    ));
    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// Worker-aware drain satellite (report §6.1 implementation bullet
/// "external admission gate and worker-aware drain", R9): a queued run that
/// never started must reach a durable terminal state during the drain grace,
/// not evaporate with the in-memory queue.
///
/// MUTATION CHECK: replace the `cancel_durable_queued_turns(..)` call with a
/// bare queue drop in either `run_supervisor` shutdown arm (worker.rs).
/// Expected failure: the accepted queued run has no terminal state after the
/// worker-aware drain completes.
#[tokio::test]
async fn worker_aware_drain_terminalizes_durable_queued_turns_before_store_close() {
    let root = test_root("w3c-live-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "worker-aware-drain",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (dependencies, fake) = fake_dependencies(vec![
        FakeStep::EmitText {
            text: "active".into(),
        },
        FakeStep::Hang,
    ]);
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "drain-client",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut client, &config, &workspace).await;
    send_request(
        &mut client,
        &config,
        "drain-active",
        submit_body(
            "drain-active-command",
            session_id.clone(),
            generation,
            "active turn",
        ),
    )
    .await;
    let (_active_run, _) = next_submit_response(&mut client).await;
    tokio::time::timeout(support::DEADLINE, async {
        while fake.requests().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("active provider request");
    send_request(
        &mut client,
        &config,
        "drain-queued",
        submit_body(
            "drain-queued-command",
            session_id.clone(),
            generation,
            "queued turn",
        ),
    )
    .await;
    let (queued_run, _) = next_submit_response(&mut client).await;
    drop(client);
    task.shutdown_handle().request("test drain");
    task.join().await.expect("daemon joins");

    // `task.join()` can return before the store's profile lock is fully
    // released under gate load (StoreLocked is self-declared RETRYABLE) —
    // bounded retry instead of a race flake (gate27 hygiene).
    let store = {
        let mut attempt = 0;
        loop {
            match Store::open(&config.store_dir) {
                Ok(store) => break store,
                Err(error) if error.retryable && attempt < 40 => {
                    attempt += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Err(error) => panic!("inspect drained store: {error:?}"),
            }
        }
    };
    let envelopes = store.journal_replay(&session_id).expect("drained replay");
    assert!(
        payloads_for_run(&envelopes, &queued_run)
            .any(|payload| payload == EventPayload::RunState(RunState::Cancelled))
    );
    assert_eq!(fake.requests().len(), 1);
}

/// Scenario 9.
///
/// MUTATION CHECK: classify a prior-generation Streaming run as resumable,
/// or fail to rediscover a prior-generation Queued run. Expected failure:
/// the interrupted prompt is sent twice, the queued prompt is never sent, or
/// the interrupted run lacks its durable recovery failure/terminal state.
/// Discard the restart attach replay or ignore its queued terminal fact.
/// Expected failure: when recovered work wins the attach race, the test waits
/// for a live duplicate that can never arrive and reaches the frame deadline.
#[tokio::test]
async fn scenario_9_restart_resumes_only_queued_and_terminalizes_streaming() {
    let root = test_root("w3c-live-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "restart-queued-only",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (dependencies, fake) = fake_dependencies(vec![
        FakeStep::EmitText {
            text: "partial before crash".into(),
        },
        FakeStep::Hang,
        FakeStep::EmitText {
            text: "queued resumed".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let first_task = ready_with_dependencies(&config, dependencies.clone()).await;
    let mut first = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "restart-before",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut first, &config, &workspace).await;
    send_request(
        &mut first,
        &config,
        "streaming-submit",
        submit_body(
            "streaming-command",
            session_id.clone(),
            generation,
            "interrupted prompt",
        ),
    )
    .await;
    let (streaming_run, _) = next_submit_response(&mut first).await;
    loop {
        if let WireFrame::Event { envelope, .. } = first.next().await
            && envelope.run_id.as_ref() == Some(&streaming_run)
            && serde_json::from_value::<EventPayload>(envelope.payload.into()).is_ok_and(
                |payload| {
                    matches!(
                        payload,
                        EventPayload::Item(haider_protocol::item::ItemEvent::Delta { .. })
                    )
                },
            )
        {
            break;
        }
    }
    send_request(
        &mut first,
        &config,
        "queued-submit",
        submit_body(
            "queued-command",
            session_id.clone(),
            generation,
            "queued prompt",
        ),
    )
    .await;
    let (queued_run, _) = next_submit_response(&mut first).await;
    assert_eq!(fake.requests().len(), 1, "queued turn has not started");

    drop(first);
    first_task.crash().await;

    let second_task = ready_with_dependencies(&config, dependencies).await;
    let mut second = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "restart-after",
        ClientKind::Headless,
    )
    .await;
    let replay = attach_existing(
        &mut second,
        &config,
        session_id.clone(),
        0,
        "restart-attach",
    )
    .await;
    if !replay
        .iter()
        .any(|envelope| run_terminal(envelope, &queued_run))
    {
        let _ = events_until_terminal(&mut second, &queued_run).await;
    }
    let envelopes = read_session(&mut second, &config, session_id, "restart-read-terminal").await;
    let streaming = payloads_for_run(&envelopes, &streaming_run).collect::<Vec<_>>();
    let queued = payloads_for_run(&envelopes, &queued_run).collect::<Vec<_>>();
    assert!(streaming.iter().any(|payload| matches!(
        payload,
        EventPayload::RunFailed { message, .. } if message.contains("interrupted")
    )));
    assert!(streaming.contains(&EventPayload::RunState(RunState::Errored)));
    assert!(queued.contains(&EventPayload::RunState(RunState::Done)));
    let requests = fake.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].messages.iter().any(|message| {
        message.blocks.iter().any(|block| {
            matches!(
                block,
                haider_protocol::provider::Block::Text { text } if text == "interrupted prompt"
            )
        })
    }));
    assert!(requests[1].messages.iter().any(|message| {
        message.blocks.iter().any(|block| {
            matches!(
                block,
                haider_protocol::provider::Block::Text { text } if text == "queued prompt"
            )
        })
    }));
    // Context survives the errored run: its committed user message reaches
    // the resumed request, while the torn (never-Completed) stream does not.
    assert!(
        requests[1].messages.iter().any(|message| {
            message.blocks.iter().any(|block| {
                matches!(
                    block,
                    haider_protocol::provider::Block::Text { text } if text == "interrupted prompt"
                )
            })
        }),
        "the errored run's committed user message is carried forward"
    );
    assert!(
        !requests[1].messages.iter().any(|message| {
            message.blocks.iter().any(|block| {
                matches!(
                    block,
                    haider_protocol::provider::Block::Text { text }
                        if text.contains("partial before crash")
                )
            })
        }),
        "a torn partial stream is never fed back"
    );

    second_task.shutdown_handle().request("test complete");
    second_task.join().await.expect("daemon joins");
}

/// W8a direct-shell law over the production UDS: the durable receipt is the
/// admission boundary, direct user provenance reaches the one EffectBroker,
/// and neither a provider turn nor a UserMessage is fabricated.
///
/// MUTATION CHECK: bypass `accept_shell_exec`, fence receipt lookup behind the
/// current generation, route through a provider turn, or use model
/// `process_exec` authorization. Expected runtime failure: the restart replay
/// returns stale-generation, the exact-once file changes, or the zero-provider,
/// no-UserMessage, or PreAuthorized(UserTyped) assertions below fail.
#[tokio::test]
async fn w8a_shell_exec_is_receipted_exactly_once_and_user_preauthorized() {
    #[cfg(windows)]
    let _windows_process_test = windows_real_process_test_guard(
        "w8a_shell_exec_is_receipted_exactly_once_and_user_preauthorized",
    )
    .await;
    let root = test_root("w8a-shell-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "w8a-shell",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (dependencies, fake) = fake_dependencies(vec![FakeStep::Finish {
        reason: FinishReason::EndTurn,
    }]);
    let first_task = ready_with_dependencies(&config, dependencies.clone()).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w8a-shell-test",
        "shell-client",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut client, &config, &workspace).await;
    let command = exact_once_shell_command();

    send_request(
        &mut client,
        &config,
        "shell-first",
        RequestBody::ShellExec {
            command_id: CommandId::new("shell-command"),
            session_id: session_id.clone(),
            worker_generation: generation,
            command: command.clone(),
            cwd: None,
        },
    )
    .await;
    // Deliberately lose the original response. Acceptance and the worker
    // handoff happen before the response send, so closing this socket leaves
    // only the durable command receipt available to the retrying client.
    drop(client);
    tokio::time::timeout(support::DEADLINE, async {
        loop {
            if fs::read_to_string(workspace.join("shell-count.txt"))
                .is_ok_and(|contents| contents == "once\n")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("shell side effect becomes visible after response loss");
    assert_eq!(
        fs::read_to_string(workspace.join("shell-count.txt")).expect("shell side effect"),
        "once\n"
    );

    let mut observer = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w8a-shell-test",
        "shell-terminal-observer",
        ClientKind::Headless,
    )
    .await;
    attach_existing(
        &mut observer,
        &config,
        session_id.clone(),
        0,
        "shell-terminal-attach",
    )
    .await;
    tokio::time::timeout(support::DEADLINE, async {
        loop {
            let durable = read_session(
                &mut observer,
                &config,
                session_id.clone(),
                "shell-terminal-read",
            )
            .await;
            if durable.iter().any(|envelope| {
                serde_json::from_value::<EventPayload>(envelope.payload.clone().into())
                    .is_ok_and(|payload| matches!(payload, EventPayload::RunState(RunState::Done)))
                    && envelope.run_id.is_some()
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("lost-response shell becomes durably terminal");
    drop(observer);
    first_task
        .shutdown_handle()
        .request("exercise receipt across restart");
    first_task.join().await.expect("first daemon joins");

    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w8a-shell-test",
        "shell-replay-after-restart",
        ClientKind::Headless,
    )
    .await;
    attach_existing(
        &mut client,
        &config,
        session_id.clone(),
        0,
        "shell-replay-attach",
    )
    .await;

    // Identical old-generation bytes must hit receipt preflight after the
    // restart, replay the original coordinates, and never re-execute.
    send_request(
        &mut client,
        &config,
        "shell-replay",
        RequestBody::ShellExec {
            command_id: CommandId::new("shell-command"),
            session_id: session_id.clone(),
            worker_generation: generation,
            command: command.clone(),
            cwd: None,
        },
    )
    .await;
    let (receipt_run_id, item_id, accepted_seq) = match next_response(&mut client).await {
        WireFrame::Response {
            body:
                ResponseBody::ShellExec {
                    run_id: Some(run_id),
                    item_id,
                    accepted_seq,
                    worker_generation,
                    ..
                },
            ..
        } => {
            assert_eq!(worker_generation, generation);
            (run_id, item_id, accepted_seq)
        }
        other => panic!("expected shell receipt replay, got {other:?}"),
    };
    assert!(accepted_seq > 0);
    assert_eq!(
        fs::read_to_string(workspace.join("shell-count.txt")).expect("exactly once after restart"),
        "once\n"
    );

    // Reusing the id with even one changed command byte is a receipt conflict.
    send_request(
        &mut client,
        &config,
        "shell-conflict",
        RequestBody::ShellExec {
            command_id: CommandId::new("shell-command"),
            session_id: session_id.clone(),
            worker_generation: generation,
            command: format!("{command} "),
            cwd: None,
        },
    )
    .await;
    assert!(matches!(
        next_response(&mut client).await,
        WireFrame::Response {
            body: ResponseBody::Error { code, .. },
            ..
        } if code == ERROR_CODE_INVALID_ARGUMENT
    ));

    let durable = read_session(&mut client, &config, session_id.clone(), "shell-read").await;
    let run_id = durable
        .iter()
        .find_map(|envelope| {
            serde_json::from_value::<EventPayload>(envelope.payload.clone().into())
                .is_ok_and(|payload| {
                    matches!(
                        payload,
                        EventPayload::Item(ItemEvent::Started { item_id: ref started, .. })
                            if started == &item_id
                    )
                })
                .then(|| envelope.run_id.clone())
                .flatten()
        })
        .expect("shell item has a durable run");
    assert_eq!(receipt_run_id, run_id);
    let payloads = payloads_for_run(&durable, &run_id).collect::<Vec<_>>();
    assert_eq!(
        payloads
            .iter()
            .filter(|payload| matches!(payload, EventPayload::UserMessage { .. }))
            .count(),
        0
    );
    assert_eq!(
        payloads
            .iter()
            .filter(|payload| matches!(
                payload,
                EventPayload::Item(ItemEvent::Started {
                    item: TurnItem::CommandExecution { call_id, command: stored, .. },
                    ..
                }) if call_id == "shell-command" && stored == &command
            ))
            .count(),
        1
    );
    let origins = payloads
        .iter()
        .filter_map(|payload| match payload {
            EventPayload::Item(ItemEvent::Completed { item, .. }) => {
                UserCommandOriginV1::from_extension_item(item)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(origins.len(), 1, "one durable user-command origin marker");
    assert_eq!(origins[0].command_item_id, item_id);
    assert_eq!(origins[0].call_id, "shell-command");
    let phases = payloads
        .iter()
        .filter_map(|payload| match payload {
            EventPayload::Effect(phase) => Some(phase.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(phases.len(), 4);
    let EffectPhase::Intent(intent) = &phases[0] else {
        panic!("effect starts with Intent");
    };
    assert!(matches!(
        &phases[1],
        EffectPhase::Authorized {
            effect,
            verdict: AuthorizationVerdict::PreAuthorized {
                source: AuthorizationSource::UserTyped,
            },
        } if effect == &intent.effect
    ));
    assert!(matches!(
        &phases[2],
        EffectPhase::Dispatched { effect } if effect == &intent.effect
    ));
    assert!(matches!(
        &phases[3],
        EffectPhase::Outcome {
            effect,
            outcome: EffectOutcome::Ok,
            ..
        } if effect == &intent.effect
    ));
    let output = payloads
        .iter()
        .filter_map(|payload| match payload {
            EventPayload::Item(ItemEvent::Delta {
                delta:
                    ItemDelta::CommandOutput {
                        stream: OutputStream::Stdout,
                        chunk_b64,
                    },
                ..
            }) => Some(BASE64.decode(chunk_b64).expect("command output base64")),
            _ => None,
        })
        .flatten()
        .collect::<Vec<_>>();
    assert_eq!(output, "héllo\n".as_bytes());
    assert!(payloads.iter().any(|payload| matches!(
        payload,
        EventPayload::Item(ItemEvent::Completed {
            item: TurnItem::CommandExecution {
                call_id,
                command: stored,
                status: haider_protocol::item::ToolStatus::Completed,
                exit_code: Some(0),
            },
            ..
        }) if call_id == "shell-command" && stored == &command
    )));
    assert_eq!(
        payloads.last(),
        Some(&EventPayload::RunState(RunState::Done))
    );
    assert_eq!(
        fs::read_to_string(workspace.join("shell-count.txt")).expect("exactly once ledger"),
        "once\n"
    );
    assert!(fake.requests().is_empty());

    send_request(
        &mut client,
        &config,
        "inventory-after-shell",
        RequestBody::ToolsInventory {
            session_id: session_id.clone(),
        },
    )
    .await;
    match next_response(&mut client).await {
        WireFrame::Response {
            body: ResponseBody::ToolsInventory { inventory, .. },
            ..
        } => assert!(
            inventory.remembered_grants.is_empty(),
            "user preauthorization must not create a provider permission grant"
        ),
        other => panic!("expected post-shell inventory, got {other:?}"),
    }

    let current_generation = current_generation(
        &mut client,
        &config,
        &session_id,
        "shell-next-turn-generation",
    )
    .await;
    send_request(
        &mut client,
        &config,
        "shell-next-turn-submit",
        submit_body(
            "shell-next-turn-command",
            session_id.clone(),
            current_generation,
            "explain the command result",
        ),
    )
    .await;
    let (next_run, _) = next_submit_response(&mut client).await;
    let _ = events_until_terminal(&mut client, &next_run).await;
    let requests = fake.requests();
    assert_eq!(
        requests.len(),
        1,
        "direct shell itself never calls a provider"
    );
    let text_blocks = requests[0]
        .messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            Block::Text { text } => Some(text.to_owned_string()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let command_record = text_blocks
        .iter()
        .position(|text| text.contains("[user-initiated shell command]"))
        .expect("next provider turn contains the committed shell record");
    let current_prompt = text_blocks
        .iter()
        .position(|text| *text == "explain the command result")
        .expect("current user prompt reaches provider");
    assert!(command_record < current_prompt);
    let command_record = &text_blocks[command_record];
    assert!(command_record.contains("origin: user_command"));
    // The record JSON-encodes command and output (json_string_fields_v1 —
    // the anti-forgery framing law pinned in haider-provider), so raw
    // containment can never match a command with escape sequences.
    let command_json = serde_json::to_string(&command).expect("command encodes");
    assert!(command_record.contains(&command_json));
    let output_json = serde_json::to_string("héllo\n").expect("output encodes");
    assert!(command_record.contains(output_json.trim_matches('"')));

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// Direct `shell.exec` owns a cancellable synthetic run. Cancelling that run
/// through the public RPC must settle its open item and stop the complete
/// supervised process tree before interrupted Idle is published.
#[tokio::test]
async fn w8a_shell_exec_cancel_kills_the_process_tree() {
    #[cfg(windows)]
    let _windows_process_test =
        windows_real_process_test_guard("w8a_shell_exec_cancel_kills_the_process_tree").await;
    let root = test_root("w8a-shell-cancel-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let heartbeat = workspace.join("heartbeat.log");
    let config = DaemonConfig::new(
        "w8a-shell-cancel",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (dependencies, fake) = fake_dependencies(vec![FakeStep::Finish {
        reason: FinishReason::EndTurn,
    }]);
    let task = ready_with_dependencies(&config, dependencies).await;
    #[cfg(windows)]
    let mut failure_diagnostics = support::FailureDiagnostics::install("w8a-shell-cancel", &task);
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w8a-shell-cancel-test",
        "shell-cancel-client",
        ClientKind::Headless,
    )
    .await;
    #[cfg(windows)]
    failure_diagnostics.watch(&client);
    let (session_id, generation) = create_and_attach(&mut client, &config, &workspace).await;
    send_request(
        &mut client,
        &config,
        "shell-cancel-start",
        RequestBody::ShellExec {
            command_id: CommandId::new("shell-cancel-command"),
            session_id: session_id.clone(),
            worker_generation: generation,
            command: cancellable_exec_command(),
            cwd: None,
        },
    )
    .await;
    let (run_id, item_id) = match next_response(&mut client).await {
        WireFrame::Response {
            body:
                ResponseBody::ShellExec {
                    run_id: Some(run_id),
                    item_id,
                    ..
                },
            ..
        } => (run_id, item_id),
        other => panic!("expected cancellable shell receipt, got {other:?}"),
    };
    let start = wait_for_direct_shell_process_tree(
        &mut client,
        &config,
        &run_id,
        &heartbeat,
        &workspace,
        &fake,
    )
    .await;
    #[cfg(not(windows))]
    assert_eq!(
        start.expect("direct shell process tree starts"),
        ProcessStartObservation::Started
    );
    #[cfg(windows)]
    let retry_start = !matches!(start, Ok(ProcessStartObservation::Started));
    #[cfg(windows)]
    let (run_id, item_id, process_start_attempts, shell_attempt_command_id) = if retry_start {
        let recovery_deadline = tokio::time::Instant::now() + support::DEADLINE;
        tokio::time::timeout_at(recovery_deadline, async {
            eprintln!(
                "haider-daemond windows-process test=w8a_shell_exec_cancel_kills_the_process_tree phase=settling-first-attempt"
            );
            let _previous_attempt = settle_process_start_attempt_before_retry(
                &mut client,
                &config,
                &session_id,
                generation,
                &run_id,
                recovery_deadline,
                "shell-cancel-timeout-cleanup",
                "shell-cancel-timeout-cleanup-command",
            )
            .await;
            eprintln!(
                "haider-daemond windows-process test=w8a_shell_exec_cancel_kills_the_process_tree phase=cleaning-first-attempt"
            );
            clean_timed_out_process_start_files(&workspace, &heartbeat).await;
            eprintln!(
                "haider-daemond windows-process test=w8a_shell_exec_cancel_kills_the_process_tree phase=submitting-retry"
            );
            send_request(
                &mut client,
                &config,
                "shell-cancel-start-retry",
                RequestBody::ShellExec {
                    command_id: CommandId::new("shell-cancel-command-retry"),
                    session_id: session_id.clone(),
                    worker_generation: generation,
                    command: cancellable_exec_command(),
                    cwd: None,
                },
            )
            .await;
            let retry_receipt = match next_response(&mut client).await {
                WireFrame::Response {
                    body:
                        ResponseBody::ShellExec {
                            run_id: Some(run_id),
                            item_id,
                            ..
                        },
                    ..
                } => (run_id, item_id),
                other => panic!("expected retry's cancellable shell receipt, got {other:?}"),
            };
            eprintln!(
                "haider-daemond windows-process test=w8a_shell_exec_cancel_kills_the_process_tree phase=waiting-for-retry-start"
            );
            assert_eq!(
                wait_for_direct_shell_process_tree(
                    &mut client,
                    &config,
                    &retry_receipt.0,
                    &heartbeat,
                    &workspace,
                    &fake,
                )
                .await
                .expect("direct shell process tree starts on its single retry"),
                ProcessStartObservation::Started,
                "the fresh retry must own the live process tree used by cancellation assertions"
            );
            (
                retry_receipt.0,
                retry_receipt.1,
                2_usize,
                "shell-cancel-command-retry",
            )
        })
        .await
        .unwrap_or_else(|error| {
            eprintln!(
                "haider-daemond windows-process test=w8a_shell_exec_cancel_kills_the_process_tree phase=failed-start-recovery-deadline result=Err({error:?})"
            );
            panic!("failed direct-shell start recovery exceeded one continuous deadline")
        })
    } else {
        (run_id, item_id, 1_usize, "shell-cancel-command")
    };

    #[cfg(not(windows))]
    send_request(
        &mut client,
        &config,
        "shell-cancel-request",
        RequestBody::TurnCancel {
            command_id: CommandId::new("shell-cancel-rpc-command"),
            session_id: session_id.clone(),
            worker_generation: generation,
            run_id: run_id.clone(),
        },
    )
    .await;
    #[cfg(not(windows))]
    tokio::time::timeout(support::DEADLINE, async {
        let mut response_seen = false;
        let mut terminal_seen = false;
        let mut interrupted_idle_seen = false;
        while !(response_seen && terminal_seen && interrupted_idle_seen) {
            match client.next().await {
                WireFrame::Response {
                    body:
                        ResponseBody::TurnCancel {
                            run_id: cancelled,
                            status: CancelStatus::Accepted,
                            ..
                        },
                    ..
                } => {
                    assert_eq!(cancelled, run_id);
                    response_seen = true;
                }
                WireFrame::Event { envelope, .. } => {
                    let Ok(payload) =
                        serde_json::from_value::<EventPayload>(envelope.payload.into())
                    else {
                        continue;
                    };
                    if envelope.run_id.as_ref() == Some(&run_id)
                        && payload == EventPayload::RunState(RunState::Cancelled)
                    {
                        terminal_seen = true;
                    }
                    if payload
                        == EventPayload::SessionState(SessionState::Idle { interrupted: true })
                    {
                        interrupted_idle_seen = true;
                    }
                }
                _ => {}
            }
        }
    })
    .await
    .expect("direct shell cancellation reaches response, terminal, and Idle");
    #[cfg(windows)]
    let cancel_events = cancel_live_windows_attempt(
        &mut client,
        &config,
        &session_id,
        generation,
        &run_id,
        "shell-cancel-request",
        "shell-cancel-rpc-command",
    )
    .await;
    #[cfg(windows)]
    assert!(matches!(
        cancel_events.last(),
        Some((_, EventPayload::RunState(RunState::Cancelled)))
    ));
    let durable = read_session(
        &mut client,
        &config,
        session_id.clone(),
        "shell-cancel-durable-read",
    )
    .await;
    assert!(payloads_for_run(&durable, &run_id).any(|payload| matches!(
        payload,
        EventPayload::Item(ItemEvent::Completed {
            item_id: completed,
            item: TurnItem::CommandExecution {
                status: haider_protocol::item::ToolStatus::Cancelled,
                ..
            },
        }) if completed == item_id
    )));
    let stopped_size = fs::metadata(&heartbeat).expect("heartbeat metadata").len();
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(
        fs::metadata(&heartbeat).expect("heartbeat metadata").len(),
        stopped_size,
        "cancelled direct-shell child or descendant kept running"
    );
    #[cfg(unix)]
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    #[cfg(windows)]
    tokio::time::sleep(std::time::Duration::from_millis(1_200)).await;
    assert!(
        !workspace.join("descendant-survived.log").exists(),
        "cancelled direct-shell descendant survived its process tree"
    );
    assert!(fake.requests().is_empty());

    let current_generation = current_generation(
        &mut client,
        &config,
        &session_id,
        "shell-cancel-next-turn-generation",
    )
    .await;
    send_request(
        &mut client,
        &config,
        "shell-cancel-next-turn-submit",
        submit_body(
            "shell-cancel-next-turn-command",
            session_id.clone(),
            current_generation,
            "explain the cancelled command",
        ),
    )
    .await;
    let (next_run, _) = next_submit_response(&mut client).await;
    let _ = events_until_terminal(&mut client, &next_run).await;
    let requests = fake.requests();
    assert_eq!(requests.len(), 1);
    let command_records = requests[0]
        .messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            Block::Text { text } if text.contains("[user-initiated shell command]") => {
                Some(text.to_owned_string())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    #[cfg(windows)]
    assert_eq!(
        command_records.len(),
        process_start_attempts,
        "the next prompt carries one chronological command record per fresh attempt"
    );
    let command_record = command_records
        .last()
        .expect("next provider turn contains the cancelled shell record");
    assert!(command_record.contains("origin: user_command"));
    #[cfg(not(windows))]
    assert!(command_record.contains("status: cancelled"));
    #[cfg(windows)]
    {
        let ordered_frames = client
            .history_snapshot()
            .into_iter()
            .filter(|frame| {
                frame.contains(run_id.as_str()) || frame.contains(shell_attempt_command_id)
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            command_record.contains("status: cancelled"),
            "actual cancelled command record (run {run_id}):\n{command_record}\n\
             ordered command records:\n{}\n\
             ordered frames observed for the actual cancelled command:\n{ordered_frames}",
            command_records.join("\n----- next command record -----\n")
        );
    }

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// MUTATION CHECK: let direct shell admission ignore active runs, advertise
/// legacy `exec`, or treat `cd` as a client-local state mutation. Expected
/// runtime failure: Busy, canonical inventory, or explicit builtin rejection
/// below changes at the real RPC boundary.
#[tokio::test]
async fn w8a_shell_busy_builtin_rejection_and_inventory_are_typed() {
    let root = test_root("w8a-shell-policy-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "w8a-shell-policy",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (dependencies, fake) = fake_dependencies(vec![FakeStep::Hang]);
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w8a-shell-policy-test",
        "shell-policy-client",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut client, &config, &workspace).await;

    send_request(
        &mut client,
        &config,
        "inventory",
        RequestBody::ToolsInventory {
            session_id: session_id.clone(),
        },
    )
    .await;
    let inventory = match next_response(&mut client).await {
        WireFrame::Response {
            body: ResponseBody::ToolsInventory { inventory, .. },
            ..
        } => inventory,
        other => panic!("expected tool inventory, got {other:?}"),
    };
    let names = inventory
        .tools
        .iter()
        .map(|entry| entry.manifest.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            "request_input",
            // D4/E2 (v0.0.925): the actor-owned plan surface and the
            // plan-gated loom_register sit with request_input —
            // NotApplicable policy, no brokered effect.
            "plan",
            "loom_register",
            // G1: the actor-owned todo surface.
            "todo_write",
            "graph_evidence",
            "fs_read",
            "fs_glob",
            "fs_search",
            "fs_write",
            "fs_edit",
            "write",
            "edit",
            "fs_path",
            "process_exec",
            "spawn_subagent",
            "message_subagent",
            // W-A: the background-task pack — output reads are effect-free
            // (the request_input pattern); kill sits under the SAME process
            // ceiling and Ask default as process_exec.
            "task_output",
            "task_kill",
            // W-B: the universal local fetch is a brokered `Network { host }`
            // effect; the lite-only client search is effect-free
            // provider-credential traffic (the request_input pattern). Both
            // live in the REGISTRY for every pair — the per-turn
            // advertisement seam is what narrows them per resolved pair.
            "web_fetch",
            "web_search",
            // CU-2: the computer-use tool — ScreenObserve/ScreenControl
            // effects, default-deny (allow_screen / allow_screen_control).
            "computer",
            // §E: effect-free session monitor registry administration.
            "monitor",
            // v0.0.970 modelcat: effect-free cached model/provider inventory.
            "list_models",
            // 965: peer discovery/messaging and SSH inventory/execution are
            // canonical registry entries with their own typed effects.
            "peer_list",
            "peer_send",
            "ssh_list",
            "ssh_shell",
        ]
    );
    assert!(!names.contains(&"exec"));
    assert!(inventory.remembered_grants.is_empty());
    let process = inventory
        .tools
        .iter()
        .find(|entry| entry.manifest.name == "process_exec")
        .expect("canonical process tool");
    assert_eq!(
        process.manifest.effects,
        [EffectClass::ProcessExec, EffectClass::RemoteExecution]
    );
    assert_eq!(process.default, ToolPermissionDefault::Ask);
    let task_output = inventory
        .tools
        .iter()
        .find(|entry| entry.manifest.name == "task_output")
        .expect("task output tool");
    assert!(task_output.manifest.effects.is_empty());
    assert_eq!(task_output.default, ToolPermissionDefault::NotApplicable);
    let task_kill = inventory
        .tools
        .iter()
        .find(|entry| entry.manifest.name == "task_kill")
        .expect("task kill tool");
    assert_eq!(task_kill.manifest.effects, [EffectClass::ProcessExec]);
    assert_eq!(task_kill.default, ToolPermissionDefault::Ask);
    // W-B pins: fetch is an Ask-by-default network effect (the manifest's
    // empty host is the CLASS placeholder — the live intent carries the real
    // host); search is effect-free and permissionless.
    let web_fetch = inventory
        .tools
        .iter()
        .find(|entry| entry.manifest.name == "web_fetch")
        .expect("local web fetch tool");
    assert_eq!(
        web_fetch.manifest.effects,
        [EffectClass::Network {
            host: String::new()
        }]
    );
    assert_eq!(web_fetch.default, ToolPermissionDefault::Ask);
    let web_search = inventory
        .tools
        .iter()
        .find(|entry| entry.manifest.name == "web_search")
        .expect("client web search tool");
    assert!(web_search.manifest.effects.is_empty());
    assert_eq!(web_search.default, ToolPermissionDefault::NotApplicable);
    let reads = inventory
        .tools
        .iter()
        .filter(|entry| {
            matches!(
                entry.manifest.name.as_str(),
                "fs_read" | "fs_search" | "fs_glob"
            )
        })
        .collect::<Vec<_>>();
    assert!(reads.iter().all(|entry| {
        entry.manifest.effects == [EffectClass::FsRead]
            && entry.default == ToolPermissionDefault::Allow
    }));

    send_request(
        &mut client,
        &config,
        "busy-turn",
        submit_body(
            "busy-turn-command",
            session_id.clone(),
            generation,
            "remain active",
        ),
    )
    .await;
    let (active_run, _) = next_submit_response(&mut client).await;
    loop {
        if let WireFrame::Event { envelope, .. } = client.next().await
            && envelope.run_id.as_ref() == Some(&active_run)
            && serde_json::from_value::<EventPayload>(envelope.payload.into())
                .is_ok_and(|payload| payload == EventPayload::RunState(RunState::Thinking))
        {
            break;
        }
    }
    tokio::time::timeout(support::DEADLINE, async {
        while fake.requests().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("hanging provider request begins");
    assert_eq!(fake.requests().len(), 1);

    send_request(
        &mut client,
        &config,
        "shell-busy",
        RequestBody::ShellExec {
            command_id: CommandId::new("shell-busy-command"),
            session_id: session_id.clone(),
            worker_generation: generation,
            command: "printf blocked".into(),
            cwd: None,
        },
    )
    .await;
    assert!(matches!(
        next_response(&mut client).await,
        WireFrame::Response {
            body: ResponseBody::Error { code, retryable: true, .. },
            ..
        } if code == ERROR_CODE_BUSY
    ));

    send_request(
        &mut client,
        &config,
        "shell-cd",
        RequestBody::ShellExec {
            command_id: CommandId::new("shell-cd-command"),
            session_id: session_id.clone(),
            worker_generation: generation,
            command: "cd nested".into(),
            cwd: None,
        },
    )
    .await;
    assert!(matches!(
        next_response(&mut client).await,
        WireFrame::Response {
            body: ResponseBody::Error { code, retryable: false, .. },
            ..
        } if code == ERROR_CODE_UNSUPPORTED_SHELL_BUILTIN
    ));

    send_request(
        &mut client,
        &config,
        "busy-turn-cancel",
        RequestBody::TurnCancel {
            command_id: CommandId::new("busy-turn-cancel-command"),
            session_id,
            worker_generation: generation,
            run_id: active_run,
        },
    )
    .await;
    assert!(matches!(
        next_response(&mut client).await,
        WireFrame::Response {
            body: ResponseBody::TurnCancel { .. },
            ..
        }
    ));

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// Startup poison fixture (B2).
///
/// MUTATION CHECK: return metadata-less prior-generation Queued work from
/// `recover_startup` instead of terminalizing it. Expected failure: the
/// daemon stops before Ready. Verified by revert in W3c1.1.
#[tokio::test]
async fn metadata_less_prior_generation_queued_run_terminalizes_and_reaches_ready() {
    let root = test_root("w3c-live-");
    let config = DaemonConfig::new(
        "poison-session",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let session_id = SessionId::new("poison-session");
    let run_id = RunId::new("poison-run");
    {
        let store = Store::open(&config.store_dir).expect("seed store");
        let generation = store.worker_generation();
        let mut events = vec![
            recovery_fixture_envelope(
                &session_id,
                &run_id,
                generation,
                "poison-queued",
                EventPayload::RunState(RunState::Queued),
                PromptRender::Omit,
            ),
            recovery_fixture_envelope(
                &session_id,
                &run_id,
                generation,
                "poison-user",
                EventPayload::UserMessage {
                    text: "cannot resume without metadata".into(),
                    attachments: Vec::new(),
                    mode: DeliveryMode::Queue,
                },
                PromptRender::Verbatim,
            ),
        ];
        store.append(&mut events).expect("poison prefix");
    }

    let (dependencies, fake) = fake_dependencies(vec![FakeStep::Hang]);
    let task = ready_with_dependencies(&config, dependencies).await;
    task.shutdown_handle().request("fixture inspected");
    task.join().await.expect("daemon joins");
    assert!(fake.requests().is_empty());

    let store = Store::open(&config.store_dir).expect("inspect recovery");
    let events = store.journal_replay(&session_id).expect("history");
    let payloads = payloads_for_run(&events, &run_id).collect::<Vec<_>>();
    assert!(payloads.contains(&EventPayload::RunState(RunState::Errored)));
    assert!(payloads.iter().any(|payload| matches!(
        payload,
        EventPayload::RunFailed {
            code: ErrorCode::Internal,
            ..
        }
    )));
    assert!(events.iter().any(|envelope| {
        matches!(
            serde_json::from_value::<EventPayload>(envelope.payload.clone().into()),
            Ok(EventPayload::SessionState(SessionState::Idle {
                interrupted: true
            }))
        )
    }));
}

/// B1 wire pin: a legacy session without typed metadata is rejected with the
/// caller's request correlation before any Queued acceptance can commit.
#[tokio::test]
async fn metadata_less_live_submit_is_correlated_invalid_argument_without_acceptance() {
    let root = test_root("w3c-live-");
    let config = DaemonConfig::new(
        "legacy-live-submit",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let session_id = SessionId::new("legacy-live-session");
    {
        let store = Store::open(&config.store_dir).expect("seed store");
        let mut events = [EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: EventId::new("legacy-idle"),
            seq: 0,
            session_id: session_id.clone(),
            branch_id: None,
            run_id: None,
            agent_id: None,
            device_id: DeviceId::new("legacy-fixture"),
            authority_epoch: 0,
            worker_generation: store.worker_generation(),
            causation_id: None,
            correlation_id: None,
            committed_at_ms: 0,
            render: RenderTargets {
                ui: true,
                durable: true,
                prompt: PromptRender::Omit,
            },
            payload: serde_json::to_value(EventPayload::SessionState(SessionState::Idle {
                interrupted: false,
            }))
            .expect("idle payload")
            .into(),
        }];
        store.append(&mut events).expect("legacy session row");
    }

    let task = ready(&config).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "legacy-submit-client",
        ClientKind::Headless,
    )
    .await;
    send_request(
        &mut client,
        &config,
        "legacy-list",
        RequestBody::SessionList {
            cursor: None,
            limit: 10,
            order: Default::default(),
        },
    )
    .await;
    let generation = match client.next_reply().await {
        WireFrame::Response {
            body: ResponseBody::SessionList { sessions, .. },
            ..
        } => {
            sessions
                .into_iter()
                .find(|summary| summary.session_id == session_id)
                .expect("legacy summary")
                .worker_generation
        }
        other => panic!("expected session list, got {other:?}"),
    };
    let _ = attach_existing(&mut client, &config, session_id.clone(), 0, "legacy-attach").await;
    send_request(
        &mut client,
        &config,
        "legacy-submit-request",
        submit_body(
            "legacy-submit-command",
            session_id.clone(),
            generation,
            "must not commit",
        ),
    )
    .await;
    let rejection = client.next_reply().await;
    assert!(
        matches!(
        rejection,
        WireFrame::Response {
            ref request_id,
            body: ResponseBody::Error { ref code, .. },
        } if *request_id == RequestId::new("legacy-submit-request")
            && code == ERROR_CODE_INVALID_ARGUMENT
        ),
        "unexpected legacy-submit response: {rejection:?}"
    );
    task.shutdown_handle().request("fixture inspected");
    task.join().await.expect("daemon joins");

    let store = Store::open(&config.store_dir).expect("inspect store");
    let events = store.journal_replay(&session_id).expect("history");
    assert!(!events.iter().any(|envelope| {
        serde_json::from_value::<EventPayload>(envelope.payload.clone().into())
            .is_ok_and(|payload| payload == EventPayload::RunState(RunState::Queued))
    }));
}

#[derive(Clone)]
struct RevokedCredentialFactory;

#[async_trait]
impl ProviderFactory for RevokedCredentialFactory {
    async fn resolve_for_turn(
        &self,
        _metadata: &SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, HaiderError> {
        Err(HaiderError::new(
            ErrorCode::CredentialMissing,
            "test credential was revoked",
            true,
        ))
    }
}

#[derive(Clone)]
struct PanicOnceFactory {
    calls: Arc<AtomicUsize>,
    fake: Arc<FakeProvider>,
}

#[async_trait]
impl ProviderFactory for PanicOnceFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, HaiderError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            panic!("injected provider factory panic");
        }
        Ok(ResolvedTurnProvider {
            provider: self.fake.clone(),
            provider_name: "fake".into(),
            model: metadata.model.clone(),
            context_window: None,
            account_alias: None,
            active_no_auth: false,
            initial_rotation: None,
            rotation_budget_consumed: false,
            attempt_resolver: None,
            compaction_promotion: None,
        })
    }
}

#[tokio::test]
async fn panicked_supervisor_terminalizes_run_and_fresh_incarnation_is_usable() {
    let root = test_root("w3c-live-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "panic-eviction",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let fake = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitText {
            text: "fresh supervisor".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let dependencies = DaemonDependencies {
        provider_factory: ProviderFactoryConfig::injected(Arc::new(PanicOnceFactory {
            calls: Arc::new(AtomicUsize::new(0)),
            fake: fake.clone(),
        })),
        ..DaemonDependencies::default()
    };
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "panic-client",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut client, &config, &workspace).await;
    send_request(
        &mut client,
        &config,
        "panic-submit",
        submit_body(
            "panic-command",
            session_id.clone(),
            generation,
            "panic once",
        ),
    )
    .await;
    let (panicked_run, _) = next_submit_response(&mut client).await;
    let panicked = events_until_terminal(&mut client, &panicked_run).await;
    assert!(
        panicked
            .iter()
            .any(|(_, payload)| matches!(payload, EventPayload::RunFailed { .. }))
    );
    assert!(matches!(
        panicked.last(),
        Some((_, EventPayload::RunState(RunState::Errored)))
    ));

    send_request(
        &mut client,
        &config,
        "fresh-submit",
        submit_body(
            "fresh-command",
            session_id,
            generation,
            "use fresh supervisor",
        ),
    )
    .await;
    let (fresh_run, _) = next_submit_response(&mut client).await;
    let fresh = events_until_terminal(&mut client, &fresh_run).await;
    assert!(matches!(
        fresh.last(),
        Some((_, EventPayload::RunState(RunState::Done)))
    ));
    assert_eq!(fake.requests().len(), 1);
    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// Revoked-credential checkpoint fixture (B2).
///
/// MUTATION CHECK: propagate a recovered supervisor start error through the
/// Ready barrier. Expected failure: startup stops instead of closing the
/// menu and terminalizing the run. Verified by revert in W3c1.1.
#[tokio::test]
async fn revoked_credential_checkpoint_terminalizes_menu_and_reaches_ready() {
    let root = test_root("w3c-live-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "revoked-checkpoint",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (first_dependencies, fake) = fake_dependencies(vec![
        FakeStep::EmitRequestInput {
            call_id: "revoked-choice".into(),
            kind: FakeInputKind::Choice,
            title: "Credential-dependent choice".into(),
            body: Vec::new(),
            options: vec![FakeInputOption {
                key: "continue".into(),
                label: "Continue".into(),
                detail: None,
            }],
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
    ]);
    let first_task = ready_with_dependencies(&config, first_dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "revoked-before",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut client, &config, &workspace).await;
    send_request(
        &mut client,
        &config,
        "revoked-submit",
        submit_body(
            "revoked-command",
            session_id.clone(),
            generation,
            "park then revoke",
        ),
    )
    .await;
    let (run_id, _) = next_submit_response(&mut client).await;
    let menu_id = loop {
        if let WireFrame::Event { envelope, .. } = client.next().await
            && envelope.run_id.as_ref() == Some(&run_id)
            && let Ok(EventPayload::MenuOpened(menu)) =
                serde_json::from_value::<EventPayload>(envelope.payload.into())
        {
            break menu.id;
        }
    };
    assert_eq!(fake.requests().len(), 1);
    drop(client);
    first_task.crash().await;

    let dependencies = DaemonDependencies {
        provider_factory: ProviderFactoryConfig::injected(Arc::new(RevokedCredentialFactory)),
        ..DaemonDependencies::default()
    };
    let second_task = ready_with_dependencies(&config, dependencies).await;
    second_task.shutdown_handle().request("fixture inspected");
    second_task.join().await.expect("daemon joins");

    let store = Store::open(&config.store_dir).expect("inspect recovery");
    let events = store.journal_replay(&session_id).expect("history");
    let payloads = payloads_for_run(&events, &run_id).collect::<Vec<_>>();
    assert!(payloads.contains(&EventPayload::RunState(RunState::Errored)));
    assert!(payloads.iter().any(|payload| matches!(
        payload,
        EventPayload::RunFailed {
            code: ErrorCode::CredentialMissing,
            ..
        }
    )));
    assert!(payloads.iter().any(|payload| matches!(
        payload,
        EventPayload::MenuClosed { menu, .. } if *menu == menu_id
    )));
    assert!(events.iter().any(|envelope| {
        matches!(
            serde_json::from_value::<EventPayload>(envelope.payload.clone().into()),
            Ok(EventPayload::SessionState(SessionState::Idle {
                interrupted: true
            }))
        )
    }));
}

/// B2 mixed fixture: an earlier recovered checkpoint may park indefinitely,
/// while a later recovered Queued item is acknowledged at safe supervisor
/// handoff so startup still reaches Ready.
///
/// MUTATION CHECK: acknowledge queued recovery only from `start_turn`.
/// Expected failure: Ready waits forever behind the unanswered checkpoint.
#[tokio::test]
async fn checkpoint_then_later_queued_recovery_reaches_ready_without_starting_queued() {
    let root = test_root("w3c-live-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "mixed-recovery",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (first_dependencies, first_fake) = fake_dependencies(vec![
        FakeStep::EmitRequestInput {
            call_id: "mixed-choice".into(),
            kind: FakeInputKind::Choice,
            title: "Park recovery".into(),
            body: Vec::new(),
            options: vec![FakeInputOption {
                key: "continue".into(),
                label: "Continue".into(),
                detail: None,
            }],
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
    ]);
    let first_task = ready_with_dependencies(&config, first_dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "mixed-before",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut client, &config, &workspace).await;
    send_request(
        &mut client,
        &config,
        "mixed-checkpoint-submit",
        submit_body(
            "mixed-checkpoint-command",
            session_id.clone(),
            generation,
            "park first",
        ),
    )
    .await;
    let (checkpoint_run, _) = next_submit_response(&mut client).await;
    let menu_id = loop {
        if let WireFrame::Event { envelope, .. } = client.next().await
            && envelope.run_id.as_ref() == Some(&checkpoint_run)
            && let Ok(EventPayload::MenuOpened(menu)) =
                serde_json::from_value::<EventPayload>(envelope.payload.into())
        {
            break menu.id;
        }
    };
    send_request(
        &mut client,
        &config,
        "mixed-queued-submit",
        submit_body(
            "mixed-queued-command",
            session_id.clone(),
            generation,
            "wait behind checkpoint",
        ),
    )
    .await;
    let (queued_run, _) = next_submit_response(&mut client).await;
    assert_eq!(first_fake.requests().len(), 1);
    drop(client);
    first_task.crash().await;

    let (second_dependencies, second_fake) = fake_dependencies(vec![FakeStep::Hang]);
    let second_task = ready_with_dependencies(&config, second_dependencies).await;
    assert!(
        second_fake.requests().is_empty(),
        "checkpoint recovery does not replay provider work and queued stays behind it"
    );
    second_task
        .shutdown_handle()
        .request("mixed fixture inspected");
    second_task.join().await.expect("daemon joins");

    let store = Store::open(&config.store_dir).expect("inspect recovery");
    let events = store.journal_replay(&session_id).expect("history");
    let checkpoint = payloads_for_run(&events, &checkpoint_run).collect::<Vec<_>>();
    let queued = payloads_for_run(&events, &queued_run).collect::<Vec<_>>();
    // DIRECTED CHANGE (W3c2 P3-4): the graceful drain now PARKS the
    // reconstructed request_input checkpoint instead of destroying it — the
    // pre-W3c2 assertions here pinned the P3-4 bug ("a graceful restart
    // destroys what a crash preserves"). The checkpoint run must survive
    // NONTERMINAL with its menu still open; the queued run behind it is
    // still terminalized at drain, which is what this test exists to pin.
    assert!(
        !checkpoint
            .iter()
            .any(|payload| matches!(payload, EventPayload::RunState(state) if state.is_terminal())),
        "the parked checkpoint must survive a graceful drain"
    );
    assert!(
        checkpoint.contains(&EventPayload::RunState(RunState::InputRequired {
            menu: menu_id.clone()
        }))
    );
    assert!(
        !checkpoint.iter().any(|payload| matches!(
            payload,
            EventPayload::MenuClosed { menu, .. } if *menu == menu_id
        )),
        "the parked menu stays open for the next generation's recovery"
    );
    assert!(queued.contains(&EventPayload::RunState(RunState::Cancelled)));
}

/// Scenario 10.
///
/// MUTATION CHECK: rerun the provider request that created request_input,
/// require the answer to carry the new generation instead of the durable
/// opening coordinates, or omit the post-registration committed-answer scan.
/// Expected failure: request count exceeds two, the replayed answer is
/// rejected, or the recovered harness remains parked forever.
#[tokio::test]
async fn scenario_10_restart_replays_request_input_without_reexecuting_prior_request() {
    let root = test_root("w3c-live-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "restart-request-input",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (dependencies, fake) = fake_dependencies(vec![
        FakeStep::EmitRequestInput {
            call_id: "restart-choice".into(),
            kind: FakeInputKind::Choice,
            title: "Resume me".into(),
            body: vec!["This menu survives a restart".into()],
            options: vec![FakeInputOption {
                key: "continue".into(),
                label: "Continue".into(),
                detail: None,
            }],
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "restart-choice".into(),
        },
        FakeStep::EmitText {
            text: "resumed after answer".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let first_task = ready_with_dependencies(&config, dependencies.clone()).await;
    let mut first = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "checkpoint-before",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut first, &config, &workspace).await;
    send_request(
        &mut first,
        &config,
        "checkpoint-submit",
        submit_body(
            "checkpoint-command",
            session_id.clone(),
            generation,
            "ask across restart",
        ),
    )
    .await;
    let (run_id, _) = next_submit_response(&mut first).await;
    let (menu_id, request_seq, opening_generation) = loop {
        if let WireFrame::Event { envelope, .. } = first.next().await
            && envelope.run_id.as_ref() == Some(&run_id)
            && let Ok(EventPayload::MenuOpened(menu)) =
                serde_json::from_value::<EventPayload>(envelope.payload.into())
        {
            break (menu.id, envelope.seq, envelope.worker_generation);
        }
    };
    assert_eq!(fake.requests().len(), 1);
    drop(first);
    first_task.crash().await;

    let second_task = ready_with_dependencies(&config, dependencies).await;
    assert_eq!(
        fake.requests().len(),
        1,
        "startup reconstructs the checkpoint without a provider request"
    );
    let mut second = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "checkpoint-after",
        ClientKind::Headless,
    )
    .await;
    let replay_frames = attach_existing(
        &mut second,
        &config,
        session_id.clone(),
        0,
        "checkpoint-replay",
    )
    .await;
    assert!(replay_frames.iter().any(|envelope| {
        envelope.run_id.as_ref() == Some(&run_id)
            && matches!(
                serde_json::from_value::<EventPayload>(envelope.payload.clone().into()),
                Ok(EventPayload::MenuOpened(ref menu)) if menu.id == menu_id
            )
    }));
    second
        .send(
            &WireFrame::MenuAnswer {
                request_id: Some(RequestId::new("checkpoint-answer")),
                command_id: CommandId::new("checkpoint-answer-command"),
                session_id: session_id.clone(),
                menu_id: menu_id.clone(),
                request_seq,
                worker_generation: opening_generation,
                option_key: "continue".into(),
                option_index: 0,
                input: None,
            },
            config.frame_limit,
        )
        .await;
    assert!(matches!(
        next_response(&mut second).await,
        WireFrame::Response {
            body: ResponseBody::MenuAnswer { .. },
            ..
        }
    ));
    second
        .send(
            &WireFrame::MenuAnswer {
                request_id: Some(RequestId::new("checkpoint-loser")),
                command_id: CommandId::new("checkpoint-loser-command"),
                session_id: session_id.clone(),
                menu_id: menu_id.clone(),
                request_seq,
                worker_generation: opening_generation,
                option_key: "continue".into(),
                option_index: 0,
                input: None,
            },
            config.frame_limit,
        )
        .await;
    assert!(matches!(
        next_response(&mut second).await,
        WireFrame::Response {
            body: ResponseBody::Error { code, .. },
            ..
        } if code == ERROR_CODE_ALREADY_RESOLVED
    ));
    let events = events_until_terminal(&mut second, &run_id).await;
    assert!(
        events
            .iter()
            .any(|(_, payload)| *payload == EventPayload::RunState(RunState::Done))
    );
    let durable = read_session(&mut second, &config, session_id, "checkpoint-durable-read").await;
    let resolution = durable
        .iter()
        .find(|envelope| {
            matches!(
                serde_json::from_value::<EventPayload>(envelope.payload.clone().into()),
                Ok(EventPayload::MenuAnswered(answer)) if answer.menu == menu_id
            )
        })
        .expect("durable menu resolution");
    assert!(
        resolution.worker_generation > opening_generation,
        "post-restart resolution is stamped with the current generation"
    );
    assert_eq!(
        payloads_for_run(&durable, &run_id)
            .filter(|payload| matches!(payload, EventPayload::ToolResult { .. }))
            .count(),
        1
    );
    assert_eq!(
        payloads_for_run(&durable, &run_id)
            .filter(|payload| matches!(payload, EventPayload::MenuOpened(_)))
            .count(),
        1,
        "request_input executes once; restart only replays its durable menu"
    );
    let requests = fake.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| message.tool_result_for("restart-choice").is_some())
    );

    second_task.shutdown_handle().request("test complete");
    second_task.join().await.expect("daemon joins");
}

#[derive(Clone)]
struct HoldingEffectFactory {
    calls: Arc<AtomicUsize>,
    effect: EffectId,
}

#[async_trait]
impl TurnToolFactory for HoldingEffectFactory {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "hold_effect".into(),
            description: "Journals dispatch, then holds the result boundary".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }]
    }

    async fn create(
        &self,
        context: WorkerToolContext,
    ) -> Result<Option<Arc<dyn ToolDispatcher>>, HaiderError> {
        Ok(Some(Arc::new(HoldingEffectDispatcher {
            context,
            calls: self.calls.clone(),
            effect: self.effect.clone(),
        })))
    }
}

struct HoldingEffectDispatcher {
    context: WorkerToolContext,
    calls: Arc<AtomicUsize>,
    effect: EffectId,
}

#[async_trait]
impl ToolDispatcher for HoldingEffectDispatcher {
    async fn execute(
        &self,
        _run_id: &RunId,
        _item_id: &haider_protocol::ids::ItemId,
        _call_id: &str,
        name: &str,
        _args: serde_json::Value,
        _cancel: &CancelToken,
    ) -> Result<ToolDispatchResult, HaiderError> {
        if name != "hold_effect" {
            return Err(HaiderError::new(
                ErrorCode::InvalidArgument,
                format!("unexpected test tool {name}"),
                false,
            ));
        }
        let payloads = [
            EventPayload::Effect(EffectPhase::Intent(EffectIntent {
                effect: self.effect.clone(),
                class: EffectClass::Network {
                    host: "example.invalid".into(),
                },
                summary: "ambiguous non-idempotent test effect".into(),
                args_digest: "blake3:w3c-scenario-11".into(),
                workspace_revision: None,
            })),
            EventPayload::Effect(EffectPhase::Authorized {
                effect: self.effect.clone(),
                verdict: AuthorizationVerdict::Allow,
            }),
            EventPayload::Effect(EffectPhase::Dispatched {
                effect: self.effect.clone(),
            }),
        ];
        let mut envelopes = payloads
            .into_iter()
            .enumerate()
            .map(|(index, payload)| EventEnvelope {
                schema_version: SCHEMA_VERSION,
                event_id: EventId::new(format!("scenario-11-effect-{}", index + 1)),
                seq: 0,
                session_id: self.context.store.session_id().clone(),
                branch_id: None,
                run_id: Some(self.context.run_id.clone()),
                agent_id: None,
                device_id: self.context.device_id.clone(),
                authority_epoch: 0,
                worker_generation: self.context.store.worker_generation(),
                causation_id: None,
                correlation_id: None,
                committed_at_ms: 0,
                render: RenderTargets {
                    ui: true,
                    durable: true,
                    prompt: PromptRender::Omit,
                },
                payload: serde_json::to_value(payload)
                    .expect("effect payload")
                    .into(),
            })
            .collect::<Vec<_>>();
        StoreHandle::append(&self.context.store, &mut envelopes).await?;
        self.calls.fetch_add(1, Ordering::SeqCst);
        future::pending::<Result<ToolDispatchResult, HaiderError>>().await
    }
}

/// Scenario 11.
///
/// MUTATION CHECK: omit pre-Ready effect reconciliation, classify RunningTool
/// as resumable, or dispatch a prior-generation effect from recovery.
/// Expected failure: no Unknown outcome is committed or dispatcher/provider
/// call counts exceed one.
#[tokio::test]
async fn scenario_11_held_effect_becomes_unknown_after_restart_and_never_redispatches() {
    let root = test_root("w3c-live-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "restart-held-effect",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (mut dependencies, fake) = fake_dependencies(vec![
        FakeStep::EmitToolCall {
            call_id: "held-call".into(),
            name: "hold_effect".into(),
            args: serde_json::json!({}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
    ]);
    let calls = Arc::new(AtomicUsize::new(0));
    let effect = EffectId::new("held-effect");
    dependencies.tool_factory = Arc::new(HoldingEffectFactory {
        calls: calls.clone(),
        effect: effect.clone(),
    });
    let first_task = ready_with_dependencies(&config, dependencies.clone()).await;
    let mut first = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "effect-before",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut first, &config, &workspace).await;
    send_request(
        &mut first,
        &config,
        "effect-submit",
        submit_body(
            "effect-command",
            session_id.clone(),
            generation,
            "dispatch once",
        ),
    )
    .await;
    let (run_id, _) = next_submit_response(&mut first).await;
    tokio::time::timeout(support::DEADLINE, async {
        while calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("effect crossed dispatch boundary");
    drop(first);
    first_task.crash().await;

    let second_task = ready_with_dependencies(&config, dependencies).await;
    let mut second = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "effect-after",
        ClientKind::Headless,
    )
    .await;
    attach_existing(&mut second, &config, session_id.clone(), 0, "effect-replay").await;
    let envelopes = read_session(&mut second, &config, session_id, "effect-read-terminal").await;
    assert_eq!(
        payloads_for_run(&envelopes, &run_id)
            .filter(|payload| matches!(
                payload,
                EventPayload::Effect(EffectPhase::Outcome {
                    effect: found,
                    outcome: EffectOutcome::Unknown,
                    ..
                }) if *found == effect
            ))
            .count(),
        1
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(fake.requests().len(), 1);

    second_task.shutdown_handle().request("test complete");
    second_task.join().await.expect("daemon joins");
}

/// Scenario 12.
///
/// MUTATION CHECK: push normalized reasoning into assistant follow-up blocks,
/// treat request usage updates as logical-turn deltas, or omit RunFailed.
/// Expected failure: request two contains Reasoning, cumulative usage is
/// wrong, or Errored is not immediately preceded by one RunFailed.
#[tokio::test]
async fn scenario_12_reasoning_safe_follow_up_cumulative_usage_and_durable_failure() {
    let root = test_root("w3c-live-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    fs::write(workspace.join("note.txt"), "tool output").expect("fixture file");
    let config = DaemonConfig::new(
        "reasoning-usage",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let first_usage = Usage {
        input: 10,
        output: 4,
        reasoning: 3,
        cached: 2,
        source: UsageSource::ProviderReported,
        account: None,
        accounts: Vec::new(),
        normalized: None,
        scope: None,
        cache_cost: None,
        request: None,
    };
    let second_usage = Usage {
        input: 6,
        output: 2,
        reasoning: 1,
        cached: 1,
        source: UsageSource::ProviderReported,
        account: None,
        accounts: Vec::new(),
        normalized: None,
        scope: None,
        cache_cost: None,
        request: None,
    };
    let expected_cumulative = Usage {
        input: 16,
        output: 6,
        reasoning: 4,
        cached: 3,
        source: UsageSource::ProviderReported,
        account: None,
        accounts: Vec::new(),
        normalized: None,
        scope: None,
        cache_cost: None,
        request: Some(expected_request_usage(2, &second_usage)),
    };
    let (dependencies, fake) = fake_dependencies(vec![
        FakeStep::EmitReasoning {
            text: "private normalized thought".into(),
        },
        FakeStep::EmitToolCall {
            call_id: "read-1".into(),
            name: "fs_read".into(),
            args: serde_json::json!({"path": "note.txt"}),
        },
        FakeStep::EmitUsage { usage: first_usage },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "read-1".into(),
        },
        FakeStep::EmitUsage {
            usage: second_usage,
        },
        FakeStep::MalformedFrame,
    ]);
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "reasoning-client",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut client, &config, &workspace).await;
    send_request(
        &mut client,
        &config,
        "submit",
        submit_body("reasoning-submit", session_id, generation, "read note"),
    )
    .await;
    let (run_id, _) = next_submit_response(&mut client).await;
    let events = events_until_terminal(&mut client, &run_id).await;
    let requests = fake.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].messages.iter().all(|message| {
        message
            .blocks
            .iter()
            .all(|block| !matches!(block, haider_protocol::provider::Block::Reasoning { .. }))
    }));
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| message.tool_result_for("read-1").is_some())
    );
    let usages = events
        .iter()
        .filter_map(|(_, payload)| match payload {
            EventPayload::Usage(usage) => Some(usage),
            _ => None,
        })
        .collect::<Vec<_>>();
    // CM1 enriches the journaled Usage with a measurement `scope` and the
    // request-local cache diagnostic; pin the accounting and request identity
    // with only those daemon-owned measurements normalized out.
    assert_eq!(
        usages.last().map(|u| {
            let mut u = (*u).clone();
            u.scope = None;
            if let Some(request) = &mut u.request {
                request.cache = None;
            }
            u
        }),
        Some(expected_cumulative.clone())
    );
    let failures = events
        .iter()
        .filter_map(|(_, payload)| match payload {
            EventPayload::RunFailed {
                code,
                message,
                retryable,
                ..
            } => Some((code, message, retryable)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(failures.len(), 1);
    let (failure_code, failure_message, failure_retryable) = failures[0];
    assert_eq!(*failure_code, ErrorCode::ProviderError);
    assert!(!*failure_retryable);
    assert!(failure_message.len() <= 512);
    assert!(
        failure_message
            .chars()
            .all(|character| !character.is_control() || character == '\n')
    );
    let failed = events
        .iter()
        .position(|(_, payload)| matches!(payload, EventPayload::RunFailed { .. }))
        .expect("RunFailed");
    let errored = events
        .iter()
        .position(|(_, payload)| *payload == EventPayload::RunState(RunState::Errored))
        .expect("Errored");
    assert_eq!(errored, failed + 1);

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// Scenario 2 (M2 prefix).
///
/// MUTATION CHECK: capture the attach head outside the actor's `Register`
/// step (actor.rs). Expected failure: `AttachCaughtUp` reports a stale
/// high-water instead of 1, or the `Created` event misses the replay.
/// (Publishing `Created` before its transaction commits is NOT
/// deterministically observable over this wire ordering — the atomicity of
/// metadata + `Created` + receipt is pinned at the store seam by
/// `haider-store/tests/session_create_tests.rs`.)
#[tokio::test]
async fn scenario_2_real_uds_creates_attaches_and_replays_typed_session() {
    let root = test_root("w3c-live-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "live-create",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    // DIRECTED CHANGE (W3c2 D3-5): the production wire path no longer
    // accepts "fake" — the creatable-provider whitelist comes from the
    // dependency configuration, so this create/attach/replay scenario boots
    // with the injected fake configuration like every turn scenario. The
    // production-path rejection is pinned separately by
    // `production_wire_path_never_accepts_the_fake_provider`.
    let (dependencies, _fake) = fake_dependencies(Vec::new());
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "control-1",
        ClientKind::Headless,
    )
    .await;

    send_request(
        &mut client,
        &config,
        "create",
        create_body("create-command", workspace.to_string_lossy().into_owned()),
    )
    .await;
    let (session_id, metadata) = created_response(client.next_reply().await);
    assert_eq!(
        metadata.cwd,
        fs::canonicalize(&workspace)
            .expect("canonical workspace")
            .to_str()
            .expect("UTF-8")
    );

    send_request(
        &mut client,
        &config,
        "attach",
        RequestBody::SessionAttach {
            session_id: session_id.clone(),
            after_seq: 0,
            mode: AttachMode::Control,
            sealed_replay: false,
        },
    )
    .await;
    assert!(matches!(
        client.next_reply().await,
        WireFrame::Response {
            body: ResponseBody::SessionAttach { .. },
            ..
        }
    ));
    let created = match client.next().await {
        WireFrame::Event { envelope, .. } => envelope,
        other => panic!("expected Created event, got {other:?}"),
    };
    assert_eq!(created.seq, 1);
    assert_eq!(
        serde_json::from_value::<EventPayload>(created.payload.clone().into())
            .expect("typed payload"),
        EventPayload::SessionState(SessionState::Created)
    );
    assert!(matches!(
        client.next().await,
        WireFrame::AttachCaughtUp {
            high_water_seq: 1,
            ..
        }
    ));

    send_request(
        &mut client,
        &config,
        "list",
        RequestBody::SessionList {
            cursor: None,
            limit: 10,
            order: Default::default(),
        },
    )
    .await;
    let summary = match client.next_reply().await {
        WireFrame::Response {
            body: ResponseBody::SessionList { sessions, .. },
            ..
        } => sessions.into_iter().next().expect("one session"),
        other => panic!("expected list response, got {other:?}"),
    };
    assert_eq!(
        summary,
        SessionSummary {
            session_id: session_id.clone(),
            head_seq: 1,
            worker_generation: created.worker_generation,
            run_state: Some(haider_rpc::ObserveRunStateWire::Idle),
            run_id: None,
            seen_at_ms: None,
            // Session-list recency is durable from creation onward; a fresh
            // session's activity coordinate is its creation timestamp.
            last_activity_ms: Some(metadata.created_at_ms),
            waiting_why: None,
            needs_input: None,
            metadata: Some(metadata.clone()),
            provider: Some(metadata.provider.clone()),
            workspace_cwd: Some(metadata.cwd.clone()),
            // A just-created session is truly empty: zero committed user
            // turns, so exactly-zero tokens is honest roster truth.
            turn_count: Some(0),
            footprint_tokens: Some(0),
            footprint_truth: Some(ContextFootprintTruth::Exact),
            title: None,
            agent_metrics: None,
            // Model truth: no model_selected fact yet, so the fold falls
            // back to the create-time metadata model.
            last_model: Some(metadata.model.clone()),
            cache_lifetime_hit_basis_points: None,
            cache_reread_hit_basis_points: None,
            parent_session_id: None,
            // Lineage truth (session_lineage_v1): a session no delegation
            // names is a Root — the live daemon reports it typed.
            kind: Some(haider_rpc::SessionKindWire::Root),
            agent_type: None,
            effort: metadata.effort.clone(),
            fast: Some(metadata.fast),
            account_alias: None,
            forked_from: None,
        }
    );

    send_request(
        &mut client,
        &config,
        "read",
        RequestBody::SessionRead {
            session_id,
            range: SeqRange {
                start_seq: 1,
                end_seq: 1,
            },
        },
    )
    .await;
    assert!(matches!(
        client.next_reply().await,
        WireFrame::Response {
            body: ResponseBody::SessionRead { result },
            ..
        } if result.metadata == Some(metadata) && result.envelopes.len() == 1
    ));

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// Scenario 2 satellite — the R2 receipt-idempotency law on the wire: a
/// lost `session.create` response is recoverable by same-command retry, and
/// a same-command different-body reuse is rejected.
///
/// MUTATION CHECK: in `session_create` (session_hub/rpc.rs), move
/// `validate_workspace` before the `session_create_receipt` preflight, or
/// drop the digest comparison in the store's receipt lookup. Expected
/// failure: the retry after workspace removal fails, or the changed-body
/// reuse is accepted.
#[tokio::test]
async fn session_create_lost_response_retry_survives_removed_cwd_and_rejects_changed_body() {
    let root = test_root("w3c-live-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let alternate = root.path().join("alternate");
    fs::create_dir(&alternate).expect("alternate workspace");
    let config = DaemonConfig::new(
        "create-idempotency",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    // DIRECTED CHANGE (W3c2 D3-5): "fake" is no longer creatable on the
    // production wire path; this receipt-idempotency scenario boots with
    // the injected fake configuration (the production rejection is pinned
    // by `production_wire_path_never_accepts_the_fake_provider`).
    let (dependencies, _fake) = fake_dependencies(Vec::new());
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut submitter = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "submitter",
        ClientKind::Headless,
    )
    .await;
    let mut observer = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "observer",
        ClientKind::Headless,
    )
    .await;
    let original = workspace.to_string_lossy().into_owned();

    send_request(
        &mut submitter,
        &config,
        "lost-response",
        create_body("same-command", original.clone()),
    )
    .await;
    // Observe durable truth from another connection, then drop the first
    // connection without ever reading its response.
    let first_session = loop {
        send_request(
            &mut observer,
            &config,
            "observe-list",
            RequestBody::SessionList {
                cursor: None,
                limit: 10,
                order: Default::default(),
            },
        )
        .await;
        match observer.next_reply().await {
            WireFrame::Response {
                body: ResponseBody::SessionList { sessions, .. },
                ..
            } if !sessions.is_empty() => break sessions[0].session_id.clone(),
            WireFrame::Response {
                body: ResponseBody::SessionList { .. },
                ..
            } => tokio::task::yield_now().await,
            other => panic!("unexpected observer frame: {other:?}"),
        }
    };
    drop(submitter);
    fs::remove_dir(&workspace).expect("remove committed workspace");

    send_request(
        &mut observer,
        &config,
        "retry",
        create_body("same-command", original),
    )
    .await;
    let (retried_session, _) = created_response(observer.next_reply().await);
    assert_eq!(retried_session, first_session);

    send_request(
        &mut observer,
        &config,
        "changed",
        create_body("same-command", alternate.to_string_lossy().into_owned()),
    )
    .await;
    assert!(matches!(
        observer.next_reply().await,
        WireFrame::Response {
            body: ResponseBody::Error { ref code, .. },
            ..
        } if code == ERROR_CODE_INVALID_ARGUMENT
    ));

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// W9b additive create law: overrides are receipt-bound durable metadata.
///
/// MUTATION CHECK: omit `permission_overrides` from the canonical request,
/// persisted metadata, or response. Expected RUNTIME failure: the same-command
/// changed-flags request replays instead of conflicting, or list/reopen loses
/// the exact override values.
#[tokio::test]
async fn session_create_permission_overrides_are_digest_bound_and_persisted() {
    let root = test_root("w9b-create-overrides-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "create-overrides",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (dependencies, _fake) = fake_dependencies(Vec::new());
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w9b-test",
        "override-client",
        ClientKind::Headless,
    )
    .await;
    let expected = SessionPermissionOverridesV1 {
        read_only: false,
        allow_writes: true,
        allow_exec: false,
        allow_mobile: false,
        auto_allow: false,
    };
    let body = |overrides| RequestBody::SessionCreateWithPermissionOverrides {
        command_id: CommandId::new("override-command"),
        cwd: workspace.to_string_lossy().into_owned(),
        provider: "fake".into(),
        model: "fake-v1".into(),
        max_tokens: 4096,
        permission_overrides: overrides,
        cache_policy: None,
        interaction_mode: haider_protocol::session::SessionInteractionModeV1::Interactive,
        ssh_scope: None,
        account_alias: None,
        resolve_provider: false,
        resolve_model: false,
        effort: None,
        fast: None,
    };

    send_request(
        &mut client,
        &config,
        "create-overrides",
        body(Some(expected)),
    )
    .await;
    let (session_id, metadata) = created_response(client.next_reply().await);
    assert_eq!(metadata.permission_overrides, Some(expected));

    send_request(
        &mut client,
        &config,
        "retry-overrides",
        body(Some(expected)),
    )
    .await;
    let (retried, retried_metadata) = created_response(client.next_reply().await);
    assert_eq!(retried, session_id);
    assert_eq!(retried_metadata.permission_overrides, Some(expected));

    send_request(&mut client, &config, "changed-overrides", body(None)).await;
    assert!(matches!(
        client.next_reply().await,
        WireFrame::Response {
            body: ResponseBody::Error { ref code, .. },
            ..
        } if code == ERROR_CODE_INVALID_ARGUMENT
    ));

    send_request(
        &mut client,
        &config,
        "list-overrides",
        RequestBody::SessionList {
            cursor: None,
            limit: 10,
            order: Default::default(),
        },
    )
    .await;
    assert!(matches!(
        client.next_reply().await,
        WireFrame::Response {
            body: ResponseBody::SessionList { sessions, .. },
            ..
        } if sessions.iter().any(|session| {
            session.session_id == session_id
                && session.metadata.as_ref().is_some_and(|metadata| {
                    metadata.permission_overrides == Some(expected)
                })
        })
    ));

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// Scenario 2 satellite — the R7 capability and feature-advertisement law:
/// `session.create` requires Control, and the ready `Welcome` advertises
/// exactly the additive methods this daemon implements.
///
/// MUTATION CHECK: authorize `session.create` as View or advertise features
/// without implementing the receipt-backed method. Expected failure: the
/// View-only client creates a session, or the ready Welcome lacks the feature.
#[tokio::test]
async fn session_create_requires_control_and_ready_welcome_advertises_implemented_feature() {
    let root = test_root("w3c-live-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "create-capability",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    // DIRECTED CHANGE (W3c2 D3-5): same injected-configuration boot as the
    // other fake-provider scenarios; capability policy under test is
    // provider-independent.
    let (dependencies, _fake) = fake_dependencies(Vec::new());
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut viewer = UdsClient::connect_with_capabilities(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "viewer",
        ClientKind::Headless,
        CapabilitySet::from([Capability::View]),
    )
    .await;
    // The shared handshake consumed Welcome, so reconnect raw to inspect the
    // advertised feature set explicitly.
    let mut feature_client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "feature-client",
        ClientKind::Headless,
    )
    .await;
    send_request(
        &mut viewer,
        &config,
        "denied-create",
        create_body("viewer-command", workspace.to_string_lossy().into_owned()),
    )
    .await;
    assert!(matches!(
        viewer.next_reply().await,
        WireFrame::Response {
            body: ResponseBody::Error { ref code, .. },
            ..
        } if code == ERROR_CODE_CAPABILITY_DENIED
    ));

    send_request(
        &mut feature_client,
        &config,
        "list",
        RequestBody::SessionList {
            cursor: None,
            limit: 1,
            order: Default::default(),
        },
    )
    .await;
    assert!(matches!(
        feature_client.next_reply().await,
        WireFrame::Response {
            body: ResponseBody::SessionList { ref sessions, .. },
            ..
        } if sessions.is_empty()
    ));
    send_request(
        &mut feature_client,
        &config,
        "control-create",
        create_body(
            "control-create-command",
            workspace.to_string_lossy().into_owned(),
        ),
    )
    .await;
    let (session_id, generation) = match feature_client.next_reply().await {
        WireFrame::Response {
            body:
                ResponseBody::SessionCreate {
                    session_id,
                    worker_generation,
                    ..
                },
            ..
        } => (session_id, worker_generation),
        other => panic!("expected control create, got {other:?}"),
    };
    send_request(
        &mut viewer,
        &config,
        "denied-submit",
        submit_body(
            "viewer-submit-command",
            session_id.clone(),
            generation,
            "must not submit",
        ),
    )
    .await;
    assert!(matches!(
        viewer.next_reply().await,
        WireFrame::Response {
            body: ResponseBody::Error { ref code, .. },
            ..
        } if code == ERROR_CODE_CAPABILITY_DENIED
    ));
    send_request(
        &mut viewer,
        &config,
        "denied-cancel",
        RequestBody::TurnCancel {
            command_id: CommandId::new("viewer-cancel-command"),
            session_id,
            worker_generation: generation,
            run_id: RunId::new("not-visible-to-viewer"),
        },
    )
    .await;
    assert!(matches!(
        viewer.next_reply().await,
        WireFrame::Response {
            body: ResponseBody::Error { ref code, .. },
            ..
        } if code == ERROR_CODE_CAPABILITY_DENIED
    ));

    // Inspect a fresh raw handshake because connect_control intentionally
    // consumes Welcome.
    let mut raw = UdsClient::connect(&config.endpoint_path(), config.frame_limit)
        .await
        .expect("raw connect");
    raw.send(
        &WireFrame::Hello(haider_rpc::Hello {
            protocol_min: haider_rpc::WIRE_PROTOCOL_VERSION,
            protocol_max: haider_rpc::WIRE_PROTOCOL_VERSION,
            client_name: "feature-inspector".into(),
            client_version: "test".into(),
            client_instance_id: "feature-inspector".into(),
            client_kind: ClientKind::Headless,
            capabilities_requested: CapabilitySet::from([Capability::View]),
            max_receive_frame: u32::try_from(config.frame_limit).expect("frame limit"),
            encodings: Vec::new(),
        }),
        config.frame_limit,
    )
    .await;
    assert!(matches!(
        raw.next().await,
        WireFrame::Welcome(haider_rpc::Welcome { features, .. })
            if features.contains(FEATURE_SESSION_MUTATION_V1)
                && features.contains(FEATURE_SESSION_PERMISSION_OVERRIDES_V1)
                && features.contains(FEATURE_TURN_CONTROL_V1)
    ));

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// W4a2 P0 approval-bypass and command-shape sentinel plus live acceptance.
///
/// MUTATION CHECKS:
/// - pre-authorize `ProcessExec` or move spawn before `EffectBroker::begin`:
///   a marker appears before the real menu CAS commits;
/// - make the session grant class-wide or ignore its command-shape digest:
///   `different.log` appears or the different command does not re-prompt.
/// Both mutations are expected to fail this test and were verified by revert
/// in W4a2.
#[tokio::test]
async fn w4a2_exec_is_cas_gated_streams_output_and_grants_only_the_exact_shape() {
    #[cfg(windows)]
    let _windows_process_test = windows_real_process_test_guard(
        "w4a2_exec_is_cas_gated_streams_output_and_grants_only_the_exact_shape",
    )
    .await;
    let root = test_root("w4a2-exec-approval-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let denied_path = workspace.join("denied.log");
    let runs_path = workspace.join("runs.log");
    let different_path = workspace.join("different.log");
    let denied_command = denied_exec_command();
    let approved_command = approved_exec_command();
    let different_command = different_exec_command();
    let config = DaemonConfig::new(
        "w4a2-exec-approval",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (dependencies, fake) = fake_dependencies(vec![
        FakeStep::EmitToolCall {
            call_id: "exec-denied".into(),
            name: "exec".into(),
            args: serde_json::json!({"command": denied_command.clone()}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "exec-denied".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::EmitToolCall {
            call_id: "exec-approved".into(),
            name: "exec".into(),
            args: serde_json::json!({"command": approved_command.clone()}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "exec-approved".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::EmitToolCall {
            call_id: "exec-same-shape".into(),
            name: "exec".into(),
            args: serde_json::json!({"command": approved_command.clone()}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "exec-same-shape".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::EmitToolCall {
            call_id: "exec-different-shape".into(),
            name: "exec".into(),
            args: serde_json::json!({"command": different_command.clone()}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "exec-different-shape".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let first_task = ready_with_dependencies(&config, dependencies.clone()).await;
    let mut submitter = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w4a2-test",
        "exec-submitter",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut submitter, &config, &workspace).await;

    send_request(
        &mut submitter,
        &config,
        "exec-deny-submit",
        submit_body(
            "exec-deny-command",
            session_id.clone(),
            generation,
            "try a denied command",
        ),
    )
    .await;
    let (denied_run, _) = next_submit_response(&mut submitter).await;
    let (deny_menu, deny_seq, deny_generation) =
        next_permission_menu_before_create(&mut submitter, &denied_path).await;
    assert!(
        deny_menu
            .body
            .iter()
            .any(|line| line.contains(&denied_command)),
        "the approval menu must show the exact command"
    );
    assert!(
        deny_menu
            .body
            .iter()
            .any(|line| line.contains("exact command shape")),
        "the menu must state the narrow session scope"
    );
    let mut answerer = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w4a2-test",
        "exec-answerer",
        ClientKind::Headless,
    )
    .await;
    attach_existing(
        &mut answerer,
        &config,
        session_id.clone(),
        deny_seq,
        "exec-answerer-attach",
    )
    .await;
    answer_menu(
        &mut answerer,
        &config,
        "exec-deny-answer",
        "exec-deny-answer-command",
        session_id.clone(),
        deny_menu.id,
        deny_seq,
        deny_generation,
        "deny",
        2,
    )
    .await;
    let denied_events = events_until_terminal(&mut submitter, &denied_run).await;
    assert!(!denied_path.exists(), "deny must spawn no process mutation");
    let denied_preview = denied_events
        .iter()
        .find_map(|(_, payload)| match payload {
            EventPayload::ToolResult { call_id, result } if call_id == "exec-denied" => {
                Some(&result.preview)
            }
            _ => None,
        })
        .expect("typed denied exec result");
    let denied_json: serde_json::Value = serde_json::from_str(denied_preview).expect("denied JSON");
    assert_eq!(denied_json["status"], "denied");

    send_request(
        &mut submitter,
        &config,
        "exec-approve-submit",
        submit_body(
            "exec-approve-command",
            session_id.clone(),
            generation,
            "approve this exact command shape",
        ),
    )
    .await;
    let (approved_run, _) = next_submit_response(&mut submitter).await;
    let (approve_menu, approve_seq, approve_generation) =
        next_permission_menu_before_create(&mut submitter, &runs_path).await;
    answer_menu(
        &mut answerer,
        &config,
        "exec-approve-answer",
        "exec-approve-answer-command",
        session_id.clone(),
        approve_menu.id,
        approve_seq,
        approve_generation,
        "approve_for_session",
        1,
    )
    .await;
    let approved_events = events_until_terminal(&mut submitter, &approved_run).await;
    assert_eq!(
        fs::read_to_string(&runs_path).expect("approved command ran"),
        "run\n"
    );
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    for (_, payload) in &approved_events {
        if let EventPayload::Item(ItemEvent::Delta {
            delta: ItemDelta::CommandOutput { stream, chunk_b64 },
            ..
        }) = payload
        {
            let bytes = BASE64.decode(chunk_b64).expect("command output base64");
            match stream {
                OutputStream::Stdout => stdout.extend(bytes),
                OutputStream::Stderr => stderr.extend(bytes),
            }
        }
    }
    assert_eq!(stdout, b"stdout-ok");
    assert_eq!(stderr, b"stderr-ok");
    let approved_preview = approved_events
        .iter()
        .find_map(|(_, payload)| match payload {
            EventPayload::ToolResult { call_id, result } if call_id == "exec-approved" => {
                Some(&result.preview)
            }
            _ => None,
        })
        .expect("exec tool result");
    let approved_json: serde_json::Value =
        serde_json::from_str(approved_preview).expect("exec result JSON");
    assert_eq!(approved_json["status"], "completed");
    assert_eq!(approved_json["exit_code"], 0);
    assert_eq!(approved_json["limits"]["max_output_bytes"], 1024 * 1024);
    assert_eq!(approved_json["limits"]["wall_timeout_ms"], 60_000);

    drop(answerer);
    drop(submitter);
    first_task
        .shutdown_handle()
        .request("restart command-shape durability test");
    first_task.join().await.expect("first daemon joins");

    let second_task = ready_with_dependencies(&config, dependencies).await;
    let mut restarted = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w4a2-test",
        "exec-restarted",
        ClientKind::Headless,
    )
    .await;
    attach_existing(
        &mut restarted,
        &config,
        session_id.clone(),
        0,
        "exec-restart-attach",
    )
    .await;
    let restarted_generation =
        current_generation(&mut restarted, &config, &session_id, "exec-restart-list").await;
    send_request(
        &mut restarted,
        &config,
        "exec-same-submit",
        submit_body(
            "exec-same-command",
            session_id.clone(),
            restarted_generation,
            "run the approved shape again",
        ),
    )
    .await;
    let (same_run, _) = next_submit_response(&mut restarted).await;
    let same_events = events_until_terminal(&mut restarted, &same_run).await;
    assert!(
        same_events
            .iter()
            .all(|(_, payload)| !matches!(payload, EventPayload::MenuOpened(_))),
        "the same durable command shape must not re-prompt after restart"
    );
    assert_eq!(
        fs::read_to_string(&runs_path).expect("same shape ran"),
        "run\nrun\n"
    );

    send_request(
        &mut restarted,
        &config,
        "exec-different-submit",
        submit_body(
            "exec-different-command",
            session_id.clone(),
            restarted_generation,
            "try a different command shape",
        ),
    )
    .await;
    let (different_run, _) = next_submit_response(&mut restarted).await;
    let (different_menu, different_seq, different_generation) =
        next_permission_menu_before_create(&mut restarted, &different_path).await;
    assert!(
        different_menu
            .body
            .iter()
            .any(|line| line.contains(&different_command))
    );
    answer_menu(
        &mut restarted,
        &config,
        "exec-different-deny",
        "exec-different-deny-command",
        session_id,
        different_menu.id,
        different_seq,
        different_generation,
        "deny",
        2,
    )
    .await;
    let different_events = events_until_terminal(&mut restarted, &different_run).await;
    assert!(
        !different_path.exists(),
        "a different command shape must not inherit the session grant"
    );
    assert!(matches!(
        different_events.last(),
        Some((_, EventPayload::RunState(RunState::Done)))
    ));
    assert_eq!(fake.requests().len(), 8);

    second_task.shutdown_handle().request("test complete");
    second_task.join().await.expect("second daemon joins");
}

/// W4a1 P0 approval-bypass sentinel.
///
/// MUTATION CHECK: bypass the `ApprovalRequired` branch in
/// `HarnessActor::execute_general_tool`, or pre-authorize FsWrite in the
/// production policy. Expected failure: `denied.txt` appears before the
/// committed CAS answer, denial mutates it, or the first approved write lands
/// before its menu is answered. Verified by revert in W4a1.
#[tokio::test]
async fn w4a1_fs_write_requires_committed_cas_and_session_grant_survives_restart() {
    let root = test_root("w4a1-write-approval-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let denied_path = workspace.join("denied.txt");
    let approved_path = workspace.join("approved.txt");
    let inherited_path = workspace.join("inherited.txt");
    let config = DaemonConfig::new(
        "w4a1-write-approval",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (dependencies, fake) = fake_dependencies(vec![
        FakeStep::EmitToolCall {
            call_id: "write-denied".into(),
            name: "fs_write".into(),
            args: serde_json::json!({"path": "denied.txt", "content": "must not land"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "write-denied".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::EmitToolCall {
            call_id: "write-approved".into(),
            name: "fs_write".into(),
            args: serde_json::json!({"path": "approved.txt", "content": "approved once"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "write-approved".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::EmitToolCall {
            call_id: "write-inherited".into(),
            name: "fs_write".into(),
            args: serde_json::json!({"path": "inherited.txt", "content": "session grant"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "write-inherited".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let first_task = ready_with_dependencies(&config, dependencies.clone()).await;
    let mut submitter = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w4a1-test",
        "write-submitter",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut submitter, &config, &workspace).await;

    send_request(
        &mut submitter,
        &config,
        "deny-submit",
        submit_body(
            "deny-command",
            session_id.clone(),
            generation,
            "try a denied write",
        ),
    )
    .await;
    let (denied_run, _) = next_submit_response(&mut submitter).await;
    let (deny_menu, deny_seq, deny_generation) =
        next_permission_menu_before_create(&mut submitter, &denied_path).await;
    assert!(
        !denied_path.exists(),
        "write dispatched before committed approval"
    );
    assert!(
        deny_menu
            .body
            .iter()
            .any(|line| line.contains("denied.txt"))
    );
    let mut answerer = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w4a1-test",
        "write-answerer",
        ClientKind::Headless,
    )
    .await;
    attach_existing(
        &mut answerer,
        &config,
        session_id.clone(),
        deny_seq,
        "answerer-attach",
    )
    .await;
    answer_menu(
        &mut answerer,
        &config,
        "deny-answer",
        "deny-answer-command",
        session_id.clone(),
        deny_menu.id,
        deny_seq,
        deny_generation,
        "deny",
        2,
    )
    .await;
    let denied_events = events_until_terminal(&mut submitter, &denied_run).await;
    assert!(!denied_path.exists(), "denied write mutated the workspace");
    let denied_result = denied_events.iter().find_map(|(_, payload)| match payload {
        EventPayload::ToolResult { call_id, result } if call_id == "write-denied" => {
            Some(result.preview.clone())
        }
        _ => None,
    });
    let denied_json: serde_json::Value =
        serde_json::from_str(&denied_result.expect("typed denied tool result"))
            .expect("denied result JSON");
    assert_eq!(denied_json["status"], "denied");

    send_request(
        &mut submitter,
        &config,
        "approve-submit",
        submit_body(
            "approve-command",
            session_id.clone(),
            generation,
            "approve writes for this session",
        ),
    )
    .await;
    let (approved_run, _) = next_submit_response(&mut submitter).await;
    let (approve_menu, approve_seq, approve_generation) =
        next_permission_menu_before_create(&mut submitter, &approved_path).await;
    assert!(
        !approved_path.exists(),
        "approved write landed before the CAS answer"
    );
    answer_menu(
        &mut answerer,
        &config,
        "approve-answer",
        "approve-answer-command",
        session_id.clone(),
        approve_menu.id,
        approve_seq,
        approve_generation,
        "approve_for_session",
        1,
    )
    .await;
    let approved_events = events_until_terminal(&mut submitter, &approved_run).await;
    assert!(matches!(
        approved_events.last(),
        Some((_, EventPayload::RunState(RunState::Done)))
    ));
    assert_eq!(
        fs::read_to_string(&approved_path).expect("approved file"),
        "approved once"
    );

    drop(answerer);
    drop(submitter);
    first_task
        .shutdown_handle()
        .request("restart durability test");
    first_task.join().await.expect("first daemon joins");

    let second_task = ready_with_dependencies(&config, dependencies).await;
    let mut restarted = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w4a1-test",
        "write-restarted",
        ClientKind::Headless,
    )
    .await;
    attach_existing(
        &mut restarted,
        &config,
        session_id.clone(),
        0,
        "restart-attach",
    )
    .await;
    let restarted_generation =
        current_generation(&mut restarted, &config, &session_id, "restart-list").await;
    send_request(
        &mut restarted,
        &config,
        "inherited-submit",
        submit_body(
            "inherited-command",
            session_id,
            restarted_generation,
            "use the durable session write grant",
        ),
    )
    .await;
    let (inherited_run, _) = next_submit_response(&mut restarted).await;
    let inherited_events = events_until_terminal(&mut restarted, &inherited_run).await;
    assert!(
        inherited_events
            .iter()
            .all(|(_, payload)| !matches!(payload, EventPayload::MenuOpened(_))),
        "approve-for-session must not re-prompt after restart"
    );
    assert_eq!(
        fs::read_to_string(&inherited_path).expect("inherited write"),
        "session grant"
    );
    assert_eq!(fake.requests().len(), 6);

    second_task.shutdown_handle().request("test complete");
    second_task.join().await.expect("second daemon joins");
}

/// W4a1 live acceptance: the production dispatcher applies the model's
/// structured patch only after a second control attachment wins the real CAS.
#[tokio::test]
async fn w4a1_real_fs_edit_round_trips_approval_and_returns_tool_result() {
    let root = test_root("w4a1-patch-approval-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let target = workspace.join("note.txt");
    fs::write(&target, "before\n").expect("seed target");
    let config = DaemonConfig::new(
        "w4a1-patch-approval",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (dependencies, fake) = fake_dependencies(vec![
        FakeStep::EmitToolCall {
            call_id: "read-note".into(),
            name: "fs_read".into(),
            args: serde_json::json!({"path": "note.txt"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "read-note".into(),
        },
        FakeStep::EmitToolCall {
            call_id: "edit-note".into(),
            name: "fs_edit".into(),
            args: serde_json::json!({
                "path": "note.txt",
                "edits": [{"old": "before", "new": "after"}]
            }),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "edit-note".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut submitter = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w4a1-test",
        "patch-submitter",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut submitter, &config, &workspace).await;
    send_request(
        &mut submitter,
        &config,
        "patch-submit",
        submit_body(
            "patch-command",
            session_id.clone(),
            generation,
            "patch note.txt",
        ),
    )
    .await;
    let (run_id, _) = next_submit_response(&mut submitter).await;
    let (menu, request_seq, opening_generation) = next_permission_menu(&mut submitter).await;
    assert_eq!(
        fs::read_to_string(&target).expect("pre-approval read"),
        "before\n"
    );
    assert!(menu.body.iter().any(|line| line.contains("note.txt")));
    assert!(
        menu.body
            .iter()
            .any(|line| line.contains("Edit 1:") && line.contains("occurrence"))
    );

    let mut answerer = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w4a1-test",
        "patch-answerer",
        ClientKind::Headless,
    )
    .await;
    attach_existing(
        &mut answerer,
        &config,
        session_id.clone(),
        request_seq,
        "patch-answer-attach",
    )
    .await;
    answer_menu(
        &mut answerer,
        &config,
        "patch-answer",
        "patch-answer-command",
        session_id,
        menu.id,
        request_seq,
        opening_generation,
        "approve_once",
        0,
    )
    .await;
    let events = events_until_terminal(&mut submitter, &run_id).await;
    assert!(matches!(
        events.last(),
        Some((_, EventPayload::RunState(RunState::Done)))
    ));
    assert_eq!(
        fs::read_to_string(&target).expect("patched target"),
        "after\n"
    );
    assert_eq!(
        events
            .iter()
            .filter(|(_, payload)| matches!(payload, EventPayload::ToolResult { .. }))
            .count(),
        1
    );
    assert_eq!(fake.requests().len(), 3);

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

#[tokio::test]
async fn w4a1_pending_patch_approval_restarts_on_the_original_menu_cas() {
    let root = test_root("w4a1-patch-menu-restart-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let target = workspace.join("pending.txt");
    fs::write(&target, "before").expect("seed target");
    let config = DaemonConfig::new(
        "w4a1-patch-menu-restart",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (dependencies, fake) = fake_dependencies(vec![
        FakeStep::EmitToolCall {
            call_id: "read-pending".into(),
            name: "fs_read".into(),
            args: serde_json::json!({"path": "pending.txt"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "read-pending".into(),
        },
        FakeStep::EmitToolCall {
            call_id: "pending-edit".into(),
            name: "fs_edit".into(),
            args: serde_json::json!({
                "path": "pending.txt",
                "edits": [{"old": "before", "new": "after"}]
            }),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "pending-edit".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let first_task = ready_with_dependencies(&config, dependencies.clone()).await;
    let mut first = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w4a1-test",
        "pending-before",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut first, &config, &workspace).await;
    send_request(
        &mut first,
        &config,
        "pending-submit",
        submit_body(
            "pending-command",
            session_id.clone(),
            generation,
            "open patch approval",
        ),
    )
    .await;
    let (run_id, _) = next_submit_response(&mut first).await;
    let (menu, request_seq, opening_generation) = next_permission_menu(&mut first).await;
    assert_eq!(
        fs::read_to_string(&target).expect("pending target"),
        "before"
    );
    drop(first);
    first_task.crash().await;

    let second_task = ready_with_dependencies(&config, dependencies).await;
    let mut second = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w4a1-test",
        "pending-after",
        ClientKind::Headless,
    )
    .await;
    attach_existing(
        &mut second,
        &config,
        session_id.clone(),
        0,
        "pending-replay",
    )
    .await;
    assert_eq!(
        fs::read_to_string(&target).expect("pre-answer target"),
        "before"
    );
    answer_menu(
        &mut second,
        &config,
        "pending-answer",
        "pending-answer-command",
        session_id,
        menu.id,
        request_seq,
        opening_generation,
        "approve_once",
        0,
    )
    .await;
    let events = events_until_terminal(&mut second, &run_id).await;
    assert!(matches!(
        events.last(),
        Some((_, EventPayload::RunState(RunState::Done)))
    ));
    assert_eq!(
        fs::read_to_string(&target).expect("resumed target"),
        "after"
    );
    assert_eq!(fake.requests().len(), 3);

    second_task.shutdown_handle().request("test complete");
    second_task.join().await.expect("second daemon joins");
}

#[derive(Clone)]
struct PreDispatchCrashFactory {
    pause: Arc<PreDispatchPause>,
    calls: Arc<AtomicUsize>,
}

struct PreDispatchPause {
    pause_next: AtomicBool,
    reached: Semaphore,
    effect: StdMutex<Option<EffectId>>,
}

struct PreDispatchCrashJournal {
    context: WorkerToolContext,
    pause: Arc<PreDispatchPause>,
}

#[async_trait]
impl JournalSink for PreDispatchCrashJournal {
    async fn append(&mut self, payload: EventPayload) -> ToolResult<()> {
        if let EventPayload::Effect(EffectPhase::Dispatched { effect }) = &payload
            && self.pause.pause_next.swap(false, Ordering::SeqCst)
        {
            *self.pause.effect.lock().expect("paused effect lock") = Some(effect.clone());
            self.pause.reached.add_permits(1);
            future::pending::<()>().await;
        }
        TestHubJournal {
            context: self.context.clone(),
        }
        .append(payload)
        .await
    }

    fn supports_checkpoint_batches(&self) -> bool {
        true
    }

    fn supports_checkpoint_artifacts(&self) -> bool {
        true
    }

    async fn put_checkpoint_artifact(
        &mut self,
        bytes: &[u8],
    ) -> ToolResult<haider_protocol::ids::ArtifactRef> {
        self.context
            .store
            .put_artifact(bytes.to_vec())
            .await
            .map_err(|error| ToolError::cas(error.message))
    }

    async fn append_checkpointed(
        &mut self,
        outcome: EventPayload,
        checkpoint: EventPayload,
    ) -> ToolResult<()> {
        TestHubJournal {
            context: self.context.clone(),
        }
        .append_checkpointed(outcome, checkpoint)
        .await
    }
}

#[async_trait]
impl TurnToolFactory for PreDispatchCrashFactory {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "fs_edit".into(),
            description: "test-paused real fs_edit".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }]
    }

    async fn create(
        &self,
        context: WorkerToolContext,
    ) -> Result<Option<Arc<dyn ToolDispatcher>>, HaiderError> {
        let broker = EffectBroker::new(
            Box::new(PreDispatchCrashJournal {
                context: context.clone(),
                pause: self.pause.clone(),
            }),
            &context.metadata.cwd,
            context.store.session_id().clone(),
            context.store.worker_generation(),
        )
        .map_err(|error| HaiderError::new(ErrorCode::Internal, error.to_string(), false))?;
        let mut policy = PermissionPolicy::default();
        policy.ask(EffectClass::FsWrite);
        Ok(Some(Arc::new(PreDispatchCrashDispatcher {
            broker: tokio::sync::Mutex::new(Some(broker)),
            policy: tokio::sync::Mutex::new(policy),
            context,
            calls: self.calls.clone(),
        })))
    }
}

struct PreDispatchCrashDispatcher {
    broker: tokio::sync::Mutex<Option<EffectBroker>>,
    policy: tokio::sync::Mutex<PermissionPolicy>,
    context: WorkerToolContext,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl ToolDispatcher for PreDispatchCrashDispatcher {
    async fn execute(
        &self,
        run_id: &RunId,
        _item_id: &haider_protocol::ids::ItemId,
        _call_id: &str,
        name: &str,
        args: serde_json::Value,
        _cancel: &CancelToken,
    ) -> Result<ToolDispatchResult, HaiderError> {
        assert_eq!(name, "fs_edit");
        self.calls.fetch_add(1, Ordering::SeqCst);
        let path = args["path"].as_str().expect("path");
        let old = args["edits"][0]["old"].as_str().expect("old anchor");
        let new = args["edits"][0]["new"].as_str().expect("new text");
        let mut broker = self.broker.lock().await;
        let broker = broker.as_mut().expect("open broker");
        let bytes = fs::read(std::path::Path::new(&self.context.metadata.cwd).join(path))
            .expect("read edit baseline");
        broker
            .restore_freshness([FileFreshness {
                path: path.into(),
                digest: format!("blake3:{}", blake3::hash(&bytes).to_hex()),
            }])
            .expect("restore edit baseline");
        let policy = self.policy.lock().await;
        let result = broker
            .fs_edit(
                &FsEdit::new(path, old, new),
                &policy,
                &TurnAttribution::new(self.context.store.session_id().clone(), run_id.clone()),
                &haider_tools::ChangeLedger::new(),
            )
            .await;
        match result {
            Ok(result) => Ok(ToolDispatchResult::Completed(result)),
            Err(haider_tools::ToolError::AuthorizationRequired { menu }) => {
                let menu = broker.permission_menu(&menu).cloned().ok_or_else(|| {
                    HaiderError::new(
                        ErrorCode::Internal,
                        "test broker approval menu disappeared",
                        false,
                    )
                })?;
                Ok(ToolDispatchResult::ApprovalRequired(menu))
            }
            Err(error) => Err(HaiderError::new(
                ErrorCode::Internal,
                error.to_string(),
                false,
            )),
        }
    }

    async fn resolve_approval(&self, _menu: &Menu, answer: &MenuAnswer) -> Result<(), HaiderError> {
        let mut broker = self.broker.lock().await;
        let mut policy = self.policy.lock().await;
        broker
            .as_mut()
            .expect("open broker")
            .resolve_permission(answer, &mut policy)
            .map_err(|error| HaiderError::new(ErrorCode::Internal, error.to_string(), false))
    }

    async fn close(&self) -> Result<(), HaiderError> {
        let Some(broker) = self.broker.lock().await.take() else {
            return Ok(());
        };
        broker.close().await.map(|_| ()).map_err(|error| {
            HaiderError::new(ErrorCode::EffectUnknownOutcome, error.to_string(), false)
        })
    }
}

/// Approval is already durably committed when the fresh effect reaches its
/// persist-before-dispatch boundary. A crash here may resume the effect once
/// or safely lose it, but can never apply before `Dispatched`, apply twice, or
/// dispatch ahead of the committed grant.
#[tokio::test]
async fn w4a1_committed_approval_crash_before_dispatched_is_safely_lost() {
    let root = test_root("w4a1-pre-dispatch-crash-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let target = workspace.join("pre-dispatch.txt");
    fs::write(&target, "before").expect("seed target");
    let config = DaemonConfig::new(
        "w4a1-pre-dispatch-crash",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (mut dependencies, fake) = fake_dependencies(vec![
        FakeStep::EmitToolCall {
            call_id: "pre-dispatch-patch".into(),
            name: "fs_edit".into(),
            args: serde_json::json!({
                "path": "pre-dispatch.txt",
                "edits": [{"old": "before", "new": "after"}]
            }),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "pre-dispatch-patch".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let pause = Arc::new(PreDispatchPause {
        pause_next: AtomicBool::new(true),
        reached: Semaphore::new(0),
        effect: StdMutex::new(None),
    });
    let calls = Arc::new(AtomicUsize::new(0));
    dependencies.tool_factory = Arc::new(PreDispatchCrashFactory {
        pause: pause.clone(),
        calls: calls.clone(),
    });

    let first_task = ready_with_dependencies(&config, dependencies.clone()).await;
    let mut first = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w4a1-test",
        "pre-dispatch-before",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut first, &config, &workspace).await;
    send_request(
        &mut first,
        &config,
        "pre-dispatch-submit",
        submit_body(
            "pre-dispatch-command",
            session_id.clone(),
            generation,
            "approve then crash before dispatch",
        ),
    )
    .await;
    let (run_id, _) = next_submit_response(&mut first).await;
    let (menu, request_seq, opening_generation) = next_permission_menu(&mut first).await;
    answer_menu(
        &mut first,
        &config,
        "pre-dispatch-answer",
        "pre-dispatch-answer-command",
        session_id.clone(),
        menu.id.clone(),
        request_seq,
        opening_generation,
        "approve_once",
        0,
    )
    .await;
    pause
        .reached
        .acquire()
        .await
        .expect("fresh Dispatched append reached")
        .forget();

    let before_crash = read_session(
        &mut first,
        &config,
        session_id.clone(),
        "pre-dispatch-before-read",
    )
    .await;
    let before_payloads = before_crash
        .iter()
        .filter(|envelope| envelope.run_id.as_ref() == Some(&run_id))
        .filter_map(|envelope| {
            serde_json::from_value::<EventPayload>(envelope.payload.clone().into())
                .ok()
                .map(|payload| (envelope.seq, payload))
        })
        .collect::<Vec<_>>();
    let answer_seq = before_payloads
        .iter()
        .find_map(|(seq, payload)| {
            matches!(
                payload,
                EventPayload::MenuAnswered(answer) if answer.menu == menu.id
            )
            .then_some(*seq)
        })
        .expect("committed approval answer");
    let allowed_seq = before_payloads
        .iter()
        .find_map(|(seq, payload)| {
            matches!(
                payload,
                EventPayload::Effect(EffectPhase::Authorized {
                    verdict: AuthorizationVerdict::Allow,
                    ..
                })
            )
            .then_some(*seq)
        })
        .expect("fresh effect authorized from committed approval");
    assert!(answer_seq < allowed_seq);
    assert_eq!(
        before_payloads
            .iter()
            .filter(|(_, payload)| matches!(
                payload,
                EventPayload::Effect(EffectPhase::Dispatched { .. })
            ))
            .count(),
        0
    );
    assert!(matches!(
        before_payloads
            .iter()
            .rev()
            .find(|(_, payload)| matches!(payload, EventPayload::RunState(_))),
        Some((_, EventPayload::RunState(RunState::RunningTool)))
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(pause.effect.lock().expect("paused effect lock").is_some());
    assert_eq!(
        fs::read_to_string(&target).expect("pre-crash target"),
        "before"
    );

    drop(first);
    first_task.crash().await;

    let second_task = ready_with_dependencies(&config, dependencies).await;
    let mut second = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w4a1-test",
        "pre-dispatch-after",
        ClientKind::Headless,
    )
    .await;
    attach_existing(
        &mut second,
        &config,
        session_id.clone(),
        0,
        "pre-dispatch-replay",
    )
    .await;
    let after_restart =
        read_session(&mut second, &config, session_id, "pre-dispatch-after-read").await;
    let after_payloads = after_restart
        .iter()
        .filter(|envelope| envelope.run_id.as_ref() == Some(&run_id))
        .filter_map(|envelope| {
            serde_json::from_value::<EventPayload>(envelope.payload.clone().into())
                .ok()
                .map(|payload| (envelope.seq, payload))
        })
        .collect::<Vec<_>>();
    let dispatched = after_payloads
        .iter()
        .filter_map(|(seq, payload)| match payload {
            EventPayload::Effect(EffectPhase::Dispatched { .. }) => Some(*seq),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(dispatched.len() <= 1, "effect must never double-dispatch");
    assert!(
        dispatched
            .iter()
            .all(|dispatch_seq| answer_seq < *dispatch_seq),
        "a dispatch may only follow the committed grant"
    );
    if dispatched.is_empty() {
        assert_eq!(
            fs::read_to_string(&target).expect("safely lost target"),
            "before"
        );
        assert!(matches!(
            after_payloads
                .iter()
                .rev()
                .find(|(_, payload)| matches!(payload, EventPayload::RunState(_))),
            Some((_, EventPayload::RunState(RunState::Errored)))
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(fake.requests().len(), 1);
    } else {
        assert_eq!(
            fs::read_to_string(&target).expect("resumed target"),
            "after"
        );
        assert!(calls.load(Ordering::SeqCst) <= 3);
        assert!(fake.requests().len() <= 2);
    }

    second_task.shutdown_handle().request("test complete");
    second_task.join().await.expect("second daemon joins");
}

#[derive(Clone)]
struct HeldPatchLedger {
    reached: Arc<Semaphore>,
    release: Arc<(StdMutex<bool>, Condvar)>,
}

impl HeldPatchLedger {
    fn release(&self) {
        let (released, wake) = &*self.release;
        *released.lock().expect("release lock") = true;
        wake.notify_all();
    }
}

struct HeldPatchReleaseGuard(Option<HeldPatchLedger>);

impl HeldPatchReleaseGuard {
    fn new(ledger: HeldPatchLedger) -> Self {
        Self(Some(ledger))
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for HeldPatchReleaseGuard {
    fn drop(&mut self) {
        if let Some(ledger) = self.0.take() {
            ledger.release();
        }
    }
}

impl ChangeLedgerSink for HeldPatchLedger {
    fn record_fs_write(
        &self,
        _session: SessionId,
        _turn: RunId,
        _record: FsWriteRecord,
    ) -> ToolResult<()> {
        self.reached.add_permits(1);
        let (released, wake) = &*self.release;
        let mut released = released.lock().expect("release lock");
        while !*released {
            released = wake.wait(released).expect("release wait");
        }
        Ok(())
    }
}

#[derive(Clone)]
struct HeldRealPatchFactory {
    calls: Arc<AtomicUsize>,
    ledger: HeldPatchLedger,
}

#[async_trait]
impl TurnToolFactory for HeldRealPatchFactory {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "fs_edit".into(),
            description: "test-held real fs_edit".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }]
    }

    async fn create(
        &self,
        context: WorkerToolContext,
    ) -> Result<Option<Arc<dyn ToolDispatcher>>, HaiderError> {
        let mut policy = PermissionPolicy::default();
        policy.allow(EffectClass::FsWrite);
        let broker = EffectBroker::new(
            Box::new(TestHubJournal {
                context: context.clone(),
            }),
            &context.metadata.cwd,
            context.store.session_id().clone(),
            context.store.worker_generation(),
        )
        .map_err(|error| HaiderError::new(ErrorCode::Internal, error.to_string(), false))?;
        Ok(Some(Arc::new(HeldRealPatchDispatcher {
            broker: tokio::sync::Mutex::new(Some(broker)),
            policy,
            context,
            calls: self.calls.clone(),
            ledger: self.ledger.clone(),
        })))
    }
}

struct TestHubJournal {
    context: WorkerToolContext,
}

impl TestHubJournal {
    async fn append_payloads(&self, payloads: Vec<EventPayload>) -> ToolResult<()> {
        let mut envelopes = payloads
            .into_iter()
            .map(|payload| {
                Ok(EventEnvelope {
                    schema_version: SCHEMA_VERSION,
                    event_id: self.context.event_ids.next(),
                    seq: 0,
                    session_id: self.context.store.session_id().clone(),
                    branch_id: None,
                    run_id: Some(self.context.run_id.clone()),
                    agent_id: None,
                    device_id: self.context.device_id.clone(),
                    authority_epoch: 0,
                    worker_generation: self.context.store.worker_generation(),
                    causation_id: None,
                    correlation_id: None,
                    committed_at_ms: 0,
                    render: RenderTargets {
                        ui: true,
                        durable: true,
                        prompt: PromptRender::Omit,
                    },
                    payload: serde_json::to_value(payload)
                        .map_err(|error| haider_tools::ToolError::Runtime {
                            message: error.to_string(),
                        })?
                        .into(),
                })
            })
            .collect::<ToolResult<Vec<_>>>()?;
        StoreHandle::append(&self.context.store, &mut envelopes)
            .await
            .map_err(|error| haider_tools::ToolError::Runtime {
                message: error.message,
            })?;
        Ok(())
    }
}

#[async_trait]
impl JournalSink for TestHubJournal {
    async fn append(&mut self, payload: EventPayload) -> ToolResult<()> {
        self.append_payloads(vec![payload]).await
    }

    fn supports_checkpoint_batches(&self) -> bool {
        true
    }

    fn supports_checkpoint_artifacts(&self) -> bool {
        true
    }

    async fn put_checkpoint_artifact(
        &mut self,
        bytes: &[u8],
    ) -> ToolResult<haider_protocol::ids::ArtifactRef> {
        self.context
            .store
            .put_artifact(bytes.to_vec())
            .await
            .map_err(|error| ToolError::cas(error.message))
    }

    async fn append_checkpointed(
        &mut self,
        outcome: EventPayload,
        checkpoint: EventPayload,
    ) -> ToolResult<()> {
        self.append_payloads(vec![outcome, checkpoint]).await
    }
}

struct HeldRealPatchDispatcher {
    broker: tokio::sync::Mutex<Option<EffectBroker>>,
    policy: PermissionPolicy,
    context: WorkerToolContext,
    calls: Arc<AtomicUsize>,
    ledger: HeldPatchLedger,
}

#[async_trait]
impl ToolDispatcher for HeldRealPatchDispatcher {
    async fn execute(
        &self,
        run_id: &RunId,
        _item_id: &haider_protocol::ids::ItemId,
        _call_id: &str,
        name: &str,
        args: serde_json::Value,
        _cancel: &CancelToken,
    ) -> Result<ToolDispatchResult, HaiderError> {
        assert_eq!(name, "fs_edit");
        self.calls.fetch_add(1, Ordering::SeqCst);
        let path = args["path"].as_str().expect("path");
        let old = args["edits"][0]["old"].as_str().expect("old anchor");
        let new = args["edits"][0]["new"].as_str().expect("new text");
        let mut broker = self.broker.lock().await;
        let broker = broker.as_mut().expect("open broker");
        let bytes = fs::read(std::path::Path::new(&self.context.metadata.cwd).join(path))
            .expect("read edit baseline");
        broker
            .restore_freshness([FileFreshness {
                path: path.into(),
                digest: format!("blake3:{}", blake3::hash(&bytes).to_hex()),
            }])
            .expect("restore edit baseline");
        let result = broker
            .fs_edit(
                &FsEdit::new(path, old, new),
                &self.policy,
                &TurnAttribution::new(self.context.store.session_id().clone(), run_id.clone()),
                &self.ledger,
            )
            .await
            .map_err(|error| HaiderError::new(ErrorCode::Internal, error.to_string(), false))?;
        Ok(ToolDispatchResult::Completed(result))
    }

    async fn close(&self) -> Result<(), HaiderError> {
        let Some(broker) = self.broker.lock().await.take() else {
            return Ok(());
        };
        broker.close().await.map(|_| ()).map_err(|error| {
            HaiderError::new(ErrorCode::EffectUnknownOutcome, error.to_string(), false)
        })
    }
}

/// MUTATION CHECK: remove startup Dispatched-without-Outcome reconciliation
/// or retry interrupted RunningTool work. Expected failure: Unknown is absent
/// or the real patch dispatcher call count exceeds one. Verified by revert in
/// W4a1.
#[tokio::test]
async fn w4a1_dispatched_real_fs_edit_restarts_as_unknown_without_redispatch() {
    let root = test_root("w4a1-patch-restart-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let target = workspace.join("restart.txt");
    fs::write(&target, "before").expect("seed target");
    let config = DaemonConfig::new(
        "w4a1-patch-restart",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (mut dependencies, fake) = fake_dependencies(vec![
        FakeStep::EmitToolCall {
            call_id: "restart-patch".into(),
            name: "fs_edit".into(),
            args: serde_json::json!({
                "path": "restart.txt",
                "edits": [{"old": "before", "new": "after"}]
            }),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
    ]);
    let calls = Arc::new(AtomicUsize::new(0));
    let ledger = HeldPatchLedger {
        reached: Arc::new(Semaphore::new(0)),
        release: Arc::new((StdMutex::new(false), Condvar::new())),
    };
    let mut release_on_unwind = HeldPatchReleaseGuard::new(ledger.clone());
    dependencies.tool_factory = Arc::new(HeldRealPatchFactory {
        calls: calls.clone(),
        ledger: ledger.clone(),
    });
    let first_task = ready_with_dependencies(&config, dependencies.clone()).await;
    let mut first = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w4a1-test",
        "patch-before-crash",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut first, &config, &workspace).await;
    send_request(
        &mut first,
        &config,
        "restart-patch-submit",
        submit_body(
            "restart-patch-command",
            session_id.clone(),
            generation,
            "patch once then crash",
        ),
    )
    .await;
    let (run_id, _) = next_submit_response(&mut first).await;
    ledger
        .reached
        .acquire()
        .await
        .expect("real patch reached post-apply ledger boundary")
        .forget();
    assert_eq!(
        fs::read_to_string(&target).expect("applied target"),
        "after"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    drop(first);
    first_task.crash().await;
    ledger.release();
    release_on_unwind.disarm();

    let second_task = ready_with_dependencies(&config, dependencies).await;
    let mut second = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w4a1-test",
        "patch-after-crash",
        ClientKind::Headless,
    )
    .await;
    attach_existing(
        &mut second,
        &config,
        session_id.clone(),
        0,
        "patch-restart-attach",
    )
    .await;
    let envelopes = read_session(&mut second, &config, session_id, "patch-restart-read").await;
    let intents = payloads_for_run(&envelopes, &run_id)
        .filter_map(|payload| match payload {
            EventPayload::Effect(EffectPhase::Intent(intent))
                if intent.class == EffectClass::FsWrite =>
            {
                Some(intent.effect)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(intents.len(), 1);
    assert_eq!(
        payloads_for_run(&envelopes, &run_id)
            .filter(|payload| matches!(
                payload,
                EventPayload::Effect(EffectPhase::Outcome {
                    effect,
                    outcome: EffectOutcome::Unknown,
                    ..
                }) if *effect == intents[0]
            ))
            .count(),
        1
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(fake.requests().len(), 1);
    assert_eq!(
        fs::read_to_string(&target).expect("target after restart"),
        "after"
    );

    second_task.shutdown_handle().request("test complete");
    second_task.join().await.expect("second daemon joins");
}

/// W4a2 restart law: a real command that crossed Dispatched is ambiguous
/// after daemon death and must become Unknown, never execute a second time.
#[tokio::test]
async fn w4a2_dispatched_exec_restarts_as_unknown_without_rerun() {
    #[cfg(windows)]
    let _windows_process_test =
        windows_real_process_test_guard("w4a2_dispatched_exec_restarts_as_unknown_without_rerun")
            .await;
    let root = test_root("w4a2-exec-restart-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let attempts = workspace.join("attempts.log");
    let command = restart_exec_command();
    let config = DaemonConfig::new(
        "w4a2-exec-restart",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (dependencies, fake) = fake_dependencies(vec![
        FakeStep::EmitToolCall {
            call_id: "restart-exec".into(),
            name: "exec".into(),
            args: serde_json::json!({"command": command}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
    ]);
    let first_task = ready_with_dependencies(&config, dependencies.clone()).await;
    let mut first = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w4a2-test",
        "exec-before-crash",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut first, &config, &workspace).await;
    send_request(
        &mut first,
        &config,
        "restart-exec-submit",
        submit_body(
            "restart-exec-command",
            session_id.clone(),
            generation,
            "run once then crash",
        ),
    )
    .await;
    let (run_id, _) = next_submit_response(&mut first).await;
    let (menu, request_seq, opening_generation) = next_permission_menu(&mut first).await;
    let mut answerer = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w4a2-test",
        "exec-restart-answerer",
        ClientKind::Headless,
    )
    .await;
    attach_existing(
        &mut answerer,
        &config,
        session_id.clone(),
        request_seq,
        "exec-restart-answerer-attach",
    )
    .await;
    answer_menu(
        &mut answerer,
        &config,
        "restart-exec-answer",
        "restart-exec-answer-command",
        session_id.clone(),
        menu.id,
        request_seq,
        opening_generation,
        "approve_once",
        0,
    )
    .await;
    loop {
        if let WireFrame::Event { envelope, .. } = first.next().await
            && envelope.run_id.as_ref() == Some(&run_id)
            && let Ok(EventPayload::Item(ItemEvent::Delta {
                delta: ItemDelta::CommandOutput { chunk_b64, .. },
                ..
            })) = serde_json::from_value::<EventPayload>(envelope.payload.into())
            && BASE64
                .decode(chunk_b64)
                .expect("command output base64")
                .windows(b"started".len())
                .any(|window| window == b"started")
        {
            break;
        }
    }
    assert_eq!(
        fs::read_to_string(&attempts).expect("first dispatch ran"),
        "attempt\n"
    );
    drop(answerer);
    drop(first);
    first_task.crash().await;

    let second_task = ready_with_dependencies(&config, dependencies).await;
    let mut second = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w4a2-test",
        "exec-after-crash",
        ClientKind::Headless,
    )
    .await;
    attach_existing(
        &mut second,
        &config,
        session_id.clone(),
        0,
        "exec-restart-attach",
    )
    .await;
    let envelopes = read_session(&mut second, &config, session_id, "exec-restart-read").await;
    let dispatched = payloads_for_run(&envelopes, &run_id)
        .filter_map(|payload| match payload {
            EventPayload::Effect(EffectPhase::Dispatched { effect }) => Some(effect),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(dispatched.len(), 1);
    assert_eq!(
        payloads_for_run(&envelopes, &run_id)
            .filter(|payload| matches!(
                payload,
                EventPayload::Effect(EffectPhase::Outcome {
                    effect,
                    outcome: EffectOutcome::Unknown,
                    ..
                }) if *effect == dispatched[0]
            ))
            .count(),
        1,
        "startup must reconcile the real exec dispatch to Unknown"
    );
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    assert_eq!(
        fs::read_to_string(&attempts).expect("attempt ledger"),
        "attempt\n",
        "restart must not run the command again"
    );
    assert_eq!(fake.requests().len(), 1);

    second_task.shutdown_handle().request("test complete");
    second_task.join().await.expect("second daemon joins");
}

/// W4a2 cancellation truth / group-sweep sentinel.
///
/// MUTATION CHECK: remove `ProcessExecution::drop` cancellation or make the
/// dispatcher detach the handle. The heartbeat continues growing after
/// Cancelled+Idle and this test fails. On Windows, also swallow the reserved
/// keepalive Pong restored from ee2b1ce: the per-frame deadline preempts the
/// higher-level fresh-attempt observation. Verified by revert in W4a2/win2.
#[tokio::test]
async fn w4a2_cancelled_exec_child_process_group_dies() {
    #[cfg(windows)]
    let _windows_process_test =
        windows_real_process_test_guard("w4a2_cancelled_exec_child_process_group_dies").await;
    #[cfg(windows)]
    // SAFETY: Windows owns environment storage and permits process-wide
    // mutation while other threads exist. This test-only key is set before the
    // in-process daemon starts and is never consumed as application input.
    #[allow(unsafe_code)]
    unsafe {
        std::env::set_var("HAIDER_TEST_PROCESS_TRACE", "1");
    }
    let root = test_root("w4a2-exec-cancel-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let heartbeat = workspace.join("heartbeat.log");
    #[cfg(not(windows))]
    let command = cancellable_exec_command();
    #[cfg(windows)]
    let command = cancellable_exec_fixture_command(&workspace);
    let config = DaemonConfig::new(
        "w4a2-exec-cancel",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    #[cfg(not(windows))]
    let (dependencies, fake) = fake_dependencies(vec![
        FakeStep::EmitToolCall {
            call_id: "cancel-exec".into(),
            name: "exec".into(),
            args: serde_json::json!({"command": command}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
    ]);
    #[cfg(windows)]
    let (dependencies, fake) = windows_exec_fake_dependencies(
        cancellable_exec_attempt_script(command.clone()),
        cancellable_exec_attempt_script(command),
    );
    let task = ready_with_dependencies(&config, dependencies).await;
    #[cfg(windows)]
    let mut failure_diagnostics = support::FailureDiagnostics::install("w4a2-exec-cancel", &task);
    #[cfg(windows)]
    let mut process_tree_diagnostics = WindowsExecTreeFailureDiagnostics::new(&config, &workspace);
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w4a2-test",
        "exec-cancel-client",
        ClientKind::Headless,
    )
    .await;
    #[cfg(windows)]
    failure_diagnostics.watch(&client);
    let (session_id, generation) = create_and_attach(&mut client, &config, &workspace).await;
    send_request(
        &mut client,
        &config,
        "cancel-exec-submit",
        submit_body(
            "cancel-exec-command",
            session_id.clone(),
            generation,
            "start then cancel the command",
        ),
    )
    .await;
    let (run_id, _) = next_submit_response(&mut client).await;
    let (menu, request_seq, opening_generation) = next_permission_menu(&mut client).await;
    let mut answerer = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w4a2-test",
        "exec-cancel-answerer",
        ClientKind::Headless,
    )
    .await;
    #[cfg(windows)]
    failure_diagnostics.watch(&answerer);
    attach_existing(
        &mut answerer,
        &config,
        session_id.clone(),
        request_seq,
        "exec-cancel-answerer-attach",
    )
    .await;
    let approved_menu = menu.id.clone();
    answer_menu(
        &mut answerer,
        &config,
        "cancel-exec-answer",
        "cancel-exec-answer-command",
        session_id.clone(),
        menu.id,
        request_seq,
        opening_generation,
        "approve_once",
        0,
    )
    .await;
    #[cfg(windows)]
    process_tree_diagnostics.set_phase("waiting-for-first-start");
    #[cfg(windows)]
    eprintln!(
        "haider-daemond windows-process test=w4a2_cancelled_exec_child_process_group_dies phase=waiting-for-first-start"
    );
    let (start, _start_observer_diagnostics) = wait_for_exec_child_started(
        &mut client,
        &config,
        &session_id,
        &run_id,
        &heartbeat,
        &workspace,
        &fake,
        &approved_menu,
    )
    .await;
    #[cfg(windows)]
    process_tree_diagnostics.observe_process_start(
        "first-attempt",
        &start,
        _start_observer_diagnostics,
    );
    #[cfg(windows)]
    process_tree_diagnostics.set_phase("first-start-observed");
    #[cfg(windows)]
    eprintln!(
        "haider-daemond windows-process test=w4a2_cancelled_exec_child_process_group_dies phase=first-start-observed result={start:?}"
    );
    #[cfg(not(windows))]
    assert_eq!(
        start.expect("exec child starts within the deadline"),
        ProcessStartObservation::Started
    );
    #[cfg(windows)]
    let retry_start = !matches!(start, Ok(ProcessStartObservation::Started));
    #[cfg(windows)]
    let (run_id, answerer, process_start_attempts) = if retry_start {
        // The first observer already spent the full long-start allowance.
        // Cleanup, fresh submission, approval, and observation now share one
        // ordinary RPC deadline instead of restarting a bound at every phase.
        let recovery_deadline = tokio::time::Instant::now() + support::DEADLINE;
        tokio::time::timeout_at(recovery_deadline, async {
            process_tree_diagnostics.set_phase("settling-first-attempt");
            eprintln!(
                "haider-daemond windows-process test=w4a2_cancelled_exec_child_process_group_dies phase=settling-first-attempt"
            );
            drop(answerer);
            let _previous_attempt = settle_process_start_attempt_before_retry(
                &mut client,
                &config,
                &session_id,
                generation,
                &run_id,
                recovery_deadline,
                "cancel-exec-timeout-cleanup",
                "cancel-exec-timeout-cleanup-command",
            )
            .await;
            process_tree_diagnostics.set_phase("cleaning-first-attempt");
            eprintln!(
                "haider-daemond windows-process test=w4a2_cancelled_exec_child_process_group_dies phase=cleaning-first-attempt"
            );
            clean_timed_out_process_start_files(&workspace, &heartbeat).await;
            process_tree_diagnostics.set_phase("submitting-retry");
            eprintln!(
                "haider-daemond windows-process test=w4a2_cancelled_exec_child_process_group_dies phase=submitting-retry"
            );
            fake.arm_retry();
            send_request(
                &mut client,
                &config,
                "cancel-exec-submit-retry",
                submit_body(
                    "cancel-exec-command-retry",
                    session_id.clone(),
                    generation,
                    "start then cancel the command",
                ),
            )
            .await;
            let (retry_run_id, _) = next_submit_response(&mut client).await;
            let (retry_menu, retry_request_seq, retry_opening_generation) =
                next_permission_menu(&mut client).await;
            let mut retry_answerer = UdsClient::connect_control(
                &config.endpoint_path(),
                config.frame_limit,
                "w4a2-test",
                "exec-cancel-answerer-retry",
                ClientKind::Headless,
            )
            .await;
            failure_diagnostics.watch(&retry_answerer);
            attach_existing(
                &mut retry_answerer,
                &config,
                session_id.clone(),
                retry_request_seq,
                "exec-cancel-answerer-attach-retry",
            )
            .await;
            let retry_approved_menu = retry_menu.id.clone();
            answer_menu(
                &mut retry_answerer,
                &config,
                "cancel-exec-answer-retry",
                "cancel-exec-answer-command-retry",
                session_id.clone(),
                retry_menu.id,
                retry_request_seq,
                retry_opening_generation,
                "approve_once",
                0,
            )
            .await;
            process_tree_diagnostics.set_phase("waiting-for-retry-start");
            eprintln!(
                "haider-daemond windows-process test=w4a2_cancelled_exec_child_process_group_dies phase=waiting-for-retry-start"
            );
            let (retry_start, retry_observer_diagnostics) = wait_for_exec_child_started(
                    &mut client,
                    &config,
                    &session_id,
                    &retry_run_id,
                    &heartbeat,
                    &workspace,
                    &fake,
                    &retry_approved_menu,
                )
                .await;
            process_tree_diagnostics.observe_process_start(
                "retry-attempt",
                &retry_start,
                retry_observer_diagnostics,
            );
            assert_eq!(
                retry_start
                .expect("exec child starts within the deadline on its single retry"),
                ProcessStartObservation::Started,
                "the single retry must start a live process tree"
            );
            (retry_run_id, retry_answerer, 2_usize)
        })
        .await
        .unwrap_or_else(|error| {
            let heartbeat_bytes = fs::metadata(&heartbeat).map_or(0, |metadata| metadata.len());
            eprintln!(
                "haider-daemond windows-process test=w4a2_cancelled_exec_child_process_group_dies phase=failed-start-recovery-deadline result=Err({error:?}) ps_alive={} heartbeat_bytes={heartbeat_bytes} workspace_entries={:?} first_attempt_requests={} retry_requests={}",
                workspace.join("ps-alive.log").exists(),
                workspace_listing(&workspace),
                fake.first_attempt_request_count(),
                fake.retry_request_count(),
            );
            panic!("failed exec start recovery exceeded one continuous deadline")
        })
    } else {
        (run_id, answerer, 1_usize)
    };
    #[cfg(windows)]
    {
        process_tree_diagnostics.set_phase("verifying-live-descendant-before-cancel");
        process_tree_diagnostics.observe_live_tree();
    }
    assert!(heartbeat.exists(), "real child started");
    #[cfg(windows)]
    assert!(
        workspace.join("descendant-started.log").exists(),
        "the descendant existed before cancellation"
    );
    drop(answerer);
    #[cfg(not(windows))]
    send_request(
        &mut client,
        &config,
        "cancel-exec",
        RequestBody::TurnCancel {
            command_id: CommandId::new("cancel-exec-rpc-command"),
            session_id: session_id.clone(),
            worker_generation: generation,
            run_id: run_id.clone(),
        },
    )
    .await;
    #[cfg(not(windows))]
    let events = events_until_terminal(&mut client, &run_id).await;
    #[cfg(windows)]
    process_tree_diagnostics.set_phase("cancelling-live-attempt");
    #[cfg(windows)]
    eprintln!(
        "haider-daemond windows-process test=w4a2_cancelled_exec_child_process_group_dies phase=cancelling-live-attempt"
    );
    #[cfg(windows)]
    let events = cancel_live_windows_attempt(
        &mut client,
        &config,
        &session_id,
        generation,
        &run_id,
        "cancel-exec",
        "cancel-exec-rpc-command",
    )
    .await;
    #[cfg(windows)]
    process_tree_diagnostics.set_phase("cancelled-and-idle");
    #[cfg(windows)]
    eprintln!(
        "haider-daemond windows-process test=w4a2_cancelled_exec_child_process_group_dies phase=cancelled-and-idle"
    );
    assert!(matches!(
        events.last(),
        Some((_, EventPayload::RunState(RunState::Cancelled)))
    ));
    #[cfg(not(windows))]
    assert!(
        next_idle(&mut client).await,
        "cancelled exec settles interrupted Idle only after dispatcher close"
    );
    #[cfg(windows)]
    process_tree_diagnostics.set_phase("reading-durable-session");
    #[cfg(windows)]
    eprintln!(
        "haider-daemond windows-process test=w4a2_cancelled_exec_child_process_group_dies phase=reading-durable-session"
    );
    let durable = read_session(
        &mut client,
        &config,
        session_id.clone(),
        "cancel-exec-durable-read",
    )
    .await;
    #[cfg(windows)]
    process_tree_diagnostics.set_phase("validating-durable-tool-terminal");
    #[cfg(windows)]
    eprintln!(
        "haider-daemond windows-process test=w4a2_cancelled_exec_child_process_group_dies phase=durable-session-read"
    );
    let run_items = payloads_for_run(&durable, &run_id)
        .filter_map(|payload| match payload {
            EventPayload::Item(item) => Some(item),
            _ => None,
        })
        .collect::<Vec<_>>();
    let tool_item = run_items
        .iter()
        .find_map(|item| match item {
            ItemEvent::Started {
                item_id,
                item: TurnItem::ToolCall { call_id, name, .. },
            } if call_id == "cancel-exec" && name == "exec" => Some(item_id.clone()),
            _ => None,
        })
        .expect("exec tool item started");
    let completed = run_items
        .iter()
        .position(|item| {
            matches!(
                item,
                ItemEvent::Completed { item_id, .. } if item_id == &tool_item
            )
        })
        .expect("exec tool item completed on cancellation");
    assert!(
        run_items[completed + 1..].iter().all(|item| !matches!(
            item,
            ItemEvent::Delta {
                item_id,
                delta: ItemDelta::CommandOutput { .. },
            } if item_id == &tool_item
        )),
        "command output delta followed the tool item's terminal event"
    );
    #[cfg(windows)]
    process_tree_diagnostics.set_phase("checking-heartbeat-stopped");
    let stopped_size = fs::metadata(&heartbeat).expect("heartbeat metadata").len();
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(
        fs::metadata(&heartbeat).expect("heartbeat metadata").len(),
        stopped_size,
        "cancelled child or descendant kept running"
    );
    #[cfg(windows)]
    {
        process_tree_diagnostics.set_phase("checking-descendant-did-not-survive");
        tokio::time::sleep(std::time::Duration::from_millis(1_200)).await;
        assert!(
            !workspace.join("descendant-survived.log").exists(),
            "cancelled process left a surviving descendant"
        );
    }
    #[cfg(not(windows))]
    assert_eq!(fake.request_count(), 1);
    #[cfg(windows)]
    process_tree_diagnostics.set_phase("checking-attempt-counts");
    #[cfg(windows)]
    assert!(
        (1..=2).contains(&fake.first_attempt_request_count()),
        "attempt 1 has one tool-call request and at most its isolated Hang continuation"
    );
    #[cfg(windows)]
    assert_eq!(
        fake.retry_request_count(),
        process_start_attempts - 1,
        "the separately armed retry provider receives exactly one tool-call request"
    );

    task.shutdown_handle().request("test complete");
    #[cfg(windows)]
    {
        process_tree_diagnostics.set_phase("joining-daemon");
        eprintln!(
            "haider-daemond windows-process test=w4a2_cancelled_exec_child_process_group_dies phase=joining-daemon"
        );
        tokio::time::timeout(support::DEADLINE, task.join())
            .await
            .expect("cancelled-exec daemon join deadline")
            .expect("daemon joins");
        eprintln!(
            "haider-daemond windows-process test=w4a2_cancelled_exec_child_process_group_dies phase=complete"
        );
    }
    #[cfg(not(windows))]
    task.join().await.expect("daemon joins");
}

/// Scenario 13: the manifest for the production-seam mutation sweep.
///
/// Each entry is `(test fn, workspace-relative file, seam to revert)`: the
/// focused test that must fail when the listed seam is reverted, and where a
/// re-runner finds it (thirteen of twenty-eight live outside this file). The sweep
/// itself is executed by hand — revert each seam, run the named test, record
/// the observation in the commit message; this manifest keeps that procedure
/// honest by construction: the fifteen in-file entries are compile-time
/// references to their test functions (a rename breaks the build), and every
/// listed file path is asserted to exist in the workspace.
///
/// MUTATION CHECK: delete an entry, point two seams at the same focused
/// test, or let a listed file move without updating its coordinate.
/// Expected failure: the completeness, uniqueness, or path-existence
/// assertions below fail (an in-file test rename fails compilation first).
#[test]
fn scenario_13_mutation_seam_sweep_manifest_covers_each_load_bearing_boundary() {
    // Compile-time linkage for the in-file entries: renaming any of these
    // fifteen tests without updating the manifest is a build error.
    let _in_file_sweep_links: [fn(); 15] = [
        scenario_4_lost_submit_response_replays_one_run_and_one_provider_request,
        scenario_7_two_menu_answers_race_and_only_first_commit_wins,
        scenario_8_wire_cancel_closes_open_items_and_cancelled_is_run_terminal,
        scenario_9_restart_resumes_only_queued_and_terminalizes_streaming,
        scenario_10_restart_replays_request_input_without_reexecuting_prior_request,
        scenario_11_held_effect_becomes_unknown_after_restart_and_never_redispatches,
        scenario_12_reasoning_safe_follow_up_cumulative_usage_and_durable_failure,
        w4a1_fs_write_requires_committed_cas_and_session_grant_survives_restart,
        w4a1_real_fs_edit_round_trips_approval_and_returns_tool_result,
        w4a1_pending_patch_approval_restarts_on_the_original_menu_cas,
        w4a1_committed_approval_crash_before_dispatched_is_safely_lost,
        w4a1_dispatched_real_fs_edit_restarts_as_unknown_without_redispatch,
        w4a2_exec_is_cas_gated_streams_output_and_grants_only_the_exact_shape,
        w4a2_dispatched_exec_restarts_as_unknown_without_rerun,
        w4a2_cancelled_exec_child_process_group_dies,
    ];
    const HERE: &str = "crates/haider-daemond/tests/live_turn_rpc_tests.rs";
    let sweep = [
        (
            "scenario_4_lost_submit_response_replays_one_run_and_one_provider_request",
            HERE,
            "durable receipt preflight and admit_pending's active-run/in-queue run-id dedup",
        ),
        (
            "superseded_worker_lease_is_fenced_before_store_append",
            "crates/haider-daemon/tests/session_hub_tests.rs",
            "hub WorkerAppend active lease-token validation",
        ),
        (
            "scenario_7_two_menu_answers_race_and_only_first_commit_wins",
            HERE,
            "SQLite first-committed-wins menu CAS",
        ),
        (
            "scenario_9_restart_resumes_only_queued_and_terminalizes_streaming",
            HERE,
            "interrupted-run resumability reduction (turn_recovery.rs)",
        ),
        (
            "scenario_11_held_effect_becomes_unknown_after_restart_and_never_redispatches",
            HERE,
            "pre-Ready ambiguous-effect reconciliation",
        ),
        (
            "scenario_12_reasoning_safe_follow_up_cumulative_usage_and_durable_failure",
            HERE,
            "reasoning omission, cumulative usage, and RunFailed ordering",
        ),
        (
            "replay_live_barrier_is_contiguous_at_every_forced_boundary",
            "crates/haider-daemon/tests/session_hub_tests.rs",
            "persist-before-publish and serialized register-plus-head",
        ),
        (
            "full_internal_catch_up_receiver_reregisters_and_resumes_from_store",
            "crates/haider-daemon/tests/session_hub_tests.rs",
            "bounded catch-up with store as the only lag buffer",
        ),
        (
            "scenario_10_restart_replays_request_input_without_reexecuting_prior_request",
            HERE,
            "recovery Ready barrier and recovered-menu generation authorization",
        ),
        (
            "scenario_8_wire_cancel_closes_open_items_and_cancelled_is_run_terminal",
            HERE,
            "lease-bound cancellation wake and terminal event ordering",
        ),
        (
            "aggregate_idle_is_skipped_when_a_new_run_is_durably_active",
            "crates/haider-store/tests/turn_command_tests.rs",
            "transactional aggregate SessionState ownership",
        ),
        (
            "tool_result_is_presented_after_its_completed_tool_call",
            "crates/haider-core/tests/prompt_history_tests.rs",
            "provider-valid tool history reconstruction",
        ),
        (
            "branch_agent_and_nonterminal_history_are_excluded_structurally",
            "crates/haider-core/tests/prompt_history_tests.rs",
            "prompt scope + terminal-run exclusion (Fable D2-5)",
        ),
        (
            "dropping_an_owned_stream_aborts_its_producer",
            "crates/haider-provider/tests/fake_provider_tests.rs",
            "owned provider producer cancellation",
        ),
        (
            "w4a1_fs_write_requires_committed_cas_and_session_grant_survives_restart",
            HERE,
            "server-side committed-CAS approval gate and durable class-scoped session grant",
        ),
        (
            "w4a1_real_fs_edit_round_trips_approval_and_returns_tool_result",
            HERE,
            "production fs_edit model definition, structured hunk dispatch, and approval preview",
        ),
        (
            "w4a1_pending_patch_approval_restarts_on_the_original_menu_cas",
            HERE,
            "permission checkpoint recovery preserves the original durable menu CAS",
        ),
        (
            "w4a1_committed_approval_crash_before_dispatched_is_safely_lost",
            HERE,
            "committed grant precedes fresh Dispatched and pre-dispatch crash is once-or-lost",
        ),
        (
            "mutating_paths_reject_parent_and_absolute_workspace_escapes",
            "crates/haider-tools/tests/filesystem_tools_tests.rs",
            "canonical workspace boundary for parent and absolute mutating paths",
        ),
        (
            "mutating_paths_reject_leaf_and_parent_symlink_escapes",
            "crates/haider-tools/tests/filesystem_tools_tests.rs",
            "canonical workspace boundary for leaf and parent symlink escapes",
        ),
        (
            "w4a1_dispatched_real_fs_edit_restarts_as_unknown_without_redispatch",
            HERE,
            "real fs_edit dispatch crash window reconciles Unknown without retry",
        ),
        (
            "external_leaf_replacement_before_edit_rename_is_typed_path_change",
            "crates/haider-tools/src/filesystem.rs",
            "anchored target-identity recheck immediately before atomic edit replacement",
        ),
        (
            "w4a2_exec_is_cas_gated_streams_output_and_grants_only_the_exact_shape",
            HERE,
            "server-side ProcessExec committed-CAS gate and durable exact command-shape grant",
        ),
        (
            "w4a2_dispatched_exec_restarts_as_unknown_without_rerun",
            HERE,
            "real exec dispatch crash window reconciles Unknown without command rerun",
        ),
        (
            "w4a2_cancelled_exec_child_process_group_dies",
            HERE,
            "turn cancellation drops exec ownership into supervised process-group termination",
        ),
        (
            "hard_output_cap_terminates_the_process_group_and_reports_the_ledgered_limit",
            "crates/haider-tools/tests/process_tools_tests.rs",
            "combined stdout/stderr hard cap terminates the process group",
        ),
        (
            "wall_timeout_terminates_the_process_group_and_reports_the_ledgered_limit",
            "crates/haider-tools/tests/process_tools_tests.rs",
            "wall-clock deadline terminates the process group",
        ),
        (
            "dropping_process_execution_cancels_and_kills_the_child_group",
            "crates/haider-tools/tests/process_tools_tests.rs",
            "dropped daemon tool execution wakes supervised process-group cancellation",
        ),
        (
            "same_inode_cwd_moved_outside_and_symlinked_back_is_refused_before_spawn",
            "crates/haider-tools/tests/process_tools_tests.rs",
            "fresh no-follow cwd rewalk rejects same-inode relocation outside the workspace",
        ),
    ];
    let tests = sweep
        .iter()
        .map(|(test, _, _)| *test)
        .collect::<std::collections::HashSet<_>>();
    let seams = sweep
        .iter()
        .map(|(_, _, seam)| *seam)
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(sweep.len(), 29);
    assert_eq!(tests.len(), sweep.len());
    assert_eq!(seams.len(), sweep.len());
    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root");
    for (test, file, _) in &sweep {
        let path = workspace_root.join(file);
        assert!(
            path.is_file(),
            "manifest coordinate for `{test}` does not exist: {file}"
        );
        let source = fs::read_to_string(&path).expect("manifest coordinate is readable");
        assert!(
            source.contains(&format!("fn {test}")),
            "manifest coordinate {file} no longer defines `{test}`"
        );
    }
}

// MUTATION CHECK (P3-4 park-not-cancel): revert the supervisor's drain arm
// to cancel a parked `request_input` turn (replace the InputRequired park
// with `active_cancel.cancel()`). Expected failure: the graceful drain
// terminalizes the run, the post-restart attach replays no reconstructable
// checkpoint, and the answer below cannot complete the run — the requests()
// == 2 and no-terminal-during-drain assertions fail.
// Verified by revert on 2026-07-27.
#[tokio::test]
async fn graceful_drain_parks_a_request_input_checkpoint_for_recovery() {
    let root = test_root("w3c-live-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let config = DaemonConfig::new(
        "drain-parks-request-input",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let (dependencies, fake) = fake_dependencies(vec![
        FakeStep::EmitRequestInput {
            call_id: "park-choice".into(),
            kind: FakeInputKind::Choice,
            title: "Park me".into(),
            body: vec!["This menu survives a GRACEFUL restart".into()],
            options: vec![FakeInputOption {
                key: "continue".into(),
                label: "Continue".into(),
                detail: None,
            }],
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "park-choice".into(),
        },
        FakeStep::EmitText {
            text: "resumed after graceful drain".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let first_task = ready_with_dependencies(&config, dependencies.clone()).await;
    let mut first = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "park-before",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(&mut first, &config, &workspace).await;
    send_request(
        &mut first,
        &config,
        "park-submit",
        submit_body(
            "park-command",
            session_id.clone(),
            generation,
            "ask across graceful restart",
        ),
    )
    .await;
    let (run_id, _) = next_submit_response(&mut first).await;
    let (menu_id, request_seq, opening_generation) = loop {
        if let WireFrame::Event { envelope, .. } = first.next().await
            && envelope.run_id.as_ref() == Some(&run_id)
            && let Ok(EventPayload::MenuOpened(menu)) =
                serde_json::from_value::<EventPayload>(envelope.payload.into())
        {
            break (menu.id, envelope.seq, envelope.worker_generation);
        }
    };
    // Wait for the checkpoint to be DURABLY parked (InputRequired commits
    // after MenuOpened): the park law covers a parked run; a drain racing
    // the parking commit itself legitimately cancels.
    loop {
        if let WireFrame::Event { envelope, .. } = first.next().await
            && envelope.run_id.as_ref() == Some(&run_id)
            && matches!(
                serde_json::from_value::<EventPayload>(envelope.payload.into()),
                Ok(EventPayload::RunState(RunState::InputRequired { .. }))
            )
        {
            break;
        }
    }
    assert_eq!(fake.requests().len(), 1);
    drop(first);

    // THE law under test: a GRACEFUL drain (not a crash) must preserve the
    // parked checkpoint exactly as the crash path does.
    first_task.shutdown_handle().request("routine restart");
    let outcome = first_task.join().await.expect("daemon joins");
    assert!(
        matches!(outcome, haider_daemon::ShutdownOutcome::Graceful),
        "parking must not force the drain: {outcome:?}"
    );

    let second_task = ready_with_dependencies(&config, dependencies).await;
    assert_eq!(
        fake.requests().len(),
        1,
        "recovery reconstructs the parked checkpoint without a provider request"
    );
    let mut second = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "w3c-live-test",
        "park-after",
        ClientKind::Headless,
    )
    .await;
    let replay_frames =
        attach_existing(&mut second, &config, session_id.clone(), 0, "park-replay").await;
    assert!(replay_frames.iter().any(|envelope| {
        envelope.run_id.as_ref() == Some(&run_id)
            && matches!(
                serde_json::from_value::<EventPayload>(envelope.payload.clone().into()),
                Ok(EventPayload::MenuOpened(ref menu)) if menu.id == menu_id
            )
    }));
    // The graceful drain appended NO terminal and NO cancellation for the
    // parked run: its durable tail is still the open request_input state.
    assert!(
        !replay_frames.iter().any(|envelope| {
            envelope.run_id.as_ref() == Some(&run_id)
                && matches!(
                    serde_json::from_value::<EventPayload>(envelope.payload.clone().into()),
                    Ok(EventPayload::RunState(
                        RunState::Cancelled | RunState::Cancelling | RunState::Errored
                    )) | Ok(EventPayload::RunFailed { .. })
                )
        }),
        "graceful drain must park, never cancel, a request_input checkpoint"
    );
    second
        .send(
            &WireFrame::MenuAnswer {
                request_id: Some(RequestId::new("park-answer")),
                command_id: CommandId::new("park-answer-command"),
                session_id: session_id.clone(),
                menu_id: menu_id.clone(),
                request_seq,
                worker_generation: opening_generation,
                option_key: "continue".into(),
                option_index: 0,
                input: None,
            },
            config.frame_limit,
        )
        .await;
    assert!(matches!(
        next_response(&mut second).await,
        WireFrame::Response {
            body: ResponseBody::MenuAnswer { .. },
            ..
        }
    ));
    let events = events_until_terminal(&mut second, &run_id).await;
    assert!(
        events
            .iter()
            .any(|(_, payload)| *payload == EventPayload::RunState(RunState::Done))
    );
    let durable = read_session(&mut second, &config, session_id, "park-durable-read").await;
    assert_eq!(
        payloads_for_run(&durable, &run_id)
            .filter(|payload| matches!(payload, EventPayload::MenuOpened(_)))
            .count(),
        1,
        "request_input executes once; the graceful restart only replays its durable menu"
    );
    let requests = fake.requests();
    assert_eq!(requests.len(), 2, "only the post-answer request runs");
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| message.tool_result_for("park-choice").is_some())
    );

    second_task.shutdown_handle().request("test complete");
    second_task.join().await.expect("daemon joins");
}
