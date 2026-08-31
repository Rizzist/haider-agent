#![allow(clippy::expect_used)]

#[cfg(unix)]
use crate::connection::{ConnectionContext, DrainNotice, serve};
use crate::delegation::{
    DelegationHandle, MessageCoordinates, SpawnCoordinates, callsign_from_identity,
};
use crate::session_hub::{SessionHub, SessionHubConfig};
use crate::worker::{
    BrokerToolFactory, ProviderFactory, ResolvedTurnProvider, TurnToolFactory, WorkerDependencies,
    WorkerManager, WorkerToolContext,
};
use async_trait::async_trait;
use haider_core::{
    BranchCreateCommand, CancelToken, EventIdGenerator, SessionCreateCommand, SqliteStoreHandle,
    StoreHandle, ToolDispatchResult, TurnAcceptCommand, TurnAdmissionDisposition,
    TurnCancelCommand,
};
use haider_protocol::DeliveryMode;
use haider_protocol::EventPayload;
use haider_protocol::agent::{AgentMessageDelivery, AgentMessageReceipt, AgentMessaged, ChipState};
use haider_protocol::effect::{EffectOutcome, EffectPhase};
use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
use haider_protocol::error::ErrorCode;
#[cfg(unix)]
use haider_protocol::ids::MenuId;
use haider_protocol::ids::{AgentId, BranchId, DeviceId, EventId, RunId, SessionId};
use haider_protocol::item::{ItemEvent, TurnItem};
#[cfg(unix)]
use haider_protocol::menu::Menu;
use haider_protocol::provider::{Block, CapabilityDoc, FinishReason};
use haider_protocol::session::SessionMetadataV1;
use haider_protocol::state::{RunState, WaitReason};
#[cfg(unix)]
use haider_provider::FakeInputKind;
use haider_provider::{
    FakeProvider, FakeStep, Provider, ProviderError, ProviderStream, TurnRequest,
};
#[cfg(unix)]
use haider_rpc::{
    AttachMode, Capability, CapabilitySet, ClientKind, CommandId, Hello, RequestBody, RequestId,
    ResponseBody, WIRE_PROTOCOL_VERSION, WireFrame, uds_codec,
};
use haider_tools::{MessageSubagent, SpawnSubagent};
#[cfg(unix)]
use std::collections::VecDeque;
#[cfg(unix)]
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
#[cfg(unix)]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(unix)]
use tokio::net::UnixStream;
#[cfg(unix)]
use tokio::sync::watch;
use tokio::sync::{Notify, mpsc};
use tokio::time::{Duration, timeout};

/// LAW E1d: a child resolves to the intersection of its requested grant and
/// its durable parent's ceiling. MUTATION: return the requested grant without
/// intersection and `fs_write` reappears, failing this test.
#[test]
fn e1d_child_grant_cannot_exceed_parent_and_within_ceiling_survives() {
    use haider_protocol::agent::Grant;
    use haider_protocol::effect::EffectClass;

    let parent = Grant {
        tools: vec!["fs_read".into(), "spawn_subagent".into()],
        effect_ceiling: vec![EffectClass::FsRead, EffectClass::AgentSpawn],
    };
    let resolved = crate::worker::intersect_grant(crate::worker::default_child_grant(), &parent);

    assert!(resolved.tools.contains(&"fs_read".to_owned()));
    assert!(resolved.tools.contains(&"spawn_subagent".to_owned()));
    assert!(!resolved.tools.contains(&"fs_write".to_owned()));
    assert!(!resolved.tools.contains(&"process_exec".to_owned()));
    assert!(resolved.effect_ceiling.contains(&EffectClass::FsRead));
    assert!(!resolved.effect_ceiling.contains(&EffectClass::FsWrite));

    let factory: Arc<dyn TurnToolFactory> = Arc::new(crate::worker::BrokerToolFactory);
    let declared = crate::worker::advertised_tool_definitions(
        &factory,
        Some(&resolved),
        "fake",
        crate::worker::WebCapabilityDegrade::default(),
    );
    let names = declared
        .iter()
        .map(|definition| definition.name.as_str())
        .collect::<Vec<_>>();
    assert!(names.contains(&"fs_read"));
    assert!(!names.contains(&"fs_write"));
    assert!(!names.contains(&"process_exec"));
}

#[test]
fn e1a_worker_maps_denial_anchor_miss_and_nonzero_process_to_failure_status() {
    use haider_protocol::ids::EffectId;
    use haider_protocol::item::ToolStatus;
    use haider_protocol::tool::ToolResultStatus;
    use haider_tools::{FsEditAnchorMismatch, ProcessResult};

    let denied = crate::worker::typed_tool_result(&haider_tools::ToolError::PermissionDenied {
        reason: "policy says no".into(),
    })
    .expect("typed denial");
    assert_eq!(denied.status, ToolResultStatus::Rejected);
    assert!(
        denied
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("denied"))
    );

    let anchor = crate::worker::typed_tool_result(&haider_tools::ToolError::EditAnchor(
        FsEditAnchorMismatch {
            path: "missing.txt".into(),
            matches: 0,
            replace_all: false,
        },
    ))
    .expect("typed anchor conflict");
    assert_eq!(anchor.status, ToolResultStatus::Conflict);
    assert!(
        anchor
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("anchor"))
    );

    let failed = crate::worker::process_result(ProcessResult {
        call_id: "exit-1".into(),
        effect: EffectId::new("effect-exit-1"),
        command_arg_digest: "blake3:args".into(),
        workspace_revision: None,
        status: ToolStatus::Failed,
        exit_code: Some(1),
        signal: None,
        output_bytes: 0,
        output_elided_bytes_at_least: 0,
        source_output_elided_bytes_at_least: 0,
        transcript_digest: format!("blake3:{}", blake3::hash(&[]).to_hex()),
        inline_output: Vec::new(),
        artifact: None,
        escalation_note: None,
        limit_reached: None,
        wall_timeout_ms: 1_000,
        max_output_bytes: 1_024,
        transcript_high_water_bytes: 0,
        lifecycle_events: Vec::new(),
    });
    assert_eq!(failed.status, ToolResultStatus::Failed);
    assert_eq!(failed.reason.as_deref(), Some("process exited with code 1"));
}

fn process_accounting_fixture(
    name: &str,
    output: &str,
    failed: bool,
) -> haider_tools::ProcessResult {
    use base64::Engine as _;

    haider_tools::ProcessResult {
        call_id: format!("fixture-{name}"),
        effect: haider_protocol::ids::EffectId::new(format!("effect-{name}")),
        command_arg_digest: format!("blake3:args-{name}"),
        workspace_revision: None,
        status: if failed {
            haider_protocol::item::ToolStatus::Failed
        } else {
            haider_protocol::item::ToolStatus::Completed
        },
        exit_code: failed.then_some(1),
        signal: None,
        output_bytes: output.len(),
        output_elided_bytes_at_least: 0,
        source_output_elided_bytes_at_least: 0,
        transcript_digest: format!("blake3:{}", blake3::hash(output.as_bytes()).to_hex()),
        inline_output: vec![haider_tools::ProcessOutputChunk {
            stream: haider_protocol::item::OutputStream::Stdout,
            chunk_b64: base64::engine::general_purpose::STANDARD.encode(output),
        }],
        artifact: None,
        escalation_note: None,
        limit_reached: None,
        wall_timeout_ms: 60_000,
        max_output_bytes: 2 * 1024 * 1024,
        transcript_high_water_bytes: output.len(),
        lifecycle_events: Vec::new(),
    }
}

