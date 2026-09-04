//! v0.0.967 core-loop gate: production daemon + real IPC, with only the
//! provider boundary scripted. Tools, delegation, store, projection, and RPC
//! framing are the production implementations.

#![allow(clippy::expect_used)]

mod support;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use haider_accounts::{MemoryVault, Vault as _};
use haider_core::{CancelToken, ToolDispatchResult, ToolDispatcher};
use haider_daemon::{
    DaemonConfig, DaemonDependencies, ProviderFactory, ProviderFactoryConfig, ResolvedTurnProvider,
    TurnToolFactory, WorkerToolContext,
};
use haider_protocol::DeliveryMode;
use haider_protocol::EventPayload;
use haider_protocol::cache::CacheRequestAttemptV1;
use haider_protocol::effect::{EffectClass, FileFreshness};
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::headless::{
    HeadlessRunEventPayload, HeadlessRunSpecV1, RunBudgetDecisionReasonV1, RunBudgetDimensionV1,
    RunBudgetExhaustedV1, RunBudgetV1,
};
use haider_protocol::ids::{CredentialAlias, ItemId, MenuId, RunId, SessionId};
use haider_protocol::item::{ItemDelta, ItemEvent, OutputStream, TurnItem};
use haider_protocol::loom::LoomAgentType;
use haider_protocol::menu::{Menu, MenuAnswer};
use haider_protocol::provider::{Block, CapabilityDoc, FinishReason, Usage, UsageSource};
use haider_protocol::session::{
    SessionInteractionModeV1, SessionMetadataV1, SessionPermissionOverridesV1,
};
use haider_protocol::session_fork::{ForkContextEpoch, SessionForkPromptSelector, SessionForked};
use haider_protocol::state::RunState;
use haider_provider::{
    AnthropicProvider, FakeProvider, FakeStep, PreparedTurn, Provider, ProviderError,
    ProviderStream, ToolDefinition, TurnRequest,
};
use haider_rpc::{
    AttachMode, CancelStatus, ClientKind, CommandId, FleetAgentStateWire, MenuInput, RequestBody,
    RequestId, ResponseBody, SeqRange, SessionFleetSnapshot, SshAuthInputWire, SshProfileInputWire,
    SshScopeWire, WireFrame,
};
use haider_tools::SessionGrant;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use support::{UdsClient, ready_with_dependencies, test_root};
use tokio::sync::Semaphore;

// This binary has five tests that launch the production Windows PowerShell,
// and two also launch real descendants. Running them concurrently on the
// hosted Windows runner starves process creation and pushes the whole binary
// against its 900-second crate cap. Keep protocol-only tests parallel and
// serialize only these real-process tests, matching live_turn_rpc_tests.
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
struct RoutingFactory {
    providers: Arc<BTreeMap<String, Arc<dyn Provider>>>,
    fail_on_resolve: Arc<BTreeSet<String>>,
    max_provider_requests_per_turn: Option<usize>,
}

struct CacheAwareFixtureProvider {
    renderer: AnthropicProvider,
    scripted: Arc<FakeProvider>,
}

/// Holds exactly one physical provider open after the daemon has admitted and
/// journaled it. The next open crosses the same scripted boundary normally,
/// so restart recovery, not a synthetic provider failure, decides the run.
struct CrashAfterAdmissionProvider {
    scripted: Arc<FakeProvider>,
    blocked_ordinal: usize,
    attempts: AtomicUsize,
    requests: StdMutex<Vec<TurnRequest>>,
    entered: Semaphore,
}

impl CrashAfterAdmissionProvider {
    fn new(script: Vec<FakeStep>, blocked_ordinal: usize) -> Self {
        Self {
            scripted: Arc::new(FakeProvider::new(script)),
            blocked_ordinal,
            attempts: AtomicUsize::new(0),
            requests: StdMutex::new(Vec::new()),
            entered: Semaphore::new(0),
        }
    }

    async fn wait_until_blocked(&self) {
        self.entered
            .acquire()
            .await
            .expect("crash-admission barrier remains open")
            .forget();
    }

    fn requests(&self) -> Vec<TurnRequest> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

struct RefreshBarrier {
    needle: String,
    occurrence: usize,
    matches: AtomicUsize,
    armed: AtomicBool,
    entered: Semaphore,
    release: tokio::sync::Notify,
}

impl RefreshBarrier {
    fn new(needle: impl Into<String>, occurrence: usize) -> Self {
        Self {
            needle: needle.into(),
            occurrence,
            matches: AtomicUsize::new(0),
            armed: AtomicBool::new(true),
            entered: Semaphore::new(0),
            release: tokio::sync::Notify::new(),
        }
    }

    async fn wait_until_entered(&self) {
        self.entered
            .acquire()
            .await
            .expect("refresh barrier remains open")
            .forget();
    }

    async fn pause_if_armed(&self, tail: &Option<String>) {
        if tail
            .as_deref()
            .is_some_and(|tail| tail.contains(&self.needle))
            && self.matches.fetch_add(1, Ordering::AcqRel) + 1 == self.occurrence
            && self.armed.swap(false, Ordering::AcqRel)
        {
            self.entered.add_permits(1);
            self.release.notified().await;
        }
    }
}

struct BarrierToolFactory {
    inner: Arc<dyn TurnToolFactory>,
    barrier: Arc<RefreshBarrier>,
}

struct BarrierDispatcher {
    inner: Arc<dyn ToolDispatcher>,
    barrier: Arc<RefreshBarrier>,
}

#[async_trait]
impl ToolDispatcher for BarrierDispatcher {
    async fn preflight_tool_call(&self, name: &str) -> Result<(), HaiderError> {
        self.inner.preflight_tool_call(name).await
    }

    async fn execute(
        &self,
        run_id: &RunId,
        item_id: &ItemId,
        call_id: &str,
        name: &str,
        args: serde_json::Value,
        cancel: &CancelToken,
    ) -> Result<ToolDispatchResult, HaiderError> {
        self.inner
            .execute(run_id, item_id, call_id, name, args, cancel)
            .await
    }

    async fn refresh_volatile_context_tail(&self) -> Result<Option<String>, HaiderError> {
        let tail = self.inner.refresh_volatile_context_tail().await?;
        self.barrier.pause_if_armed(&tail).await;
        Ok(tail)
    }

    async fn activate_approval(
        &self,
        run_id: &RunId,
        checkpoint: &haider_core::RequestInputCheckpoint,
    ) -> Result<(), HaiderError> {
        self.inner.activate_approval(run_id, checkpoint).await
    }

    async fn resolve_approval(&self, menu: &Menu, answer: &MenuAnswer) -> Result<(), HaiderError> {
        self.inner.resolve_approval(menu, answer).await
    }

    async fn cancel(&self) -> Result<(), HaiderError> {
        self.inner.cancel().await
    }

    async fn close(&self) -> Result<(), HaiderError> {
        self.inner.close().await
    }
}

#[async_trait]
impl TurnToolFactory for BarrierToolFactory {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.inner.definitions()
    }

    fn shared_definitions(&self) -> Arc<[ToolDefinition]> {
        self.inner.shared_definitions()
    }

    async fn create(
        &self,
        context: WorkerToolContext,
    ) -> Result<Option<Arc<dyn ToolDispatcher>>, HaiderError> {
        Ok(self.inner.create(context).await?.map(|inner| {
            Arc::new(BarrierDispatcher {
                inner,
                barrier: Arc::clone(&self.barrier),
            }) as Arc<dyn ToolDispatcher>
        }))
    }

    async fn create_with_turn_snapshot(
        &self,
        context: WorkerToolContext,
        durable_grants: Vec<SessionGrant>,
        durable_bindings: HashMap<MenuId, (EffectClass, String)>,
        durable_freshness: HashMap<String, FileFreshness>,
        effect_dispatched: Arc<AtomicBool>,
    ) -> Result<Option<Arc<dyn ToolDispatcher>>, HaiderError> {
        Ok(self
            .inner
            .create_with_turn_snapshot(
                context,
                durable_grants,
                durable_bindings,
                durable_freshness,
                effect_dispatched,
            )
            .await?
            .map(|inner| {
                Arc::new(BarrierDispatcher {
                    inner,
                    barrier: Arc::clone(&self.barrier),
                }) as Arc<dyn ToolDispatcher>
            }))
    }
}

#[async_trait]
impl Provider for CacheAwareFixtureProvider {
    fn prepare_turn(&self, request: &TurnRequest) -> Option<PreparedTurn> {
        self.renderer.prepare_turn(request)
    }

    fn prepare_turn_with_tools(
        &self,
        request: &TurnRequest,
        tools: &[ToolDefinition],
    ) -> Option<PreparedTurn> {
        self.renderer.prepare_turn_with_tools(request, tools)
    }

    async fn capabilities(&self) -> CapabilityDoc {
        self.scripted.capabilities().await
    }

    async fn stream_turn(&self, request: TurnRequest) -> Result<ProviderStream, ProviderError> {
        self.scripted.stream_turn(request).await
    }
}

#[async_trait]
impl Provider for CrashAfterAdmissionProvider {
    async fn capabilities(&self) -> CapabilityDoc {
        self.scripted.capabilities().await
    }

    async fn stream_turn(&self, request: TurnRequest) -> Result<ProviderStream, ProviderError> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request.clone());
        let ordinal = self.attempts.fetch_add(1, Ordering::AcqRel) + 1;
        if ordinal == self.blocked_ordinal {
            self.entered.add_permits(1);
            return std::future::pending().await;
        }
        self.scripted.stream_turn(request).await
    }
}

#[async_trait]
impl ProviderFactory for RoutingFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, HaiderError> {
        if self.fail_on_resolve.contains(&metadata.provider) {
            return Err(HaiderError::new(
                ErrorCode::Internal,
                format!("fixture provider `{}` failed to launch", metadata.provider),
                false,
            ));
        }
        let provider = self.providers.get(&metadata.provider).ok_or_else(|| {
            HaiderError::new(
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
            account_alias: Some(format!("{}-fixture-account", metadata.provider)),
            active_no_auth: false,
            initial_rotation: None,
            rotation_budget_consumed: false,
            attempt_resolver: None,
            compaction_promotion: None,
        })
    }

    fn max_provider_requests_per_turn_override(&self) -> Option<usize> {
        self.max_provider_requests_per_turn
    }
}

fn dependencies(
    providers: impl IntoIterator<Item = (String, Arc<dyn Provider>)>,
    fail_on_resolve: impl IntoIterator<Item = String>,
) -> DaemonDependencies {
    let providers = BTreeMap::from_iter(providers);
    let creatable = providers
        .keys()
        .cloned()
        .chain(fail_on_resolve)
        .collect::<BTreeSet<_>>();
    let fail_on_resolve = creatable
        .iter()
        .filter(|provider| !providers.contains_key(*provider))
        .cloned()
        .collect();
    DaemonDependencies {
        provider_factory: ProviderFactoryConfig::Injected {
            factory: Arc::new(RoutingFactory {
                providers: Arc::new(providers),
                fail_on_resolve: Arc::new(fail_on_resolve),
                max_provider_requests_per_turn: None,
            }),
            providers: creatable,
        },
        ..DaemonDependencies::default()
    }
}

fn fake_dependencies_with_request_limit(
    script: Vec<FakeStep>,
    limit: usize,
) -> (DaemonDependencies, Arc<FakeProvider>) {
    let fake = Arc::new(FakeProvider::new(script));
    let provider: Arc<dyn Provider> = fake.clone();
    let mut dependencies = dependencies([("fake".into(), provider)], []);
    let ProviderFactoryConfig::Injected { factory, providers } = dependencies.provider_factory
    else {
        unreachable!("fixture dependencies always use an injected provider factory")
    };
    let routing = RoutingFactory {
        providers: Arc::new(BTreeMap::from([(
            "fake".into(),
            fake.clone() as Arc<dyn Provider>,
        )])),
        fail_on_resolve: Arc::new(BTreeSet::new()),
        max_provider_requests_per_turn: Some(limit),
    };
    let _ = factory;
    dependencies.provider_factory = ProviderFactoryConfig::Injected {
        factory: Arc::new(routing),
        providers,
    };
    (dependencies, fake)
}

fn fake_dependencies(script: Vec<FakeStep>) -> (DaemonDependencies, Arc<FakeProvider>) {
    let fake = Arc::new(FakeProvider::new(script));
    let provider: Arc<dyn Provider> = fake.clone();
    (dependencies([("fake".into(), provider)], []), fake)
}

fn cache_aware_dependencies(script: Vec<FakeStep>) -> (DaemonDependencies, Arc<FakeProvider>) {
    let vault = MemoryVault::default();
    let alias = CredentialAlias::new("core-loop-cache-secret");
    vault
        .put(&alias, b"fixture-secret-never-sent")
        .expect("stage fixture credential");
    let renderer = AnthropicProvider::new_custom_no_auth(
        vault.resolve(&alias).expect("resolve fixture credential"),
        "cache-model",
        "http://127.0.0.1:18181/v1",
    )
    .expect("construct real Anthropic renderer")
    .with_prompt_caching_verified(true);
    let fake = Arc::new(FakeProvider::new(script));
    let provider: Arc<dyn Provider> = Arc::new(CacheAwareFixtureProvider {
        renderer,
        scripted: fake.clone(),
    });
    (dependencies([("cache-fixture".into(), provider)], []), fake)
}

