#![allow(clippy::expect_used)]
//! W-A daemon-level background task laws.
//!
//! LT1 (immediate return + foreground unchanged), LT2 daemon half (bounded
//! completion fact + CAS artifact), LT3 daemon half (idle completion fact
//! carries the prompt notice), LT4 (active-run steer with the nudge COUNT
//! asserted — the W6 vacuous-pin lesson), LT5 (brokered kill journaling +
//! bounded cursor reads), LT6 (restart adoption via the injected liveness
//! seam), LT7 (concurrency cap + session-close/shutdown fences).

use crate::session_hub::{SessionHub, SessionHubConfig};
use crate::tasks::{TaskFacade, TaskLiveState};
use crate::worker::{
    BrokerToolFactory, ProviderFactory, ResolvedTurnProvider, TurnToolFactory, WorkerDependencies,
    WorkerManager, WorkerToolContext,
};
use async_trait::async_trait;
use haider_core::{
    CancelToken, EventIdGenerator, SessionCreateCommand, SqliteStoreHandle, StoreHandle,
    ToolDispatchResult, ToolDispatcher, TurnAcceptCommand,
};
use haider_protocol::DeliveryMode;
use haider_protocol::EventPayload;
use haider_protocol::effect::{EffectOutcome, EffectPhase};
use haider_protocol::envelope::{PromptRender, RawEnvelope};
use haider_protocol::ids::{DeviceId, EventId, ItemId, RunId, SessionId, TaskId};
use haider_protocol::provider::{Block, CapabilityDoc, FinishReason};
use haider_protocol::session::{SessionMetadataV1, SessionPermissionOverridesV1};
use haider_protocol::state::RunState;
use haider_protocol::task::{
    TASK_CONCURRENCY_CAP, TaskCompleted, TaskCompletionDelivery, TaskEventPayload, TaskStarted,
    TaskTerminalState,
};
use haider_provider::{FakeProvider, Provider, ProviderError, ProviderStream, TurnRequest};
use haider_tools::PidLiveness;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::{Notify, mpsc};
use tokio::time::{Duration, timeout};

fn overrides() -> Option<SessionPermissionOverridesV1> {
    Some(SessionPermissionOverridesV1 {
        allow_writes: true,
        allow_exec: true,
        allow_mobile: false,
        auto_allow: false,
    })
}

fn task_metadata(cwd: &str) -> SessionMetadataV1 {
    SessionMetadataV1 {
        cwd: cwd.to_owned(),
        provider: "fake".into(),
        account_alias: None,
        model: "fake-model".into(),
        max_tokens: 4096,
        system_prompt_version: Some(crate::worker::SystemPromptBuilder::VERSION.into()),
        permission_overrides: overrides(),
        interaction_mode: Default::default(),
        title: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        context_economy: Default::default(),
        created_at_ms: 1,
        agent_type: None,
    }
}

async fn create_task_session(hub: &SessionHub, name: &str, cwd: &str) -> SessionId {
    let session_id = SessionId::new(name.to_owned());
    hub.create_internal_session(SessionCreateCommand {
        command_id: format!("create-{name}"),
        request_digest: format!("create-{name}-digest"),
        request_json: format!("{{\"session\":\"{name}\"}}"),
        session_id: session_id.clone(),
        cwd: cwd.to_owned(),
        provider: "fake".into(),
        model: "fake-model".into(),
        max_tokens: 4096,
        permission_overrides: overrides(),
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
        event_id: EventId::new(format!("created-{name}")),
        device_id: DeviceId::new(format!("{name}-device")),
    })
    .await
    .expect("create task session");
    session_id
}