/// Measures the complete serialized process-result projection, including JSON
/// escaping and marker/accounting overhead, rather than only reducer payloads.
#[test]
fn process_model_boundary_accounting_is_signed_deterministic_and_full_projection() {
    let listing = (0..100)
        .map(|index| format!("target/debug/deps/library-{index:03}.rlib"))
        .collect::<Vec<_>>()
        .join("\n");
    let grep = "src/lib.rs:9:needle\n".repeat(80);
    let cargo = concat!(
        "error[E0425]: cannot find value `missing` in this scope\n",
        " --> src/main.rs:3:5\n3 | missing();\n  | ^^^^^^^ not found\n",
        "For more information about this error, try rustc --explain E0425.\n",
        "error: could not compile `fixture` due to 1 previous error\n",
    );
    let file = "plain source line without adapter noise\n".repeat(80);
    let fixtures = [
        ("listing", listing, false),
        ("grep", grep, false),
        ("cargo", cargo.to_owned(), true),
        ("3kb-file", file, false),
    ];
    let mut total_before = 0u64;
    let mut total_after = 0u64;
    let mut total_net = 0i64;
    for (name, output, failed) in fixtures {
        let input = process_accounting_fixture(name, &output, failed);
        let result = crate::worker::process_result(input.clone());
        let replay = crate::worker::process_result(input);
        assert_eq!(result, replay, "same input must reproduce exactly: {name}");
        let value: serde_json::Value =
            serde_json::from_str(&result.preview).expect("process preview JSON");
        let (before, after, net) = if let Some(savings_value) = value.get("context_savings_detail")
        {
            let savings: haider_protocol::context::OutputSavings =
                serde_json::from_value(savings_value.clone()).expect("typed savings detail");
            assert_eq!(savings.scope, "process_result_model_boundary");
            assert_eq!(
                savings.output_bytes,
                u64::try_from(haider_tools::provider_request_text_projection_bytes(
                    &result.preview,
                ))
                .expect("fixture length fits u64")
            );
            assert_eq!(
                savings.estimated_net_tokens_saved,
                i64::try_from(savings.estimated_tokens_before).expect("fixture tokens fit i64")
                    - i64::try_from(savings.estimated_tokens_after)
                        .expect("fixture tokens fit i64")
            );
            assert!(
                value["output"]
                    .as_str()
                    .is_some_and(|output| output.lines().any(|line| {
                        serde_json::from_str::<serde_json::Value>(line)
                            .ok()
                            .and_then(|line| line.get("haider_elision_v1").cloned())
                            .is_some()
                    })),
                "{name}: {:?}",
                value["output"]
            );
            (
                savings.estimated_tokens_before,
                savings.estimated_tokens_after,
                savings.estimated_net_tokens_saved,
            )
        } else {
            let tokens = u64::try_from(
                haider_tools::provider_request_text_projection_bytes(&result.preview)
                    .saturating_add(3)
                    / 4,
            )
            .expect("fixture tokens fit u64");
            (tokens, tokens, 0)
        };
        total_before = total_before.saturating_add(before);
        total_after = total_after.saturating_add(after);
        total_net = total_net.saturating_add(net);
        eprintln!(
            "process-boundary fixture={name} before_tokens_estimate={before} after_tokens_estimate={after} net_tokens_saved_estimate={net}"
        );
    }
    assert_eq!(
        total_net,
        i64::try_from(total_before).expect("fixture tokens fit i64")
            - i64::try_from(total_after).expect("fixture tokens fit i64")
    );
    assert!(total_net > 0);
    let saved_per_million_input_tokens = total_net.saturating_mul(1_000_000)
        / i64::try_from(total_before).expect("fixture tokens fit i64");
    assert_eq!(
        (total_before, total_after, total_net),
        (2_787, 1_041, 1_746)
    );
    assert_eq!(saved_per_million_input_tokens, 626_480);
    eprintln!(
        "process-boundary cumulative measurement=provider_request_bytes_div_four_v1 before_tokens_estimate={total_before} after_tokens_estimate={total_after} net_tokens_saved_estimate={total_net} saved_per_1m_input_tokens_estimate={saved_per_million_input_tokens}"
    );

    let diagnostic = format!(
        "COMMAND cargo test --locked\n{}\nFAILURE: final linker diagnostic\n",
        (0..2_000)
            .map(|index| format!("unique progress line {index}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let source = process_accounting_fixture("head-tail", &diagnostic, true);
    let raw_chunk = source.inline_output[0].chunk_b64.clone();
    let first = crate::worker::process_result(source.clone());
    let second = crate::worker::process_result(source);
    assert_eq!(
        first.preview, second.preview,
        "elision must be deterministic"
    );
    let value: serde_json::Value =
        serde_json::from_str(&first.preview).expect("process preview JSON");
    let output = value["output"].as_str().expect("bounded output text");
    assert!(output.contains("COMMAND cargo test --locked"));
    assert!(output.contains("FAILURE: final linker diagnostic"));
    assert!(output.contains("\"haider_elision_v1\""));
    assert_eq!(
        raw_chunk,
        process_accounting_fixture("head-tail", &diagnostic, true).inline_output[0].chunk_b64,
        "model projection must not mutate the captured output"
    );
}

/// MUTATION CHECK: hard-code parent chip projection to `branch_id: None` or
/// omit `parent_branch_id` from the durable record. Expected RUNTIME failure:
/// the late child chip below paints main instead of branch A.
#[test]
fn delegation_parent_projection_is_pinned_to_the_spawn_branch() {
    use haider_core::{DelegationRecord, DelegationState};
    use haider_protocol::agent::{AgentManifest, AgentRole, Grant, Placement};
    use haider_protocol::ids::{AgentId, BranchId, ItemId, LeaseId};

    let branch_id = BranchId::new("parent-branch-a");
    let agent_id = AgentId::new("branch-child-agent");
    let record = DelegationRecord {
        agent_id: agent_id.clone(),
        child_session_id: SessionId::new("branch-child-session"),
        child_run_id: RunId::new("branch-child-run"),
        parent_session_id: SessionId::new("branch-parent-session"),
        parent_run_id: RunId::new("branch-parent-run"),
        parent_branch_id: Some(branch_id.clone()),
        call_id: "branch-spawn-call".into(),
        tool_item_id: ItemId::new("branch-spawn-item"),
        parent_agent_id: None,
        root_session_id: SessionId::new("branch-parent-session"),
        depth: 1,
        task: "test branch pin".into(),
        prompt: "report late".into(),
        manifest: AgentManifest {
            agent: agent_id.clone(),
            role: AgentRole::Subagent,
            task: "test branch pin".into(),
            callsign: None,
            model_profile: "fake-model".into(),
            grant: Grant {
                tools: Vec::new(),
                effect_ceiling: Vec::new(),
            },
            budget_tokens: Some(64),
            placement: Placement::Local,
            lease: LeaseId::new("branch-child-lease"),
            fencing_epoch: 1,
            attempt: 0,
            parent: None,
            coordinates: None,
            cli_scope: None,
        },
        state: DelegationState::Running,
        report: None,
    };
    let envelope = crate::delegation::chip_projection_envelope(
        &record,
        "late-chip-event",
        EventId::new("child-cause"),
        ChipState::Done,
        DeviceId::new("branch-chip-device"),
        7,
    )
    .expect("projection envelope");
    assert_eq!(envelope.branch_id, Some(branch_id.clone()));
    assert_eq!(envelope.run_id, Some(record.parent_run_id.clone()));
    assert!(matches!(
        serde_json::from_value::<EventPayload>(envelope.payload),
        Ok(EventPayload::AgentChipState { agent, chip: ChipState::Done })
            if agent == agent_id
    ));
    let snapshot = haider_protocol::agent::AgentMetricsSnapshot {
        agent: Some(agent_id.clone()),
        session_id: record.child_session_id.clone(),
        head_seq: 12,
        started_at_ms: 100,
        terminal_at_ms: None,
        live: true,
        tool_attempts: 2,
        usage: None,
    };
    let envelope = crate::delegation::metrics_projection_envelope(
        &record,
        "branch-metrics-event",
        EventId::new("child-metrics-cause"),
        snapshot.clone(),
        DeviceId::new("branch-metrics-device"),
        7,
    )
    .expect("metrics projection envelope");
    assert_eq!(envelope.branch_id, Some(branch_id.clone()));
    assert_eq!(envelope.run_id, Some(record.parent_run_id.clone()));
    assert_eq!(envelope.render.prompt, PromptRender::Omit);
    assert!(envelope.render.ui && envelope.render.durable);
    assert_eq!(
        haider_protocol::agent::AgentMetricsSnapshot::from_payload_value(&envelope.payload),
        Some(snapshot)
    );
    let message = format!("{}suffix", "é".repeat(205));
    let envelope = crate::delegation::agent_messaged_envelope(
        &record,
        "branch-message-event",
        EventId::new("child-message-cause"),
        &message,
        AgentMessageDelivery::DeliveredQueued,
        DeviceId::new("branch-message-device"),
        8,
    )
    .expect("message projection envelope");
    assert_eq!(envelope.branch_id, Some(branch_id));
    assert_eq!(envelope.run_id, Some(record.parent_run_id));
    assert_eq!(envelope.render.prompt, PromptRender::Omit);
    assert!(envelope.render.ui && envelope.render.durable);
    let fact = AgentMessaged::from_payload_value(&envelope.payload).expect("agent fact");
    assert_eq!(fact.agent, agent_id);
    assert_eq!(fact.delivery, AgentMessageDelivery::DeliveredQueued);
    assert_eq!(fact.preview.chars().count(), 200);
    assert_eq!(fact.preview, message.chars().take(200).collect::<String>());
}

/// MUTATION CHECK: drop the parent branch while converting
/// `SpawnCoordinates` into the durable record, or bypass spawn idempotency.
/// Expected RUNTIME failure: replay loses branch A or creates a second child
/// relation/run for the same parent tool call. Also remove the bounded
/// terminal tail: the final collection below then waits forever because this
/// fixture deliberately never appends the child SessionState::Idle fence.
#[tokio::test]
async fn established_spawn_captures_parent_branch_and_replays_one_child() {
    use haider_protocol::ids::{BranchId, ItemId};

    let root = tempfile::tempdir().expect("temp profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let parent_session = SessionId::new("branch-spawn-parent");
    let parent_run = RunId::new("branch-spawn-parent-run");
    let parent_branch = BranchId::new("branch-spawn-a");
    let cwd = std::fs::canonicalize(std::env::current_dir().expect("cwd"))
        .expect("canonical cwd")
        .to_string_lossy()
        .into_owned();
    hub.create_internal_session(SessionCreateCommand {
        command_id: "create-branch-spawn-parent".into(),
        request_digest: "create-branch-spawn-parent-digest".into(),
        request_json: r#"{"session":"branch-spawn-parent"}"#.into(),
        session_id: parent_session.clone(),
        cwd: cwd.clone(),
        provider: "fake".into(),
        model: "fake-model".into(),
        max_tokens: 4096,
        permission_overrides: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
        event_id: EventId::new("created-branch-spawn-parent"),
        device_id: DeviceId::new("branch-spawn-device"),
    })
    .await
    .expect("create parent");
    let metadata = SessionMetadataV1 {
        cwd,
        provider: "fake".into(),
        account_alias: None,
        model: "fake-model".into(),
        max_tokens: 4096,
        system_prompt_version: Some(crate::worker::SystemPromptBuilder::VERSION.into()),
        permission_overrides: None,
        interaction_mode: Default::default(),
        title: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        context_economy: Default::default(),
        created_at_ms: 1,
        agent_type: None,
    };
    let coordinates = || SpawnCoordinates {
        parent_session_id: parent_session.clone(),
        parent_run_id: parent_run.clone(),
        parent_branch_id: Some(parent_branch.clone()),
        parent_agent_id: None,
        tool_item_id: ItemId::new("branch-spawn-item"),
        call_id: "branch-spawn-call".into(),
        metadata: metadata.clone(),
        agent_type: None,
        lockdown: false,
        auto_hermetic: false,
    };
    let request = SpawnSubagent {
        task: "test branch pin".into(),
        prompt: "report after branch switch".into(),
        model: None,
        provider: None,
        workflow: None,
        workflow_trigger: None,
        parent_slot: None,
        workflow_author: false,
        agent_type: None,
    };
    let delegation = DelegationHandle::new(hub.clone());
    let first = delegation
        .establish(coordinates(), request.clone())
        .await
        .expect("establish child");
    let replay = delegation
        .establish(coordinates(), request)
        .await
        .expect("replay child establishment");
    assert_eq!(first.ticket.id, replay.ticket.id);

    let records = hub
        .delegations_for_parent_run(parent_session, parent_run)
        .await
        .expect("parent delegations");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].parent_branch_id, Some(parent_branch));
    let child_events = store
        .read(&records[0].child_session_id, 0, 128)
        .await
        .expect("child events");
    let spawn_prompts = child_events
        .iter()
        .filter(|event| event.run_id.as_ref() == Some(&records[0].child_run_id))
        .filter_map(|event| {
            let EventPayload::UserMessage { text, .. } =
                serde_json::from_value::<EventPayload>(event.payload.clone()).ok()?
            else {
                return None;
            };
            Some((event, text))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        spawn_prompts.len(),
        1,
        "replaying one spawn must not create a second live child turn"
    );
    let (spawn_prompt, text) = &spawn_prompts[0];
    assert_eq!(
        text,
        "Delegated task: test branch pin\n\nreport after branch switch\n\nReturn a concise final report for the parent agent."
    );
    assert!(spawn_prompt.render.ui && spawn_prompt.render.durable);
    assert_eq!(spawn_prompt.render.prompt, PromptRender::Verbatim);
    assert!(
        child_events
            .iter()
            .filter(|event| event.run_id.as_ref() == Some(&records[0].child_run_id))
            .all(|event| event.branch_id.is_none()),
        "the child session retains its own main branch"
    );

    let child_session = records[0].child_session_id.clone();
    let child_run = records[0].child_run_id.clone();
    let mut terminal = [EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new("branch-spawn-child-done"),
        seq: 0,
        session_id: child_session,
        branch_id: None,
        run_id: Some(child_run),
        agent_id: Some(first.ticket.manifest.agent.clone()),
        device_id: DeviceId::new("branch-spawn-device"),
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
        payload: serde_json::to_value(EventPayload::RunState(RunState::Done))
            .expect("child terminal payload"),
    }];
    hub.append(&mut terminal)
        .await
        .expect("append child terminal without idle settlement");
    let bounded =
        DelegationHandle::with_settlement_tail_timeout(hub.clone(), Duration::from_millis(50));
    let collected = timeout(
        Duration::from_secs(1),
        bounded.collect(&first.ticket, &CancelToken::new()),
    )
    .await
    .expect("durable child terminal releases the parent without idle settlement")
    .expect("collect terminal child");
    assert_eq!(
        collected.report.summary,
        "subagent completed without a text report"
    );

    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

struct InspectingProvider {
    inner: FakeProvider,
    store: SqliteStoreHandle,
    parent_session: SessionId,
    outcome_preceded_child: AtomicBool,
}

#[async_trait]
impl Provider for InspectingProvider {
    async fn capabilities(&self) -> CapabilityDoc {
        self.inner.capabilities().await
    }

    async fn stream_turn(&self, request: TurnRequest) -> Result<ProviderStream, ProviderError> {
        let is_child = request.messages.iter().any(|message| {
            message.blocks.iter().any(|block| {
                matches!(block, Block::Text { text } if text.starts_with("Delegated task:"))
            })
        });
        if is_child {
            let mut cursor = 0;
            let mut spawn_terminal = false;
            let mut observed = Vec::new();
            loop {
                let page = StoreHandle::read(&self.store, &self.parent_session, cursor, 256)
                    .await
                    .expect("read parent effect journal");
                if page.is_empty() {
                    break;
                }
                cursor = page.last().map_or(cursor, |event| event.seq);
                spawn_terminal |= page.into_iter().any(|event| {
                    observed.push(event.payload.clone());
                    serde_json::from_value::<EventPayload>(event.payload).is_ok_and(|payload| {
                        matches!(
                            payload,
                            EventPayload::Effect(EffectPhase::Outcome {
                                outcome: EffectOutcome::Ok,
                                ..
                            })
                        )
                    })
                });
            }
            assert!(spawn_terminal, "child started before outcome: {observed:?}");
            self.outcome_preceded_child
                .store(spawn_terminal, Ordering::SeqCst);
        }
        self.inner.stream_turn(request).await
    }
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

struct GatedSteerProvider {
    requests: Mutex<Vec<TurnRequest>>,
    request_count: AtomicUsize,
    gate_request: usize,
    gate_started: Arc<Notify>,
    release_gate: Arc<Notify>,
}

impl GatedSteerProvider {
    fn new(gate_request: usize) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            request_count: AtomicUsize::new(0),
            gate_request,
            gate_started: Arc::new(Notify::new()),
            release_gate: Arc::new(Notify::new()),
        }
    }

    fn requests(&self) -> Vec<TurnRequest> {
        self.requests.lock().expect("request lock").clone()
    }
}

#[async_trait]
impl Provider for GatedSteerProvider {
    async fn capabilities(&self) -> CapabilityDoc {
        FakeProvider::new(Vec::new()).capabilities().await
    }

    async fn stream_turn(&self, request: TurnRequest) -> Result<ProviderStream, ProviderError> {
        self.requests.lock().expect("request lock").push(request);
        let request_index = self.request_count.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = mpsc::channel(4);
        if request_index == self.gate_request {
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
                        text: "steer incorporated".into(),
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

/// MUTATION CHECK: route a running-child message through a fresh queued run,
/// skip the worker nudge, omit the parent fact, count preview bytes, or drop
/// the child handoff line. Expected RUNTIME failure: the second request is not
/// the same-round steer, the receipt/fact changes, or the UTF-8/path pins fail.
#[tokio::test]
#[cfg(unix)]
async fn message_subagent_steers_running_child_and_journals_bounded_parent_fact() {
    use haider_protocol::ids::ItemId;

    let root = tempfile::tempdir().expect("temp profile");
    let workspace = tempfile::tempdir().expect("workspace");
    let workspace = std::fs::canonicalize(workspace.path()).expect("canonical workspace");
    let workspace_text = workspace.to_string_lossy().into_owned();
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let provider = Arc::new(GatedSteerProvider::new(0));
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
    let parent_session = SessionId::new("message-running-parent");
    let parent_run = RunId::new("message-running-parent-run");
    hub.create_internal_session(SessionCreateCommand {
        command_id: "create-message-running-parent".into(),
        request_digest: "create-message-running-parent-digest".into(),
        request_json: r#"{"session":"message-running-parent"}"#.into(),
        session_id: parent_session.clone(),
        cwd: workspace_text.clone(),
        provider: "fake".into(),
        model: "fake-model".into(),
        max_tokens: 4096,
        permission_overrides: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
        event_id: EventId::new("created-message-running-parent"),
        device_id: DeviceId::new("message-running-device"),
    })
    .await
    .expect("create parent");
    terminalize_test_parent(&hub, &parent_session, &parent_run, "message-running").await;
    let handoff = crate::delegation::handoff_dir(&workspace_text, &parent_session);
    assert!(!handoff.exists(), "handoff must be lazy until first spawn");
    let delegation = DelegationHandle::new(hub.clone());
    let established = delegation
        .establish(
            SpawnCoordinates {
                parent_session_id: parent_session.clone(),
                parent_run_id: parent_run.clone(),
                parent_branch_id: None,
                parent_agent_id: None,
                tool_item_id: ItemId::new("message-running-spawn-item"),
                call_id: "message-running-spawn-call".into(),
                agent_type: None,
                lockdown: false,
                auto_hermetic: false,
                metadata: SessionMetadataV1 {
                    cwd: workspace_text.clone(),
                    provider: "fake".into(),
                    account_alias: None,
                    model: "fake-model".into(),
                    max_tokens: 4096,
                    system_prompt_version: Some(crate::worker::SystemPromptBuilder::VERSION.into()),
                    permission_overrides: None,
                    interaction_mode: Default::default(),
                    title: None,
                    effort: None,
                    fast: false,
                    cache_policy: Default::default(),
                    context_economy: Default::default(),
                    created_at_ms: 1,
                    agent_type: None,
                },
            },
            SpawnSubagent {
                task: "parser audit".into(),
                prompt: "inspect the parser state machine".into(),
                model: None,
                provider: None,
                workflow: None,
                workflow_trigger: None,
                parent_slot: None,
                workflow_author: false,
                agent_type: None,
            },
        )
        .await
        .expect("establish child");
    let manifest_handoff = established
        .ticket
        .manifest
        .coordinates
        .as_ref()
        .and_then(|coordinates| coordinates.get("handoff_dir"))
        .and_then(serde_json::Value::as_str)
        .expect("manifest handoff coordinate");
    assert_eq!(manifest_handoff, handoff.to_string_lossy());
    assert_eq!(
        std::fs::read(handoff.join(".gitignore")).expect("ignore"),
        b"*"
    );
    let child_record = hub
        .delegation(established.ticket.manifest.agent.clone())
        .await
        .expect("delegation lookup")
        .expect("delegation row");
    delegation.launch(&established).await.expect("launch child");
    timeout(Duration::from_secs(5), provider.gate_started.notified())
        .await
        .expect("first child request");
    let first_request = provider.requests().remove(0);
    let handoff_line = format!(
        "Ephemeral parent handoff directory: {manifest_handoff} (EPHEMERAL; use it for shared specs, never durable storage)."
    );
    assert!(
        first_request
            .system_prompt
            .as_deref()
            .is_some_and(|system| !system.contains(&handoff_line)),
        "session-specific handoff coordinates must stay out of the shared base"
    );
    assert!(first_request.messages.iter().any(|message| {
        matches!(message.blocks.as_slice(), [Block::Text { text }] if text.contains(&handoff_line))
    }));

    let mut control = UdsControlClient::connect(hub.clone()).await;
    control.attach_control(parent_session.clone()).await;
    let message = format!("{}tail", "界".repeat(205));
    let receipt = control
        .message_agent(
            "message-running-command",
            parent_session.clone(),
            store.worker_generation(),
            established.ticket.manifest.agent.clone(),
            message.clone(),
        )
        .await
        .expect("steer running child over RPC");
    assert_eq!(receipt.delivery, AgentMessageDelivery::DeliveredSteer);
    assert_eq!(receipt.child_run_id, child_record.child_run_id);

    let replay = control
        .message_agent(
            "message-running-command",
            parent_session.clone(),
            store.worker_generation(),
            established.ticket.manifest.agent.clone(),
            message.clone(),
        )
        .await
        .expect("same message command replays");
    assert_eq!(replay, receipt);
    let conflict = control
        .message_agent(
            "message-running-command",
            parent_session.clone(),
            store.worker_generation(),
            established.ticket.manifest.agent.clone(),
            "different text must not cross the durable command fence".into(),
        )
        .await
        .expect_err("changed replay semantics rejected");
    assert_eq!(conflict.0, "invalid_argument");
    assert!(conflict.1.contains("different semantics"));
    provider.release_gate.notify_one();

    timeout(Duration::from_secs(5), async {
        while provider.requests().len() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("steered provider request");
    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        2,
        "same-command replay must not inject twice"
    );
    assert!(requests[1].messages.iter().any(|provider_message| {
        provider_message
            .blocks
            .iter()
            .any(|block| matches!(block, Block::Text { text } if text == &message))
    }));
    let child_events = store
        .read(&child_record.child_session_id, 0, 256)
        .await
        .expect("child journal");
    let steered_runs = child_events
        .iter()
        .filter(|event| {
            serde_json::from_value::<EventPayload>(event.payload.clone()).is_ok_and(|payload| {
                matches!(payload, EventPayload::UserMessage { text, .. } if text == message)
            })
        })
        .filter_map(|event| event.run_id.clone())
        .collect::<Vec<_>>();
    assert_eq!(steered_runs, vec![receipt.child_run_id.clone()]);

    let parent_events = store
        .read(&parent_session, 0, 256)
        .await
        .expect("parent journal");
    let fact = parent_events
        .iter()
        .find_map(|event| {
            AgentMessaged::from_payload_value(&event.payload).map(|fact| (event, fact))
        })
        .expect("parent AgentMessaged fact");
    assert_eq!(fact.1.agent, established.ticket.manifest.agent);
    assert_eq!(fact.1.delivery, AgentMessageDelivery::DeliveredSteer);
    assert_eq!(fact.1.preview.chars().count(), 200);
    assert_eq!(
        fact.1.preview,
        message.chars().take(200).collect::<String>()
    );
    assert!(fact.0.render.ui && fact.0.render.durable);
    assert_eq!(fact.0.render.prompt, PromptRender::Omit);

    control.close().await;
    hub.delete_session(parent_session.clone())
        .await
        .expect("delete quiesced parent session");
    assert!(
        store
            .session_metadata(&parent_session)
            .await
            .expect("read deleted parent metadata")
            .is_none()
    );
    assert!(
        !handoff.exists(),
        "parent deletion cleans ephemeral handoff"
    );
    manager.shutdown().await.expect("manager shutdown");
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

/// MUTATION CHECK: reuse the terminal spawn run or queue without submitting
/// it immediately. Expected RUNTIME failure: the receipt is not queued, the
/// run id is unchanged, or the second provider request never starts.
#[tokio::test]
async fn message_subagent_starts_an_idle_child_immediately() {
    use haider_protocol::ids::ItemId;

    let root = tempfile::tempdir().expect("temp profile");
    let workspace = tempfile::tempdir().expect("workspace");
    let workspace = std::fs::canonicalize(workspace.path())
        .expect("canonical workspace")
        .to_string_lossy()
        .into_owned();
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let provider = Arc::new(GatedSteerProvider::new(1));
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
    let parent_session = SessionId::new("message-idle-parent");
    let parent_run = RunId::new("message-idle-parent-run");
    hub.create_internal_session(SessionCreateCommand {
        command_id: "create-message-idle-parent".into(),
        request_digest: "create-message-idle-parent-digest".into(),
        request_json: r#"{"session":"message-idle-parent"}"#.into(),
        session_id: parent_session.clone(),
        cwd: workspace.clone(),
        provider: "fake".into(),
        model: "fake-model".into(),
        max_tokens: 4096,
        permission_overrides: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
        event_id: EventId::new("created-message-idle-parent"),
        device_id: DeviceId::new("message-idle-device"),
    })
    .await
    .expect("create parent");
    terminalize_test_parent(&hub, &parent_session, &parent_run, "message-idle").await;
    let delegation = DelegationHandle::new(hub.clone());
    let established = delegation
        .establish(
            SpawnCoordinates {
                parent_session_id: parent_session.clone(),
                parent_run_id: parent_run.clone(),
                parent_branch_id: None,
                parent_agent_id: None,
                tool_item_id: ItemId::new("message-idle-spawn-item"),
                call_id: "message-idle-spawn-call".into(),
                agent_type: None,
                lockdown: false,
                auto_hermetic: false,
                metadata: SessionMetadataV1 {
                    cwd: workspace.clone(),
                    provider: "fake".into(),
                    account_alias: None,
                    model: "fake-model".into(),
                    max_tokens: 4096,
                    system_prompt_version: Some(crate::worker::SystemPromptBuilder::VERSION.into()),
                    permission_overrides: None,
                    interaction_mode: Default::default(),
                    title: None,
                    effort: None,
                    fast: false,
                    cache_policy: Default::default(),
                    context_economy: Default::default(),
                    created_at_ms: 1,
                    agent_type: None,
                },
            },
            SpawnSubagent {
                task: "first pass".into(),
                prompt: "finish once".into(),
                model: None,
                provider: None,
                workflow: None,
                workflow_trigger: None,
                parent_slot: None,
                workflow_author: false,
                agent_type: None,
            },
        )
        .await
        .expect("establish child");
    let child_record = hub
        .delegation(established.ticket.manifest.agent.clone())
        .await
        .expect("delegation lookup")
        .expect("delegation row");
    let child_session = child_record.child_session_id.clone();
    delegation.launch(&established).await.expect("launch child");
    wait_for_state(&store, &child_session, |state| *state == RunState::Done).await;
    let parent_lease = hub
        .acquire_worker_lease(parent_session.clone())
        .await
        .expect("parent tool lease");
    let dispatcher = TurnToolFactory::create(
        &BrokerToolFactory,
        WorkerToolContext {
            lockdown: None,
            diagnostics: None,
            metadata: SessionMetadataV1 {
                cwd: workspace,
                provider: "fake".into(),
                account_alias: None,
                model: "fake-model".into(),
                max_tokens: 4096,
                system_prompt_version: Some(crate::worker::SystemPromptBuilder::VERSION.into()),
                permission_overrides: None,
                interaction_mode: Default::default(),
                title: None,
                effort: None,
                fast: false,
                cache_policy: Default::default(),
                context_economy: Default::default(),
                created_at_ms: 1,
                agent_type: None,
            },
            store: parent_lease,
            run_id: parent_run.clone(),
            run_deadline: None,
            branch_id: None,
            device_id: DeviceId::new("message-idle-tool-device"),
            event_ids: Arc::new(EventIdGenerator::new("message-idle-tool-event")),
            delegation: delegation.clone(),
            tasks: crate::tasks::TaskFacade::new(hub.clone()),
            agent_id: None,
            session_context_tail: String::new(),
            grant: None,
            mobile_use_active: false,
            cli_scope: None,
            typed_workflow_execution: None,
            loom_provider_fenced: false,
            web_search: None,
        },
    )
    .await
    .expect("create production tool dispatcher")
    .expect("dispatcher available");
    let tool_result = dispatcher
        .execute(
            &parent_run,
            &haider_protocol::ids::ItemId::new("message-idle-tool-item"),
            "message-idle-tool-call",
            "message_subagent",
            serde_json::json!({
                "agent": established.ticket.manifest.agent.clone(),
                "message": "perform the follow-up pass"
            }),
            &CancelToken::new(),
        )
        .await
        .expect("execute production message_subagent tool");
    let ToolDispatchResult::Completed(tool_result) = tool_result else {
        panic!("message_subagent must complete with a receipt")
    };
    let receipt: AgentMessageReceipt =
        serde_json::from_str(&tool_result.preview).expect("decode tool delivery receipt");
    assert_eq!(receipt.delivery, AgentMessageDelivery::DeliveredQueued);
    assert_ne!(receipt.child_run_id, child_record.child_run_id);
    timeout(Duration::from_secs(5), provider.gate_started.notified())
        .await
        .expect("idle child starts second request");
    let running_tool_result = dispatcher
        .execute(
            &parent_run,
            &haider_protocol::ids::ItemId::new("message-idle-running-tool-item"),
            "message-idle-running-tool-call",
            "message_subagent",
            serde_json::json!({
                "agent": established.ticket.manifest.agent,
                "message": "steer the live follow-up before it finishes"
            }),
            &CancelToken::new(),
        )
        .await
        .expect("execute running-child message_subagent tool");
    let ToolDispatchResult::Completed(running_tool_result) = running_tool_result else {
        panic!("running message_subagent must complete with a receipt")
    };
    let running_receipt: AgentMessageReceipt = serde_json::from_str(&running_tool_result.preview)
        .expect("decode running tool delivery receipt");
    assert_eq!(
        running_receipt.delivery,
        AgentMessageDelivery::DeliveredSteer
    );
    assert_eq!(running_receipt.child_run_id, receipt.child_run_id);
    provider.release_gate.notify_one();
    timeout(Duration::from_secs(5), async {
        while provider.requests().len() < 3 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("running child consumes tool steer");
    assert!(
        provider.requests()[1]
            .messages
            .iter()
            .any(|provider_message| {
                provider_message.blocks.iter().any(|block| {
            matches!(block, Block::Text { text } if text == "perform the follow-up pass")
        })
            })
    );
    assert!(provider.requests()[2].messages.iter().any(|provider_message| {
        provider_message.blocks.iter().any(|block| {
            matches!(block, Block::Text { text } if text == "steer the live follow-up before it finishes")
        })
    }));
    manager.shutdown().await.expect("manager shutdown");
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

/// MUTATION CHECK: authorize by target id alone or by ancestry instead of
/// exact direct ownership. Expected RUNTIME failure: the foreign parent call
/// succeeds or loses the typed `not_owned_child` detail.
#[tokio::test]
async fn only_own_children_are_messageable_with_typed_error() {
    use haider_protocol::ids::ItemId;

    let root = tempfile::tempdir().expect("temp profile");
    let workspace = tempfile::tempdir().expect("workspace");
    let workspace = std::fs::canonicalize(workspace.path())
        .expect("canonical workspace")
        .to_string_lossy()
        .into_owned();
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let owner = SessionId::new("message-owner-parent");
    hub.create_internal_session(SessionCreateCommand {
        command_id: "create-message-owner-parent".into(),
        request_digest: "create-message-owner-parent-digest".into(),
        request_json: r#"{"session":"message-owner-parent"}"#.into(),
        session_id: owner.clone(),
        cwd: workspace.clone(),
        provider: "fake".into(),
        model: "fake-model".into(),
        max_tokens: 4096,
        permission_overrides: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
        event_id: EventId::new("created-message-owner-parent"),
        device_id: DeviceId::new("message-owner-device"),
    })
    .await
    .expect("create owner");
    let delegation = DelegationHandle::new(hub.clone());
    let established = delegation
        .establish(
            SpawnCoordinates {
                parent_session_id: owner,
                parent_run_id: RunId::new("message-owner-run"),
                parent_branch_id: None,
                parent_agent_id: None,
                tool_item_id: ItemId::new("message-owner-item"),
                call_id: "message-owner-call".into(),
                agent_type: None,
                lockdown: false,
                auto_hermetic: false,
                metadata: SessionMetadataV1 {
                    cwd: workspace,
                    provider: "fake".into(),
                    account_alias: None,
                    model: "fake-model".into(),
                    max_tokens: 4096,
                    system_prompt_version: Some(crate::worker::SystemPromptBuilder::VERSION.into()),
                    permission_overrides: None,
                    interaction_mode: Default::default(),
                    title: None,
                    effort: None,
                    fast: false,
                    cache_policy: Default::default(),
                    context_economy: Default::default(),
                    created_at_ms: 1,
                    agent_type: None,
                },
            },
            SpawnSubagent {
                task: "owned child".into(),
                prompt: "do not cross parents".into(),
                model: None,
                provider: None,
                workflow: None,
                workflow_trigger: None,
                parent_slot: None,
                workflow_author: false,
                agent_type: None,
            },
        )
        .await
        .expect("establish owned child");
    let error = delegation
        .message(
            MessageCoordinates {
                parent_session_id: SessionId::new("foreign-parent"),
                parent_agent_id: None,
                command_id: "foreign-message".into(),
            },
            MessageSubagent {
                agent: established.ticket.manifest.agent,
                message: "steal this child".into(),
            },
        )
        .await
        .expect_err("foreign parent rejected");
    assert_eq!(error.code, ErrorCode::InvalidArgument);
    assert_eq!(
        error
            .details
            .as_ref()
            .and_then(|details| details["kind"].as_str()),
        Some("not_owned_child")
    );
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

/// MUTATION CHECK: submit the child before terminalizing AgentSpawn, keep the
/// tool effect open for the child's lifetime, skip Waiting(LocalChild), or
/// resume without the report. Expected runtime failure: the child provider
/// observes no spawn outcome, the parent state chain is wrong, or its second
/// request lacks `child report`.
#[tokio::test]
async fn production_spawn_effect_wait_and_report_chain_is_end_to_end() {
    let root = tempfile::tempdir().expect("temp profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let parent_session = SessionId::new("w6a-parent-session");
    let provider = Arc::new(InspectingProvider {
        inner: FakeProvider::new(vec![
            FakeStep::EmitToolCall {
                call_id: "spawn-call".into(),
                name: "spawn_subagent".into(),
                args: serde_json::json!({
                    "task": "tests",
                    "prompt": "run the focused test suite"
                }),
            },
            FakeStep::Finish {
                reason: FinishReason::ToolUse,
            },
            FakeStep::EmitText {
                text: "child report".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
            FakeStep::ExpectToolResult {
                call_id: "spawn-call".into(),
            },
            FakeStep::EmitText {
                text: "parent merged report".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ]),
        store: store.clone(),
        parent_session: parent_session.clone(),
        outcome_preceded_child: AtomicBool::new(false),
    });
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
    let manager_handle = manager.handle();
    hub.install_worker_manager(manager_handle.clone())
        .expect("install manager");
    let cwd = std::fs::canonicalize(std::env::current_dir().expect("cwd"))
        .expect("canonical cwd")
        .to_string_lossy()
        .into_owned();
    hub.create_internal_session(SessionCreateCommand {
        command_id: "create-w6a-parent".into(),
        request_digest: "create-w6a-parent-digest".into(),
        request_json: r#"{"session":"w6a-parent"}"#.into(),
        session_id: parent_session.clone(),
        cwd,
        provider: "fake".into(),
        model: "fake-model".into(),
        max_tokens: 4096,
        permission_overrides: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
        event_id: EventId::new("created-w6a-parent"),
        device_id: DeviceId::new("w6a-test-device"),
    })
    .await
    .expect("create parent");
    let fork_run = RunId::new("w6a-parent-fork-run");
    hub.accept_internal_turn(TurnAcceptCommand {
        command_id: "submit-w6a-parent-fork".into(),
        request_digest: "submit-w6a-parent-fork-digest".into(),
        request_json: r#"{"turn":"w6a-parent-fork"}"#.into(),
        session_id: parent_session.clone(),
        worker_generation: store.worker_generation(),
        run_id: fork_run.clone(),
        agent_id: None,
        branch_id: None,
        text: "stable delegation fork".into(),
        attachments: Vec::new(),
        mode: DeliveryMode::Queue,
        queued_event_id: EventId::new("w6a-parent-fork-queued"),
        user_event_id: EventId::new("w6a-parent-fork-user"),
        active_event_id: EventId::new("w6a-parent-fork-active"),
        device_id: DeviceId::new("w6a-test-device"),
    })
    .await
    .expect("accept delegation fork");
    let mut fork_done = [EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new("w6a-parent-fork-done"),
        seq: 0,
        session_id: parent_session.clone(),
        branch_id: None,
        run_id: Some(fork_run.clone()),
        agent_id: None,
        device_id: DeviceId::new("w6a-test-device"),
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
        payload: serde_json::to_value(EventPayload::RunState(RunState::Done))
            .expect("fork done payload"),
    }];
    hub.append(&mut fork_done)
        .await
        .expect("terminalize delegation fork");
    let fork_events = store
        .read(&parent_session, 0, 64)
        .await
        .expect("fork events");
    let (fork_node, fork_seq) = fork_events
        .iter()
        .find_map(|event| {
            let EventPayload::NodeCommitted(node) =
                serde_json::from_value::<EventPayload>(event.payload.clone()).ok()?
            else {
                return None;
            };
            (event.run_id.as_ref() == Some(&fork_run)).then_some((node.node, event.seq))
        })
        .expect("delegation fork node");
    let branch_a = BranchId::new("w6a-parent-branch-a");
    let branch_b = BranchId::new("w6a-parent-branch-b");
    for (command_id, branch_id) in [
        ("create-w6a-parent-a", branch_a.clone()),
        ("create-w6a-parent-b", branch_b),
    ] {
        let request_json = serde_json::json!({"branch": branch_id}).to_string();
        store
            .create_branch(BranchCreateCommand {
                command_id: command_id.into(),
                request_digest: blake3::hash(request_json.as_bytes()).to_hex().to_string(),
                request_json,
                session_id: parent_session.clone(),
                worker_generation: store.worker_generation(),
                branch_id,
                source_branch_id: None,
                fork_node_id: fork_node.clone(),
                fork_seq,
                name: None,
                event_id: EventId::new(format!("event-{command_id}")),
                device_id: DeviceId::new("w6a-test-device"),
            })
            .await
            .expect("create parent branch");
    }
    let parent_run = RunId::new("w6a-parent-run");
    let accepted = hub
        .accept_internal_turn(TurnAcceptCommand {
            command_id: "submit-w6a-parent".into(),
            request_digest: "submit-w6a-parent-digest".into(),
            request_json: r#"{"turn":"w6a-parent"}"#.into(),
            session_id: parent_session.clone(),
            worker_generation: store.worker_generation(),
            run_id: parent_run.clone(),
            agent_id: None,
            branch_id: Some(branch_a.clone()),
            text: "delegate the tests".into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Steer,
            queued_event_id: EventId::new("w6a-parent-queued"),
            user_event_id: EventId::new("w6a-parent-user"),
            active_event_id: EventId::new("w6a-parent-active"),
            device_id: DeviceId::new("w6a-test-device"),
        })
        .await
        .expect("accept parent");
    assert_eq!(accepted.disposition, TurnAdmissionDisposition::Started);
    manager_handle
        .submit(accepted)
        .await
        .expect("submit parent worker");

    timeout(Duration::from_secs(10), async {
        loop {
            let events = store
                .read(&parent_session, 0, 512)
                .await
                .expect("read parent");
            if events.iter().any(|event| {
                event.run_id.as_ref() == Some(&parent_run)
                    && serde_json::from_value::<EventPayload>(event.payload.clone()).is_ok_and(
                        |payload| matches!(payload, EventPayload::RunState(RunState::Done)),
                    )
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("parent completes");

    let requests = provider.inner.requests();
    assert!(
        provider.outcome_preceded_child.load(Ordering::SeqCst),
        "spawn outcome must commit before child provider work: {requests:?}"
    );
    assert_eq!(requests.len(), 3);
    // W6c deliberately supersedes W6a's nonrecursive assertion: children
    // retain the tool so the depth cap can return a provider-readable result.
    assert!(
        requests[1]
            .tools
            .iter()
            .any(|tool| tool.name == "spawn_subagent"),
        "W6c children may recurse through the same production tool"
    );
    assert!(requests[2].messages.iter().any(|message| {
        message.blocks.iter().any(|block| {
            matches!(
                block,
                Block::ToolResult { call_id, preview, .. }
                    if call_id == "spawn-call"
                        && preview.starts_with("agent: agent-")
                        && preview.ends_with("\n\nchild report")
            )
        })
    }));
    let parent_events = store.read(&parent_session, 0, 512).await.expect("parent");
    for event in parent_events
        .iter()
        .filter(|event| event.run_id.as_ref() == Some(&parent_run))
    {
        if matches!(
            serde_json::from_value::<EventPayload>(event.payload.clone()),
            Ok(EventPayload::SessionState(_))
        ) {
            assert_eq!(event.branch_id, None);
        } else {
            assert_eq!(event.branch_id, Some(branch_a.clone()));
        }
    }
    let payloads = parent_events
        .iter()
        .filter_map(|event| serde_json::from_value::<EventPayload>(event.payload.clone()).ok())
        .collect::<Vec<_>>();
    let waiting = payloads
        .iter()
        .position(|payload| {
            matches!(
                payload,
                EventPayload::RunState(RunState::Waiting {
                    reason: WaitReason::LocalChild
                })
            )
        })
        .expect("parent waited");
    let resumed = payloads
        .iter()
        .enumerate()
        .skip(waiting + 1)
        .find_map(|(index, payload)| {
            matches!(payload, EventPayload::RunState(RunState::Thinking)).then_some(index)
        })
        .expect("parent resumed");
    assert!(
        !payloads[waiting + 1..resumed]
            .iter()
            .any(|payload| matches!(payload, EventPayload::SessionState(_)))
    );
    assert!(payloads.iter().any(|payload| {
        matches!(
            payload,
            EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::ChildResult { report },
                ..
            }) if report.summary == "child report"
        )
    }));
    let spawned = payloads
        .iter()
        .find_map(|payload| match payload {
            EventPayload::AgentSpawned(manifest) => Some(manifest.clone()),
            _ => None,
        })
        .expect("spawn manifest");
    assert_eq!(spawned.task, "tests");
    let projected_prompt = parent_events
        .iter()
        .find(|event| {
            event.agent_id.as_ref() == Some(&spawned.agent)
                && serde_json::from_value::<EventPayload>(event.payload.clone()).is_ok_and(
                    |payload| {
                        matches!(
                            payload,
                            EventPayload::UserMessage { text, .. }
                                if text.contains("run the focused test suite")
                        )
                    },
                )
        })
        .expect("spawn prompt projected into child-scoped parent timeline");
    assert_eq!(projected_prompt.branch_id, Some(branch_a.clone()));
    assert!(projected_prompt.render.ui && projected_prompt.render.durable);
    assert_eq!(projected_prompt.render.prompt, PromptRender::Omit);
    // X1: the handle is stable identity data, not a restatement or parsing of
    // the already-separate task. Malformed/non-digest input stays unnamed.
    let identity = spawned
        .agent
        .as_str()
        .strip_prefix("agent-")
        .expect("minted agent id prefix");
    assert_eq!(
        spawned.callsign,
        callsign_from_identity(identity),
        "manifest persists the deterministic digest handle"
    );
    assert_ne!(spawned.callsign.as_deref(), Some(spawned.task.as_str()));
    assert_eq!(callsign_from_identity("not-a-delegation-digest"), None);
    let delegation = hub
        .delegation(spawned.agent.clone())
        .await
        .expect("delegation lookup")
        .expect("delegation row");
    let child_events = store
        .read(&delegation.child_session_id, 0, 512)
        .await
        .expect("child events");
    assert!(child_events.iter().all(|event| {
        event.run_id.as_ref() != Some(&delegation.child_run_id)
            || event.agent_id.as_ref() == Some(&spawned.agent)
    }));

    // OWNER DIRECTIVE (W6d): delegation is AUTOMATIC — the child is
    // created with writes+exec pre-allowed regardless of the parent's own
    // overrides (the parent here carries None), so a child tool call can
    // never park on a human.
    // MUTATION CHECK: inherit the parent's overrides (or None) in
    // `spawn_child`'s create — this assertion fails.
    let child_metadata = store
        .session_metadata(&delegation.child_session_id)
        .await
        .expect("child metadata read")
        .expect("child metadata present");
    let overrides = child_metadata
        .permission_overrides
        .expect("child overrides present");
    assert!(overrides.allow_writes && overrides.allow_exec);

    manager.shutdown().await.expect("manager shutdown");
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

fn test_cwd() -> String {
    std::fs::canonicalize(std::env::current_dir().expect("cwd"))
        .expect("canonical cwd")
        .to_string_lossy()
        .into_owned()
}

async fn accept_parent(
    hub: &SessionHub,
    session_id: &SessionId,
    run_id: &RunId,
    label: &str,
) -> haider_core::AcceptedTurn {
    accept_parent_with_interaction_mode(
        hub,
        session_id,
        run_id,
        label,
        haider_protocol::session::SessionInteractionModeV1::Interactive,
    )
    .await
}

async fn accept_parent_with_interaction_mode(
    hub: &SessionHub,
    session_id: &SessionId,
    run_id: &RunId,
    label: &str,
    interaction_mode: haider_protocol::session::SessionInteractionModeV1,
) -> haider_core::AcceptedTurn {
    hub.create_internal_session_with_interaction_mode(
        SessionCreateCommand {
            command_id: format!("create-{label}"),
            request_digest: format!("create-{label}-digest"),
            request_json: format!(r#"{{"session":"{label}"}}"#),
            session_id: session_id.clone(),
            cwd: test_cwd(),
            provider: "fake".into(),
            model: "fake-model".into(),
            max_tokens: 4096,
            permission_overrides: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
            event_id: EventId::new(format!("created-{label}")),
            device_id: DeviceId::new("w6c-test-device"),
        },
        interaction_mode,
    )
    .await
    .expect("create parent");
    hub.accept_internal_turn(TurnAcceptCommand {
        command_id: format!("submit-{label}"),
        request_digest: format!("submit-{label}-digest"),
        request_json: format!(r#"{{"turn":"{label}"}}"#),
        session_id: session_id.clone(),
        worker_generation: hub.worker_generation(),
        run_id: run_id.clone(),
        agent_id: None,
        branch_id: None,
        text: "delegate recursively".into(),
        attachments: Vec::new(),
        mode: DeliveryMode::Steer,
        queued_event_id: EventId::new(format!("queued-{label}")),
        user_event_id: EventId::new(format!("user-{label}")),
        active_event_id: EventId::new(format!("active-{label}")),
        device_id: DeviceId::new("w6c-test-device"),
    })
    .await
    .expect("accept parent")
}

async fn terminalize_test_parent(
    hub: &SessionHub,
    session_id: &SessionId,
    run_id: &RunId,
    label: &str,
) {
    hub.accept_internal_turn(TurnAcceptCommand {
        command_id: format!("submit-{label}-parent"),
        request_digest: format!("submit-{label}-parent-digest"),
        request_json: format!(r#"{{"turn":"{label}-parent"}}"#),
        session_id: session_id.clone(),
        worker_generation: hub.worker_generation(),
        run_id: run_id.clone(),
        agent_id: None,
        branch_id: None,
        text: format!("parent coordinates for {label}"),
        attachments: Vec::new(),
        mode: DeliveryMode::Queue,
        queued_event_id: EventId::new(format!("queued-{label}-parent")),
        user_event_id: EventId::new(format!("user-{label}-parent")),
        active_event_id: EventId::new(format!("active-{label}-parent")),
        device_id: DeviceId::new("s1-message-test-device"),
    })
    .await
    .expect("accept test parent");
    let mut terminal = [EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(format!("done-{label}-parent")),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: Some(run_id.clone()),
        agent_id: None,
        device_id: DeviceId::new("s1-message-test-device"),
        authority_epoch: 0,
        worker_generation: hub.worker_generation(),
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::to_value(EventPayload::RunState(RunState::Done))
            .expect("parent done payload"),
    }];
    hub.append(&mut terminal)
        .await
        .expect("terminalize test parent");
}

async fn wait_for_state(
    store: &SqliteStoreHandle,
    session_id: &SessionId,
    expected: impl Fn(&RunState) -> bool,
) {
    timeout(Duration::from_secs(5), async {
        loop {
            let events = store.read(session_id, 0, 1024).await.expect("read run");
            if events.iter().any(|event| {
                serde_json::from_value::<EventPayload>(event.payload.clone()).is_ok_and(
                    |payload| matches!(payload, EventPayload::RunState(state) if expected(&state)),
                )
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("expected run state");
}

/// Autonomous interaction mode is durable child-session policy, not a root
/// headless reducer trick: a child request_input without a default returns a
/// typed tool rejection and both child and parent terminate.
#[cfg(unix)]
#[tokio::test]
async fn autonomous_child_request_input_cannot_hold_parent_forever() {
    let root = tempfile::tempdir().expect("temp profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "autonomous-child-spawn".into(),
            name: "spawn_subagent".into(),
            args: serde_json::json!({"task":"ask","prompt":"ask, then continue"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::EmitRequestInput {
            call_id: "autonomous-child-ask".into(),
            kind: FakeInputKind::Question,
            title: "which value?".into(),
            body: Vec::new(),
            options: Vec::new(),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "autonomous-child-ask".into(),
        },
        FakeStep::EmitText {
            text: "child continued without invented input".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::ExpectToolResult {
            call_id: "autonomous-child-spawn".into(),
        },
        FakeStep::EmitText {
            text: "parent collected autonomous child".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let manager = WorkerManager::start(
        hub.clone(),
        WorkerDependencies {
            diagnostics: None,
            provider_factory: Arc::new(FixedProviderFactory { provider }),
            tool_factory: Arc::new(BrokerToolFactory),
            delegation: Some(DelegationHandle::with_stall_deadline(
                hub.clone(),
                Duration::from_secs(3),
            )),
            web_search: None,
        },
        false,
    );
    let manager_handle = manager.handle();
    hub.install_worker_manager(manager_handle.clone())
        .expect("install manager");
    let parent_session = SessionId::new("autonomous-child-parent");
    let parent_run = RunId::new("autonomous-child-parent-run");
    let accepted = accept_parent_with_interaction_mode(
        &hub,
        &parent_session,
        &parent_run,
        "autonomous-child",
        haider_protocol::session::SessionInteractionModeV1::Autonomous,
    )
    .await;
    manager_handle
        .submit(accepted)
        .await
        .expect("submit parent");
    wait_for_state(&store, &parent_session, |state| {
        matches!(state, RunState::Done)
    })
    .await;

    let child = hub
        .delegations_for_parent_run(parent_session.clone(), parent_run)
        .await
        .expect("delegation")
        .pop()
        .expect("child");
    let child_metadata = store
        .session_metadata(&child.child_session_id)
        .await
        .expect("metadata read")
        .expect("metadata");
    assert_eq!(
        child_metadata.interaction_mode,
        haider_protocol::session::SessionInteractionModeV1::Autonomous
    );
    let payloads = typed_payloads(
        &store
            .read(&child.child_session_id, 0, 1024)
            .await
            .expect("child events"),
    );
    assert!(!payloads.iter().any(|payload| matches!(
        payload,
        EventPayload::RunState(RunState::InputRequired { .. })
    )));
    assert!(payloads.iter().any(|payload| matches!(
        payload,
        EventPayload::ToolResult { result, .. }
            if serde_json::from_str::<serde_json::Value>(&result.preview)
                .is_ok_and(|value| value["code"] == "no_human_available")
    )));

    manager.shutdown().await.expect("manager shutdown");
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

// Every caller of this helper lives in a `cfg(unix)` test (the UDS/menu
// scenarios), so on Windows it has no callers at all and `-D warnings`
// turns dead_code into a build failure. Gate the helper with its callers.
#[cfg(unix)]
fn typed_payloads(events: &[haider_protocol::envelope::RawEnvelope]) -> Vec<EventPayload> {
    events
        .iter()
        // Raw journals may contain additive payload kinds introduced after
        // this exhaustive core enum. Keep this helper scoped to the typed
        // W6 payloads it is named for instead of rejecting forward facts.
        .filter_map(|event| serde_json::from_value(event.payload.clone()).ok())
        .collect()
}

#[cfg(unix)]
enum ParkedChildMode {
    Complete,
    StallAfterApproval,
}

#[cfg(unix)]
struct ParkedChildHarness {
    _root: tempfile::TempDir,
    store: SqliteStoreHandle,
    hub: SessionHub,
    manager: WorkerManager,
    parent_session: SessionId,
    child: haider_core::DelegationRecord,
    menu: Menu,
    request_seq: u64,
}

#[cfg(unix)]
async fn start_parked_child(
    label: &str,
    mode: ParkedChildMode,
    stall_deadline: Duration,
) -> ParkedChildHarness {
    let root = tempfile::tempdir().expect("temp profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let mut script = vec![
        FakeStep::EmitToolCall {
            call_id: format!("{label}-spawn"),
            name: "spawn_subagent".into(),
            args: serde_json::json!({"task":"permission","prompt":"run one command"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        // W6d owner directive: children are AUTO-ALLOWED for writes/exec —
        // a permission park is unreachable. The parked-on-human laws ride
        // the still-real `request_input` (InputRequired) park instead.
        FakeStep::EmitRequestInput {
            call_id: format!("{label}-ask"),
            kind: FakeInputKind::Question,
            title: "which value?".into(),
            body: Vec::new(),
            options: Vec::new(),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: format!("{label}-ask"),
        },
    ];
    match mode {
        ParkedChildMode::Complete => script.extend([
            FakeStep::EmitText {
                text: "child continued after permission".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ]),
        ParkedChildMode::StallAfterApproval => script.push(FakeStep::Hang),
    }
    script.extend([
        FakeStep::ExpectToolResult {
            call_id: format!("{label}-spawn"),
        },
        FakeStep::EmitText {
            text: "parent collected permission child".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let provider = Arc::new(FakeProvider::new(script));
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let manager = WorkerManager::start(
        hub.clone(),
        WorkerDependencies {
            diagnostics: None,
            provider_factory: Arc::new(FixedProviderFactory { provider }),
            tool_factory: Arc::new(BrokerToolFactory),
            delegation: Some(DelegationHandle::with_stall_deadline(
                hub.clone(),
                stall_deadline,
            )),
            web_search: None,
        },
        false,
    );
    let manager_handle = manager.handle();
    hub.install_worker_manager(manager_handle.clone())
        .expect("install manager");
    let parent_session = SessionId::new(format!("w6d-{label}-parent"));
    let parent_run = RunId::new(format!("w6d-{label}-parent-run"));
    let accepted = accept_parent(&hub, &parent_session, &parent_run, label).await;
    manager_handle
        .submit(accepted)
        .await
        .expect("submit permission parent");
    wait_for_state(&store, &parent_session, |state| {
        matches!(
            state,
            RunState::Waiting {
                reason: WaitReason::LocalChild
            }
        )
    })
    .await;
    let child = hub
        .delegations_for_parent_run(parent_session.clone(), parent_run)
        .await
        .expect("permission delegation")
        .pop()
        .expect("permission child");
    wait_for_state(&store, &child.child_session_id, |state| {
        matches!(state, RunState::InputRequired { .. })
    })
    .await;
    let events = store
        .read(&child.child_session_id, 0, 1024)
        .await
        .expect("permission child events");
    let permission_menu = events
        .iter()
        .find_map(|envelope| {
            let payload = serde_json::from_value::<EventPayload>(envelope.payload.clone()).ok()?;
            match payload {
                EventPayload::MenuOpened(menu)
                    if matches!(menu.kind, haider_protocol::menu::MenuKind::Question) =>
                {
                    Some((menu, envelope.seq))
                }
                _ => None,
            }
        })
        .expect("permission menu and opening sequence");
    ParkedChildHarness {
        _root: root,
        store,
        hub,
        manager,
        parent_session,
        child,
        menu: permission_menu.0,
        request_seq: permission_menu.1,
    }
}

#[cfg(unix)]
struct UdsControlClient {
    stream: UnixStream,
    decoder: uds_codec::Decoder,
    pending: VecDeque<WireFrame>,
    drain_sender: watch::Sender<Option<DrainNotice>>,
    serve_task: tokio::task::JoinHandle<()>,
    writer_owner: tokio::task::JoinHandle<()>,
}

#[cfg(unix)]
impl UdsControlClient {
    async fn connect(hub: SessionHub) -> Self {
        let (server, stream) = UnixStream::pair().expect("live UDS pair");
        let (writers, mut writer_tasks) = mpsc::unbounded_channel();
        let writer_owner = tokio::spawn(async move {
            while let Some(task) = writer_tasks.recv().await {
                let _ = task.await;
            }
        });
        let context = ConnectionContext {
            profile_id: "w6d-test-profile".into(),
            instance_id: "w6d-test-instance".into(),
            daemon_generation: hub.worker_generation(),
            frame_limit: haider_rpc::DEFAULT_FRAME_LIMIT,
            outbound_queue_capacity: 64,
            outbound_queued_bytes: 4 * 1024 * 1024,
            max_connections: 4,
            handshake_timeout: Duration::from_secs(5),
            writers,
            owner_uid: rustix::process::geteuid().as_raw(),
            hub,
            shutdown: crate::lifecycle::ShutdownHandle::channel().0,
            endpoint_path: PathBuf::from("/tmp/w6d-child-control.sock"),
            pid_file_path: PathBuf::from("/tmp/haiderd.pid"),
        };
        let (drain_sender, drain) = watch::channel(Option::<DrainNotice>::None);
        let serve_task = tokio::spawn(async move {
            let _ = serve(server, context, drain).await;
        });
        let mut client = Self {
            stream,
            decoder: uds_codec::Decoder::new(haider_rpc::DEFAULT_FRAME_LIMIT),
            pending: VecDeque::new(),
            drain_sender,
            serve_task,
            writer_owner,
        };
        client
            .send(WireFrame::Hello(Hello {
                protocol_min: WIRE_PROTOCOL_VERSION,
                protocol_max: WIRE_PROTOCOL_VERSION,
                client_name: "w6d-child-control".into(),
                client_version: "test".into(),
                client_instance_id: "w6d-child-control-1".into(),
                client_kind: ClientKind::Cli,
                capabilities_requested: CapabilitySet::from([
                    Capability::View,
                    Capability::Control,
                ]),
                max_receive_frame: u32::try_from(haider_rpc::DEFAULT_FRAME_LIMIT)
                    .expect("test frame limit fits u32"),
                encodings: Vec::new(),
            }))
            .await;
        loop {
            if matches!(client.next().await, WireFrame::Welcome(_)) {
                break;
            }
        }
        client
    }

    async fn send(&mut self, frame: WireFrame) {
        let bytes =
            uds_codec::encode(&frame, haider_rpc::DEFAULT_FRAME_LIMIT).expect("UDS frame encodes");
        self.stream
            .write_all(&bytes)
            .await
            .expect("UDS frame writes");
    }

    async fn next(&mut self) -> WireFrame {
        if let Some(frame) = self.pending.pop_front() {
            return frame;
        }
        let mut buffer = [0_u8; 8192];
        loop {
            let read = self
                .stream
                .read(&mut buffer)
                .await
                .expect("UDS frame reads");
            assert_ne!(read, 0, "UDS server closed before the expected frame");
            let batch = self.decoder.push(&buffer[..read]);
            assert!(
                batch.error.is_none(),
                "UDS decoder error: {:?}",
                batch.error
            );
            self.pending.extend(batch.frames);
            if let Some(frame) = self.pending.pop_front() {
                return frame;
            }
        }
    }

    async fn attach_control(&mut self, session_id: SessionId) {
        let request_id = RequestId::new("w6d-child-attach");
        self.send(WireFrame::Request {
            request_id: request_id.clone(),
            body: RequestBody::SessionAttach {
                session_id,
                after_seq: 0,
                mode: AttachMode::Control,
                sealed_replay: false,
            },
        })
        .await;
        let attachment_id = loop {
            match self.next().await {
                WireFrame::Response {
                    request_id: observed,
                    body: ResponseBody::SessionAttach { attachment_id, .. },
                } if observed == request_id => break attachment_id,
                WireFrame::Response {
                    request_id: observed,
                    body: ResponseBody::Error { code, message, .. },
                } if observed == request_id => panic!("child attach failed: {code}: {message}"),
                _ => {}
            }
        };
        loop {
            if matches!(
                self.next().await,
                WireFrame::AttachCaughtUp {
                    attachment_id: observed,
                    ..
                } if observed == attachment_id
            ) {
                return;
            }
        }
    }

    async fn answer_question(
        &mut self,
        session_id: SessionId,
        menu_id: MenuId,
        request_seq: u64,
        worker_generation: u64,
    ) {
        let request_id = RequestId::new("w6d-child-answer");
        self.send(WireFrame::MenuAnswer {
            request_id: Some(request_id.clone()),
            command_id: CommandId::new("w6d-child-answer-command"),
            session_id,
            menu_id,
            request_seq,
            worker_generation,
            // A zero-option Question menu: empty key, index 0, the typed
            // text rides `input` (the store's option-less validation arm).
            option_key: "".into(),
            option_index: 0,
            input: Some(haider_rpc::MenuInput::Text {
                text: "w6d-answer".into(),
            }),
        })
        .await;
        loop {
            match self.next().await {
                WireFrame::Response {
                    request_id: observed,
                    body: ResponseBody::MenuAnswer { .. },
                } if observed == request_id => return,
                WireFrame::Response {
                    request_id: observed,
                    body: ResponseBody::Error { code, message, .. },
                } if observed == request_id => {
                    panic!("child menu answer failed: {code}: {message}")
                }
                _ => {}
            }
        }
    }

    async fn message_agent(
        &mut self,
        command_id: &str,
        session_id: SessionId,
        worker_generation: u64,
        agent: AgentId,
        text: String,
    ) -> Result<AgentMessageReceipt, (String, String)> {
        let request_id = RequestId::new(format!("agent-message-{command_id}"));
        self.send(WireFrame::Request {
            request_id: request_id.clone(),
            body: RequestBody::AgentMessage {
                command_id: CommandId::new(command_id),
                session_id,
                worker_generation,
                agent,
                text,
            },
        })
        .await;
        loop {
            match self.next().await {
                WireFrame::Response {
                    request_id: observed,
                    body: ResponseBody::AgentMessage { receipt },
                } if observed == request_id => return Ok(receipt),
                WireFrame::Response {
                    request_id: observed,
                    body: ResponseBody::Error { code, message, .. },
                } if observed == request_id => return Err((code, message)),
                _ => {}
            }
        }
    }

    async fn close(self) {
        let Self {
            mut stream,
            drain_sender,
            serve_task,
            writer_owner,
            ..
        } = self;
        drop(drain_sender);
        let _ = stream.shutdown().await;
        drop(stream);
        timeout(Duration::from_secs(5), serve_task)
            .await
            .expect("UDS serve task stops")
            .expect("UDS serve task joins");
        timeout(Duration::from_secs(5), writer_owner)
            .await
            .expect("UDS writer owner stops")
            .expect("UDS writer owner joins");
    }
}

#[cfg(unix)]
async fn wait_for_chip(
    store: &SqliteStoreHandle,
    session_id: &SessionId,
    agent: &haider_protocol::ids::AgentId,
    expected: ChipState,
) {
    timeout(Duration::from_secs(5), async {
        loop {
            let payloads = typed_payloads(
                &store
                    .read(session_id, 0, 1024)
                    .await
                    .expect("read parent chips"),
            );
            if payloads.iter().any(|payload| {
                matches!(
                    payload,
                    EventPayload::AgentChipState { agent: observed, chip }
                        if observed == agent && *chip == expected
                )
            }) {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("expected parent chip state");
}

/// MUTATION CHECK: drop the child's run-state mirror or map permission parks
/// to Thinking. Expected RUNTIME failure: the parent journal never carries
/// the exact PermissionRequired chip for the delegated agent.
#[tokio::test]
#[cfg(unix)]
async fn child_permission_park_is_visible_in_the_parent_chip_journal() {
    let harness = start_parked_child(
        "permission-chip",
        ParkedChildMode::Complete,
        Duration::from_secs(30),
    )
    .await;
    wait_for_chip(
        &harness.store,
        &harness.parent_session,
        &harness.child.agent_id,
        ChipState::InputRequired,
    )
    .await;

    harness.manager.shutdown().await.expect("manager shutdown");
    harness.hub.shutdown().await.expect("hub shutdown");
    harness.store.close().await.expect("store close");
}

/// MUTATION CHECK: let the stall clock ignore a PermissionRequired park or
/// leave supervision disabled after the park resolves. Expected RUNTIME
/// failure: the child is nudged/cancelled before approval, or it never receives
/// exactly one nudge and cancellation after unpark.
#[tokio::test]
#[cfg(unix)]
async fn permission_park_pauses_stall_supervision_and_unpark_rearms_it() {
    // 300ms: under parallel suite load the child's first round can
    // exceed a 35ms stall deadline BEFORE the park commits — a legal
    // pre-park nudge that the parked-silence law must not miss-read
    // (5/5 standalone, flaky in-suite at 35ms).
    let harness = start_parked_child(
        "permission-stall",
        ParkedChildMode::StallAfterApproval,
        Duration::from_millis(300),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(160)).await;
    let parked_payloads = typed_payloads(
        &harness
            .store
            .read(&harness.child.child_session_id, 0, 1024)
            .await
            .expect("parked child events"),
    );
    assert!(!parked_payloads.iter().any(|payload| matches!(
        payload,
        EventPayload::UserMessage { text, .. } if text == "report your status or conclude"
    )));
    assert!(
        !parked_payloads
            .iter()
            .any(|payload| matches!(payload, EventPayload::RunState(RunState::Cancelled)))
    );

    let mut control = UdsControlClient::connect(harness.hub.clone()).await;
    control
        .attach_control(harness.child.child_session_id.clone())
        .await;
    control
        .answer_question(
            harness.child.child_session_id.clone(),
            harness.menu.id.clone(),
            harness.request_seq,
            harness.store.worker_generation(),
        )
        .await;
    wait_for_state(&harness.store, &harness.child.child_session_id, |state| {
        *state == RunState::Cancelled
    })
    .await;
    wait_for_state(&harness.store, &harness.parent_session, |state| {
        *state == RunState::Done
    })
    .await;
    let resumed_payloads = typed_payloads(
        &harness
            .store
            .read(&harness.child.child_session_id, 0, 1024)
            .await
            .expect("resumed child events"),
    );
    assert_eq!(
        resumed_payloads
            .iter()
            .filter(|payload| matches!(
                payload,
                EventPayload::UserMessage { text, .. }
                    if text == "report your status or conclude"
            ))
            .count(),
        1
    );

    control.close().await;
    harness.manager.shutdown().await.expect("manager shutdown");
    harness.hub.shutdown().await.expect("hub shutdown");
    harness.store.close().await.expect("store close");
}

/// MUTATION CHECK: refuse Control attach to a normal child session, route the
/// answer outside the live UDS menu CAS, or fail to wake the child's parked
/// actor. Expected RUNTIME failure: attach/answer returns a typed error, the
/// child never reaches Done, or the parent never collects the child's report.
#[tokio::test]
#[cfg(unix)]
async fn control_attach_and_menu_answer_over_uds_complete_a_child_session() {
    let harness = start_parked_child(
        "permission-uds",
        ParkedChildMode::Complete,
        Duration::from_secs(30),
    )
    .await;
    let mut control = UdsControlClient::connect(harness.hub.clone()).await;
    control
        .attach_control(harness.child.child_session_id.clone())
        .await;
    control
        .answer_question(
            harness.child.child_session_id.clone(),
            harness.menu.id.clone(),
            harness.request_seq,
            harness.store.worker_generation(),
        )
        .await;
    wait_for_state(&harness.store, &harness.child.child_session_id, |state| {
        *state == RunState::Done
    })
    .await;
    wait_for_state(&harness.store, &harness.parent_session, |state| {
        *state == RunState::Done
    })
    .await;
    let parent_payloads = typed_payloads(
        &harness
            .store
            .read(&harness.parent_session, 0, 1024)
            .await
            .expect("parent collected events"),
    );
    assert!(parent_payloads.iter().any(|payload| matches!(
        payload,
        EventPayload::AgentReport(report)
            if report.agent == harness.child.agent_id
                && report.summary == "child continued after permission"
    )));

    control.close().await;
    harness.manager.shutdown().await.expect("manager shutdown");
    harness.hub.shutdown().await.expect("hub shutdown");
    harness.store.close().await.expect("store close");
}

/// MUTATION CHECK: remove the nudge step or allow a second nudge. Expected
/// runtime failure: the exact durable nudge count is not one. MUTATION CHECK:
/// remove the grace cancellation. Expected runtime failure: the parent never
/// reaches Done with the stall-reason report.
#[tokio::test]
#[cfg(unix)]
async fn stalled_child_is_nudged_once_cancelled_and_settles_the_parent_report() {
    let root = tempfile::tempdir().expect("temp profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "stall-spawn".into(),
            name: "spawn_subagent".into(),
            args: serde_json::json!({"task":"stall","prompt":"wait forever"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::Hang,
        FakeStep::ExpectToolResult {
            call_id: "stall-spawn".into(),
        },
        FakeStep::EmitText {
            text: "parent resumed after stall".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let delegation = DelegationHandle::with_stall_deadline(hub.clone(), Duration::from_millis(35));
    let manager = WorkerManager::start(
        hub.clone(),
        WorkerDependencies {
            diagnostics: None,
            provider_factory: Arc::new(FixedProviderFactory {
                provider: provider.clone(),
            }),
            tool_factory: Arc::new(BrokerToolFactory),
            delegation: Some(delegation),
            web_search: None,
        },
        false,
    );
    let manager_handle = manager.handle();
    hub.install_worker_manager(manager_handle.clone())
        .expect("install manager");
    let parent_session = SessionId::new("w6c-stall-parent");
    let parent_run = RunId::new("w6c-stall-parent-run");
    let accepted = accept_parent(&hub, &parent_session, &parent_run, "w6c-stall").await;
    manager_handle
        .submit(accepted)
        .await
        .expect("submit parent");
    wait_for_state(&store, &parent_session, |state| *state == RunState::Done).await;

    let delegations = hub
        .delegations_for_parent_run(parent_session.clone(), parent_run)
        .await
        .expect("delegations");
    assert_eq!(delegations.len(), 1);
    let child = &delegations[0];
    let child_events = store
        .read(&child.child_session_id, 0, 1024)
        .await
        .expect("child events");
    let child_payloads = typed_payloads(&child_events);
    assert_eq!(
        child_payloads
            .iter()
            .filter(|payload| matches!(
                payload,
                EventPayload::UserMessage { text, .. }
                    if text == "report your status or conclude"
            ))
            .count(),
        1,
        "stall policy permits exactly one durable nudge"
    );
    assert!(
        child_payloads
            .iter()
            .any(|payload| { matches!(payload, EventPayload::RunState(RunState::Cancelled)) })
    );
    let parent_payloads = typed_payloads(
        &store
            .read(&parent_session, 0, 1024)
            .await
            .expect("parent events"),
    );
    assert!(parent_payloads.iter().any(|payload| {
        matches!(
            payload,
            EventPayload::AgentReport(report)
                if report.summary.contains("stalled after one nudge")
        )
    }));

    manager.shutdown().await.expect("manager shutdown");
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

/// MUTATION CHECK: base the deadline on spawn time instead of the newest
/// committed progress. Expected runtime failure: a nudge UserMessage appears
/// while the slow child is still emitting reasoning deltas.
#[tokio::test]
#[cfg(unix)]
async fn committed_child_progress_resets_the_stall_deadline() {
    let root = tempfile::tempdir().expect("temp profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let mut script = vec![
        FakeStep::EmitToolCall {
            call_id: "slow-spawn".into(),
            name: "spawn_subagent".into(),
            args: serde_json::json!({"task":"slow","prompt":"make steady progress"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
    ];
    for index in 0..7 {
        script.push(FakeStep::Delay { ms: 100 });
        script.push(FakeStep::EmitReasoning {
            text: format!("heartbeat-{index}"),
        });
    }
    script.extend([
        FakeStep::EmitText {
            text: "slow child report".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::ExpectToolResult {
            call_id: "slow-spawn".into(),
        },
        FakeStep::EmitText {
            text: "slow child merged".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let provider = Arc::new(FakeProvider::new(script));
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let manager = WorkerManager::start(
        hub.clone(),
        WorkerDependencies {
            diagnostics: None,
            provider_factory: Arc::new(FixedProviderFactory {
                provider: provider.clone(),
            }),
            tool_factory: Arc::new(BrokerToolFactory),
            delegation: Some(DelegationHandle::with_stall_deadline(
                hub.clone(),
                Duration::from_millis(500),
            )),
            web_search: None,
        },
        false,
    );
    let manager_handle = manager.handle();
    hub.install_worker_manager(manager_handle.clone())
        .expect("install manager");
    let parent_session = SessionId::new("w6c-progress-parent");
    let parent_run = RunId::new("w6c-progress-parent-run");
    let accepted = accept_parent(&hub, &parent_session, &parent_run, "w6c-progress").await;
    manager_handle
        .submit(accepted)
        .await
        .expect("submit parent");
    wait_for_state(&store, &parent_session, |state| *state == RunState::Done).await;

    let child = hub
        .delegations_for_parent_run(parent_session, parent_run)
        .await
        .expect("delegations")
        .pop()
        .expect("child");
    let child_payloads = typed_payloads(
        &store
            .read(&child.child_session_id, 0, 1024)
            .await
            .expect("child events"),
    );
    assert!(!child_payloads.iter().any(|payload| matches!(
        payload,
        EventPayload::UserMessage { text, .. } if text == "report your status or conclude"
    )));

    manager.shutdown().await.expect("manager shutdown");
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

/// MUTATION CHECK (review round, W6c): make the post-nudge cancel window
/// ignore committed progress (deadline from `nudge_at_ms` alone). Expected
/// runtime failure: the recovering child below — silent long enough to
/// draw the nudge, then steadily productive — is cancelled anyway, so the
/// no-Cancelled assertion (and the child's own report reaching the
/// parent) dies. A nudge is a question, not a sentence.
#[tokio::test]
#[cfg(unix)]
async fn a_child_that_recovers_after_the_nudge_is_never_cancelled() {
    // Linux CI can defer both Tokio tasks past a 5ms nominal margin. Give the
    // silent child two complete 25ms poll opportunities after its 35ms-style
    // threshold, while retaining enough post-nudge room for the first
    // recovered heartbeat. The 120ms heartbeat train still outlasts a mutant
    // cancellation window measured from the nudge alone.
    // ONE generous timing envelope for every platform: a fast host satisfies
    // the same margins a deferred-scheduling CI runner needs, and a single
    // choreography can't drift green-on-one-OS/red-on-the-other (the cfg-split
    // round did exactly that).
    const STALL_DEADLINE: Duration = Duration::from_millis(100);
    const INITIAL_SILENCE_MS: u64 = 175;

    let root = tempfile::tempdir().expect("temp profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let mut script = vec![
        FakeStep::EmitToolCall {
            call_id: "recover-spawn".into(),
            name: "spawn_subagent".into(),
            args: serde_json::json!({"task":"recover","prompt":"pause then work"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        // Silent past the deadline AND across a complete 25ms poll tick; a
        // shorter silence can fall between samples and never draw the nudge.
        // Recovery still begins inside the post-nudge cancellation window.
        FakeStep::Delay {
            ms: INITIAL_SILENCE_MS,
        },
    ];
    for index in 0..10 {
        script.push(FakeStep::EmitReasoning {
            text: format!("recovered-heartbeat-{index}"),
        });
        script.push(FakeStep::Delay { ms: 12 });
    }
    script.extend([
        FakeStep::EmitText {
            text: "recovered child report".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        // The nudge landed as a MID-RUN steer: the child's run continues
        // with one more provider round to answer it before terminalizing
        // (by design — a steered turn is the same logical run).
        FakeStep::EmitText {
            text: "status acknowledged — concluding".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::ExpectToolResult {
            call_id: "recover-spawn".into(),
        },
        FakeStep::EmitText {
            text: "parent merged the recovered report".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let provider = Arc::new(FakeProvider::new(script));
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let delegation = DelegationHandle::with_stall_deadline(hub.clone(), STALL_DEADLINE);
    let manager = WorkerManager::start(
        hub.clone(),
        WorkerDependencies {
            diagnostics: None,
            provider_factory: Arc::new(FixedProviderFactory {
                provider: provider.clone(),
            }),
            tool_factory: Arc::new(BrokerToolFactory),
            delegation: Some(delegation),
            web_search: None,
        },
        false,
    );
    let manager_handle = manager.handle();
    hub.install_worker_manager(manager_handle.clone())
        .expect("install manager");
    let parent_session = SessionId::new("w6c-recover-parent");
    let parent_run = RunId::new("w6c-recover-parent-run");
    let accepted = accept_parent(&hub, &parent_session, &parent_run, "w6c-recover").await;
    manager_handle
        .submit(accepted)
        .await
        .expect("submit parent");
    wait_for_state(&store, &parent_session, |state| *state == RunState::Done).await;

    let child = hub
        .delegations_for_parent_run(parent_session.clone(), parent_run)
        .await
        .expect("delegations")
        .pop()
        .expect("child");
    let child_payloads = typed_payloads(
        &store
            .read(&child.child_session_id, 0, 1024)
            .await
            .expect("child events"),
    );
    assert_eq!(
        child_payloads
            .iter()
            .filter(|payload| matches!(
                payload,
                EventPayload::UserMessage { text, .. }
                    if text == "report your status or conclude"
            ))
            .count(),
        1,
        "the nudge DID fire — without it this pin proves nothing"
    );
    assert!(
        !child_payloads
            .iter()
            .any(|payload| matches!(payload, EventPayload::RunState(RunState::Cancelled))),
        "post-nudge progress averts the cancel"
    );
    let parent_payloads = typed_payloads(
        &store
            .read(&parent_session, 0, 1024)
            .await
            .expect("parent events"),
    );
    assert!(
        parent_payloads.iter().any(|payload| matches!(
            payload,
            EventPayload::Item(haider_protocol::item::ItemEvent::Completed { item, .. })
                if matches!(item, haider_protocol::item::TurnItem::AgentMessage { text }
                    if text == "parent merged the recovered report")
        )),
        "the parent merges the child's OWN report, not a stall summary"
    );

    manager.shutdown().await.expect("manager shutdown");
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

/// MUTATION CHECK: remove the coordinator's cancellation sweep. Expected
/// runtime failure: the parent reaches Cancelled while its child remains
/// Streaming, tripping the child terminal-state wait below.
#[tokio::test]
async fn parent_cancel_sweeps_its_outstanding_child() {
    let root = tempfile::tempdir().expect("temp profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "cancel-spawn".into(),
            name: "spawn_subagent".into(),
            args: serde_json::json!({"task":"orphan","prompt":"hang"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::Hang,
    ]));
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
    let manager_handle = manager.handle();
    hub.install_worker_manager(manager_handle.clone())
        .expect("install manager");
    let parent_session = SessionId::new("w6c-cancel-parent");
    let parent_run = RunId::new("w6c-cancel-parent-run");
    let accepted = accept_parent(&hub, &parent_session, &parent_run, "w6c-cancel").await;
    manager_handle
        .submit(accepted)
        .await
        .expect("submit parent");
    wait_for_state(&store, &parent_session, |state| {
        matches!(
            state,
            RunState::Waiting {
                reason: WaitReason::LocalChild
            }
        )
    })
    .await;
    let child = hub
        .delegations_for_parent_run(parent_session.clone(), parent_run.clone())
        .await
        .expect("delegations")
        .pop()
        .expect("child");
    let cancel_json = serde_json::json!({
        "session_id": parent_session,
        "run_id": parent_run,
        "reason": "test-parent-cancel",
    })
    .to_string();
    hub.cancel_internal_turn(TurnCancelCommand {
        command_id: "cancel-w6c-parent".into(),
        request_digest: blake3::hash(cancel_json.as_bytes()).to_hex().to_string(),
        request_json: cancel_json,
        session_id: parent_session.clone(),
        worker_generation: hub.worker_generation(),
        run_id: parent_run,
        cancelling_event_id: EventId::new("w6c-parent-cancelling"),
        device_id: DeviceId::new("w6c-test-device"),
    })
    .await
    .expect("cancel parent");
    wait_for_state(&store, &parent_session, |state| {
        *state == RunState::Cancelled
    })
    .await;
    wait_for_state(&store, &child.child_session_id, |state| {
        *state == RunState::Cancelled
    })
    .await;
    let child_head = timeout(Duration::from_secs(5), async {
        loop {
            let events = store
                .read(&child.child_session_id, 0, 1024)
                .await
                .expect("child terminal journal");
            let cancelled = events
                .iter()
                .filter_map(|event| {
                    serde_json::from_value::<EventPayload>(event.payload.clone())
                        .is_ok_and(|payload| {
                            matches!(payload, EventPayload::RunState(RunState::Cancelled))
                        })
                        .then_some(event.seq)
                })
                .max();
            let idle = events
                .iter()
                .filter_map(|event| {
                    serde_json::from_value::<EventPayload>(event.payload.clone())
                        .is_ok_and(|payload| {
                            matches!(
                                payload,
                                EventPayload::SessionState(
                                    haider_protocol::state::SessionState::Idle { .. }
                                )
                            )
                        })
                        .then_some(event.seq)
                })
                .max();
            if let Some(head) = cancelled
                .zip(idle)
                .and_then(|(cancelled, idle)| (idle > cancelled).then_some(idle))
            {
                break head;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("child reaches durable idle fence");
    let terminal_metrics = timeout(Duration::from_secs(5), async {
        loop {
            let events = store
                .read(&parent_session, 0, 1024)
                .await
                .expect("parent metrics journal");
            if let Some(snapshot) = events
                .iter()
                .filter_map(|event| {
                    haider_protocol::agent::AgentMetricsSnapshot::from_payload_value(&event.payload)
                })
                .filter(|snapshot| snapshot.agent.as_ref() == Some(&child.agent_id))
                .max_by_key(|snapshot| snapshot.head_seq)
                .filter(|snapshot| snapshot.head_seq == child_head)
            {
                break snapshot;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("terminal child metrics mirrored to parent");
    assert_eq!(terminal_metrics.head_seq, child_head);
    assert!(!terminal_metrics.live);
    assert!(terminal_metrics.terminal_at_ms.is_some());

    manager.shutdown().await.expect("manager shutdown");
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

/// MUTATION CHECK: reset depth to one for recursive children, remove the cap,
/// or return a dispatcher error at the cap. Expected runtime failure: the
/// ancestry chain/depths differ, a fourth delegation appears, or the root
/// turn does not reach Done after receiving the typed cap result.
#[tokio::test]
async fn recursion_chains_ancestry_and_depth_four_is_a_typed_continuable_error() {
    let root = tempfile::tempdir().expect("temp profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "depth-1".into(),
            name: "spawn_subagent".into(),
            args: serde_json::json!({"task":"depth-1","prompt":"spawn depth two"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::EmitToolCall {
            call_id: "depth-2".into(),
            name: "spawn_subagent".into(),
            args: serde_json::json!({"task":"depth-2","prompt":"spawn depth three"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::EmitToolCall {
            call_id: "depth-3".into(),
            name: "spawn_subagent".into(),
            args: serde_json::json!({"task":"depth-3","prompt":"test the cap"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::EmitToolCall {
            call_id: "depth-4-rejected".into(),
            name: "spawn_subagent".into(),
            args: serde_json::json!({"task":"depth-4","prompt":"must be rejected"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "depth-4-rejected".into(),
        },
        FakeStep::EmitText {
            text: "depth three continued".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::ExpectToolResult {
            call_id: "depth-3".into(),
        },
        FakeStep::EmitText {
            text: "depth two report".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::ExpectToolResult {
            call_id: "depth-2".into(),
        },
        FakeStep::EmitText {
            text: "depth one report".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::ExpectToolResult {
            call_id: "depth-1".into(),
        },
        FakeStep::EmitText {
            text: "root merged recursion".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
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
    let manager_handle = manager.handle();
    hub.install_worker_manager(manager_handle.clone())
        .expect("install manager");
    let parent_session = SessionId::new("w6c-recursion-root");
    let parent_run = RunId::new("w6c-recursion-root-run");
    let accepted = accept_parent(&hub, &parent_session, &parent_run, "w6c-recursion").await;
    manager_handle.submit(accepted).await.expect("submit root");
    wait_for_state(&store, &parent_session, |state| *state == RunState::Done).await;

    let depth_one = hub
        .delegations_for_parent_run(parent_session.clone(), parent_run)
        .await
        .expect("depth one")
        .pop()
        .expect("depth one row");
    let depth_two = hub
        .delegations_for_parent_run(
            depth_one.child_session_id.clone(),
            depth_one.child_run_id.clone(),
        )
        .await
        .expect("depth two")
        .pop()
        .expect("depth two row");
    let depth_three = hub
        .delegations_for_parent_run(
            depth_two.child_session_id.clone(),
            depth_two.child_run_id.clone(),
        )
        .await
        .expect("depth three")
        .pop()
        .expect("depth three row");
    assert_eq!(
        (depth_one.depth, depth_two.depth, depth_three.depth),
        (1, 2, 3)
    );
    assert_eq!(depth_one.root_session_id, parent_session);
    assert_eq!(depth_two.root_session_id, parent_session);
    assert_eq!(depth_three.root_session_id, parent_session);
    assert_eq!(depth_two.parent_agent_id, Some(depth_one.agent_id.clone()));
    assert_eq!(
        depth_three.parent_agent_id,
        Some(depth_two.agent_id.clone())
    );
    assert!(
        hub.delegations_for_parent_run(
            depth_three.child_session_id.clone(),
            depth_three.child_run_id.clone(),
        )
        .await
        .expect("cap children")
        .is_empty(),
        "the rejected depth-four call must not establish a delegation"
    );
    let requests = provider.requests();
    assert!(requests.iter().any(|request| {
        request.messages.iter().any(|message| {
            message.blocks.iter().any(|block| {
                matches!(
                    block,
                    Block::ToolResult { call_id, preview, .. }
                        if call_id == "depth-4-rejected"
                            && preview.contains("recursion_depth_limit")
                )
            })
        })
    }));

    manager.shutdown().await.expect("manager shutdown");
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

/// HARD-CAP E2 LAW: after 512 durable live children across unrelated trees,
/// `spawn_subagent` returns the owner-pinned rejected tool card and the parent
/// reaches Done on its next provider round. No seeded child runs provider work.
#[tokio::test]
async fn global_subagent_cap_is_a_typed_tool_rejection_and_parent_continues() {
    use haider_core::{DelegationRecord, DelegationState, SUBAGENT_LIVE_LIMIT};
    use haider_protocol::agent::{AgentManifest, AgentRole, Grant, Placement};
    use haider_protocol::error::ErrorAction;
    use haider_protocol::ids::{ItemId, LeaseId};

    let root = tempfile::tempdir().expect("temp profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let seed_parent = SessionId::new("cap-seed-parent");
    hub.create_internal_session(SessionCreateCommand {
        command_id: "create-cap-seed-parent".into(),
        request_digest: "create-cap-seed-parent-digest".into(),
        request_json: r#"{"session":"cap-seed-parent"}"#.into(),
        session_id: seed_parent.clone(),
        cwd: test_cwd(),
        provider: "fake".into(),
        model: "fake-model".into(),
        max_tokens: 4096,
        permission_overrides: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
        event_id: EventId::new("created-cap-seed-parent"),
        device_id: DeviceId::new("cap-test-device"),
    })
    .await
    .expect("seed parent");
    for index in 0..SUBAGENT_LIVE_LIMIT {
        let suffix = format!("{index:03}");
        let child_session_id = SessionId::new(format!("cap-seed-child-{suffix}"));
        hub.create_internal_session(SessionCreateCommand {
            command_id: format!("create-cap-seed-child-{suffix}"),
            request_digest: format!("create-cap-seed-child-{suffix}-digest"),
            request_json: format!(r#"{{"session":"cap-seed-child-{suffix}"}}"#),
            session_id: child_session_id.clone(),
            cwd: test_cwd(),
            provider: "fake".into(),
            model: "fake-model".into(),
            max_tokens: 4096,
            permission_overrides: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
            event_id: EventId::new(format!("created-cap-seed-child-{suffix}")),
            device_id: DeviceId::new("cap-test-device"),
        })
        .await
        .expect("seed child session");
        let agent_id = AgentId::new(format!("cap-seed-agent-{suffix}"));
        hub.create_delegation(DelegationRecord {
            agent_id: agent_id.clone(),
            child_session_id,
            child_run_id: RunId::new(format!("cap-seed-run-{suffix}")),
            parent_session_id: seed_parent.clone(),
            parent_run_id: RunId::new(format!("cap-seed-parent-run-{suffix}")),
            parent_branch_id: None,
            call_id: format!("cap-seed-call-{suffix}"),
            tool_item_id: ItemId::new(format!("cap-seed-item-{suffix}")),
            parent_agent_id: None,
            root_session_id: seed_parent.clone(),
            depth: 1,
            task: format!("seed {suffix}"),
            prompt: "remain live without provider work".into(),
            manifest: AgentManifest {
                agent: agent_id,
                role: AgentRole::Subagent,
                task: format!("seed {suffix}"),
                callsign: None,
                model_profile: "fake-model".into(),
                grant: Grant {
                    tools: Vec::new(),
                    effect_ceiling: Vec::new(),
                },
                budget_tokens: Some(4096),
                placement: Placement::Local,
                lease: LeaseId::new(format!("cap-seed-lease-{suffix}")),
                fencing_epoch: hub.worker_generation(),
                attempt: 0,
                parent: None,
                coordinates: None,
                cli_scope: None,
            },
            state: DelegationState::Spawned,
            report: None,
        })
        .await
        .expect("seed live delegation");
    }

    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "cap-513-rejected".into(),
            name: "spawn_subagent".into(),
            args: serde_json::json!({"task":"overflow","prompt":"must reject typed"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "cap-513-rejected".into(),
        },
        FakeStep::EmitText {
            text: "parent continued after cap".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
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
    let manager_handle = manager.handle();
    hub.install_worker_manager(manager_handle.clone())
        .expect("install manager");
    let parent_session = SessionId::new("cap-rejection-parent");
    let parent_run = RunId::new("cap-rejection-parent-run");
    let accepted = accept_parent(&hub, &parent_session, &parent_run, "cap-rejection").await;
    manager_handle
        .submit(accepted)
        .await
        .expect("submit parent");
    wait_for_state(&store, &parent_session, RunState::is_terminal).await;
    let terminal_events = store
        .read(&parent_session, 0, 1024)
        .await
        .expect("parent terminal journal");
    assert!(
        terminal_events.iter().any(|event| {
            serde_json::from_value::<EventPayload>(event.payload.clone())
                .is_ok_and(|payload| matches!(payload, EventPayload::RunState(RunState::Done)))
        }),
        "parent must continue to Done after cap rejection: {terminal_events:#?}"
    );

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].messages.iter().any(|message| {
        message.blocks.iter().any(|block| {
            matches!(
                block,
                Block::ToolResult { call_id, preview, .. }
                    if call_id == "cap-513-rejected"
                        && preview.contains("subagent_limit_reached")
                        && preview.contains("\"live_count\":512")
            )
        })
    }));
    let events = store
        .read(&parent_session, 0, 1024)
        .await
        .expect("parent journal");
    let presentation = events
        .into_iter()
        .filter_map(|event| serde_json::from_value::<EventPayload>(event.payload).ok())
        .find_map(|payload| match payload {
            EventPayload::ToolResult { call_id, result } if call_id == "cap-513-rejected" => {
                result.presentation
            }
            _ => None,
        })
        .expect("typed cap tool presentation");
    assert_eq!(presentation.subcode.as_str(), "subagent-limit-reached");
    assert_eq!(presentation.title, "Subagent limit reached");
    assert!(presentation.detail.contains("512"));
    assert_eq!(presentation.allowed_actions, vec![ErrorAction::Retry]);

    manager.shutdown().await.expect("manager shutdown");
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

/// MUTATION CHECK: terminalize delegated children during startup recovery or
/// arm supervision only at spawn time. Expected runtime failure: the resumed
/// parent receives a generic restart failure (or waits forever) instead of
/// the one-nudge stall report.
#[tokio::test]
#[cfg(unix)]
async fn coordinator_restart_mid_wait_rearms_supervision_from_durable_progress() {
    use crate::turn_recovery::{RecoveredWork, recover_interrupted_turns};

    let root = tempfile::tempdir().expect("temp profile");
    let first_store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "restart-spawn".into(),
            name: "spawn_subagent".into(),
            args: serde_json::json!({"task":"restart","prompt":"remain stalled"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::Hang,
        FakeStep::ExpectToolResult {
            call_id: "restart-spawn".into(),
        },
        FakeStep::EmitText {
            text: "parent resumed after daemon restart".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let first_hub = SessionHub::new(first_store.clone(), SessionHubConfig::default()).expect("hub");
    let manager = WorkerManager::start(
        first_hub.clone(),
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
    let first_manager_handle = manager.handle();
    first_hub
        .install_worker_manager(first_manager_handle.clone())
        .expect("install manager");
    let parent_session = SessionId::new("w6c-restart-parent");
    let parent_run = RunId::new("w6c-restart-parent-run");
    let accepted = accept_parent(&first_hub, &parent_session, &parent_run, "w6c-restart").await;
    first_manager_handle
        .submit(accepted)
        .await
        .expect("submit parent");
    wait_for_state(&first_store, &parent_session, |state| {
        matches!(
            state,
            RunState::Waiting {
                reason: WaitReason::LocalChild
            }
        )
    })
    .await;
    let child = first_hub
        .delegations_for_parent_run(parent_session.clone(), parent_run.clone())
        .await
        .expect("delegation")
        .pop()
        .expect("child");

    manager.crash().await;
    first_hub.shutdown().await.expect("first hub shutdown");
    drop(first_hub);
    first_store.close().await.expect("first store close");
    tokio::time::sleep(Duration::from_millis(40)).await;

    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("restarted store");
    let recovered = recover_interrupted_turns(&store, &DeviceId::new("w6c-restart-device"))
        .await
        .expect("turn recovery");
    let child_before_resume = typed_payloads(
        &store
            .read(&child.child_session_id, 0, 1024)
            .await
            .expect("preserved child"),
    );
    assert!(
        !child_before_resume
            .iter()
            .any(|payload| matches!(payload, EventPayload::RunState(state) if state.is_terminal()))
    );

    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("restarted hub");
    let manager = WorkerManager::start(
        hub.clone(),
        WorkerDependencies {
            diagnostics: None,
            provider_factory: Arc::new(FixedProviderFactory {
                provider: provider.clone(),
            }),
            tool_factory: Arc::new(BrokerToolFactory),
            delegation: Some(DelegationHandle::with_stall_deadline(
                hub.clone(),
                Duration::from_millis(30),
            )),
            web_search: None,
        },
        false,
    );
    let manager_handle = manager.handle();
    hub.install_worker_manager(manager_handle.clone())
        .expect("install restarted manager");
    let mut resumed_parent = false;
    for work in recovered {
        match work {
            RecoveredWork::ChildWait(recovered) => {
                resumed_parent = true;
                manager_handle
                    .recover_child_wait(recovered.accepted, recovered.checkpoint)
                    .await
                    .expect("resume parent child wait");
            }
            RecoveredWork::Queued(accepted) => manager_handle
                .recover_queued(accepted)
                .await
                .expect("recover queued work"),
            RecoveredWork::Retry(accepted) => manager_handle
                .recover_retry(accepted)
                .await
                .expect("recover retry work"),
            RecoveredWork::Checkpoint(recovered) => manager_handle
                .recover_checkpoint(
                    recovered.accepted,
                    recovered.checkpoint,
                    recovered.committed_answer,
                )
                .await
                .expect("recover checkpoint"),
            RecoveredWork::PartialStream(recovered) => manager_handle
                .recover_partial_stream(
                    recovered.accepted,
                    recovered.checkpoint,
                    recovered.committed_answer,
                )
                .await
                .expect("recover partial stream"),
            RecoveredWork::RouteWait(recovered) => manager_handle
                .recover_route_wait(recovered.accepted, recovered.checkpoint)
                .await
                .expect("recover route wait"),
        }
    }
    assert!(resumed_parent, "parent child wait must survive restart");
    wait_for_state(&store, &parent_session, |state| *state == RunState::Done).await;
    let parent_payloads = typed_payloads(
        &store
            .read(&parent_session, 0, 1024)
            .await
            .expect("resumed parent"),
    );
    assert!(parent_payloads.iter().any(|payload| matches!(
        payload,
        EventPayload::AgentReport(report) if report.summary.contains("stalled after one nudge")
    )));
    let payloads = typed_payloads(
        &store
            .read(&child.child_session_id, 0, 1024)
            .await
            .expect("child after resume"),
    );
    assert_eq!(
        payloads
            .iter()
            .filter(|payload| matches!(
                payload,
                EventPayload::UserMessage { text, .. }
                    if text == "report your status or conclude"
            ))
            .count(),
        1
    );
    assert!(
        payloads
            .iter()
            .any(|payload| matches!(payload, EventPayload::RunState(RunState::Cancelled)))
    );

    manager.shutdown().await.expect("manager shutdown");
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

/// Lineage truth (`session_lineage_v1`): the sessions listing reduces the
/// durable delegation record — a delegation's child summarizes as
/// `Subagent` carrying its exact parent id; a session no delegation names
/// summarizes as `Root` with no parent. Id shape plays no part.
///
/// MUTATION CHECK: drop the `delegation_for_child_session` lookup from
/// `session_summaries` (hardcode `Root`/`None`), or derive the kind from a
/// `session-child-` id prefix. Expected runtime failure: the child row
/// below loses its parent/kind (or a prefix-free child id misclassifies).
#[tokio::test]
async fn session_summaries_carry_typed_lineage_from_the_delegation_record() {
    use haider_core::{DelegationRecord, DelegationState};
    use haider_protocol::agent::{AgentManifest, AgentRole, Grant, Placement};
    use haider_protocol::ids::{ItemId, LeaseId};

    let root = tempfile::tempdir().expect("temp profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let parent = SessionId::new("lineage-parent");
    // Deliberately prefix-free child id: the delegation record, not the id
    // shape, must drive the classification.
    let child = SessionId::new("lineage-offspring");
    for (session_id, label) in [(&parent, "parent"), (&child, "child")] {
        hub.create_internal_session(SessionCreateCommand {
            command_id: format!("create-lineage-{label}"),
            request_digest: format!("create-lineage-{label}-digest"),
            request_json: format!(r#"{{"session":"lineage-{label}"}}"#),
            session_id: session_id.clone(),
            cwd: test_cwd(),
            provider: "fake".into(),
            model: "fake-model".into(),
            max_tokens: 4096,
            permission_overrides: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
            event_id: EventId::new(format!("created-lineage-{label}")),
            device_id: DeviceId::new("lineage-device"),
        })
        .await
        .expect("seed session");
    }
    let agent_id = AgentId::new("lineage-agent");
    hub.create_delegation(DelegationRecord {
        agent_id: agent_id.clone(),
        child_session_id: child.clone(),
        child_run_id: RunId::new("lineage-child-run"),
        parent_session_id: parent.clone(),
        parent_run_id: RunId::new("lineage-parent-run"),
        parent_branch_id: None,
        call_id: "lineage-call".into(),
        tool_item_id: ItemId::new("lineage-item"),
        parent_agent_id: None,
        root_session_id: parent.clone(),
        depth: 1,
        task: "lineage".into(),
        prompt: "lineage".into(),
        manifest: AgentManifest {
            agent: agent_id,
            role: AgentRole::Subagent,
            task: "lineage".into(),
            callsign: None,
            model_profile: "fake-model".into(),
            grant: Grant {
                tools: Vec::new(),
                effect_ceiling: Vec::new(),
            },
            budget_tokens: Some(4096),
            placement: Placement::Local,
            lease: LeaseId::new("lineage-lease"),
            fencing_epoch: hub.worker_generation(),
            attempt: 0,
            parent: None,
            coordinates: None,
            cli_scope: None,
        },
        state: DelegationState::Spawned,
        report: None,
    })
    .await
    .expect("record delegation");

    let summaries =
        crate::session_hub::rpc::session_summaries(&hub, &[parent.clone(), child.clone()])
            .await
            .expect("summaries");
    let by_id = |id: &SessionId| {
        summaries
            .iter()
            .find(|summary| &summary.session_id == id)
            .expect("summary present")
    };
    let child_summary = by_id(&child);
    assert_eq!(
        child_summary.kind,
        Some(haider_rpc::SessionKindWire::Subagent)
    );
    assert_eq!(child_summary.parent_session_id.as_ref(), Some(&parent));
    let parent_summary = by_id(&parent);
    assert_eq!(parent_summary.kind, Some(haider_rpc::SessionKindWire::Root));
    assert_eq!(parent_summary.parent_session_id, None);

    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

/// W-flow inline identity: `session.select_agent_type` is registry-
/// validated, receipted, and reversible — a registered id binds (metadata
/// + fact through the actor arm), an unregistered id is a typed refusal
/// that binds NOTHING, and `None` reverts to plain.
///
/// MUTATION CHECK: drop the loom-registry existence check from
/// `select_session_agent_type`, or stop writing `metadata.agent_type`.
/// Expected runtime failure: the typo below binds silently, or the bound
/// metadata read returns `None`.
#[tokio::test]
async fn agent_type_selection_is_registry_validated_receipted_and_reversible() {
    let root = tempfile::tempdir().expect("temp profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    crate::loom_seed::seed_loom_registry(&store)
        .await
        .expect("seed registry");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let session = SessionId::new("agent-type-session");
    hub.create_internal_session(SessionCreateCommand {
        command_id: "create-agent-type".into(),
        request_digest: "create-agent-type-digest".into(),
        request_json: r#"{"session":"agent-type"}"#.into(),
        session_id: session.clone(),
        cwd: test_cwd(),
        provider: "fake".into(),
        model: "fake-model".into(),
        max_tokens: 4096,
        permission_overrides: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
        event_id: EventId::new("created-agent-type"),
        device_id: DeviceId::new("agent-type-device"),
    })
    .await
    .expect("seed session");
    let command =
        |suffix: &str, agent_type: Option<&str>| haider_core::SessionSelectAgentTypeCommand {
            command_id: format!("select-agent-type-{suffix}"),
            request_digest: format!("select-agent-type-{suffix}-digest"),
            request_json: format!(r#"{{"select":"{suffix}"}}"#),
            session_id: session.clone(),
            worker_generation: hub.worker_generation(),
            agent_type: agent_type.map(str::to_owned),
            event_id: EventId::new(format!("agent-type-selected-{suffix}")),
            device_id: DeviceId::new("agent-type-device"),
        };

    let outcome = hub
        .select_session_agent_type(command("bind", Some("scout")))
        .await
        .expect("bind scout");
    let haider_core::SessionSelectAgentTypeOutcome::Committed { selected, .. } = outcome else {
        panic!("fresh bind commits");
    };
    assert_eq!(selected.agent_type.as_deref(), Some("scout"));
    let metadata = hub
        .session_metadata(&session)
        .await
        .expect("metadata read")
        .expect("metadata present");
    assert_eq!(metadata.agent_type.as_deref(), Some("scout"));

    let refusal = hub
        .select_session_agent_type(command("typo", Some("scoot")))
        .await
        .expect_err("unregistered id refuses");
    assert!(
        refusal.to_string().contains("not registered"),
        "the refusal names the registry miss: {refusal}"
    );
    let metadata = hub
        .session_metadata(&session)
        .await
        .expect("metadata read")
        .expect("metadata present");
    assert_eq!(
        metadata.agent_type.as_deref(),
        Some("scout"),
        "a refused selection binds nothing"
    );

    let outcome = hub
        .select_session_agent_type(command("revert", None))
        .await
        .expect("revert to plain");
    let haider_core::SessionSelectAgentTypeOutcome::Committed { selected, .. } = outcome else {
        panic!("revert commits");
    };
    assert_eq!(selected.agent_type, None);
    let metadata = hub
        .session_metadata(&session)
        .await
        .expect("metadata read")
        .expect("metadata present");
    assert_eq!(metadata.agent_type, None, "None reverts to plain");

    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}