fn tool_round(
    call_id: &str,
    name: &str,
    args: serde_json::Value,
    continuation: &str,
) -> Vec<FakeStep> {
    vec![
        FakeStep::EmitToolCall {
            call_id: call_id.into(),
            name: name.into(),
            args,
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: call_id.into(),
        },
        FakeStep::EmitText {
            text: continuation.into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]
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

async fn next_response(client: &mut UdsClient) -> WireFrame {
    loop {
        let frame = client.next().await;
        if matches!(frame, WireFrame::Response { .. }) {
            return frame;
        }
    }
}

async fn create_and_attach(
    client: &mut UdsClient,
    config: &DaemonConfig,
    workspace: &Path,
    provider: &str,
    model: &str,
    permission_overrides: Option<SessionPermissionOverridesV1>,
    ssh_scope: Option<SshScopeWire>,
) -> (SessionId, u64) {
    create_and_attach_with_mode(
        client,
        config,
        workspace,
        provider,
        model,
        permission_overrides,
        ssh_scope,
        SessionInteractionModeV1::Interactive,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn create_and_attach_with_mode(
    client: &mut UdsClient,
    config: &DaemonConfig,
    workspace: &Path,
    provider: &str,
    model: &str,
    permission_overrides: Option<SessionPermissionOverridesV1>,
    ssh_scope: Option<SshScopeWire>,
    interaction_mode: SessionInteractionModeV1,
) -> (SessionId, u64) {
    send_request(
        client,
        config,
        "create",
        RequestBody::SessionCreateWithPermissionOverrides {
            command_id: CommandId::new("create-command"),
            cwd: workspace.to_string_lossy().into_owned(),
            provider: provider.into(),
            model: model.into(),
            max_tokens: 4096,
            permission_overrides,
            cache_policy: None,
            interaction_mode,
            ssh_scope,
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

async fn submit_turn(
    client: &mut UdsClient,
    config: &DaemonConfig,
    command_id: &str,
    session_id: SessionId,
    generation: u64,
    text: &str,
) -> RunId {
    send_request(
        client,
        config,
        command_id,
        RequestBody::TurnSubmit {
            command_id: CommandId::new(command_id),
            session_id,
            worker_generation: generation,
            text: text.into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Queue,
        },
    )
    .await;
    loop {
        match client.next().await {
            WireFrame::Response {
                body: ResponseBody::TurnSubmit { run_id, .. },
                ..
            } => return run_id,
            WireFrame::Response {
                body: ResponseBody::Error { code, message, .. },
                ..
            } => panic!("turn.submit `{command_id}` failed ({code}): {message}"),
            WireFrame::ProtocolError(error) => {
                panic!("turn.submit `{command_id}` failed: {error}")
            }
            _ => {}
        }
    }
}

async fn register_workflow_chain(
    client: &mut UdsClient,
    config: &DaemonConfig,
    workflow_id: &str,
    node_names: &[&str],
) {
    for (index, node) in node_names.iter().enumerate() {
        let agent_id = node.to_ascii_lowercase();
        send_request(
            client,
            config,
            &format!("register-agent-{agent_id}"),
            RequestBody::LoomRegisterAgentType {
                record: LoomAgentType {
                    id: agent_id.clone(),
                    name: format!("{node} fixture"),
                    job: format!("produce artifact {}", index + 1),
                    in_type: format!("Artifact{index}"),
                    out_type: format!("Artifact{}", index + 1),
                    clis: Vec::new(),
                    apis: Vec::new(),
                    denials: Vec::new(),
                    skills: Vec::new(),
                    scripts: Vec::new(),
                    color: String::new(),
                    glyph: String::new(),
                    rev: 0,
                },
                expected_rev: Some(0),
                expected_digest: None,
            },
        )
        .await;
        match next_response(client).await {
            WireFrame::Response {
                body: ResponseBody::LoomRegistered { .. },
                ..
            } => {}
            other => panic!("registering agent type {agent_id} failed: {other:?}"),
        }
    }
    let terminal_type = format!("Artifact{}", node_names.len());
    let nodes = node_names
        .iter()
        .map(|node| {
            format!(
                "{} @{} \"produce {}\" :cmd",
                node.to_ascii_lowercase(),
                node.to_ascii_lowercase(),
                node.to_ascii_lowercase()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    send_request(
        client,
        config,
        "register-workflow",
        RequestBody::LoomRegisterWorkflow {
            source: format!("{workflow_id}: Artifact0 -> {terminal_type}\n{nodes}"),
            expected_rev: Some(0),
            expected_digest: None,
        },
    )
    .await;
    match next_response(client).await {
        WireFrame::Response {
            body: ResponseBody::LoomRegistered { .. },
            ..
        } => {}
        other => panic!("registering workflow {workflow_id} failed: {other:?}"),
    }
}

async fn pin_workflow(
    client: &mut UdsClient,
    config: &DaemonConfig,
    session_id: SessionId,
    generation: u64,
    workflow_id: &str,
) {
    send_request(
        client,
        config,
        "pin-workflow",
        RequestBody::GraphPin {
            command_id: CommandId::new("pin-workflow-command"),
            session_id,
            worker_generation: generation,
            template: workflow_id.into(),
            expected_digest: None,
        },
    )
    .await;
    match next_response(client).await {
        WireFrame::Response {
            body: ResponseBody::GraphPin { .. },
            ..
        } => {}
        other => panic!("pinning workflow {workflow_id} failed: {other:?}"),
    }
}

async fn start_headless_run(
    client: &mut UdsClient,
    config: &DaemonConfig,
    workspace: &Path,
    session_id: SessionId,
    generation: u64,
    command_id: &str,
) -> RunId {
    start_headless_run_with_budget(
        client,
        config,
        workspace,
        session_id,
        generation,
        command_id,
        "fake",
        "fake-v1",
        RunBudgetV1::default(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn start_headless_run_with_budget(
    client: &mut UdsClient,
    config: &DaemonConfig,
    workspace: &Path,
    session_id: SessionId,
    generation: u64,
    command_id: &str,
    provider: &str,
    model: &str,
    budget: RunBudgetV1,
) -> RunId {
    send_request(
        client,
        config,
        command_id,
        RequestBody::HeadlessRunStart {
            command_id: CommandId::new(command_id),
            session_id,
            worker_generation: generation,
            text: "execute the pinned workflow".into(),
            attachments: Vec::new(),
            spec: HeadlessRunSpecV1 {
                cwd: workspace.to_string_lossy().into_owned(),
                provider: provider.into(),
                model: model.into(),
                max_output_tokens: 4096,
                effort: None,
                fast: false,
                seed: Some(968),
                permission_overrides: SessionPermissionOverridesV1::default(),
                trust_hooks: false,
                budget,
                request_deadline_unix_ms: None,
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
            } => return run_id,
            WireFrame::Response {
                body: ResponseBody::Error { code, message, .. },
                ..
            } => panic!("headless start `{command_id}` failed ({code}): {message}"),
            _ => {}
        }
    }
}

fn workflow_script(node_names: &[&str]) -> Vec<FakeStep> {
    node_names
        .iter()
        .enumerate()
        .flat_map(|(index, node)| {
            tool_round(
                &format!("evidence-{}", index + 1),
                "graph_evidence",
                serde_json::json!({
                    "node": node,
                    "verdict": "green",
                    "detail": format!("artifact-marker-{}", index + 1),
                }),
                &format!("stage-{}-finished", index + 1),
            )
        })
        .collect()
}

fn reported_usage(input: u64) -> Usage {
    Usage {
        input,
        output: 0,
        reasoning: 0,
        cached: 0,
        source: UsageSource::ProviderReported,
        account: None,
        accounts: Vec::new(),
        normalized: None,
        scope: None,
        cache_cost: None,
        request: None,
    }
}

fn budget_facts(envelopes: &[RawEnvelope], run_id: &RunId) -> Vec<RunBudgetExhaustedV1> {
    envelopes
        .iter()
        .filter(|envelope| envelope.run_id.as_ref() == Some(run_id))
        .filter_map(|envelope| {
            HeadlessRunEventPayload::from_payload_value(&envelope.payload).and_then(|payload| {
                match payload {
                    HeadlessRunEventPayload::RunBudgetExhausted(exhausted) => Some(exhausted),
                    HeadlessRunEventPayload::HeadlessRunConfigured(_)
                    | HeadlessRunEventPayload::RunDeadlineExceeded(_) => None,
                }
            })
        })
        .collect()
}

fn provider_request_attempts(
    envelopes: &[RawEnvelope],
    run_id: &RunId,
) -> Vec<CacheRequestAttemptV1> {
    envelopes
        .iter()
        .filter(|envelope| envelope.run_id.as_ref() == Some(run_id))
        .filter_map(|envelope| {
            serde_json::from_value::<EventPayload>(envelope.payload.clone().into())
                .ok()
                .and_then(|payload| match payload {
                    EventPayload::Item(ItemEvent::Completed { item, .. }) => {
                        CacheRequestAttemptV1::from_extension_item(&item)
                    }
                    _ => None,
                })
        })
        .collect()
}

async fn wait_for_delegated_question(client: &mut UdsClient, run_id: &RunId) -> (Menu, u64, u64) {
    // Registry #94: the seam's headless run has a 30s absolute budget. The
    // observer owns that complete production wait plus 1s for publication.
    tokio::time::timeout(std::time::Duration::from_secs(31), async {
        loop {
            let Some(frame) = client.try_next().await else {
                panic!("connection closed while waiting for delegated question")
            };
            let WireFrame::Event { envelope, .. } = frame else {
                continue;
            };
            if envelope.run_id.as_ref() != Some(run_id) {
                continue;
            }
            let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload.into())
            else {
                continue;
            };
            match payload {
                EventPayload::MenuOpened(menu) if menu.origin == "delegated-child" => {
                    return (menu, envelope.seq, envelope.worker_generation);
                }
                EventPayload::RunFailed { code, message, .. } => {
                    panic!("parent failed before surfacing child question: {code:?}: {message}")
                }
                EventPayload::RunState(state) if state.is_terminal() => {
                    panic!("parent terminalized before surfacing child question: {state:?}")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("delegated question is surfaced inside the run budget")
}

async fn wait_for_cancelled_child_reap(
    client: &mut UdsClient,
    config: &DaemonConfig,
    parent_session: SessionId,
    request_prefix: &str,
) -> (SessionId, Vec<RawEnvelope>) {
    // Registry #94: delegated cancellation reserves a 1s settlement tail;
    // the outer 5s bound is 1s production cleanup + 4s scheduling/store I/O.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut attempt = 0_u32;
        loop {
            let snapshot = fleet(
                client,
                config,
                parent_session.clone(),
                &format!("{request_prefix}-fleet-{attempt}"),
            )
            .await;
            let Some(node) = snapshot.roots.first() else {
                attempt = attempt.saturating_add(1);
                tokio::task::yield_now().await;
                continue;
            };
            assert!(
                matches!(
                    node.state,
                    FleetAgentStateWire::Live | FleetAgentStateWire::Cancelled
                ),
                "budget cleanup produced the wrong child terminal: {:?}",
                node.state
            );
            let child_session = node.session_id.clone();
            let journal = read_session(
                client,
                config,
                child_session.clone(),
                &format!("{request_prefix}-child-read-{attempt}"),
            )
            .await;
            let cancelled = journal.iter().position(|envelope| {
                serde_json::from_value::<EventPayload>(envelope.payload.clone().into()).is_ok_and(
                    |payload| matches!(payload, EventPayload::RunState(RunState::Cancelled)),
                )
            });
            let idle = journal.iter().position(|envelope| {
                serde_json::from_value::<EventPayload>(envelope.payload.clone().into()).is_ok_and(
                    |payload| {
                        matches!(
                            payload,
                            EventPayload::SessionState(
                                haider_protocol::state::SessionState::Idle { .. }
                            )
                        )
                    },
                )
            });
            if node.state == FleetAgentStateWire::Cancelled
                && cancelled
                    .zip(idle)
                    .is_some_and(|(cancelled, idle)| idle > cancelled)
            {
                return (child_session, journal);
            }
            attempt = attempt.saturating_add(1);
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("budget-cancelled child reaches its durable Idle reap fence")
}

fn request_contains(request: &TurnRequest, needle: &str) -> bool {
    request.messages.iter().any(|message| {
        message
            .blocks
            .iter()
            .any(|block| matches!(block, Block::Text { text } if text.contains(needle)))
    })
}

async fn workflow_graph_state(
    client: &mut UdsClient,
    config: &DaemonConfig,
    session_id: SessionId,
) -> haider_protocol::graph::WorkflowGraphState {
    send_request(
        client,
        config,
        "workflow-state",
        RequestBody::WorkflowGraphState {
            session_id,
            graph_id: None,
        },
    )
    .await;
    match next_response(client).await {
        WireFrame::Response {
            body: ResponseBody::WorkflowGraphState { state: Some(state) },
            ..
        } => state,
        other => panic!("reading workflow graph state failed: {other:?}"),
    }
}

async fn events_until_terminal(client: &mut UdsClient, run_id: &RunId) -> Vec<EventPayload> {
    let (state, failure, events) = events_until_any_terminal(client, run_id).await;
    match state {
        RunState::Done => events,
        state => panic!(
            "run {run_id} reached unexpected terminal state {state:?}; failure={failure:?}; observed_events={}",
            events.len()
        ),
    }
}

async fn events_until_any_terminal(
    client: &mut UdsClient,
    run_id: &RunId,
) -> (RunState, Option<(ErrorCode, String)>, Vec<EventPayload>) {
    let mut events = Vec::new();
    let mut last_state = None;
    let mut failure = None;
    let completed = tokio::time::timeout(support::DEADLINE, async {
        loop {
            let Some(frame) = client.try_next().await else {
                let reason = format!(
                    "connection closed while waiting for run {run_id} to terminalize; last_state={last_state:?}; failure={failure:?}; observed_events={}",
                    events.len()
                );
                client.report_connection_failure(&reason);
                panic!("{reason}");
            };
            if let WireFrame::Event { envelope, .. } = frame {
                if envelope.run_id.as_ref() != Some(run_id) {
                    continue;
                }
                let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload.into()) else {
                    continue;
                };
                let terminal = match &payload {
                    EventPayload::RunFailed { code, message, .. } => {
                        failure = Some((*code, message.clone()));
                        false
                    }
                    EventPayload::RunState(state) => {
                        last_state = Some(state.clone());
                        state.is_terminal()
                    }
                    _ => false,
                };
                events.push(payload);
                if terminal {
                    return;
                }
            }
        }
    })
    .await;
    if completed.is_err() {
        let reason = format!(
            "run {run_id} did not reach a terminal event within {:?}; last_state={last_state:?}; failure={failure:?}; observed_events={}",
            support::DEADLINE,
            events.len()
        );
        client.report_connection_failure(&reason);
        panic!("{reason}");
    }
    (
        last_state.expect("terminal payload records a run state"),
        failure,
        events,
    )
}

async fn wait_for_session_idle(client: &mut UdsClient, session_id: &SessionId) {
    tokio::time::timeout(support::DEADLINE, async {
        loop {
            let WireFrame::Event { envelope, .. } = client.next().await else {
                continue;
            };
            if &envelope.session_id != session_id {
                continue;
            }
            if serde_json::from_value::<EventPayload>(envelope.payload.into()).is_ok_and(
                |payload| {
                    matches!(
                        payload,
                        EventPayload::SessionState(
                            haider_protocol::state::SessionState::Idle { .. }
                        )
                    )
                },
            ) {
                return;
            }
        }
    })
    .await
    .expect("session reaches durable idle after its terminal run")
}

async fn cancel_and_collect_terminal(
    client: &mut UdsClient,
    config: &DaemonConfig,
    session_id: SessionId,
    generation: u64,
    run_id: RunId,
    label: &str,
) -> Vec<EventPayload> {
    send_request(
        client,
        config,
        label,
        RequestBody::TurnCancel {
            command_id: CommandId::new(format!("{label}-command")),
            session_id,
            worker_generation: generation,
            run_id: run_id.clone(),
        },
    )
    .await;
    tokio::time::timeout(support::DEADLINE, async {
        let mut response_seen = false;
        let mut events = Vec::new();
        loop {
            match client.next().await {
                WireFrame::Response {
                    body: ResponseBody::TurnCancel { .. },
                    ..
                } => response_seen = true,
                WireFrame::Event { envelope, .. } if envelope.run_id.as_ref() == Some(&run_id) => {
                    let Ok(payload) =
                        serde_json::from_value::<EventPayload>(envelope.payload.into())
                    else {
                        continue;
                    };
                    let terminal = matches!(payload, EventPayload::RunState(RunState::Cancelled));
                    events.push(payload);
                    if terminal && response_seen {
                        return events;
                    }
                }
                _ => {}
            }
        }
    })
    .await
    .expect("cancellation response and terminal event")
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
    loop {
        if let WireFrame::Response {
            body: ResponseBody::SessionRead { result },
            ..
        } = client.next().await
        {
            return result.envelopes;
        }
    }
}

async fn attach_existing(
    client: &mut UdsClient,
    config: &DaemonConfig,
    session_id: SessionId,
    request_id: &str,
) -> Vec<RawEnvelope> {
    send_request(
        client,
        config,
        request_id,
        RequestBody::SessionAttach {
            session_id,
            after_seq: 0,
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

async fn fleet(
    client: &mut UdsClient,
    config: &DaemonConfig,
    session_id: SessionId,
    request_id: &str,
) -> SessionFleetSnapshot {
    send_request(
        client,
        config,
        request_id,
        RequestBody::SessionFleet { session_id },
    )
    .await;
    loop {
        if let WireFrame::Response {
            body: ResponseBody::SessionFleet { snapshot },
            ..
        } = client.next().await
        {
            return snapshot;
        }
    }
}

async fn tools_inventory(
    client: &mut UdsClient,
    config: &DaemonConfig,
    session_id: SessionId,
    request_id: &str,
) -> haider_protocol::tool::ToolInventorySnapshot {
    send_request(
        client,
        config,
        request_id,
        RequestBody::ToolsInventory { session_id },
    )
    .await;
    loop {
        if let WireFrame::Response {
            body: ResponseBody::ToolsInventory { inventory, .. },
            ..
        } = client.next().await
        {
            return inventory;
        }
    }
}

async fn submit_turn_allow_always(
    client: &mut UdsClient,
    config: &DaemonConfig,
    session_id: SessionId,
    generation: u64,
    text: &str,
) -> (RunId, Vec<EventPayload>) {
    let run_id = submit_turn(
        client,
        config,
        "allow-always-turn",
        session_id.clone(),
        generation,
        text,
    )
    .await;
    let events = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let mut events = Vec::new();
        loop {
            let frame = client.next().await;
            let WireFrame::Event { envelope, .. } = frame else {
                continue;
            };
            if envelope.run_id.as_ref() != Some(&run_id) {
                continue;
            }
            let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload.into())
            else {
                continue;
            };
            if let EventPayload::MenuOpened(menu) = &payload
                && let Some((index, option)) =
                    menu.options.iter().enumerate().find(|(_, option)| {
                        option.decision == Some(haider_protocol::menu::DecisionKind::AllowAlways)
                    })
            {
                let option_key = option.key.clone();
                client
                    .send(
                        &WireFrame::MenuAnswer {
                            request_id: Some(RequestId::new("allow-always-answer")),
                            command_id: CommandId::new("allow-always-answer-command"),
                            session_id: session_id.clone(),
                            menu_id: menu.id.clone(),
                            request_seq: envelope.seq,
                            worker_generation: envelope.worker_generation,
                            option_key,
                            option_index: u32::try_from(index).expect("menu option index"),
                            input: None,
                        },
                        config.frame_limit,
                    )
                    .await;
            }
            let terminal = matches!(payload, EventPayload::RunState(RunState::Done));
            events.push(payload);
            if terminal {
                return events;
            }
        }
    })
    .await
    .expect("allow-always turn reaches Done");
    (run_id, events)
}

async fn assert_isolated_test_passes(test_name: &str, marker: &str) {
    let executable = std::env::current_exe().expect("current integration-test executable");
    let mut command = tokio::process::Command::new(executable);
    command
        .arg("--exact")
        .arg(test_name)
        .arg("--nocapture")
        .env(marker, "1")
        .kill_on_drop(true);
    let output = tokio::time::timeout(std::time::Duration::from_secs(8), command.output())
        .await
        .unwrap_or_else(|_| panic!("isolated regression `{test_name}` hung past 8 seconds"))
        .expect("launch isolated regression process");
    assert!(
        output.status.success(),
        "isolated regression `{test_name}` failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn continuation_seen(events: &[EventPayload], marker: &str) -> bool {
    events.iter().any(|payload| {
        matches!(
            payload,
            EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::AgentMessage { text },
                ..
            }) if text.contains(marker)
        )
    })
}

fn tool_preview<'a>(events: &'a [EventPayload], call_id: &str) -> &'a str {
    events
        .iter()
        .find_map(|payload| match payload {
            EventPayload::ToolResult {
                call_id: seen,
                result,
            } if seen == call_id => Some(result.preview.as_str()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing tool result for {call_id}"))
}

fn stdout_bytes(events: &[EventPayload]) -> Vec<u8> {
    events
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
        .collect()
}

fn init_git_workspace(workspace: &Path) {
    let status = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(workspace)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .status()
        .expect("initialize git fixture");
    assert!(status.success(), "git fixture initializes");
}

#[cfg(unix)]
fn background_child_command() -> String {
    "(sleep 0.05; printf child) & printf leader; wait".into()
}

#[cfg(windows)]
fn background_child_command() -> String {
    concat!(
        "$job=Start-Job { Start-Sleep -Milliseconds 50; [Console]::Out.Write('child') };",
        "[Console]::Out.Write('leader');Wait-Job $job|Out-Null;Receive-Job $job"
    )
    .into()
}

#[cfg(unix)]
fn outliving_pipe_child_command() -> String {
    "sh ./outliving-leader.sh".into()
}

#[cfg(windows)]
fn outliving_pipe_child_command() -> String {
    "cmd.exe /D /S /C outliving-leader.cmd".into()
}

#[cfg(unix)]
fn install_outliving_pipe_fixture(workspace: &Path) {
    fs::write(
        workspace.join("outliving-leader.sh"),
        concat!(
            "#!/bin/sh\n",
            "(sleep 0.2; printf ran > outliving-child-ran.log; printf child) &\n",
            "printf leader\n",
            "exit 0\n"
        ),
    )
    .expect("write outliving-child leader script");
}

#[cfg(windows)]
fn install_outliving_pipe_fixture(workspace: &Path) {
    fs::write(
        workspace.join("outliving-leader.cmd"),
        concat!(
            "@echo off\r\n",
            "start \"\" /b cmd.exe /D /S /C \"ping -n 2 127.0.0.1 >nul & ",
            "echo ran>outliving-child-ran.log & ^<nul set /p =child\"\r\n",
            "<nul set /p =leader\r\n",
            "exit /b 0\r\n"
        ),
    )
    .expect("write outliving-child leader script");
}

#[cfg(unix)]
fn simple_output_command() -> String {
    "printf nonrepo-real-output".into()
}

#[cfg(windows)]
fn simple_output_command() -> String {
    "[Console]::Out.Write('nonrepo-real-output')".into()
}

#[cfg(unix)]
fn no_output_command() -> String {
    ":".into()
}

#[cfg(windows)]
fn no_output_command() -> String {
    "$null=1".into()
}

#[cfg(unix)]
fn large_output_command(bytes: usize) -> String {
    format!("/usr/bin/head -c {bytes} /dev/zero")
}

#[cfg(unix)]
fn shell_round_trip_command() -> String {
    "printf shell-real-output; printf shell-side-effect > shell.txt".into()
}

#[cfg(windows)]
fn shell_round_trip_command() -> String {
    concat!(
        "[Console]::Out.Write('shell-real-output');",
        "[IO.File]::WriteAllText('shell.txt','shell-side-effect',[Text.Encoding]::ASCII)"
    )
    .into()
}

#[cfg(unix)]
fn cancellable_process_command() -> String {
    concat!(
        "(printf started > descendant-started.log; sleep 0.5; ",
        "printf survived > descendant-survived.log) & ",
        "while :; do printf x >> heartbeat.log; sleep 0.01; done"
    )
    .into()
}

#[cfg(windows)]
fn cancellable_process_command() -> String {
    concat!(
        "$workspace=(Get-Location).Path;[Environment]::CurrentDirectory=$workspace;",
        "$start=[Diagnostics.ProcessStartInfo]::new();",
        "$start.FileName=(Join-Path ([Environment]::SystemDirectory) 'cmd.exe');",
        "$start.Arguments='/D /S /C \"echo started>descendant-started.log & ",
        "ping -n 3 127.0.0.1 >nul & echo survived>descendant-survived.log\"';",
        "$start.WorkingDirectory=$workspace;$start.UseShellExecute=$false;",
        "$child=[Diagnostics.Process]::Start($start);",
        "if($null -eq $child){throw 'descendant did not start'};$child.Dispose();",
        "$heartbeat=Join-Path $workspace 'heartbeat.log';",
        "while($true){[IO.File]::AppendAllText($heartbeat,'x',[Text.Encoding]::ASCII);",
        "Start-Sleep -Milliseconds 10}"
    )
    .into()
}

#[cfg(windows)]
fn large_output_command(bytes: usize) -> String {
    format!(
        "$b=New-Object byte[] {bytes};$s=[Console]::OpenStandardOutput();$s.Write($b,0,$b.Length)"
    )
}

#[cfg(not(windows))]
fn create_large_sparse_file(path: &Path, length: u64) -> std::io::Result<()> {
    fs::File::create(path)?.set_len(length)
}

#[cfg(windows)]
fn create_large_sparse_file(path: &Path, length: u64) -> std::io::Result<()> {
    let file = fs::File::create(path)?;
    haider_platform::fs::mark_file_sparse(&file)?;
    file.set_len(length)
}

async fn assert_headless_workflow_chain_completes(test_id: &str, node_names: &[&str]) {
    let root = test_root(&format!("{test_id}-"));
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let (dependencies, fake) = fake_dependencies(workflow_script(node_names));
    let config = DaemonConfig::new(
        test_id,
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        test_id,
        "workflow-client",
        ClientKind::Headless,
    )
    .await;
    register_workflow_chain(&mut client, &config, test_id, node_names).await;
    let (session_id, generation) = create_and_attach_with_mode(
        &mut client,
        &config,
        &workspace,
        "fake",
        "fake-v1",
        None,
        None,
        SessionInteractionModeV1::Autonomous,
    )
    .await;
    pin_workflow(
        &mut client,
        &config,
        session_id.clone(),
        generation,
        test_id,
    )
    .await;
    let run_id = start_headless_run(
        &mut client,
        &config,
        &workspace,
        session_id.clone(),
        generation,
        "headless-workflow",
    )
    .await;
    let _events = events_until_terminal(&mut client, &run_id).await;
    let requests = fake.requests();
    assert_eq!(
        requests.len(),
        node_names.len() * 2,
        "the fake script supplies exactly two terminal request segments per stage"
    );
    for (index, node) in node_names.iter().enumerate().skip(1) {
        let request = &requests[index * 2];
        assert!(
            request_contains(request, &format!("artifact-marker-{index}")),
            "stage {node} did not receive its predecessor's exact CAS artifact"
        );
        assert!(
            request_contains(
                request,
                &format!("daemon bound workflow {test_id} node {node}")
            ),
            "stage {node} did not receive its current typed executor binding"
        );
    }
    let state = workflow_graph_state(&mut client, &config, session_id).await;
    assert_eq!(
        state.phase,
        haider_protocol::graph::WorkflowGraphPhase::Completed
    );
    assert!(state.nodes.iter().all(|node| {
        node.phase == haider_protocol::graph::WorkflowNodePhase::Completed
            && !node.outputs.is_empty()
    }));
    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// P0 workflow-continuation gate. This is also the requested mutation check:
/// moving volatile refresh back outside the logical request loop makes the
/// IMPLEMENT call retain PLAN authority and the run ends workflow_unfinished.
#[tokio::test]
async fn headless_three_stage_workflow_completes_with_forwarded_artifacts() {
    assert_headless_workflow_chain_completes(
        "headless-three-stage-workflow",
        &["PLAN", "IMPLEMENT", "VERIFY"],
    )
    .await;
}

#[tokio::test]
async fn headless_five_stage_workflow_has_no_two_hop_ceiling() {
    assert_headless_workflow_chain_completes(
        "headless-five-stage-workflow",
        &["PLAN", "IMPLEMENT", "VERIFY", "PACKAGE", "PUBLISH"],
    )
    .await;
}

#[tokio::test]
async fn headless_workflow_provider_request_cap_returns_typed_loop_limit() {
    let test_id = "headless-workflow-loop-limit";
    let nodes = ["PLAN", "IMPLEMENT", "VERIFY"];
    let root = test_root("headless-workflow-loop-limit-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let request_limit = 2;
    assert!(request_limit < nodes.len());
    let (dependencies, fake) =
        fake_dependencies_with_request_limit(workflow_script(&nodes), request_limit);
    let config = DaemonConfig::new(
        test_id,
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        test_id,
        "loop-limit-client",
        ClientKind::Headless,
    )
    .await;
    register_workflow_chain(&mut client, &config, test_id, &nodes).await;
    let (session_id, generation) = create_and_attach_with_mode(
        &mut client,
        &config,
        &workspace,
        "fake",
        "fake-v1",
        None,
        None,
        SessionInteractionModeV1::Autonomous,
    )
    .await;
    pin_workflow(
        &mut client,
        &config,
        session_id.clone(),
        generation,
        test_id,
    )
    .await;
    let run_id = start_headless_run(
        &mut client,
        &config,
        &workspace,
        session_id,
        generation,
        "headless-loop-limit",
    )
    .await;
    let (state, failure, events) = events_until_any_terminal(&mut client, &run_id).await;
    assert_eq!(state, RunState::Errored);
    assert!(matches!(failure, Some((ErrorCode::LoopLimit, _))));
    assert!(!events.iter().any(|payload| {
        matches!(
            payload,
            EventPayload::RunFailed {
                code: ErrorCode::WorkflowUnfinished,
                ..
            }
        )
    }));
    assert_eq!(fake.requests().len(), request_limit);
    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// Seam: budget admission owns the terminal between autonomous workflow
/// requests. A mutation that checks only at run finalization reports
/// workflow_unfinished/loop_limit or opens the second provider request.
#[tokio::test]
async fn workflow_hop_cost_cap_terminalizes_budget_before_request_two() {
    let test_id = "workflow-hop-budget-seam";
    let root = test_root("workflow-hop-budget-seam-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let script = vec![
        FakeStep::EmitUsage {
            usage: reported_usage(180_000),
        },
        FakeStep::EmitToolCall {
            call_id: "budget-hop-evidence".into(),
            name: "graph_evidence".into(),
            args: serde_json::json!({
                "node": "PLAN",
                "verdict": "green",
                "detail": "budget-hop-one-complete",
            }),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "budget-hop-evidence".into(),
        },
        FakeStep::EmitText {
            text: "request two must remain unopened".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ];
    let fake = Arc::new(FakeProvider::new(script));
    let provider: Arc<dyn Provider> = fake.clone();
    let config = DaemonConfig::new(
        test_id,
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let task =
        ready_with_dependencies(&config, dependencies([("openai".into(), provider)], [])).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        test_id,
        "workflow-budget-client",
        ClientKind::Headless,
    )
    .await;
    register_workflow_chain(&mut client, &config, test_id, &["PLAN"]).await;
    let (session_id, generation) = create_and_attach_with_mode(
        &mut client,
        &config,
        &workspace,
        "openai",
        "gpt-5.6-sol",
        None,
        None,
        SessionInteractionModeV1::Autonomous,
    )
    .await;
    pin_workflow(
        &mut client,
        &config,
        session_id.clone(),
        generation,
        test_id,
    )
    .await;
    let run_id = start_headless_run_with_budget(
        &mut client,
        &config,
        &workspace,
        session_id.clone(),
        generation,
        "workflow-budget-run",
        "openai",
        "gpt-5.6-sol",
        RunBudgetV1 {
            max_cost_microusd: Some(1_000_000),
            // Registry #94: the non-Windows support deadline is 60s = this
            // 30s run budget + 1s terminal tail + 29s scheduling. Windows'
            // 92s platform wrapper is independently derived in support.
            max_time_ms: Some(30_000),
            ..RunBudgetV1::default()
        },
    )
    .await;
    let (state, failure, events) = events_until_any_terminal(&mut client, &run_id).await;
    assert_eq!(state, RunState::Errored);
    assert!(matches!(failure, Some((ErrorCode::BudgetExhausted, _))));
    assert!(!events.iter().any(|payload| matches!(
        payload,
        EventPayload::RunFailed {
            code: ErrorCode::WorkflowUnfinished | ErrorCode::LoopLimit,
            ..
        }
    )));
    assert_eq!(
        fake.requests().len(),
        1,
        "the cap must bind before workflow request two reaches transport"
    );
    let journal = read_session(&mut client, &config, session_id, "workflow-budget-journal").await;
    assert_eq!(
        provider_request_attempts(&journal, &run_id).len(),
        1,
        "the durable journal records exactly one provider request attempt"
    );
    let facts = budget_facts(&journal, &run_id);
    assert_eq!(facts.len(), 1, "one typed budget terminal cause");
    assert_eq!(facts[0].dimension, RunBudgetDimensionV1::Cost);
    let decision = facts[0]
        .decision
        .as_ref()
        .expect("projected request-two decision");
    assert_eq!(decision.reason, RunBudgetDecisionReasonV1::ProjectedRequest);
    assert!(decision.spent < decision.cap);
    assert!(
        decision
            .projected
            .is_some_and(|projected| decision.spent.saturating_add(projected) > decision.cap)
    );

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

#[tokio::test]
async fn headless_workflow_resumes_after_daemon_crash_between_stages() {
    let test_id = "headless-workflow-crash-recovery";
    let nodes = ["PLAN", "IMPLEMENT", "VERIFY"];
    let root = test_root("headless-workflow-crash-recovery-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let (mut dependencies, fake) = fake_dependencies(workflow_script(&nodes));
    let barrier = Arc::new(RefreshBarrier::new(
        "daemon bound workflow headless-workflow-crash-recovery node VERIFY",
        2,
    ));
    dependencies.tool_factory = Arc::new(BarrierToolFactory {
        inner: Arc::clone(&dependencies.tool_factory),
        barrier: Arc::clone(&barrier),
    });
    let config = DaemonConfig::new(
        test_id,
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let first_task = ready_with_dependencies(&config, dependencies.clone()).await;
    let mut first_client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        test_id,
        "crash-client",
        ClientKind::Headless,
    )
    .await;
    register_workflow_chain(&mut first_client, &config, test_id, &nodes).await;
    let (session_id, generation) = create_and_attach_with_mode(
        &mut first_client,
        &config,
        &workspace,
        "fake",
        "fake-v1",
        None,
        None,
        SessionInteractionModeV1::Autonomous,
    )
    .await;
    pin_workflow(
        &mut first_client,
        &config,
        session_id.clone(),
        generation,
        test_id,
    )
    .await;
    let run_id = start_headless_run(
        &mut first_client,
        &config,
        &workspace,
        session_id.clone(),
        generation,
        "headless-crash-run",
    )
    .await;
    tokio::time::timeout(support::DEADLINE, barrier.wait_until_entered())
        .await
        .expect("VERIFY request-boundary barrier is reached");
    assert_eq!(
        fake.requests().len(),
        4,
        "the daemon crashed after IMPLEMENT finalization and before VERIFY request attempt"
    );
    drop(first_client);
    first_task.crash().await;

    let second_task = ready_with_dependencies(&config, dependencies).await;
    let mut second_client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        test_id,
        "restart-client",
        ClientKind::Headless,
    )
    .await;
    let replay = attach_existing(
        &mut second_client,
        &config,
        session_id.clone(),
        "attach-after-crash",
    )
    .await;
    let replay_terminal = replay.iter().filter_map(|envelope| {
        (envelope.run_id.as_ref() == Some(&run_id))
            .then(|| serde_json::from_value::<EventPayload>(envelope.payload.clone().into()).ok())
            .flatten()
            .and_then(|payload| match payload {
                EventPayload::RunState(state) if state.is_terminal() => Some(state),
                _ => None,
            })
    });
    if !replay_terminal
        .into_iter()
        .any(|state| state == RunState::Done)
    {
        let events = events_until_terminal(&mut second_client, &run_id).await;
        assert!(
            events
                .iter()
                .any(|payload| matches!(payload, EventPayload::RunState(RunState::Done)))
        );
    }
    let requests = fake.requests();
    assert_eq!(requests.len(), 6);
    assert!(request_contains(&requests[4], "artifact-marker-2"));
    assert!(request_contains(
        &requests[4],
        "daemon bound workflow headless-workflow-crash-recovery node VERIFY"
    ));
    let state = workflow_graph_state(&mut second_client, &config, session_id).await;
    assert_eq!(
        state.phase,
        haider_protocol::graph::WorkflowGraphPhase::Completed
    );
    second_task.shutdown_handle().request("test complete");
    second_task.join().await.expect("daemon joins");
}

/// Seam: a provider attempt admitted and durably marked immediately before a
/// crash is neither spend nor a reason to forget prior spend. Recovery must
/// retry the interrupted workflow request once, retain its logical ordinal,
/// and finish under a cap that covers every completed exchange.
#[tokio::test]
async fn workflow_recovery_after_budget_admission_preserves_spend_and_ordinal() {
    let test_id = "workflow-budget-admission-recovery";
    let nodes = ["PLAN"];
    let root = test_root("workflow-budget-admission-recovery-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let provider = Arc::new(CrashAfterAdmissionProvider::new(
        vec![
            FakeStep::EmitUsage {
                usage: reported_usage(1_000),
            },
            FakeStep::EmitToolCall {
                call_id: "recovery-evidence-1".into(),
                name: "graph_evidence".into(),
                args: serde_json::json!({
                    "node": "PLAN",
                    "verdict": "green",
                    "detail": "recovery-artifact-1",
                }),
            },
            FakeStep::Finish {
                reason: FinishReason::ToolUse,
            },
            FakeStep::ExpectToolResult {
                call_id: "recovery-evidence-1".into(),
            },
            FakeStep::EmitUsage {
                usage: reported_usage(1_000),
            },
            FakeStep::EmitText {
                text: "recovery stage one finished".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ],
        1,
    ));
    let routed: Arc<dyn Provider> = provider.clone();
    let dependencies = dependencies([("openai".into(), routed)], []);
    let config = DaemonConfig::new(
        test_id,
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let first_task = ready_with_dependencies(&config, dependencies.clone()).await;
    let mut first_client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        test_id,
        "admission-crash-client",
        ClientKind::Headless,
    )
    .await;
    register_workflow_chain(&mut first_client, &config, test_id, &nodes).await;
    let (session_id, generation) = create_and_attach_with_mode(
        &mut first_client,
        &config,
        &workspace,
        "openai",
        "gpt-5.6-sol",
        None,
        None,
        SessionInteractionModeV1::Autonomous,
    )
    .await;
    pin_workflow(
        &mut first_client,
        &config,
        session_id.clone(),
        generation,
        test_id,
    )
    .await;
    let run_id = start_headless_run_with_budget(
        &mut first_client,
        &config,
        &workspace,
        session_id.clone(),
        generation,
        "admission-crash-run",
        "openai",
        "gpt-5.6-sol",
        RunBudgetV1 {
            max_cost_microusd: Some(10_000_000),
            // Registry #94: the 31s admission observer below is this 30s
            // absolute run budget plus 1s for publication.
            max_time_ms: Some(30_000),
            ..RunBudgetV1::default()
        },
    )
    .await;
    // Registry #94/#95: the provider-open observer services Ping/Pong while
    // containing the run's 30s absolute budget plus 1s publication margin.
    tokio::time::timeout(std::time::Duration::from_secs(31), async {
        loop {
            tokio::select! {
                () = provider.wait_until_blocked() => break,
                frame = first_client.try_next() => {
                    let Some(frame) = frame else {
                        panic!("connection closed before the admitted provider barrier")
                    };
                    let WireFrame::Event { envelope, .. } = frame else {
                        continue;
                    };
                    if envelope.run_id.as_ref() != Some(&run_id) {
                        continue;
                    }
                    let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload.into()) else {
                        continue;
                    };
                    match payload {
                        EventPayload::RunFailed { code, message, .. } => panic!(
                            "run failed before reaching the admitted provider barrier: {code:?}: {message}"
                        ),
                        EventPayload::RunState(state) if state.is_terminal() => panic!(
                            "run terminalized before reaching the admitted provider barrier: {state:?}"
                        ),
                        _ => {}
                    }
                }
            }
        }
    })
    .await
    .expect("first admitted provider attempt reaches the crash barrier");
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(provider.scripted.requests().len(), 0);
    drop(first_client);
    first_task.crash().await;

    let second_task = ready_with_dependencies(&config, dependencies).await;
    let mut second_client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        test_id,
        "admission-restart-client",
        ClientKind::Headless,
    )
    .await;
    let replay = attach_existing(
        &mut second_client,
        &config,
        session_id.clone(),
        "admission-restart-attach",
    )
    .await;
    let replay_payloads = replay
        .iter()
        .filter(|envelope| envelope.run_id.as_ref() == Some(&run_id))
        .filter_map(|envelope| {
            serde_json::from_value::<EventPayload>(envelope.payload.clone().into()).ok()
        })
        .collect::<Vec<_>>();
    let replay_failure = replay_payloads.iter().find_map(|payload| match payload {
        EventPayload::RunFailed { code, message, .. } => Some((*code, message.clone())),
        _ => None,
    });
    let replay_terminal = replay_payloads.iter().find_map(|payload| match payload {
        EventPayload::RunState(state) if state.is_terminal() => Some(state.clone()),
        _ => None,
    });
    if let Some(state) = replay_terminal {
        assert_eq!(
            state,
            RunState::Done,
            "recovery misclassified the admitted pre-response attempt; failure={replay_failure:?}"
        );
        assert!(replay_failure.is_none());
    } else {
        let (state, failure, _) = events_until_any_terminal(&mut second_client, &run_id).await;
        assert_eq!(
            state,
            RunState::Done,
            "recovery misclassified the admitted pre-response attempt; failure={failure:?}"
        );
        assert!(failure.is_none());
    }
    assert_eq!(
        provider.requests().len(),
        3,
        "two completed workflow requests plus one abandoned pre-response attempt"
    );
    assert_eq!(provider.scripted.requests().len(), 2);
    let journal = read_session(
        &mut second_client,
        &config,
        session_id.clone(),
        "admission-recovery-journal",
    )
    .await;
    assert!(
        budget_facts(&journal, &run_id).is_empty(),
        "a sufficient cap must not become a missing-usage budget terminal"
    );
    let attempts = provider_request_attempts(&journal, &run_id);
    assert_eq!(attempts.len(), 3, "one durable marker per physical attempt");
    assert_eq!(
        attempts
            .iter()
            .map(|attempt| attempt.ordinal)
            .collect::<Vec<_>>(),
        vec![1, 2, 3],
        "restart retry and later hop each receive a fresh physical ordinal"
    );
    let usage = journal
        .iter()
        .filter(|envelope| envelope.run_id.as_ref() == Some(&run_id))
        .filter_map(|envelope| {
            serde_json::from_value::<EventPayload>(envelope.payload.clone().into())
                .ok()
                .and_then(|payload| match payload {
                    EventPayload::Usage(usage) => Some(usage),
                    _ => None,
                })
        })
        .collect::<Vec<_>>();
    assert_eq!(usage.len(), 2, "only completed exchanges contribute spend");
    assert_eq!(usage.iter().map(|usage| usage.input).sum::<u64>(), 2_000);
    let ordinals = usage
        .iter()
        .map(|usage| {
            usage
                .request
                .as_ref()
                .expect("budgeted usage carries request ordinal")
                .ordinal
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(ordinals.len(), 2, "recovery reused a logical ordinal");
    assert_eq!(
        workflow_graph_state(&mut second_client, &config, session_id)
            .await
            .phase,
        haider_protocol::graph::WorkflowGraphPhase::Completed
    );

    second_task.shutdown_handle().request("test complete");
    second_task.join().await.expect("daemon joins");
}

/// A1: one provider script forces every real tool result back through a
/// second provider request. Removing the dispatcher, output stream, receipt
/// bounds, background-child output, or terminal projection leaves a missing
/// marker/output assertion.
#[tokio::test]
async fn tool_calls_execute_and_continue_over_real_rpc() {
    #[cfg(windows)]
    let _windows_process_test =
        windows_real_process_test_guard("tool_calls_execute_and_continue_over_real_rpc").await;
    let root = test_root("core-loop-tools-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    fs::write(workspace.join(".gitignore"), "target/\n").expect("gitignore");
    fs::create_dir(workspace.join("target")).expect("ignored build directory");
    create_large_sparse_file(
        &workspace.join("target/ignored-build.bin"),
        1024 * 1024 * 1024 * 1024,
    )
    .expect("large sparse ignored build file");
    init_git_workspace(&workspace);

    let calls = [
        (
            "exec-desc",
            "process_exec",
            serde_json::json!({"command": background_child_command()}),
            "continued-exec-desc",
        ),
        (
            "exec-none",
            "process_exec",
            serde_json::json!({"command": no_output_command()}),
            "continued-exec-none",
        ),
        (
            "exec-large",
            "process_exec",
            serde_json::json!({"command": large_output_command(512 * 1024)}),
            "continued-exec-large",
        ),
        (
            "exec-cap",
            "process_exec",
            serde_json::json!({"command": large_output_command(2 * 1024 * 1024)}),
            "continued-exec-cap",
        ),
        (
            "write",
            "fs_write",
            serde_json::json!({"path": "note.txt", "content": "alpha needle\n"}),
            "continued-write",
        ),
        (
            "read",
            "fs_read",
            serde_json::json!({"path": "note.txt"}),
            "continued-read",
        ),
        (
            "edit",
            "fs_edit",
            serde_json::json!({"path": "note.txt", "edits": [{"old": "alpha", "new": "beta"}]}),
            "continued-edit",
        ),
        (
            "search",
            "fs_search",
            serde_json::json!({"path": ".", "pattern": "beta needle", "mode": "literal"}),
            "continued-search",
        ),
    ];
    let script = calls
        .iter()
        .flat_map(|(call_id, name, args, marker)| tool_round(call_id, name, args.clone(), marker))
        .collect();
    let (dependencies, fake) = fake_dependencies(script);
    let store_root = root.path().join("store");
    let config = DaemonConfig::new(
        "core-loop-tools",
        store_root.clone(),
        root.path().join("runtime"),
    );
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "core-loop-e2e",
        "tool-client",
        ClientKind::Headless,
    )
    .await;
    let overrides = SessionPermissionOverridesV1 {
        allow_writes: true,
        allow_exec: true,
        allow_mobile: false,
        auto_allow: false,
    };
    let (session, generation) = create_and_attach(
        &mut client,
        &config,
        &workspace,
        "fake",
        "fake-v1",
        Some(overrides),
        None,
    )
    .await;

    let mut observed = BTreeMap::new();
    for (index, (call_id, _, _, marker)) in calls.iter().enumerate() {
        let run = submit_turn(
            &mut client,
            &config,
            &format!("tool-turn-{index}"),
            session.clone(),
            generation,
            marker,
        )
        .await;
        let events = events_until_terminal(&mut client, &run).await;
        assert!(
            continuation_seen(&events, marker),
            "turn did not continue after {call_id}"
        );
        observed.insert(
            *call_id,
            (
                tool_preview(&events, call_id).to_owned(),
                stdout_bytes(&events),
            ),
        );
    }

    let descendant_output = observed["exec-desc"].1.as_slice();
    // The Unix shell waits for the descendant before the leader can exit, so
    // `child` is already in the kernel pipe at the capture boundary. When the
    // stop watch wins, the Unix reader directly drains that nonblocking pipe
    // to EOF or EAGAIN before closing, so reactor scheduling cannot lose it.
    #[cfg(unix)]
    assert_eq!(descendant_output, b"leaderchild");
    // Tokio services Windows child-pipe reads on its blocking pool. That read
    // result can still be pending when the stop watch becomes ready, so the
    // leader bytes are guaranteed but Receive-Job's boundary bytes are not.
    #[cfg(windows)]
    assert!(
        descendant_output == b"leader" || descendant_output == b"leaderchild",
        "Windows foreground output must retain leader bytes and may include descendant bytes ready at the leader-exit boundary: {descendant_output:?}"
    );
    assert!(observed["exec-none"].1.is_empty());
    assert_eq!(observed["exec-large"].1.len(), 512 * 1024);
    let cap: serde_json::Value =
        serde_json::from_str(&observed["exec-cap"].0).expect("capped process result JSON");
    assert_eq!(cap["status"], "failed");
    assert_eq!(cap["limits"]["max_output_bytes"], 1024 * 1024);
    assert!(observed["read"].0.contains("alpha needle"));
    assert!(observed["search"].0.contains("beta needle"));
    assert_eq!(
        fs::read_to_string(workspace.join("note.txt")).expect("edited file"),
        "beta needle\n"
    );
    assert_eq!(fake.requests().len(), calls.len() * 2);

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// Lane 967-P1 owner decision: natural leader completion leaves descendants
/// alone. Foreground capture closes at that boundary instead of waiting on an
/// inherited pipe, so late descendant bytes are not part of the tool result;
/// durable long-running output belongs on `background=true`.
#[tokio::test]
async fn process_exec_normal_completion_leaves_outliving_descendant_alone() {
    #[cfg(windows)]
    let _windows_process_test = windows_real_process_test_guard(
        "process_exec_normal_completion_leaves_outliving_descendant_alone",
    )
    .await;
    let root = test_root("core-loop-outliving-pipe-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    init_git_workspace(&workspace);
    install_outliving_pipe_fixture(&workspace);
    let (dependencies, fake) = fake_dependencies(tool_round(
        "outliving-pipe",
        "process_exec",
        serde_json::json!({"command": outliving_pipe_child_command()}),
        "continued-outliving-pipe",
    ));
    let config = DaemonConfig::new(
        "core-loop-outliving-pipe",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "core-loop-e2e",
        "outliving-pipe-client",
        ClientKind::Headless,
    )
    .await;
    let (session, generation) = create_and_attach(
        &mut client,
        &config,
        &workspace,
        "fake",
        "fake-v1",
        Some(SessionPermissionOverridesV1 {
            allow_exec: true,
            ..SessionPermissionOverridesV1::default()
        }),
        None,
    )
    .await;
    let run = submit_turn(
        &mut client,
        &config,
        "outliving-pipe-turn",
        session,
        generation,
        "close capture when the leader exits",
    )
    .await;
    let events = events_until_terminal(&mut client, &run).await;
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert!(
        workspace.join("outliving-child-ran.log").exists(),
        "foreground process_exec swept a descendant after its leader exited"
    );
    assert_eq!(stdout_bytes(&events), b"leader");
    assert!(continuation_seen(&events, "continued-outliving-pipe"));
    assert_eq!(fake.requests().len(), 2);

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// The 966 T2 shape: a non-repository root must reach spawn and return real
/// output; `coverage=unknown` is receipt truth, never a command failure.
#[tokio::test]
async fn process_exec_runs_in_a_non_repository_workspace() {
    #[cfg(windows)]
    let _windows_process_test =
        windows_real_process_test_guard("process_exec_runs_in_a_non_repository_workspace").await;
    let root = test_root("core-loop-nonrepo-");
    let workspace = root.path().join("plain-workspace");
    fs::create_dir(&workspace).expect("non-repository workspace");
    let (dependencies, fake) = fake_dependencies(tool_round(
        "nonrepo-exec",
        "process_exec",
        serde_json::json!({"command": simple_output_command()}),
        "continued-nonrepo",
    ));
    let config = DaemonConfig::new(
        "core-loop-nonrepo",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "core-loop-e2e",
        "nonrepo-client",
        ClientKind::Headless,
    )
    .await;
    let (session, generation) = create_and_attach(
        &mut client,
        &config,
        &workspace,
        "fake",
        "fake-v1",
        Some(SessionPermissionOverridesV1 {
            allow_exec: true,
            ..SessionPermissionOverridesV1::default()
        }),
        None,
    )
    .await;
    let run = submit_turn(
        &mut client,
        &config,
        "nonrepo-turn",
        session,
        generation,
        "run outside git",
    )
    .await;
    let events = events_until_terminal(&mut client, &run).await;
    assert_eq!(stdout_bytes(&events), b"nonrepo-real-output");
    assert!(continuation_seen(&events, "continued-nonrepo"));
    assert_eq!(fake.requests().len(), 2);

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// A1: the user `!` path is a real `shell.exec` RPC, not a provider tool
/// simulation. Its subprocess output and side effect become durable, then the
/// next provider turn observes the raw command record.
#[tokio::test]
async fn direct_shell_rpc_executes_and_is_visible_to_the_next_turn() {
    #[cfg(windows)]
    let _windows_process_test = windows_real_process_test_guard(
        "direct_shell_rpc_executes_and_is_visible_to_the_next_turn",
    )
    .await;
    let root = test_root("core-loop-shell-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let (dependencies, fake) = fake_dependencies(vec![
        FakeStep::EmitText {
            text: "provider-observed-shell-record".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let config = DaemonConfig::new(
        "core-loop-shell",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "core-loop-e2e",
        "shell-client",
        ClientKind::Headless,
    )
    .await;
    let (session, generation) = create_and_attach(
        &mut client,
        &config,
        &workspace,
        "fake",
        "fake-v1",
        None,
        None,
    )
    .await;
    let command = shell_round_trip_command();
    send_request(
        &mut client,
        &config,
        "shell-exec",
        RequestBody::ShellExec {
            command_id: CommandId::new("shell-command"),
            session_id: session.clone(),
            worker_generation: generation,
            command: command.clone(),
            cwd: None,
        },
    )
    .await;
    let shell_run = match next_response(&mut client).await {
        WireFrame::Response {
            body: ResponseBody::ShellExec {
                run_id: Some(run), ..
            },
            ..
        } => run,
        other => panic!("expected shell.exec receipt, got {other:?}"),
    };
    let shell_events = events_until_terminal(&mut client, &shell_run).await;
    assert_eq!(stdout_bytes(&shell_events), b"shell-real-output");
    assert_eq!(
        fs::read_to_string(workspace.join("shell.txt")).expect("shell side effect"),
        "shell-side-effect"
    );

    let next_run = submit_turn(
        &mut client,
        &config,
        "after-shell-turn",
        session,
        generation,
        "explain the shell result",
    )
    .await;
    let events = events_until_terminal(&mut client, &next_run).await;
    assert!(continuation_seen(&events, "provider-observed-shell-record"));
    assert_eq!(fake.requests().len(), 1);
    let provider_saw_raw_command = fake.requests()[0].messages.iter().any(|message| {
        message.blocks.iter().any(|block| {
            matches!(block, Block::Text { text } if text.contains("[user-initiated shell command]") && text.contains(&command))
        })
    });
    assert!(
        provider_saw_raw_command,
        "next turn did not see shell record"
    );

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// A1: cancellation crosses RPC into the worker and process supervisor. Both
/// the leader heartbeat and its independently running descendant must stop;
/// merely returning a timeout/error while leaving either process alive fails.
#[tokio::test]
async fn cancelling_process_exec_kills_the_real_process_group() {
    #[cfg(windows)]
    let _windows_process_test =
        windows_real_process_test_guard("cancelling_process_exec_kills_the_real_process_group")
            .await;
    let root = test_root("core-loop-process-cancel-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let (dependencies, _) = fake_dependencies(vec![
        FakeStep::EmitToolCall {
            call_id: "cancel-process".into(),
            name: "process_exec".into(),
            args: serde_json::json!({"command": cancellable_process_command()}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::Hang,
    ]);
    let config = DaemonConfig::new(
        "core-loop-process-cancel",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "core-loop-e2e",
        "process-cancel-client",
        ClientKind::Headless,
    )
    .await;
    let (session, generation) = create_and_attach(
        &mut client,
        &config,
        &workspace,
        "fake",
        "fake-v1",
        Some(SessionPermissionOverridesV1 {
            allow_exec: true,
            ..SessionPermissionOverridesV1::default()
        }),
        None,
    )
    .await;
    let run = submit_turn(
        &mut client,
        &config,
        "cancel-process-turn",
        session.clone(),
        generation,
        "start cancellable process tree",
    )
    .await;
    let heartbeat = workspace.join("heartbeat.log");
    let descendant_started = workspace.join("descendant-started.log");
    const STARTUP_KEEPALIVE_NONCE: u64 = u64::MAX - 2;
    let mut startup_last_state = None;
    let mut startup_failure = None;
    let startup = tokio::time::timeout(support::DEADLINE, async {
        let mut next_keepalive = tokio::time::Instant::now() + support::KEEPALIVE_INTERVAL;
        loop {
            if fs::metadata(&heartbeat).is_ok_and(|metadata| metadata.len() >= 2)
                && descendant_started.exists()
            {
                break;
            }
            if tokio::time::Instant::now() >= next_keepalive {
                // This wait observes external files rather than wire frames,
                // but it still owns a negotiated client. Keep that live peer
                // inside the daemon's 45-second read-idle contract even when
                // a contended Windows PowerShell start consumes most of the
                // outer operation budget. The later TurnCancel remains a
                // strict write; a genuinely closed connection must still fail.
                client
                    .send(
                        &WireFrame::Ping {
                            nonce: STARTUP_KEEPALIVE_NONCE,
                        },
                        config.frame_limit,
                    )
                    .await;
                loop {
                    let Some(frame) = client.try_next().await else {
                        let reason = format!(
                            "connection closed while waiting for process tree in run {run}; last_state={startup_last_state:?}; failure={startup_failure:?}"
                        );
                        client.report_connection_failure(&reason);
                        panic!("{reason}");
                    };
                    match frame {
                        WireFrame::Pong { nonce } if nonce == STARTUP_KEEPALIVE_NONCE => break,
                        WireFrame::Event { envelope, .. }
                            if envelope.run_id.as_ref() == Some(&run) =>
                        {
                            match serde_json::from_value::<EventPayload>(envelope.payload.into()) {
                                Ok(EventPayload::RunFailed { code, message, .. }) => {
                                    startup_failure = Some((code, message));
                                }
                                Ok(EventPayload::RunState(state)) => {
                                    startup_last_state = Some(state.clone());
                                    if state.is_terminal() {
                                        panic!(
                                            "run {run} reached terminal state {state:?} before its process tree started; failure={startup_failure:?}"
                                        );
                                    }
                                }
                                _ => {}
                            }
                        }
                        _ => {}
                    }
                }
                next_keepalive = tokio::time::Instant::now() + support::KEEPALIVE_INTERVAL;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok();
    if !startup {
        let heartbeat_bytes = fs::metadata(&heartbeat).map(|metadata| metadata.len()).ok();
        let reason = format!(
            "process tree for run {run} did not start within {:?}; heartbeat_bytes={heartbeat_bytes:?}; descendant_started={}; last_state={startup_last_state:?}; failure={startup_failure:?}",
            support::DEADLINE,
            descendant_started.exists()
        );
        client.report_connection_failure(&reason);
        panic!("{reason}");
    }

    let cancel_events = cancel_and_collect_terminal(
        &mut client,
        &config,
        session,
        generation,
        run,
        "cancel-process",
    )
    .await;
    assert!(cancel_events.contains(&EventPayload::RunState(RunState::Cancelled)));
    let stopped_size = fs::metadata(&heartbeat).expect("heartbeat exists").len();
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert_eq!(
        fs::metadata(&heartbeat)
            .expect("heartbeat remains inspectable")
            .len(),
        stopped_size,
        "cancelled leader kept running"
    );
    assert!(
        !workspace.join("descendant-survived.log").exists(),
        "outliving descendant escaped the process group"
    );

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// A2: provider A delegates to provider B through the public tool schema.
/// Both providers run, B's report returns through the tool result to A, and
/// the fleet projection carries the durable task/model/provider identity.
#[tokio::test]
async fn cross_provider_subagent_returns_to_parent_and_fleet_is_truthful() {
    let root = test_root("core-loop-cross-agent-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let parent = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "cross-spawn".into(),
            name: "spawn_subagent".into(),
            args: serde_json::json!({
                "task": "cross-provider-check",
                "prompt": "return the child report",
                "model": "child-model",
                "provider": "child-provider"
            }),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "cross-spawn".into(),
        },
        FakeStep::EmitText {
            text: "parent-observed-child-completion".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let child = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitText {
            text: "real-child-report".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let parent_provider: Arc<dyn Provider> = parent.clone();
    let child_provider: Arc<dyn Provider> = child.clone();
    let dependencies = dependencies(
        [
            ("parent-provider".into(), parent_provider),
            ("child-provider".into(), child_provider),
        ],
        [],
    );
    let config = DaemonConfig::new(
        "core-loop-cross-agent",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "core-loop-e2e",
        "cross-agent-client",
        ClientKind::Headless,
    )
    .await;
    let (session, generation) = create_and_attach(
        &mut client,
        &config,
        &workspace,
        "parent-provider",
        "parent-model",
        None,
        None,
    )
    .await;
    let run = submit_turn(
        &mut client,
        &config,
        "cross-agent-turn",
        session.clone(),
        generation,
        "delegate",
    )
    .await;
    let events = events_until_terminal(&mut client, &run).await;
    assert!(continuation_seen(
        &events,
        "parent-observed-child-completion"
    ));
    assert_eq!(parent.requests().len(), 2);
    assert_eq!(child.requests().len(), 1);
    assert_eq!(child.requests()[0].model, "child-model");

    let snapshot = fleet(&mut client, &config, session, "cross-agent-fleet").await;
    assert_eq!(snapshot.roots.len(), 1);
    let node = &snapshot.roots[0];
    assert_eq!(node.task, "cross-provider-check");
    assert_eq!(node.model.as_deref(), Some("child-model"));
    assert_eq!(node.provider.as_deref(), Some("child-provider"));
    assert!(
        node.callsign
            .as_deref()
            .is_some_and(|name| !name.is_empty())
    );
    assert_eq!(node.state, FleetAgentStateWire::Done);

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// Seam: a delegated request debits the root coordinator. Crossing the root
/// cap mid-stream must budget-terminalize the parent, cancel/reap the child,
/// and retain exactly one copy of the child's committed usage.
#[tokio::test]
async fn child_spend_crossing_parent_cap_is_counted_once_and_child_is_reaped() {
    let test_id = "child-spend-parent-budget-seam";
    let root = test_root("child-spend-parent-budget-seam-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let parent = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitUsage {
            usage: reported_usage(100),
        },
        FakeStep::EmitToolCall {
            call_id: "budget-child-spawn".into(),
            name: "spawn_subagent".into(),
            args: serde_json::json!({
                "task": "cross the root cap",
                "prompt": "report after a costly exchange",
                "provider": "anthropic",
                "model": "claude-sonnet-4-5",
                "budget_tokens": 4096,
            }),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::EmitText {
            text: "parent continuation must remain unopened".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let child = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitUsage {
            usage: reported_usage(400_000),
        },
        FakeStep::Hang,
    ]));
    let parent_provider: Arc<dyn Provider> = parent.clone();
    let child_provider: Arc<dyn Provider> = child.clone();
    let config = DaemonConfig::new(
        test_id,
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let task = ready_with_dependencies(
        &config,
        dependencies(
            [
                ("openai".into(), parent_provider),
                ("anthropic".into(), child_provider),
            ],
            [],
        ),
    )
    .await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        test_id,
        "child-budget-client",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach_with_mode(
        &mut client,
        &config,
        &workspace,
        "openai",
        "gpt-5.6-sol",
        None,
        None,
        SessionInteractionModeV1::Autonomous,
    )
    .await;
    let run_id = start_headless_run_with_budget(
        &mut client,
        &config,
        &workspace,
        session_id.clone(),
        generation,
        "child-budget-run",
        "openai",
        "gpt-5.6-sol",
        RunBudgetV1 {
            max_cost_microusd: Some(1_000_000),
            // Registry #94: events_until_any_terminal's non-Windows 60s is
            // this 30s run budget + 1s terminal tail + 29s scheduling;
            // support derives Windows' stronger platform wrapper separately.
            max_time_ms: Some(30_000),
            ..RunBudgetV1::default()
        },
    )
    .await;
    let (state, failure, _) = events_until_any_terminal(&mut client, &run_id).await;
    assert_eq!(state, RunState::Errored);
    assert!(matches!(failure, Some((ErrorCode::BudgetExhausted, _))));
    assert_eq!(parent.requests().len(), 1);
    assert_eq!(child.requests().len(), 1);

    let journal = read_session(
        &mut client,
        &config,
        session_id.clone(),
        "child-budget-parent-journal",
    )
    .await;
    let facts = budget_facts(&journal, &run_id);
    assert_eq!(facts.len(), 1, "child crossing emits one root budget fact");
    let fact = &facts[0];
    assert_eq!(fact.dimension, RunBudgetDimensionV1::Cost);
    assert_eq!(fact.usage.logical_input_tokens, 400_100);
    assert_eq!(fact.usage.total_tokens, 400_100);
    assert_eq!(fact.usage.estimated_cost_microusd, Some(1_200_500));
    let decision = fact.decision.as_ref().expect("actual child spend decision");
    assert_eq!(decision.reason, RunBudgetDecisionReasonV1::ActualUsage);
    assert_eq!(decision.spent, 1_200_500);
    assert_eq!(decision.projected, None);

    let (_child_session, _child_journal) =
        wait_for_cancelled_child_reap(&mut client, &config, session_id, "child-budget-reap").await;

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// Seam: InputRequired wins while spend remains below the cap. Answering the
/// parent-projected question resumes the child; only its next UsageUpdate may
/// cross the root cap. The delegated wait itself opens no provider request.
#[tokio::test]
async fn delegated_question_surfaces_before_later_child_spend_exhausts_budget() {
    let test_id = "delegated-question-budget-seam";
    let root = test_root("delegated-question-budget-seam-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let parent = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitUsage {
            usage: reported_usage(100),
        },
        FakeStep::EmitToolCall {
            call_id: "question-child-spawn".into(),
            name: "spawn_subagent".into(),
            args: serde_json::json!({
                "task": "ask before spending",
                "prompt": "ask one question, then finish",
                "provider": "anthropic",
                "model": "claude-sonnet-4-5",
                "budget_tokens": 4096,
            }),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::EmitText {
            text: "parent continuation must remain unopened".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let child = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitUsage {
            usage: reported_usage(100),
        },
        FakeStep::EmitRequestInput {
            call_id: "budget-child-question".into(),
            kind: haider_provider::FakeInputKind::Question,
            title: "which value?".into(),
            body: vec!["The answer should resume this delegated run.".into()],
            options: Vec::new(),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "budget-child-question".into(),
        },
        FakeStep::EmitUsage {
            usage: reported_usage(400_000),
        },
        FakeStep::Hang,
    ]));
    let parent_provider: Arc<dyn Provider> = parent.clone();
    let child_provider: Arc<dyn Provider> = child.clone();
    let config = DaemonConfig::new(
        test_id,
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let task = ready_with_dependencies(
        &config,
        dependencies(
            [
                ("openai".into(), parent_provider),
                ("anthropic".into(), child_provider),
            ],
            [],
        ),
    )
    .await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        test_id,
        "question-budget-client",
        ClientKind::Headless,
    )
    .await;
    // Headless admission requires Autonomous session policy. The merged
    // delegation contract must therefore keep a child's projected question
    // answerable even though ordinary autonomous input fails closed.
    let (session_id, generation) = create_and_attach_with_mode(
        &mut client,
        &config,
        &workspace,
        "openai",
        "gpt-5.6-sol",
        None,
        None,
        SessionInteractionModeV1::Autonomous,
    )
    .await;
    let run_id = start_headless_run_with_budget(
        &mut client,
        &config,
        &workspace,
        session_id.clone(),
        generation,
        "question-budget-run",
        "openai",
        "gpt-5.6-sol",
        RunBudgetV1 {
            max_cost_microusd: Some(1_000_000),
            // Registry #94: both the 31s menu observer and the later 60s
            // terminal observer contain this 30s run budget; their margins
            // are respectively 1s publication and 1s tail + 29s scheduling.
            max_time_ms: Some(30_000),
            ..RunBudgetV1::default()
        },
    )
    .await;
    let (menu, request_seq, worker_generation) =
        wait_for_delegated_question(&mut client, &run_id).await;
    assert_eq!(parent.requests().len(), 1);
    assert_eq!(child.requests().len(), 1);
    let parked = read_session(
        &mut client,
        &config,
        session_id.clone(),
        "question-budget-parked-journal",
    )
    .await;
    assert!(
        budget_facts(&parked, &run_id).is_empty(),
        "delegated wait consumed budget admission"
    );

    client
        .send(
            &WireFrame::MenuAnswer {
                request_id: Some(RequestId::new("question-budget-answer")),
                command_id: CommandId::new("question-budget-answer-command"),
                session_id: session_id.clone(),
                menu_id: menu.id,
                request_seq,
                worker_generation,
                option_key: String::new(),
                option_index: 0,
                input: Some(MenuInput::Text {
                    text: "resume after this answer".into(),
                }),
            },
            config.frame_limit,
        )
        .await;
    match next_response(&mut client).await {
        WireFrame::Response {
            body: ResponseBody::MenuAnswer { .. },
            ..
        } => {}
        other => panic!("delegated answer failed: {other:?}"),
    }
    let (state, failure, _) = events_until_any_terminal(&mut client, &run_id).await;
    assert_eq!(state, RunState::Errored);
    assert!(matches!(failure, Some((ErrorCode::BudgetExhausted, _))));
    assert!(!matches!(failure, Some((ErrorCode::ProviderTimeout, _))));
    assert_eq!(parent.requests().len(), 1);
    assert_eq!(
        child.requests().len(),
        2,
        "the answer, not the delegated wait, admits child request two"
    );
    let journal = read_session(
        &mut client,
        &config,
        session_id,
        "question-budget-terminal-journal",
    )
    .await;
    let facts = budget_facts(&journal, &run_id);
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].dimension, RunBudgetDimensionV1::Cost);
    assert_eq!(facts[0].usage.logical_input_tokens, 400_200);
    assert_eq!(facts[0].usage.total_tokens, 400_200);
    assert_eq!(
        facts[0].decision.as_ref().map(|decision| &decision.reason),
        Some(&RunBudgetDecisionReasonV1::ActualUsage)
    );

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// A2: a child whose provider cannot resolve must become a typed failed
/// descendant and still give the parent a tool result it can continue from.
#[tokio::test]
async fn subagent_launch_failure_returns_to_parent_instead_of_hanging() {
    let root = test_root("core-loop-agent-launch-failure-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let parent = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "failed-spawn".into(),
            name: "spawn_subagent".into(),
            args: serde_json::json!({
                "task": "must-fail-to-launch",
                "prompt": "this provider fails during worker resolution",
                "model": "missing-model",
                "provider": "missing-provider"
            }),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "failed-spawn".into(),
        },
        FakeStep::EmitText {
            text: "parent-observed-launch-failure".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let parent_provider: Arc<dyn Provider> = parent.clone();
    let dependencies = dependencies(
        [("parent-provider".into(), parent_provider)],
        ["missing-provider".into()],
    );
    let config = DaemonConfig::new(
        "core-loop-agent-launch-failure",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "core-loop-e2e",
        "agent-launch-failure-client",
        ClientKind::Headless,
    )
    .await;
    let (session, generation) = create_and_attach(
        &mut client,
        &config,
        &workspace,
        "parent-provider",
        "parent-model",
        None,
        None,
    )
    .await;
    let run = submit_turn(
        &mut client,
        &config,
        "failed-agent-turn",
        session.clone(),
        generation,
        "delegate to a provider that fails",
    )
    .await;
    let events = events_until_terminal(&mut client, &run).await;
    assert!(continuation_seen(&events, "parent-observed-launch-failure"));
    let snapshot = fleet(&mut client, &config, session, "failed-agent-fleet").await;
    assert_eq!(snapshot.roots.len(), 1);
    assert_eq!(snapshot.roots[0].state, FleetAgentStateWire::Failed);
    assert_eq!(snapshot.roots[0].model.as_deref(), Some("missing-model"));
    assert_eq!(
        snapshot.roots[0].provider.as_deref(),
        Some("missing-provider")
    );

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// A2: an operator cancellation of a live child crosses the same RPC surface
/// used by clients. The child becomes durably Cancelled, its parent receives
/// the collapsed result, and no provider task is left holding the parent.
///
/// MUTATION CHECK: route through child `turn.cancel`, skip the durable cancel
/// transition, or fail to collect the cancelled report. Expected runtime
/// failure: the public response, terminal fleet state, or parent continuation
/// assertion below times out or changes shape.
#[tokio::test]
async fn manually_cancelled_running_child_releases_parent() {
    let root = test_root("core-loop-agent-cancel-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let parent = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "cancel-child-spawn".into(),
            name: "spawn_subagent".into(),
            args: serde_json::json!({
                "task": "cancel-me",
                "prompt": "remain active until an operator cancels",
                "model": "child-model",
                "provider": "child-provider"
            }),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "cancel-child-spawn".into(),
        },
        FakeStep::EmitText {
            text: "parent-observed-child-cancellation".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let child = Arc::new(FakeProvider::new(vec![FakeStep::Hang]));
    let parent_provider: Arc<dyn Provider> = parent.clone();
    let child_provider: Arc<dyn Provider> = child.clone();
    let config = DaemonConfig::new(
        "core-loop-agent-cancel",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let task = ready_with_dependencies(
        &config,
        dependencies(
            [
                ("parent-provider".into(), parent_provider),
                ("child-provider".into(), child_provider),
            ],
            [],
        ),
    )
    .await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "core-loop-e2e",
        "agent-cancel-client",
        ClientKind::Headless,
    )
    .await;
    let (session, generation) = create_and_attach(
        &mut client,
        &config,
        &workspace,
        "parent-provider",
        "parent-model",
        None,
        None,
    )
    .await;
    let parent_run = submit_turn(
        &mut client,
        &config,
        "cancel-child-parent-turn",
        session.clone(),
        generation,
        "spawn a cancellable child",
    )
    .await;
    let child_agent = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let snapshot = fleet(
                &mut client,
                &config,
                session.clone(),
                "cancel-child-fleet-wait",
            )
            .await;
            let Some(node) = snapshot
                .roots
                .first()
                .filter(|node| node.state == FleetAgentStateWire::Live)
            else {
                tokio::task::yield_now().await;
                continue;
            };
            break node.agent_id.clone();
        }
    })
    .await
    .expect("running child has a durable agent coordinate");

    send_request(
        &mut client,
        &config,
        "cancel-running-child",
        RequestBody::AgentCancel {
            command_id: CommandId::new("cancel-running-child-command"),
            session_id: session.clone(),
            worker_generation: generation,
            agent: child_agent.clone(),
        },
    )
    .await;
    let cancel_response = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        next_response(&mut client),
    )
    .await
    .expect("child-cancel response arrives");
    assert!(
        matches!(
            cancel_response,
            WireFrame::Response {
                body: ResponseBody::AgentCancel {
                    status: CancelStatus::Accepted,
                    ..
                },
                ..
            }
        ),
        "expected accepted agent.cancel response, got {cancel_response:?}"
    );
    let parent_events = tokio::time::timeout(std::time::Duration::from_secs(8), async {
        let mut attempt = 0_u32;
        loop {
            let label = format!("cancelled-parent-read-{attempt}");
            attempt = attempt.saturating_add(1);
            let journal = read_session(&mut client, &config, session.clone(), &label).await;
            let events = journal
                .into_iter()
                .filter(|envelope| envelope.run_id.as_ref() == Some(&parent_run))
                .filter_map(|envelope| serde_json::from_value(envelope.payload.into()).ok())
                .collect::<Vec<EventPayload>>();
            let terminal = events.iter().any(|payload| {
                matches!(
                    payload,
                    EventPayload::RunState(
                        RunState::Done | RunState::Errored | RunState::Cancelled
                    )
                )
            });
            if terminal && continuation_seen(&events, "parent-observed-child-cancellation") {
                break events;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled child releases parent through durable RPC projection");
    assert!(continuation_seen(
        &parent_events,
        "parent-observed-child-cancellation"
    ));
    let snapshot = fleet(&mut client, &config, session, "cancelled-child-fleet").await;
    assert_eq!(snapshot.roots.len(), 1);
    assert_eq!(snapshot.roots[0].state, FleetAgentStateWire::Cancelled);

    send_request(
        &mut client,
        &config,
        "cancel-finished-child",
        RequestBody::AgentCancel {
            command_id: CommandId::new("cancel-finished-child-command"),
            session_id: snapshot.session_id,
            worker_generation: generation,
            agent: child_agent,
        },
    )
    .await;
    let finished_response = next_response(&mut client).await;
    assert!(matches!(
        finished_response,
        WireFrame::Response {
            body: ResponseBody::AgentCancel {
                status: CancelStatus::AlreadyTerminal,
                terminal_seq: Some(_),
                ..
            },
            ..
        }
    ));
    assert_eq!(parent.requests().len(), 2);
    assert_eq!(child.requests().len(), 1);

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// A3: a prompt-oriented fork crosses the public RPC boundary after two real
/// provider turns. The source journal remains byte-for-byte untouched, the
/// selected prompt returns as an unsent draft, the built-in Anthropic renderer
/// carries the inherited cache cohort into the child request, and neither an
/// explicit SSH deny-all scope nor a remembered AllowAlways grant widens.
#[tokio::test]
async fn fork_from_prompt_preserves_source_cache_and_privilege_boundaries() {
    let root = test_root("core-loop-prompt-fork-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let script = vec![
        FakeStep::EmitToolCall {
            call_id: "fork-source-write".into(),
            name: "fs_write".into(),
            args: serde_json::json!({"path":"fork-source.txt","content":"source write\n"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "fork-source-write".into(),
        },
        FakeStep::EmitText {
            text: "first source answer".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::EmitText {
            text: "second source answer".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::EmitText {
            text: "fork child used inherited prefix".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ];
    let (dependencies, fake) = cache_aware_dependencies(script);
    let config = DaemonConfig::new(
        "core-loop-prompt-fork",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let task = ready_with_dependencies(&config, dependencies).await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "core-loop-e2e",
        "prompt-fork-client",
        ClientKind::Headless,
    )
    .await;

    send_request(
        &mut client,
        &config,
        "ssh-add",
        RequestBody::SshAdd {
            profile: SshProfileInputWire {
                name: "fork-denied-host".into(),
                description: Some("scope inheritance fixture".into()),
                host: "127.0.0.1".into(),
                port: 22,
                user: "fixture".into(),
                auth: SshAuthInputWire::Agent,
                default_cwd: None,
            },
        },
    )
    .await;
    assert!(matches!(
        next_response(&mut client).await,
        WireFrame::Response {
            body: ResponseBody::SshAdd { .. },
            ..
        }
    ));

    let (source, generation) = create_and_attach(
        &mut client,
        &config,
        &workspace,
        "cache-fixture",
        "cache-model",
        None,
        Some(SshScopeWire::None),
    )
    .await;
    let (_, first_events) = submit_turn_allow_always(
        &mut client,
        &config,
        source.clone(),
        generation,
        "write once and remember that permission",
    )
    .await;
    assert!(continuation_seen(&first_events, "first source answer"));
    assert_eq!(
        fs::read_to_string(workspace.join("fork-source.txt")).expect("real source write"),
        "source write\n"
    );
    let source_inventory = tools_inventory(
        &mut client,
        &config,
        source.clone(),
        "source-inventory-before-fork",
    )
    .await;
    assert_eq!(
        source_inventory.remembered_grants.len(),
        1,
        "fixture must establish a real AllowAlways grant before forking"
    );

    let second_run = submit_turn(
        &mut client,
        &config,
        "fork-source-second-turn",
        source.clone(),
        generation,
        "editable second prompt",
    )
    .await;
    let second_events = events_until_terminal(&mut client, &second_run).await;
    assert!(continuation_seen(&second_events, "second source answer"));
    wait_for_session_idle(&mut client, &source).await;
    let source_before =
        read_session(&mut client, &config, source.clone(), "source-before-fork").await;
    let prompt_seq = source_before
        .iter()
        .find_map(|envelope| {
            matches!(
                serde_json::from_value::<EventPayload>(envelope.payload.clone().into()),
                Ok(EventPayload::UserMessage { text, .. }) if text == "editable second prompt"
            )
            .then_some(envelope.seq)
        })
        .expect("selected source prompt has a durable sequence");
    assert!(source_before.iter().any(|envelope| {
        envelope.run_id.as_ref() == Some(&second_run)
            && serde_json::from_value::<EventPayload>(envelope.payload.clone().into())
                .is_ok_and(|payload| payload == EventPayload::RunState(RunState::Done))
    }));

    send_request(
        &mut client,
        &config,
        "prompt-fork",
        RequestBody::SessionFork {
            command_id: CommandId::new("prompt-fork-command"),
            session_id: source.clone(),
            worker_generation: generation,
            source_branch_id: None,
            fork_node_id: None,
            fork_seq: None,
            prompt: Some(SessionForkPromptSelector { seq: prompt_seq }),
            name: Some("Core-loop prompt fork".into()),
        },
    )
    .await;
    let (child, child_generation, forked_from, draft) = match next_response(&mut client).await {
        WireFrame::Response {
            body:
                ResponseBody::SessionFork {
                    session_id,
                    worker_generation,
                    forked_from,
                    draft,
                    ..
                },
            ..
        } => (session_id, worker_generation, forked_from, draft),
        other => panic!("expected prompt fork response, got {other:?}"),
    };
    assert_eq!(
        forked_from.as_ref().map(|value| value.seq),
        Some(prompt_seq)
    );
    let draft = draft.expect("prompt fork returns editable draft");
    assert_eq!(draft.text, "editable second prompt");
    assert!(draft.attachments.is_empty());

    let source_after =
        read_session(&mut client, &config, source.clone(), "source-after-fork").await;
    assert_eq!(
        source_after, source_before,
        "fork mutated the original transcript or terminal"
    );

    let child_replay = attach_existing(
        &mut client,
        &config,
        child.clone(),
        "attach-prompt-fork-child",
    )
    .await;
    assert!(!child_replay.iter().any(|envelope| {
        matches!(
            serde_json::from_value::<EventPayload>(envelope.payload.clone().into()),
            Ok(EventPayload::UserMessage { text, .. }) if text == "editable second prompt"
        )
    }));
    let fork_audit = child_replay
        .iter()
        .find_map(|envelope| SessionForked::from_payload_value(&envelope.payload))
        .expect("child carries prompt-fork audit fact");
    assert_eq!(
        fork_audit.forked_from.as_ref().map(|value| value.seq),
        Some(prompt_seq)
    );
    assert_eq!(fork_audit.context_epoch, ForkContextEpoch::Inherited);
    let inherited = fork_audit
        .inherited_cache_segment
        .expect("byte-identical copied prefix inherits cache segment");

    send_request(
        &mut client,
        &config,
        "child-ssh-list",
        RequestBody::SshList {
            session_id: Some(child.clone()),
        },
    )
    .await;
    match next_response(&mut client).await {
        WireFrame::Response {
            body: ResponseBody::SshList { profiles },
            ..
        } => {
            assert_eq!(profiles.len(), 1);
            assert!(
                !profiles[0].in_scope,
                "fork widened explicit SSH deny-all scope"
            );
        }
        other => panic!("expected child ssh.list, got {other:?}"),
    }
    let child_inventory = tools_inventory(
        &mut client,
        &config,
        child.clone(),
        "child-inventory-after-fork",
    )
    .await;
    assert!(
        child_inventory.remembered_grants.is_empty(),
        "AllowAlways grant crossed the fork audit boundary"
    );

    let child_run = submit_turn(
        &mut client,
        &config,
        "fork-child-turn",
        child.clone(),
        child_generation,
        &draft.text,
    )
    .await;
    let child_events = events_until_terminal(&mut client, &child_run).await;
    assert!(continuation_seen(
        &child_events,
        "fork child used inherited prefix"
    ));
    let requests = fake.requests();
    assert_eq!(requests.len(), 4, "two source rounds plus child request");
    let child_cache = requests
        .last()
        .and_then(|request| request.cache_metadata.as_ref())
        .expect("child provider request carries cache metadata");
    assert_eq!(child_cache.session_scope, child.as_str());
    assert_eq!(
        child_cache.cache_cohort.as_deref(),
        Some(inherited.cache_route.as_str()),
        "child request missed the inherited prompt-cache route"
    );
    assert_ne!(
        child_cache.cache_cohort.as_deref(),
        Some(child_cache.session_scope.as_str()),
        "cache hit was replaced by a fresh child cohort"
    );

    task.shutdown_handle().request("test complete");
    task.join().await.expect("daemon joins");
}

/// A2 regression shape reported by the owner. The second child run is queued
/// while the first is live. When run one reaches terminal, run two starts, so
/// the child session emits no aggregate Idle settlement for run one. The
/// parent must consume run one's durable terminal report after the bounded
/// best-effort tail and continue.
#[tokio::test]
async fn terminal_child_run_without_session_idle_still_releases_parent() {
    const ISOLATION_MARKER: &str = "HAIDER_CORE_LOOP_NO_IDLE_CHILD";
    if std::env::var_os(ISOLATION_MARKER).is_none() {
        assert_isolated_test_passes(
            "terminal_child_run_without_session_idle_still_releases_parent",
            ISOLATION_MARKER,
        )
        .await;
        return;
    }

    let root = test_root("core-loop-agent-no-idle-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let parent = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "no-idle-spawn".into(),
            name: "spawn_subagent".into(),
            args: serde_json::json!({
                "task": "terminal-without-idle",
                "prompt": "finish the first child run",
                "model": "child-model",
                "provider": "child-provider"
            }),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "no-idle-spawn".into(),
        },
        FakeStep::EmitText {
            text: "parent-progressed-without-child-idle".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let child = Arc::new(FakeProvider::new(vec![
        FakeStep::Delay { ms: 800 },
        FakeStep::EmitText {
            text: "first child run terminal report".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::Hang,
    ]));
    let parent_provider: Arc<dyn Provider> = parent.clone();
    let child_provider: Arc<dyn Provider> = child.clone();
    let config = DaemonConfig::new(
        "core-loop-agent-no-idle",
        root.path().join("store"),
        root.path().join("runtime"),
    );
    let task = ready_with_dependencies(
        &config,
        dependencies(
            [
                ("parent-provider".into(), parent_provider),
                ("child-provider".into(), child_provider),
            ],
            [],
        ),
    )
    .await;
    let mut client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "core-loop-e2e",
        "agent-no-idle-client",
        ClientKind::Headless,
    )
    .await;
    let (session, generation) = create_and_attach(
        &mut client,
        &config,
        &workspace,
        "parent-provider",
        "parent-model",
        None,
        None,
    )
    .await;
    let parent_run = submit_turn(
        &mut client,
        &config,
        "no-idle-parent-turn",
        session.clone(),
        generation,
        "delegate then continue",
    )
    .await;
    let child_session = tokio::time::timeout(support::DEADLINE, async {
        let mut attempt = 0_u32;
        loop {
            let label = format!("no-idle-fleet-wait-{attempt}");
            attempt = attempt.saturating_add(1);
            let snapshot = fleet(&mut client, &config, session.clone(), &label).await;
            if let Some(node) = snapshot.roots.first() {
                break node.session_id.clone();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("spawned child appears in real fleet projection");
    let mut child_client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "core-loop-e2e",
        "agent-no-idle-child-client",
        ClientKind::Headless,
    )
    .await;
    attach_existing(
        &mut child_client,
        &config,
        child_session.clone(),
        "attach-no-idle-child",
    )
    .await;
    let queued_run = submit_turn(
        &mut child_client,
        &config,
        "queued-child-turn",
        child_session.clone(),
        generation,
        "keep the child session active after run one",
    )
    .await;
    let queued_response = read_session(
        &mut child_client,
        &config,
        child_session.clone(),
        "queued-child-read",
    )
    .await;
    assert!(queued_response.iter().any(|envelope| {
        envelope.run_id.as_ref() == Some(&queued_run)
            && serde_json::from_value::<EventPayload>(envelope.payload.clone().into())
                .is_ok_and(|payload| matches!(payload, EventPayload::RunState(RunState::Queued)))
    }));
    let (parent_events, child_journal) =
        tokio::time::timeout(std::time::Duration::from_secs(4), async {
            let mut attempt = 0_u32;
            loop {
                let parent_label = format!("no-idle-parent-read-{attempt}");
                let journal =
                    read_session(&mut client, &config, session.clone(), &parent_label).await;
                let events = journal
                    .into_iter()
                    .filter(|envelope| envelope.run_id.as_ref() == Some(&parent_run))
                    .filter_map(|envelope| serde_json::from_value(envelope.payload.into()).ok())
                    .collect::<Vec<EventPayload>>();
                let terminal = events
                    .iter()
                    .any(|payload| matches!(payload, EventPayload::RunState(RunState::Done)));
                if terminal && continuation_seen(&events, "parent-progressed-without-child-idle") {
                    let child_label = format!("no-idle-child-read-{attempt}");
                    let child_journal = read_session(
                        &mut child_client,
                        &config,
                        child_session.clone(),
                        &child_label,
                    )
                    .await;
                    break (events, child_journal);
                }
                attempt = attempt.saturating_add(1);
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("durable child terminal releases its parent without aggregate Idle");

    assert!(continuation_seen(
        &parent_events,
        "parent-progressed-without-child-idle"
    ));
    assert!(
        parent_events
            .iter()
            .any(|payload| matches!(payload, EventPayload::RunState(RunState::Done)))
    );
    let child_terminal_seq = child_journal
        .iter()
        .find_map(|envelope| {
            if envelope
                .run_id
                .as_ref()
                .is_none_or(|run_id| run_id == &queued_run)
            {
                return None;
            }
            serde_json::from_value::<EventPayload>(envelope.payload.clone().into())
                .is_ok_and(|payload| matches!(payload, EventPayload::RunState(RunState::Done)))
                .then_some(envelope.seq)
        })
        .expect("first child run reaches durable Done");
    assert!(
        child_journal.iter().any(|envelope| {
            envelope.run_id.as_ref() == Some(&queued_run)
                && serde_json::from_value::<EventPayload>(envelope.payload.clone().into())
                    .is_ok_and(|payload| {
                        matches!(
                            payload,
                            EventPayload::RunState(
                                RunState::Thinking | RunState::Streaming | RunState::RunningTool
                            )
                        )
                    })
        }),
        "queued child run did not keep the session active"
    );
    assert!(
        !child_journal.iter().any(|envelope| {
            envelope.seq > child_terminal_seq
                && serde_json::from_value::<EventPayload>(envelope.payload.clone().into())
                    .is_ok_and(|payload| {
                        matches!(
                            payload,
                            EventPayload::SessionState(
                                haider_protocol::state::SessionState::Idle { .. }
                            )
                        )
                    })
        }),
        "child emitted Idle after the first run terminal, invalidating the regression shape"
    );
    task.shutdown_handle().request("test complete");
    tokio::time::timeout(std::time::Duration::from_secs(2), task.join())
        .await
        .expect("daemon shutdown stays bounded")
        .expect("daemon joins");
}

// ---------------------------------------------------------------------------
// One-shot boot behaviour-preservation pins (lane oneshotboot). These observe
// what a LATER daemon on the same profile can read back through the public
// doors; see docs/testing/v0.0.969/oneshotboot-tests.md.
// ---------------------------------------------------------------------------

/// The very first Loom read on a fresh profile sees the two owner-specified
/// default agent types, and a restarted daemon on the same store answers
/// with the identical registry.
///
/// MUTATION CHECK: deferring the seed past the first `loom.list` answers an
/// empty registry; re-seeding on the second boot advances a rev.
#[tokio::test]
async fn fresh_daemon_first_loom_list_returns_the_two_seeded_defaults_across_restart() {
    let root = test_root("oneshot-loom-seed-");
    let config = DaemonConfig::new(
        "oneshot-loom-seed",
        root.path().join("store"),
        root.path().join("runtime"),
    );

    async fn loom_agent_types(
        client: &mut UdsClient,
        config: &DaemonConfig,
        request_id: &str,
    ) -> Vec<LoomAgentType> {
        send_request(
            client,
            config,
            request_id,
            RequestBody::LoomList {
                include_archived: false,
            },
        )
        .await;
        match next_response(client).await {
            WireFrame::Response {
                body: ResponseBody::LoomList { agent_types, .. },
                ..
            } => agent_types,
            other => panic!("expected loom.list response, got {other:?}"),
        }
    }

    let first_task = support::ready(&config).await;
    let mut first_client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "oneshot-loom-seed",
        "first-client",
        ClientKind::Headless,
    )
    .await;
    let first = loom_agent_types(&mut first_client, &config, "loom-first").await;
    let mut ids = first
        .iter()
        .map(|record| record.id.as_str())
        .collect::<Vec<_>>();
    ids.sort_unstable();
    assert_eq!(ids, ["reviewer", "scout"], "fresh registry: {first:?}");
    for record in &first {
        assert_eq!(
            record.rev, 1,
            "the registry owns the seeded rev: {record:?}"
        );
        assert!(!record.job.is_empty());
    }
    let scout = first
        .iter()
        .find(|record| record.id == "scout")
        .expect("scout seeded");
    assert_eq!(
        (
            scout.name.as_str(),
            scout.color.as_str(),
            scout.glyph.as_str()
        ),
        ("Scout", "#7aa2f7", "⌖")
    );
    let reviewer = first
        .iter()
        .find(|record| record.id == "reviewer")
        .expect("reviewer seeded");
    assert_eq!(
        (
            reviewer.name.as_str(),
            reviewer.color.as_str(),
            reviewer.glyph.as_str()
        ),
        ("Reviewer", "#bb9af7", "⚖")
    );
    drop(first_client);
    first_task.shutdown_handle().request("restart");
    first_task.join().await.expect("first daemon joins");

    let second_task = support::ready(&config).await;
    let mut second_client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "oneshot-loom-seed",
        "second-client",
        ClientKind::Headless,
    )
    .await;
    let second = loom_agent_types(&mut second_client, &config, "loom-second").await;
    assert_eq!(
        second, first,
        "a restart neither re-seeds nor loses the defaults"
    );
    drop(second_client);
    second_task.shutdown_handle().request("test complete");
    second_task.join().await.expect("second daemon joins");
}

/// A text-file attachment put into the CAS and accepted by the first daemon
/// is re-read from the CAS by the NEXT daemon when it projects the session
/// history: the second turn's provider request still carries the file text.
///
/// MUTATION CHECK: losing the CAS object or its directory entry on the first
/// daemon's shutdown makes the second turn fail to resolve the attachment
/// (no Done terminal) or project it without the marker text.
#[tokio::test]
async fn attachment_written_by_the_first_daemon_is_projected_by_the_next_daemon() {
    const NOTE: &str = "oneshot cas marker alpha\nsecond line of the note\n";
    let root = test_root("oneshot-cas-restart-");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
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
    let config = DaemonConfig::new(
        "oneshot-cas-restart",
        root.path().join("store"),
        root.path().join("runtime"),
    );

    let first_task = ready_with_dependencies(&config, dependencies.clone()).await;
    let mut first_client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "oneshot-cas-restart",
        "first-client",
        ClientKind::Headless,
    )
    .await;
    let (session_id, generation) = create_and_attach(
        &mut first_client,
        &config,
        &workspace,
        "fake",
        "fake-v1",
        None,
        None,
    )
    .await;
    send_request(
        &mut first_client,
        &config,
        "put-note",
        RequestBody::ArtifactPut {
            data_base64: BASE64.encode(NOTE.as_bytes()),
        },
    )
    .await;
    let artifact = match next_response(&mut first_client).await {
        WireFrame::Response {
            body: ResponseBody::ArtifactPut { artifact, bytes },
            ..
        } => {
            assert_eq!(bytes, NOTE.len() as u64);
            artifact
        }
        other => panic!("expected artifact.put response, got {other:?}"),
    };
    assert!(
        root.path()
            .join("store")
            .join("cas")
            .join(&artifact.as_str()["blake3:".len().."blake3:".len() + 2])
            .join(&artifact.as_str()["blake3:".len()..])
            .is_file(),
        "the CAS object is published under the store namespace"
    );
    send_request(
        &mut first_client,
        &config,
        "submit-with-note",
        RequestBody::TurnSubmit {
            command_id: CommandId::new("submit-with-note"),
            session_id: session_id.clone(),
            worker_generation: generation,
            text: "read the note".into(),
            attachments: vec![haider_protocol::tool::AttachmentBlock::File {
                artifact: artifact.clone(),
                name: "note.txt".into(),
                lines: 2,
            }],
            mode: DeliveryMode::Queue,
        },
    )
    .await;
    let first_run = loop {
        match first_client.next().await {
            WireFrame::Response {
                body: ResponseBody::TurnSubmit { run_id, .. },
                ..
            } => break run_id,
            WireFrame::Response {
                body: ResponseBody::Error { code, message, .. },
                ..
            } => panic!("turn.submit with attachment failed ({code}): {message}"),
            _ => {}
        }
    };
    events_until_terminal(&mut first_client, &first_run).await;
    let requests = fake.requests();
    assert_eq!(requests.len(), 1);
    assert!(
        request_contains(&requests[0], "oneshot cas marker alpha"),
        "the first daemon inlines the attachment from its own CAS"
    );
    drop(first_client);
    first_task.shutdown_handle().request("restart");
    first_task.join().await.expect("first daemon joins");

    let second_task = ready_with_dependencies(&config, dependencies).await;
    let mut second_client = UdsClient::connect_control(
        &config.endpoint_path(),
        config.frame_limit,
        "oneshot-cas-restart",
        "second-client",
        ClientKind::Headless,
    )
    .await;
    send_request(
        &mut second_client,
        &config,
        "list-after-restart",
        RequestBody::SessionList {
            cursor: None,
            limit: 10,
            order: Default::default(),
        },
    )
    .await;
    let restarted_generation = match next_response(&mut second_client).await {
        WireFrame::Response {
            body: ResponseBody::SessionList { sessions, .. },
            ..
        } => {
            let row = sessions
                .iter()
                .find(|row| row.session_id == session_id)
                .expect("the session survives the restart");
            row.worker_generation
        }
        other => panic!("expected session.list response, got {other:?}"),
    };
    assert!(restarted_generation > generation);
    let replay = attach_existing(
        &mut second_client,
        &config,
        session_id.clone(),
        "attach-after-restart",
    )
    .await;
    assert!(
        replay.iter().any(|envelope| {
            envelope.payload["type"] == "user_message"
                && envelope.payload["attachments"][0]["artifact"] == artifact.as_str()
        }),
        "the replayed journal names the attachment address"
    );
    let second_run = submit_turn(
        &mut second_client,
        &config,
        "submit-after-restart",
        session_id,
        restarted_generation,
        "and again",
    )
    .await;
    events_until_terminal(&mut second_client, &second_run).await;
    let requests = fake.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        request_contains(&requests[1], "oneshot cas marker alpha"),
        "the restarted daemon re-reads the attachment from the CAS for the history projection"
    );
    assert!(request_contains(&requests[1], "and again"));
    drop(second_client);
    second_task.shutdown_handle().request("test complete");
    second_task.join().await.expect("second daemon joins");
}