/// Gives the tool run durable accepted state (production dispatch happens
/// inside an accepted, nonterminal run — broker effect phases append through
/// the worker lease and the store rejects terminal runs). The run stays
/// Queued, which the idle/active law deliberately does NOT treat as
/// steerable — a queued run has no live harness.
async fn prepare_tool_run(hub: &SessionHub, session_id: &SessionId, run_id: &RunId, label: &str) {
    hub.accept_internal_turn(TurnAcceptCommand {
        command_id: format!("submit-{label}"),
        request_digest: format!("submit-{label}-digest"),
        request_json: format!(r#"{{"turn":"{label}"}}"#),
        session_id: session_id.clone(),
        worker_generation: hub.worker_generation(),
        run_id: run_id.clone(),
        agent_id: None,
        branch_id: None,
        text: format!("tool coordinates for {label}"),
        attachments: Vec::new(),
        mode: DeliveryMode::Queue,
        queued_event_id: EventId::new(format!("queued-{label}")),
        user_event_id: EventId::new(format!("user-{label}")),
        active_event_id: EventId::new(format!("active-{label}")),
        device_id: DeviceId::new(format!("{label}-device")),
    })
    .await
    .expect("accept tool run");
}

/// Terminalizes the tool run so the session is durably IDLE (and deletable).
async fn terminalize_tool_run(
    hub: &SessionHub,
    session_id: &SessionId,
    run_id: &RunId,
    label: &str,
) {
    let mut terminal = [haider_protocol::envelope::EventEnvelope {
        schema_version: haider_protocol::envelope::SCHEMA_VERSION,
        event_id: EventId::new(format!("done-{label}")),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: Some(run_id.clone()),
        agent_id: None,
        device_id: DeviceId::new(format!("{label}-device")),
        authority_epoch: 0,
        worker_generation: hub.worker_generation(),
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: haider_protocol::envelope::RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::to_value(EventPayload::RunState(RunState::Done))
            .expect("done payload"),
    }];
    hub.append(&mut terminal)
        .await
        .expect("terminalize tool run");
}

async fn task_dispatcher(
    hub: &SessionHub,
    session_id: &SessionId,
    cwd: &str,
    label: &str,
    context_run: &RunId,
) -> Arc<dyn ToolDispatcher> {
    task_dispatcher_with_grant(hub, session_id, cwd, label, context_run, None).await
}

async fn task_dispatcher_with_grant(
    hub: &SessionHub,
    session_id: &SessionId,
    cwd: &str,
    label: &str,
    context_run: &RunId,
    grant: Option<haider_protocol::agent::Grant>,
) -> Arc<dyn ToolDispatcher> {
    let lease = hub
        .acquire_worker_lease(session_id.clone())
        .await
        .expect("task tool lease");
    TurnToolFactory::create(
        &BrokerToolFactory,
        WorkerToolContext {
            lockdown: None,
            diagnostics: None,
            metadata: task_metadata(cwd),
            store: lease,
            run_id: context_run.clone(),
            run_deadline: None,
            branch_id: None,
            device_id: DeviceId::new(format!("{label}-tool-device")),
            event_ids: Arc::new(EventIdGenerator::new(format!("{label}-tool-event"))),
            delegation: crate::delegation::DelegationHandle::new(hub.clone()),
            tasks: TaskFacade::with_kill_grace(hub.clone(), Duration::from_millis(300)),
            agent_id: None,
            session_context_tail: String::new(),
            grant,
            mobile_use_active: false,
            cli_scope: None,
            typed_workflow_execution: None,
            loom_provider_fenced: false,
            web_search: None,
        },
    )
    .await
    .expect("create tool dispatcher")
    .expect("dispatcher available")
}

/// E1d dispatch fence: even a forged/unadvertised write call is rejected
/// before the broker can create an effect or permission menu.
#[tokio::test]
async fn e1d_dispatch_rejects_tool_above_child_grant_ceiling() {
    use haider_protocol::agent::Grant;
    use haider_protocol::effect::EffectClass;
    use haider_protocol::tool::ToolResultStatus;

    let profile = tempfile::tempdir().expect("profile");
    let (_workspace, cwd) = workspace();
    let store = SqliteStoreHandle::open(profile.path())
        .await
        .expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let session_id = create_task_session(&hub, "e1-grant-dispatch", &cwd).await;
    let run_id = RunId::new("e1-grant-dispatch-run");
    prepare_tool_run(&hub, &session_id, &run_id, "e1-grant-dispatch").await;
    let dispatcher = task_dispatcher_with_grant(
        &hub,
        &session_id,
        &cwd,
        "e1-grant-dispatch",
        &run_id,
        Some(Grant {
            tools: vec!["fs_read".into()],
            effect_ceiling: vec![EffectClass::FsRead],
        }),
    )
    .await;
    let outcome = dispatcher
        .execute(
            &run_id,
            &ItemId::new("e1-grant-dispatch-item"),
            "e1-grant-dispatch-call",
            "fs_write",
            serde_json::json!({"path": "forbidden.txt", "content": "no"}),
            &CancelToken::new(),
        )
        .await
        .expect("surface-and-continue result");
    let ToolDispatchResult::Completed(result) = outcome else {
        panic!("grant violation must be a completed dispatch result");
    };
    assert_eq!(result.status, ToolResultStatus::Rejected);
    assert!(result.preview.contains("grant_ceiling_violation"));
    assert!(!std::path::Path::new(&cwd).join("forbidden.txt").exists());

    dispatcher.close().await.expect("dispatcher close");
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

async fn dispatch(
    dispatcher: &Arc<dyn ToolDispatcher>,
    run_id: &RunId,
    call_id: &str,
    name: &str,
    args: serde_json::Value,
) -> serde_json::Value {
    let result = dispatcher
        .execute(
            run_id,
            &ItemId::new(format!("{call_id}-item")),
            call_id,
            name,
            args,
            &CancelToken::new(),
        )
        .await
        .expect("tool dispatch");
    let ToolDispatchResult::Completed(result) = result else {
        panic!("tool `{name}` must complete, got a non-completed dispatch");
    };
    serde_json::from_str(&result.preview).expect("tool preview is JSON")
}

async fn read_all(store: &SqliteStoreHandle, session_id: &SessionId) -> Vec<RawEnvelope> {
    let mut envelopes = Vec::new();
    let mut cursor = 0;
    loop {
        let page = StoreHandle::read(store, session_id, cursor, 256)
            .await
            .expect("read session journal");
        if page.is_empty() {
            return envelopes;
        }
        cursor = page.last().map_or(cursor, |envelope| envelope.seq);
        envelopes.extend(page);
    }
}

fn task_facts(envelopes: &[RawEnvelope]) -> Vec<(RawEnvelope, TaskEventPayload)> {
    envelopes
        .iter()
        .filter_map(|envelope| {
            TaskEventPayload::from_payload_value(&envelope.payload)
                .map(|fact| (envelope.clone(), fact))
        })
        .collect()
}

fn completed_fact(
    envelopes: &[RawEnvelope],
    task: &TaskId,
) -> Option<(RawEnvelope, TaskCompleted)> {
    task_facts(envelopes)
        .into_iter()
        .find_map(|(envelope, fact)| match fact {
            TaskEventPayload::TaskCompleted(completed) if completed.task == *task => {
                Some((envelope, completed))
            }
            _ => None,
        })
}

fn started_fact(envelopes: &[RawEnvelope], task: &TaskId) -> Option<(RawEnvelope, TaskStarted)> {
    task_facts(envelopes)
        .into_iter()
        .find_map(|(envelope, fact)| match fact {
            TaskEventPayload::TaskStarted(started) if started.task == *task => {
                Some((envelope, started))
            }
            _ => None,
        })
}

async fn wait_for_completed(
    store: &SqliteStoreHandle,
    session_id: &SessionId,
    task: &TaskId,
) -> (RawEnvelope, TaskCompleted) {
    timeout(Duration::from_secs(10), async {
        loop {
            let envelopes = read_all(store, session_id).await;
            if let Some(found) = completed_fact(&envelopes, task) {
                return found;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("task completion fact journals")
}

fn pgid_dead(pid: i32) -> bool {
    haider_tools::probe_group_liveness(pid) == PidLiveness::Dead
}

async fn wait_for_pgid_death(pid: i32) {
    timeout(Duration::from_secs(10), async {
        while !pgid_dead(pid) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("process group dies");
}

fn workspace() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("workspace");
    let canonical = std::fs::canonicalize(dir.path()).expect("canonical workspace");
    let text = canonical.to_string_lossy().into_owned();
    (dir, text)
}

/// MUTATION CHECK (LT1 + LT5): make the background dispatch wait for the
/// child, skip the started fact, drop the kill effect journaling, or signal
/// only the leader. Expected RUNTIME failure: the dispatch no longer returns
/// a running receipt while a 30s child runs, the durable facts/effect phases
/// below disappear, or the group probe stays alive after the kill.
#[tokio::test]
async fn background_dispatch_is_immediate_and_kill_is_brokered_end_to_end() {
    let profile = tempfile::tempdir().expect("profile");
    let (_workspace, cwd) = workspace();
    let store = SqliteStoreHandle::open(profile.path())
        .await
        .expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let session_id = create_task_session(&hub, "task-e2e-session", &cwd).await;
    let run_id = RunId::new("task-e2e-tool-run");
    prepare_tool_run(&hub, &session_id, &run_id, "task-e2e").await;
    let dispatcher = task_dispatcher(&hub, &session_id, &cwd, "task-e2e", &run_id).await;

    let started = Instant::now();
    let receipt = dispatch(
        &dispatcher,
        &run_id,
        "task-e2e-spawn",
        "process_exec",
        serde_json::json!({
            "command": "printf ready; sleep 30",
            "background": true,
        }),
    )
    .await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "background dispatch must return immediately"
    );
    assert_eq!(receipt["state"], "running");
    assert_eq!(receipt["name"], "printf");
    let task = TaskId::new(receipt["task_id"].as_str().expect("task id"));

    let envelopes = read_all(&store, &session_id).await;
    let (started_envelope, started_payload) =
        started_fact(&envelopes, &task).expect("durable task_started fact");
    assert!(started_envelope.render.ui && started_envelope.render.durable);
    assert_eq!(started_envelope.render.prompt, PromptRender::Verbatim);
    assert_eq!(started_payload.command, "printf ready; sleep 30");
    assert!(!pgid_dead(started_payload.pid), "the task group is alive");

    // Live bounded reads: tail without a cursor, then a cursor page (LT5).
    let tail = timeout(Duration::from_secs(5), async {
        loop {
            let output = dispatch(
                &dispatcher,
                &run_id,
                "task-e2e-output",
                "task_output",
                serde_json::json!({"task_id": task.as_str()}),
            )
            .await;
            if output["tail"] == "ready" {
                return output;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("live tail reflects child output");
    assert_eq!(tail["state"], "running");
    assert_eq!(tail["output_bytes"], 5);
    let page = dispatch(
        &dispatcher,
        &run_id,
        "task-e2e-cursor",
        "task_output",
        serde_json::json!({"task_id": task.as_str(), "cursor": 2}),
    )
    .await;
    assert_eq!(page["chunk"], "ady");
    assert_eq!(page["next_cursor"], 5);
    assert_eq!(page["exhausted"], true);

    // Brokered kill: intent + Ok outcome journal, and the pgid dies.
    let killed = dispatch(
        &dispatcher,
        &run_id,
        "task-e2e-kill",
        "task_kill",
        serde_json::json!({"task_id": task.as_str()}),
    )
    .await;
    assert_eq!(killed["status"], "killed");
    assert_eq!(killed["state"]["state"], "killed");
    wait_for_pgid_death(started_payload.pid).await;

    let envelopes = read_all(&store, &session_id).await;
    let kill_intent = envelopes
        .iter()
        .find_map(|envelope| {
            match serde_json::from_value::<EventPayload>(envelope.payload.clone()) {
                Ok(EventPayload::Effect(EffectPhase::Intent(intent)))
                    if intent.summary.starts_with("kill background task") =>
                {
                    Some(intent)
                }
                _ => None,
            }
        })
        .expect("kill effect intent journals");
    assert!(
        envelopes.iter().any(|envelope| {
            matches!(
                serde_json::from_value::<EventPayload>(envelope.payload.clone()),
                Ok(EventPayload::Effect(EffectPhase::Outcome {
                    effect,
                    outcome: EffectOutcome::Ok,
                    ..
                })) if effect == kill_intent.effect
            )
        }),
        "kill effect terminalizes Ok"
    );
    let (_, completed) = wait_for_completed(&store, &session_id, &task).await;
    assert_eq!(completed.state, TaskTerminalState::Killed);

    dispatcher.close().await.expect("dispatcher close");
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

/// MUTATION CHECK (LT1 foreground regression): route the plain foreground
/// shape through the task registry or journal task facts for it. Expected
/// RUNTIME failure: the foreground dispatch stops returning the completed
/// process result inline, or a task fact appears for a foreground command.
#[tokio::test]
async fn foreground_process_exec_is_unchanged_and_journals_no_task_facts() {
    let profile = tempfile::tempdir().expect("profile");
    let (_workspace, cwd) = workspace();
    let store = SqliteStoreHandle::open(profile.path())
        .await
        .expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let session_id = create_task_session(&hub, "task-foreground-session", &cwd).await;
    let run_id = RunId::new("task-foreground-tool-run");
    prepare_tool_run(&hub, &session_id, &run_id, "task-foreground").await;
    let dispatcher = task_dispatcher(&hub, &session_id, &cwd, "task-foreground", &run_id).await;

    let result = dispatch(
        &dispatcher,
        &run_id,
        "task-foreground-call",
        "process_exec",
        serde_json::json!({"command": "printf foreground-done"}),
    )
    .await;
    assert_eq!(result["exit_code"], 0, "foreground blocks to completion");
    assert_eq!(result["status"], "completed");
    assert_eq!(
        result["transcript_digest"],
        format!("blake3:{}", blake3::hash(b"foreground-done").to_hex())
    );
    assert!(result["process_signal"]["effect_id"].is_string());
    assert!(result["subject_digest"].is_string());
    let envelopes = read_all(&store, &session_id).await;
    assert!(
        task_facts(&envelopes).is_empty(),
        "a foreground command must never journal task facts"
    );
    let signals = envelopes
        .iter()
        .filter_map(|envelope| {
            serde_json::from_value::<EventPayload>(envelope.payload.clone())
                .ok()
                .and_then(|payload| match payload {
                    EventPayload::ProcessSignalRecorded(signal) => Some(signal),
                    _ => None,
                })
        })
        .collect::<Vec<_>>();
    assert_eq!(
        signals.len(),
        1,
        "foreground exec records one trusted signal"
    );
    assert_eq!(signals[0].run_id, run_id);
    assert_eq!(signals[0].exit_code, Some(0));
    assert_eq!(
        Some(signals[0].subject_digest.as_str()),
        result["subject_digest"].as_str()
    );

    dispatcher.close().await.expect("dispatcher close");
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

/// LAW E1a background half: a nonzero background exit is Failed, never the
/// green Completed state. MUTATION: map every observed exit code to Completed
/// and this assertion fails.
#[tokio::test]
async fn e1a_background_nonzero_exit_is_failed() {
    let profile = tempfile::tempdir().expect("profile");
    let (_workspace, cwd) = workspace();
    let store = SqliteStoreHandle::open(profile.path())
        .await
        .expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let session_id = create_task_session(&hub, "e1-background-fail", &cwd).await;
    let run_id = RunId::new("e1-background-fail-run");
    prepare_tool_run(&hub, &session_id, &run_id, "e1-background-fail").await;
    let dispatcher = task_dispatcher(&hub, &session_id, &cwd, "e1-background-fail", &run_id).await;

    let receipt = dispatch(
        &dispatcher,
        &run_id,
        "e1-background-fail-call",
        "process_exec",
        serde_json::json!({
            "command": "exit 7",
            "background": true,
            "name": "failing",
        }),
    )
    .await;
    let task = TaskId::new(receipt["task_id"].as_str().expect("task id"));
    let (_, completed) = wait_for_completed(&store, &session_id, &task).await;
    assert_eq!(
        completed.state,
        TaskTerminalState::Failed {
            reason: "process exited with code 7".into()
        }
    );

    let signal_receipt = dispatch(
        &dispatcher,
        &run_id,
        "e1-background-signal-call",
        "process_exec",
        serde_json::json!({
            "command": "kill -9 $$",
            "background": true,
            "name": "signalled",
        }),
    )
    .await;
    let signal_task = TaskId::new(signal_receipt["task_id"].as_str().expect("task id"));
    let (_, signalled) = wait_for_completed(&store, &session_id, &signal_task).await;
    assert!(matches!(
        signalled.state,
        TaskTerminalState::Failed { ref reason }
            if reason.contains("signal 9") && reason.contains("out-of-memory")
    ));

    dispatcher.close().await.expect("dispatcher close");
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

/// MUTATION CHECK (LT2 + LT3 daemon halves): drop the CAS artifact, stop
/// marking truncation, or journal the idle completion with `Omit`. Expected
/// RUNTIME failure: the completed fact loses its artifact/tail/truncation
/// truth or its Verbatim prompt render for the next turn.
#[tokio::test]
async fn idle_completion_fact_is_bounded_with_cas_artifact_and_prompt_notice() {
    let profile = tempfile::tempdir().expect("profile");
    let (_workspace, cwd) = workspace();
    let store = SqliteStoreHandle::open(profile.path())
        .await
        .expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let session_id = create_task_session(&hub, "task-idle-session", &cwd).await;
    let run_id = RunId::new("task-idle-tool-run");
    prepare_tool_run(&hub, &session_id, &run_id, "task-idle").await;
    let dispatcher = task_dispatcher(&hub, &session_id, &cwd, "task-idle", &run_id).await;

    let receipt = dispatch(
        &dispatcher,
        &run_id,
        "task-idle-spawn",
        "process_exec",
        serde_json::json!({
            "command": "sleep 0.5; printf idle-task-output",
            "background": true,
            "name": "emitter",
        }),
    )
    .await;
    let task = TaskId::new(receipt["task_id"].as_str().expect("task id"));
    assert_eq!(receipt["name"], "emitter");
    // The session is durably IDLE before the task completes: the completion
    // must deliver queued, with the fact carrying the prompt notice.
    terminalize_tool_run(&hub, &session_id, &run_id, "task-idle").await;

    let (envelope, completed) = wait_for_completed(&store, &session_id, &task).await;
    assert_eq!(
        completed.state,
        TaskTerminalState::Completed { exit_code: Some(0) }
    );
    assert_eq!(completed.delivery, TaskCompletionDelivery::DeliveredQueued);
    assert_eq!(completed.output_bytes, 16);
    assert_eq!(completed.tail, "idle-task-output");
    assert!(!completed.truncated, "under-cap output is not truncated");
    assert!(envelope.render.ui && envelope.render.durable);
    assert_eq!(
        envelope.render.prompt,
        PromptRender::Verbatim,
        "the idle (queued) completion fact carries the next-turn prompt notice"
    );
    let artifact = completed.artifact.expect("completion artifact in the CAS");
    let bytes = store
        .get(&artifact)
        .await
        .expect("read completion artifact");
    assert_eq!(bytes, b"idle-task-output");

    dispatcher.close().await.expect("dispatcher close");
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

struct FixedProviderFactory {
    provider: Arc<dyn Provider>,
}

#[async_trait]
impl ProviderFactory for FixedProviderFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, haider_protocol::error::HaiderError> {
        Ok(ResolvedTurnProvider {
            provider: self.provider.clone(),
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

/// Effect-phase sink for the steer test's standalone broker: the four-phase
/// law is pinned elsewhere (tools + e2e dispatch tests); LT4's subject is
/// steer delivery only.
struct DropJournal;

#[async_trait]
impl haider_tools::JournalSink for DropJournal {
    async fn append(&mut self, _payload: EventPayload) -> haider_tools::ToolResult<()> {
        Ok(())
    }
}

struct GatedProvider {
    requests: Mutex<Vec<TurnRequest>>,
    request_count: AtomicUsize,
    gate_started: Arc<Notify>,
    release_gate: Arc<Notify>,
}

impl GatedProvider {
    fn new() -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            request_count: AtomicUsize::new(0),
            gate_started: Arc::new(Notify::new()),
            release_gate: Arc::new(Notify::new()),
        }
    }

    fn requests(&self) -> Vec<TurnRequest> {
        self.requests.lock().expect("request lock").clone()
    }
}

#[async_trait]
impl Provider for GatedProvider {
    async fn capabilities(&self) -> CapabilityDoc {
        FakeProvider::new(Vec::new()).capabilities().await
    }

    async fn stream_turn(&self, request: TurnRequest) -> Result<ProviderStream, ProviderError> {
        self.requests.lock().expect("request lock").push(request);
        let request_index = self.request_count.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = mpsc::channel(4);
        if request_index == 0 {
            self.gate_started.notify_one();
            let release_gate = Arc::clone(&self.release_gate);
            tokio::spawn(async move {
                release_gate.notified().await;
                let _ = sender
                    .send(Ok(haider_protocol::provider::StreamEvent::Finish {
                        reason: FinishReason::EndTurn,
                    }))
                    .await;
            });
        } else {
            tokio::spawn(async move {
                let _ = sender
                    .send(Ok(haider_protocol::provider::StreamEvent::TextDelta {
                        text: "notice incorporated".into(),
                    }))
                    .await;
                let _ = sender
                    .send(Ok(haider_protocol::provider::StreamEvent::Finish {
                        reason: FinishReason::EndTurn,
                    }))
                    .await;
            });
        }
        Ok(receiver.into())
    }
}

/// MUTATION CHECK (LT4 — assert the COUNT, the W6 vacuous-pin lesson):
/// deliver the active-run completion as queued, skip the durable steer, or
/// double-inject it. Expected RUNTIME failure: the completion notice is
/// missing from the steered provider round, the durable steer count is not
/// EXACTLY one, the fact's delivery is not `delivered_steer`, or a second
/// prompt copy leaks (the steer-delivered fact must render `Omit`).
#[tokio::test]
async fn active_run_completion_steers_mid_turn_with_exactly_one_durable_nudge() {
    let profile = tempfile::tempdir().expect("profile");
    let (_workspace, cwd) = workspace();
    // This fixture journals the background effect to `DropJournal`, outside
    // the session store. Use a repository receipt so the no-op workspace path
    // remains precise and does not fabricate provenance for that absent effect.
    std::fs::create_dir(std::path::Path::new(&cwd).join(".git")).expect("repository marker");
    let store = SqliteStoreHandle::open(profile.path())
        .await
        .expect("store");
    let provider = Arc::new(GatedProvider::new());
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let manager = WorkerManager::start(
        hub.clone(),
        WorkerDependencies {
            diagnostics: None,
            provider_factory: Arc::new(FixedProviderFactory {
                provider: provider.clone(),
            }),
            tool_factory: Arc::new(BrokerToolFactory),
            delegation: None,
            web_search: None,
        },
        false,
    );
    hub.install_worker_manager(manager.handle())
        .expect("install manager");
    let session_id = create_task_session(&hub, "task-steer-session", &cwd).await;

    // A REAL provider turn, gated open so the run stays active.
    let live_run = RunId::new("task-steer-live-run");
    let accept_json = r#"{"turn":"task-steer-live"}"#;
    let accepted = hub
        .accept_internal_turn(TurnAcceptCommand {
            command_id: "task-steer-live-turn".into(),
            request_digest: crate::delegation::digest_bytes(accept_json.as_bytes()),
            request_json: accept_json.into(),
            session_id: session_id.clone(),
            worker_generation: hub.worker_generation(),
            branch_id: None,
            run_id: live_run.clone(),
            agent_id: None,
            text: "hold this turn open".into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Queue,
            queued_event_id: EventId::new("task-steer-live-queued"),
            user_event_id: EventId::new("task-steer-live-user"),
            active_event_id: EventId::new("task-steer-live-active"),
            device_id: DeviceId::new("task-steer-device"),
        })
        .await
        .expect("accept live turn");
    hub.submit_internal_turn(accepted)
        .await
        .expect("submit live turn");
    timeout(Duration::from_secs(5), provider.gate_started.notified())
        .await
        .expect("live turn reaches the provider");

    // A fast task completes WHILE the run is active. The spawn goes through
    // the facade with its own broker: acquiring a second worker lease here
    // would supersede (fence) the live supervisor's lease — in production
    // the dispatcher shares the turn's lease, which a test cannot borrow.
    let facade = TaskFacade::with_kill_grace(hub.clone(), Duration::from_millis(300));
    let mut broker = haider_tools::EffectBroker::new(
        Box::new(DropJournal),
        &cwd,
        session_id.clone(),
        hub.worker_generation(),
    )
    .expect("test broker");
    let mut policy = haider_tools::PermissionPolicy::default();
    policy.allow(haider_protocol::effect::EffectClass::ProcessExec);
    let spawn_result = facade
        .spawn_background(
            crate::tasks::TaskSpawnContext {
                session_id: session_id.clone(),
                run_id: live_run.clone(),
                branch_id: None,
                agent_id: None,
                call_id: "task-steer-spawn".into(),
            },
            "printf steer-payload".into(),
            None,
            Some("steered".into()),
            &mut broker,
            &policy,
        )
        .await
        .expect("background spawn while the run is live");
    let receipt: serde_json::Value =
        serde_json::from_str(&spawn_result.preview).expect("running receipt");
    assert_eq!(receipt["state"], "running");
    let task = TaskId::new(receipt["task_id"].as_str().expect("task id"));
    let (envelope, completed) = wait_for_completed(&store, &session_id, &task).await;
    assert_eq!(completed.delivery, TaskCompletionDelivery::DeliveredSteer);
    assert_eq!(
        envelope.render.prompt,
        PromptRender::Omit,
        "the steer user message owns the ONE prompt copy"
    );

    provider.release_gate.notify_one();
    timeout(Duration::from_secs(5), async {
        while provider.requests().len() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("steered provider round starts");
    let requests = provider.requests();
    let notice_in_round = |request: &TurnRequest| {
        request.messages.iter().any(|message| {
            message.blocks.iter().any(|block| {
                matches!(
                    block,
                    Block::Text { text }
                        if text.contains("[background task finished] steered")
                            && text.contains("steer-payload")
                )
            })
        })
    };
    assert!(
        notice_in_round(&requests[1]),
        "the completion notice reaches the steered round mid-turn"
    );

    // THE COUNT: exactly one durable steer user message carries the notice.
    let envelopes = read_all(&store, &session_id).await;
    let durable_steers = envelopes
        .iter()
        .filter(|envelope| {
            matches!(
                serde_json::from_value::<EventPayload>(envelope.payload.clone()),
                Ok(EventPayload::UserMessage { text, mode: DeliveryMode::Steer, .. })
                    if text.contains("[background task finished] steered")
            )
        })
        .count();
    assert_eq!(durable_steers, 1, "steer delivery journals EXACTLY once");

    timeout(Duration::from_secs(5), async {
        loop {
            let envelopes = read_all(&store, &session_id).await;
            let done = envelopes.iter().any(|envelope| {
                envelope.run_id.as_ref() == Some(&live_run)
                    && matches!(
                        serde_json::from_value::<EventPayload>(envelope.payload.clone()),
                        Ok(EventPayload::RunState(RunState::Done))
                    )
            });
            if done {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("live turn completes after the steer");

    let _ = broker.close().await;
    manager.shutdown().await.expect("manager shutdown");
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

/// MUTATION CHECK (LT7 cap + shutdown fence): drop the concurrency cap or
/// let the daemon exit without killing running pgids. Expected RUNTIME
/// failure: the ninth task starts instead of the typed refusal, or a group
/// survives `shutdown_background_tasks` / loses its Killed completion fact.
#[tokio::test]
async fn ninth_task_is_refused_and_shutdown_fence_kills_running_groups() {
    let profile = tempfile::tempdir().expect("profile");
    let (_workspace, cwd) = workspace();
    let store = SqliteStoreHandle::open(profile.path())
        .await
        .expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let session_id = create_task_session(&hub, "task-cap-session", &cwd).await;
    let run_id = RunId::new("task-cap-tool-run");
    prepare_tool_run(&hub, &session_id, &run_id, "task-cap").await;
    let dispatcher = task_dispatcher(&hub, &session_id, &cwd, "task-cap", &run_id).await;

    let mut tasks = Vec::new();
    for index in 0..TASK_CONCURRENCY_CAP {
        let receipt = dispatch(
            &dispatcher,
            &run_id,
            &format!("task-cap-spawn-{index}"),
            "process_exec",
            serde_json::json!({"command": "sleep 30", "background": true}),
        )
        .await;
        assert_eq!(receipt["state"], "running");
        tasks.push(TaskId::new(receipt["task_id"].as_str().expect("task id")));
    }
    assert_eq!(hub.task_registry().running_count(&session_id), 8);
    let refused = dispatch(
        &dispatcher,
        &run_id,
        "task-cap-spawn-9",
        "process_exec",
        serde_json::json!({"command": "sleep 30", "background": true}),
    )
    .await;
    assert_eq!(refused["status"], "refused");
    assert_eq!(refused["kind"], "task_cap_reached");
    assert_eq!(refused["running"], 8);
    assert_eq!(refused["cap"], 8);

    let envelopes = read_all(&store, &session_id).await;
    let pids = tasks
        .iter()
        .map(|task| started_fact(&envelopes, task).expect("started fact").1.pid)
        .collect::<Vec<_>>();
    hub.shutdown_background_tasks().await;
    for pid in &pids {
        wait_for_pgid_death(*pid).await;
    }
    assert_eq!(hub.task_registry().running_count(&session_id), 0);
    let envelopes = read_all(&store, &session_id).await;
    for task in &tasks {
        let (_, completed) = completed_fact(&envelopes, task).expect("killed completion fact");
        assert_eq!(completed.state, TaskTerminalState::Killed);
    }

    // The refusal is not sticky: capacity freed, a new task starts.
    let receipt = dispatch(
        &dispatcher,
        &run_id,
        "task-cap-after",
        "process_exec",
        serde_json::json!({"command": "printf freed", "background": true}),
    )
    .await;
    assert_eq!(receipt["state"], "running");
    let task = TaskId::new(receipt["task_id"].as_str().expect("task id"));
    let (_, completed) = wait_for_completed(&store, &session_id, &task).await;
    assert_eq!(
        completed.state,
        TaskTerminalState::Completed { exit_code: Some(0) }
    );

    dispatcher.close().await.expect("dispatcher close");
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

/// MUTATION CHECK (LT6): ignore the injected liveness seam, skip the stale
/// pgid kill, or journal nothing for an orphan. Expected RUNTIME failure:
/// the fake probe is never consulted (probe ledger), the live stale group
/// survives adoption, the orphan completion fact is missing, or a second
/// adoption duplicates it.
#[tokio::test]
async fn restart_adoption_reaps_stale_pgids_through_the_liveness_seam() {
    use std::os::unix::process::CommandExt as _;
    let profile = tempfile::tempdir().expect("profile");
    let (_workspace, cwd) = workspace();
    let store = SqliteStoreHandle::open(profile.path())
        .await
        .expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let session_id = create_task_session(&hub, "task-adopt-session", &cwd).await;
    let facade = TaskFacade::with_kill_grace(hub.clone(), Duration::from_millis(200));

    // Prior-life state: a REAL still-running group and a long-dead pid, each
    // with a started fact and no completed fact.
    let mut stale = std::process::Command::new("/bin/sh");
    stale.arg("-c").arg("sleep 30");
    stale.process_group(0);
    let mut stale = stale.spawn().expect("spawn stale fixture");
    let stale_pid = i32::try_from(stale.id()).expect("stale pid fits");
    let spawn_run = RunId::new("task-adopt-prior-run");
    let mut facts = Vec::new();
    for (event_id, task, name, pid) in [
        (
            "task-started-task-adopt-live",
            "task-adopt-live",
            "stale-live",
            stale_pid,
        ),
        (
            "task-started-task-adopt-dead",
            "task-adopt-dead",
            "stale-dead",
            999_999_990,
        ),
    ] {
        let started = TaskStarted {
            task: TaskId::new(task),
            name: name.into(),
            command: "sleep 30".into(),
            pid,
            started_at_ms: 1,
        };
        facts.push(crate::tasks::test_task_fact_envelope(
            &hub,
            &session_id,
            &spawn_run,
            event_id,
            started.to_payload_value().expect("payload"),
        ));
    }
    hub.append(&mut facts)
        .await
        .expect("append prior-life facts");

    let probed = Arc::new(Mutex::new(Vec::<i32>::new()));
    let ledger = Arc::clone(&probed);
    facade
        .adopt_session_with_probe(&session_id, move |pid| {
            ledger.lock().expect("probe ledger").push(pid);
            if pid == stale_pid {
                PidLiveness::Alive
            } else {
                PidLiveness::Dead
            }
        })
        .await
        .expect("adoption");
    {
        let probed = probed.lock().expect("probe ledger");
        assert!(probed.contains(&stale_pid), "the seam judges the live pid");
        assert!(
            probed.contains(&999_999_990),
            "the seam judges the dead pid"
        );
    }
    let status = stale.wait().expect("reap stale fixture zombie");
    assert!(!status.success(), "the stale group was killed by adoption");
    wait_for_pgid_death(stale_pid).await;

    let envelopes = read_all(&store, &session_id).await;
    let (live_envelope, live_completed) =
        completed_fact(&envelopes, &TaskId::new("task-adopt-live")).expect("live orphan fact");
    assert_eq!(
        live_completed.state,
        TaskTerminalState::Failed {
            reason: "orphaned by daemon restart; stale process group reaped".into()
        }
    );
    assert_eq!(
        live_completed.delivery,
        TaskCompletionDelivery::DeliveredQueued
    );
    assert_eq!(live_envelope.render.prompt, PromptRender::Verbatim);
    let (_, dead_completed) =
        completed_fact(&envelopes, &TaskId::new("task-adopt-dead")).expect("dead orphan fact");
    assert_eq!(
        dead_completed.state,
        TaskTerminalState::Failed {
            reason: "orphaned by daemon restart (process group already gone; output lost)".into()
        }
    );
    for task in ["task-adopt-live", "task-adopt-dead"] {
        let entry = hub
            .task_registry()
            .get(&session_id, &TaskId::new(task))
            .expect("adopted registry entry");
        assert!(matches!(entry.state, TaskLiveState::Terminal(_)));
    }

    // Adoption is once-per-life: a second call must not duplicate facts.
    facade
        .adopt_session_with_probe(&session_id, |_| PidLiveness::Alive)
        .await
        .expect("idempotent adoption");
    let envelopes = read_all(&store, &session_id).await;
    let completed_count = task_facts(&envelopes)
        .into_iter()
        .filter(|(_, fact)| matches!(fact, TaskEventPayload::TaskCompleted(_)))
        .count();
    assert_eq!(completed_count, 2, "no duplicate orphan completions");

    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

/// MUTATION CHECK (LT7 fence): let session deletion leave the pgid running,
/// or let turn end (dispatcher close) kill a background task. Expected
/// RUNTIME failure: the group survives `delete_session`, dies at dispatcher
/// close, or the registry keeps the deleted session's projection.
#[tokio::test]
async fn session_delete_fence_kills_the_running_group() {
    let profile = tempfile::tempdir().expect("profile");
    let (_workspace, cwd) = workspace();
    let store = SqliteStoreHandle::open(profile.path())
        .await
        .expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let session_id = create_task_session(&hub, "task-delete-session", &cwd).await;
    let run_id = RunId::new("task-delete-tool-run");
    prepare_tool_run(&hub, &session_id, &run_id, "task-delete").await;
    let dispatcher = task_dispatcher(&hub, &session_id, &cwd, "task-delete", &run_id).await;
    let receipt = dispatch(
        &dispatcher,
        &run_id,
        "task-delete-spawn",
        "process_exec",
        serde_json::json!({"command": "sleep 30", "background": true}),
    )
    .await;
    let task = TaskId::new(receipt["task_id"].as_str().expect("task id"));
    let envelopes = read_all(&store, &session_id).await;
    let pid = started_fact(&envelopes, &task).expect("started fact").1.pid;
    assert!(!pgid_dead(pid));

    terminalize_tool_run(&hub, &session_id, &run_id, "task-delete").await;
    // Turn end (dispatcher close) must NOT kill the task — esc/turn end and
    // session delete are different fences.
    dispatcher.close().await.expect("dispatcher close");
    assert!(
        !pgid_dead(pid),
        "turn end must never kill a background task"
    );

    hub.delete_session(session_id.clone())
        .await
        .expect("delete session");
    wait_for_pgid_death(pid).await;
    assert!(hub.task_registry().get(&session_id, &task).is_none());
    assert!(
        store
            .session_metadata(&session_id)
            .await
            .expect("read deleted metadata")
            .is_none()
    );

    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

/// turnhygiene pin. MUTATION CHECK: decode process output before hashing,
/// drop invalid bytes from the count, or skip the per-call process signal
/// when the transcript is not UTF-8. Expected RUNTIME failure: the model
/// projection is not the lossy text, `output_bytes`/digest stop describing
/// the raw bytes, or the journal lacks exactly one matching signal.
#[tokio::test]
async fn foreground_process_exec_projects_non_utf8_output_lossily_and_keeps_the_exact_digest() {
    let profile = tempfile::tempdir().expect("profile");
    let (_workspace, cwd) = workspace();
    let store = SqliteStoreHandle::open(profile.path())
        .await
        .expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let session_id = create_task_session(&hub, "task-non-utf8-session", &cwd).await;
    let run_id = RunId::new("task-non-utf8-tool-run");
    prepare_tool_run(&hub, &session_id, &run_id, "task-non-utf8").await;
    let dispatcher = task_dispatcher(&hub, &session_id, &cwd, "task-non-utf8", &run_id).await;

    let result = dispatch(
        &dispatcher,
        &run_id,
        "task-non-utf8-call",
        "process_exec",
        serde_json::json!({"command": "printf '\\377abc'; printf err >&2"}),
    )
    .await;
    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["status"], "completed");
    assert_eq!(
        result["output_bytes"], 7,
        "raw byte count includes the invalid byte"
    );
    let output = result["output"].as_str().expect("model-visible output");
    assert!(
        output.starts_with('\u{FFFD}'),
        "the invalid byte is projected as U+FFFD, not dropped: {output:?}"
    );
    assert!(
        output.contains("abc"),
        "valid bytes survive the projection: {output:?}"
    );
    assert!(
        output.contains("err"),
        "stderr is part of the projection: {output:?}"
    );
    let raw_digest = result["transcript_digest"]
        .as_str()
        .expect("transcript digest")
        .to_owned();
    assert!(raw_digest.starts_with("blake3:"));
    assert_ne!(
        raw_digest,
        format!("blake3:{}", blake3::hash(output.as_bytes()).to_hex()),
        "the digest covers the raw bytes, never the lossy projection"
    );

    let envelopes = read_all(&store, &session_id).await;
    let signals = envelopes
        .iter()
        .filter_map(|envelope| {
            serde_json::from_value::<EventPayload>(envelope.payload.clone())
                .ok()
                .and_then(|payload| match payload {
                    EventPayload::ProcessSignalRecorded(signal) => Some(signal),
                    _ => None,
                })
        })
        .collect::<Vec<_>>();
    assert_eq!(signals.len(), 1, "one process signal per foreground call");
    assert_eq!(signals[0].run_id, run_id);
    assert_eq!(signals[0].call_id, "task-non-utf8-call");
    assert_eq!(signals[0].exit_code, Some(0));
    assert_eq!(signals[0].transcript_digest, raw_digest);
    assert_eq!(
        Some(signals[0].subject_digest.as_str()),
        result["subject_digest"].as_str()
    );

    dispatcher.close().await.expect("dispatcher close");
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}
