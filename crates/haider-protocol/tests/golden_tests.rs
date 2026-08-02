//! Golden serialization fixtures — the REAL freeze artifact.
//!
//! Each case serializes a representative value and compares byte-for-byte with
//! `tests/fixtures/<name>.json`. Run with `UPDATE_FIXTURES=1` to (re)write
//! fixtures — doing so in a patch is a schema change and needs the freeze
//! process (version bump, ADR, review). Round-trips are also asserted.
#![allow(clippy::expect_used)] // tests may expect; the lint guards src/ only

use haider_protocol::EventPayload;
use haider_protocol::agent::ChipState;
use haider_protocol::branch::{BranchCreated, BranchDescriptor, BranchEventPayload};
use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets};
use haider_protocol::ids::*;
use haider_protocol::menu::{
    AnswerVia, Menu, MenuAnswer, MenuCloseReason, MenuKind, MenuOption, MenuScope,
};
use haider_protocol::project_instructions::{
    ProjectInstructionFileFact, ProjectInstructionsEventPayload, ProjectInstructionsLoaded,
};
use haider_protocol::state::{RunState, SessionState, VerifyStep, WaitReason};
use haider_protocol::tool::{
    DispatchMode, RememberedGrantScope, RememberedSessionGrant, ToolInventoryEntry,
    ToolInventorySnapshot, ToolManifest, ToolPermissionDefault,
};
use haider_protocol::verify::{Diagnostic, GateReport, Severity, VerifyVerdict};
use serde::{Serialize, de::DeserializeOwned};
use std::path::PathBuf;

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(format!("{name}.json"))
}

fn golden<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(name: &str, value: &T) {
    let serialized = serde_json::to_string_pretty(value).expect("serialize");
    let path = fixture_path(name);
    if std::env::var("UPDATE_FIXTURES").is_ok() {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, &serialized).expect("write fixture");
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("missing fixture {name} — run with UPDATE_FIXTURES=1"));
    assert_eq!(
        serialized, expected,
        "fixture drift in {name}: schema change requires the freeze process"
    );
    let back: T = serde_json::from_str(&expected).expect("round-trip");
    assert_eq!(&back, value, "round-trip mismatch in {name}");
}

fn envelope(payload: EventPayload) -> EventEnvelope<EventPayload> {
    EventEnvelope {
        schema_version: haider_protocol::envelope::SCHEMA_VERSION,
        event_id: EventId::new("ev-0001"),
        seq: 42,
        session_id: SessionId::new("s-billing"),
        branch_id: Some(BranchId::new("b-main")),
        run_id: Some(RunId::new("r-7")),
        agent_id: Some(AgentId::new("a-head")),
        device_id: DeviceId::new("d-mac"),
        authority_epoch: 3,
        worker_generation: 5,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 1_753_500_000_000,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Verbatim,
        },
        payload,
    }
}

/// MUTATION CHECK: replace `PermissionRequired` with `InputRequired` in the
/// W8a fixture. Expected runtime failure: the canonical permission-state
/// golden differs while the historical input-state fixture remains intact.
#[test]
fn golden_run_states() {
    golden(
        "run_state_waiting_verify_hold",
        &RunState::Waiting {
            reason: WaitReason::VerifyHold,
        },
    );
    golden(
        "run_state_verifying_check",
        &RunState::Verifying {
            step: VerifyStep::Check,
        },
    );
    golden("run_state_concluding", &RunState::Concluding);
    golden(
        "run_state_input_required",
        &RunState::InputRequired {
            menu: MenuId::new("m-1"),
        },
    );
    golden(
        "run_state_permission_required",
        &RunState::PermissionRequired {
            menu: MenuId::new("m-permission-1"),
        },
    );
}

/// MUTATION CHECK: remove or rename the additive permission chip state.
/// Expected RUNTIME failure: the exact JSON value no longer round-trips.
#[test]
fn permission_required_chip_state_is_an_additive_wire_value() {
    let encoded = serde_json::to_string(&ChipState::PermissionRequired).expect("serialize chip");
    assert_eq!(encoded, r#""permission_required""#);
    let decoded: ChipState = serde_json::from_str(&encoded).expect("deserialize chip");
    assert_eq!(decoded, ChipState::PermissionRequired);
}

/// MUTATION CHECK: fabricate an inventory entry or omit the remembered grant.
/// Expected runtime failure: the additive read-contract fixture differs at
/// runtime and no longer round-trips to the daemon snapshot shape.
#[test]
fn golden_tool_inventory_snapshot() {
    golden(
        "tool_inventory_snapshot",
        &ToolInventorySnapshot {
            tools: vec![ToolInventoryEntry {
                manifest: ToolManifest {
                    name: "process_exec".into(),
                    description: "Run one command".into(),
                    effects: vec![haider_protocol::effect::EffectClass::ProcessExec],
                    dispatch: DispatchMode::Await,
                    input_schema: serde_json::json!({
                        "type": "object",
                        "required": ["command"]
                    }),
                },
                default: ToolPermissionDefault::Ask,
            }],
            remembered_grants: vec![RememberedSessionGrant {
                class: haider_protocol::effect::EffectClass::ProcessExec,
                scope: RememberedGrantScope::CommandShape {
                    args_digest: "blake3-command-shape".into(),
                },
            }],
        },
    );
}

#[test]
fn golden_session_idle_interrupted() {
    golden(
        "session_idle_interrupted",
        &SessionState::Idle { interrupted: true },
    );
}

/// MUTATION CHECK: remove or rename any durable branch coordinate. Expected
/// RUNTIME failure: the additive topology fact golden no longer round-trips.
#[test]
fn golden_branch_created_fact() {
    golden(
        "branch_created",
        &BranchEventPayload::BranchCreated(BranchCreated {
            branch: BranchDescriptor {
                branch_id: BranchId::new("branch-plan-b"),
                name: "Plan B".into(),
                source_branch_id: Some(BranchId::new("branch-plan-a")),
                fork_node_id: NodeId::new("node-fork-42"),
                fork_seq: 42,
                created_seq: 57,
                created_at_ms: 1_753_500_000_057,
                head_node_id: NodeId::new("node-fork-42"),
                head_seq: 42,
            },
        }),
    );
}

/// MUTATION CHECK: remove, rename, or reorder any project-instruction audit
/// coordinate. Expected RUNTIME failure: the additive fact golden differs or
/// no longer round-trips while remaining unknown to the core payload enum.
#[test]
fn golden_project_instructions_loaded_fact() {
    let fact = ProjectInstructionsLoaded {
        files: vec![
            ProjectInstructionFileFact {
                path: "/workspace/HAIDER.md".into(),
                digest: "0123456789abcdef".repeat(4),
                bytes: 34,
                truncated: false,
            },
            ProjectInstructionFileFact {
                path: "/workspace/crate/AGENTS.md".into(),
                digest: "fedcba9876543210".repeat(4),
                bytes: 49_152,
                truncated: true,
            },
        ],
    };
    golden(
        "project_instructions_loaded",
        &ProjectInstructionsEventPayload::ProjectInstructionsLoaded(fact.clone()),
    );
    let value = fact.to_payload_value().expect("serialize additive fact");
    assert!(serde_json::from_value::<EventPayload>(value.clone()).is_err());
    assert_eq!(
        ProjectInstructionsLoaded::from_payload_value(&value),
        Some(fact)
    );
}

#[test]
fn golden_menu_permission() {
    let menu = Menu {
        id: MenuId::new("m-perm-1"),
        kind: MenuKind::Permission {
            effect_summary: "fs_write: src/lib.rs".into(),
        },
        title: "Allow patch to src/lib.rs?".into(),
        body: vec!["+24 −6".into()],
        options: vec![
            MenuOption {
                key: "allow".into(),
                label: "Allow".into(),
                detail: None,
                decision: Some(haider_protocol::menu::DecisionKind::AllowOnce),
            },
            MenuOption {
                key: "deny".into(),
                label: "Deny".into(),
                detail: Some("blocks the edit".into()),
                decision: Some(haider_protocol::menu::DecisionKind::RejectOnce),
            },
        ],
        blocking: true,
        scope: MenuScope::Session,
        origin: "fs_patch".into(),
        ttl_ms: None,
        timeout_option: None,
    };
    golden("menu_permission", &menu);
    golden(
        "menu_answer",
        &MenuAnswer {
            menu: MenuId::new("m-perm-1"),
            option_key: Some("allow".into()),
            option_index: 0,
            value: None,
            via: AnswerVia::Rpc,
        },
    );
    golden(
        "menu_closed",
        &EventPayload::MenuClosed {
            menu: MenuId::new("m-perm-1"),
            reason: MenuCloseReason::Cancelled,
        },
    );
}

#[test]
fn golden_verify_verdicts() {
    golden(
        "verdict_verified",
        &VerifyVerdict::Verified {
            revision: WorkspaceRevision::new("blake3:abc"),
        },
    );
    golden(
        "verdict_aggregate",
        &VerifyVerdict::IncludedInAggregate {
            revision: WorkspaceRevision::new("blake3:abc"),
        },
    );
    golden("verdict_deferred", &VerifyVerdict::Deferred);
    golden(
        "verdict_failed_env",
        &VerifyVerdict::FailedEnv {
            item: "rustfmt missing".into(),
        },
    );
}

#[test]
fn golden_gate_report_red() {
    let report = GateReport {
        verdict: VerifyVerdict::ErroredWithReport,
        new_errors: vec![Diagnostic {
            file: "src/lib.rs".into(),
            line: 42,
            col: 9,
            severity: Severity::Error,
            code: Some("E0308".into()),
            message: "expected u32, found &str".into(),
            tool: "cargo".into(),
            fingerprint: "fp:strict:1".into(),
            fingerprint_tolerant: "fp:tol:1".into(),
        }],
        new_warnings: vec![],
        preexisting: 3,
        cycles_used: 3,
        duration_ms: 4100,
        raw_log: Some(ArtifactRef::new("blake3:log")),
        format: None,
        tests: None,
    };
    golden("gate_report_red", &report);
}

#[test]
fn golden_item_lifecycle() {
    use haider_protocol::item::*;
    golden(
        "item_started_tool_call",
        &ItemEvent::Started {
            item_id: ItemId::new("it-1"),
            item: TurnItem::ToolCall {
                call_id: "c-1".into(),
                name: "fs_patch".into(),
                args: serde_json::json!({"path": "src/lib.rs"}),
                status: ToolStatus::InProgress,
            },
        },
    );
    golden(
        "item_completed_tool_cancelled",
        &ItemEvent::Completed {
            item_id: ItemId::new("it-3"),
            item: TurnItem::ToolCall {
                call_id: "c-2".into(),
                name: "fs_read".into(),
                args: serde_json::json!({}),
                status: ToolStatus::Cancelled,
            },
        },
    );
    golden(
        "item_delta_command_output",
        &ItemEvent::Delta {
            item_id: ItemId::new("it-2"),
            delta: ItemDelta::CommandOutput {
                stream: OutputStream::Stdout,
                chunk_b64: "aGk=".into(),
            },
        },
    );
    // ADDITIVE (TUI3b): compaction items may carry the before/after token
    // footprint — optional fields, absent = old shape (fixtures above are
    // untouched; this is a NEW fixture).
    golden(
        "item_completed_compaction_with_tokens",
        &ItemEvent::Completed {
            item_id: ItemId::new("it-4"),
            item: TurnItem::ContextCompaction {
                summary_artifact: haider_protocol::ids::ArtifactRef::new("blake3:demo"),
                tokens_before: Some(170_000),
                tokens_after: Some(12_000),
            },
        },
    );
}

#[test]
fn golden_full_envelope() {
    golden(
        "envelope_run_state",
        &envelope(EventPayload::RunState(RunState::Thinking)),
    );
}

#[test]
fn raw_envelope_tolerates_unknown_payload() {
    // Forward-compat law: an envelope with a payload kind from the FUTURE must
    // still parse as RawEnvelope with every envelope field intact.
    let json = serde_json::to_string(&envelope(EventPayload::IdleDecayed))
        .expect("serialize")
        .replace("idle_decayed", "hologram_projected");
    let raw: haider_protocol::envelope::RawEnvelope =
        serde_json::from_str(&json).expect("raw parse");
    assert_eq!(raw.seq, 42);
    assert_eq!(raw.payload["type"], "hologram_projected");
}

#[test]
fn unknown_fields_are_tolerated() {
    // Adding a field in a future minor must not break old readers.
    let json = r#"{"state":"concluding","future_field":true}"#;
    let state: RunState = serde_json::from_str(json).expect("tolerant parse");
    assert_eq!(state, RunState::Concluding);
}

#[test]
fn parked_never_terminal() {
    for state in [
        RunState::Waiting {
            reason: WaitReason::LocalChild,
        },
        RunState::Verifying {
            step: VerifyStep::Check,
        },
        RunState::Compacting,
        RunState::EffectOutcomeUnknown,
    ] {
        assert!(state.is_parked());
        assert!(!state.is_terminal());
    }
    assert!(RunState::Done.is_terminal());
}

#[test]
fn golden_effect_phases() {
    use haider_protocol::effect::*;
    golden(
        "effect_intent",
        &EffectPhase::Intent(EffectIntent {
            effect: EffectId::new("ef-1"),
            class: EffectClass::FsWrite,
            summary: "patch src/lib.rs".into(),
            args_digest: "d:abc".into(),
            workspace_revision: Some(WorkspaceRevision::new("blake3:r1")),
        }),
    );
    golden(
        "effect_authorized_ask",
        &EffectPhase::Authorized {
            effect: EffectId::new("ef-1"),
            verdict: AuthorizationVerdict::Ask {
                menu: MenuId::new("m-1"),
            },
        },
    );
    golden(
        "effect_authorized_user_typed",
        &EffectPhase::Authorized {
            effect: EffectId::new("ef-shell"),
            verdict: AuthorizationVerdict::PreAuthorized {
                source: AuthorizationSource::UserTyped,
            },
        },
    );
    golden(
        "effect_outcome_cancelled",
        &EffectPhase::Outcome {
            effect: EffectId::new("ef-shell"),
            outcome: EffectOutcome::Cancelled,
            freshness: None,
        },
    );
    golden(
        "effect_outcome_cancelled_escalated",
        &EffectPhase::Outcome {
            effect: EffectId::new("ef-shell-leaked"),
            outcome: EffectOutcome::CancelledEscalated {
                note: "SIGKILL escalation failed".into(),
            },
            freshness: None,
        },
    );
    golden(
        "effect_outcome_unknown",
        &EffectPhase::Outcome {
            effect: EffectId::new("ef-1"),
            outcome: EffectOutcome::Unknown,
            freshness: None,
        },
    );
    golden(
        "effect_outcome_ok_with_file_freshness",
        &EffectPhase::Outcome {
            effect: EffectId::new("ef-read"),
            outcome: EffectOutcome::Ok,
            freshness: Some(FileFreshness {
                path: "src/lib.rs".into(),
                digest: "blake3:fresh".into(),
            }),
        },
    );
}

#[test]
fn golden_attachments() {
    use haider_protocol::tool::AttachmentBlock;
    golden(
        "attachment_image",
        &AttachmentBlock::Image {
            artifact: ArtifactRef::new("blake3:img"),
            mime: "image/png".into(),
            width: Some(800),
            height: None,
        },
    );
    golden(
        "attachment_pasted_text",
        &AttachmentBlock::PastedText {
            artifact: ArtifactRef::new("blake3:txt"),
            lines: 120,
        },
    );
}

#[test]
fn golden_credentials() {
    use haider_protocol::credential::*;
    golden(
        "credential_descriptor_oauth",
        &CredentialDescriptor {
            alias: CredentialAlias::new("personal-max"),
            provider: "anthropic".into(),
            base_url: None,
            auth_method: AuthMethod::OAuth,
            identity: "user@example.com".into(),
            status: CredentialStatus::Ok,
            active: true,
        },
    );
    golden(
        "rotation_event",
        &RotationEvent {
            provider: "openai".into(),
            from: CredentialAlias::new("work-chatgpt"),
            to: CredentialAlias::new("billing-key"),
            cause: RotationCause::RateLimit,
        },
    );
}

#[test]
fn credential_base_url_is_additive_optional_and_unknown_tolerant() {
    use haider_protocol::credential::CredentialDescriptor;

    let old_wire = r#"{
        "alias":"custom-endpoint",
        "provider":"openai-compatible",
        "auth_method":"api_key",
        "identity":"local",
        "status":{"status":"ok"},
        "active":true,
        "future_hint":"ignored"
    }"#;
    let old: CredentialDescriptor = serde_json::from_str(old_wire).expect("old descriptor");
    assert_eq!(old.base_url, None);

    let with_endpoint = old_wire.replace(
        r#""auth_method":"api_key""#,
        r#""base_url":"http://127.0.0.1:11434/v1","auth_method":"api_key""#,
    );
    let current: CredentialDescriptor =
        serde_json::from_str(&with_endpoint).expect("descriptor with endpoint");
    assert_eq!(
        current.base_url.as_deref(),
        Some("http://127.0.0.1:11434/v1")
    );
    assert!(
        !serde_json::to_value(old)
            .expect("serialize old descriptor")
            .as_object()
            .is_some_and(|object| object.contains_key("base_url")),
        "None stays absent on the wire"
    );
}

#[test]
fn oauth_wire_name_is_oauth() {
    let json = serde_json::to_string(&haider_protocol::credential::AuthMethod::OAuth).expect("ser");
    assert_eq!(json, "\"oauth\"", "acronym must not mangle to o_auth");
}

#[test]
fn golden_usage_account_tagged() {
    use haider_protocol::provider::*;
    golden(
        "usage_account_tagged",
        &Usage {
            input: 1000,
            output: 200,
            reasoning: 50,
            cached: 800,
            source: UsageSource::ProviderReported,
            account: Some(CredentialAlias::new("personal-max")),
            accounts: Vec::new(),
        },
    );
}

/// MUTATION CHECK: relabel an estimated footprint as exact, omit a token
/// split, or change the additive extension kind. Expected runtime failure:
/// the frozen durable-event fixture differs or no longer round-trips.
#[test]
fn golden_context_footprint_exact_extension() {
    use haider_protocol::context::{ContextFootprint, ContextFootprintTruth};

    let footprint = ContextFootprint {
        input_tokens: 118_000,
        output_tokens: 7_000,
        cached_input_tokens: 42_000,
        used_tokens: 167_000,
        context_window: Some(200_000),
        reserved_output_tokens: 30_000,
        soft_threshold_tokens: Some(170_000),
        estimated_turns_to_threshold: Some(1),
        truth: ContextFootprintTruth::Exact,
    };
    golden(
        "context_footprint_exact_extension",
        &footprint.extension_item().expect("extension serializes"),
    );
}

#[test]
fn golden_agent_family() {
    use haider_protocol::agent::*;
    golden(
        "agent_manifest",
        &AgentManifest {
            agent: AgentId::new("a-child-1"),
            role: AgentRole::Subagent,
            task: "tests".into(),
            callsign: Some("Hasan".into()),
            model_profile: "gpt-5.6".into(),
            grant: Grant {
                tools: vec!["fs_read".into(), "fs_patch".into()],
                effect_ceiling: vec![haider_protocol::effect::EffectClass::FsWrite],
            },
            budget_tokens: Some(500_000),
            placement: Placement::Local,
            lease: LeaseId::new("l-1"),
            fencing_epoch: 2,
            attempt: 1,
            parent: Some(AgentId::new("a-head")),
            coordinates: None,
        },
    );
    golden(
        "child_report",
        &ChildReport {
            agent: AgentId::new("a-child-1"),
            summary: "tests green".into(),
            verified: ReportVerification::Verified,
            workspace_revision: Some(WorkspaceRevision::new("blake3:r2")),
        },
    );
}

/// MUTATION CHECK: remove the serde default from `AgentManifest::task`.
/// Expected runtime failure: a pre-W6 manifest no longer decodes, breaking
/// additive protocol compatibility with old durable journals and peers.
#[test]
fn pre_w6_agent_manifest_decodes_without_task_label() {
    let old = serde_json::json!({
        "agent": "old-child",
        "role": "subagent",
        "model_profile": "old-model",
        "grant": {"tools": [], "effect_ceiling": []},
        "placement": {"placement": "local"},
        "lease": "old-lease",
        "fencing_epoch": 1
    });
    let manifest: haider_protocol::agent::AgentManifest =
        serde_json::from_value(old).expect("old manifest remains decodable");
    assert!(manifest.task.is_empty());
    assert!(
        !serde_json::to_value(manifest)
            .expect("serialize manifest")
            .as_object()
            .is_some_and(|object| object.contains_key("task"))
    );
}

#[test]
fn golden_tree_nodes() {
    use haider_protocol::history::*;
    golden(
        "node_compaction",
        &TreeNode {
            node: NodeId::new("n-9"),
            parent: Some(NodeId::new("n-8")),
            kind: NodeKind::Compaction {
                covers_from: NodeId::new("n-1"),
                covers_to: NodeId::new("n-7"),
                summary_artifact: ArtifactRef::new("blake3:sum"),
                tokens_before: 118_000,
                tokens_after: 24_000,
                resume_cause: CompactionResume::AutoMidTurn,
            },
        },
    );
    golden(
        "node_todos",
        &TreeNode {
            node: NodeId::new("n-10"),
            parent: Some(NodeId::new("n-9")),
            kind: NodeKind::Todos {
                items: vec![TodoItem {
                    id: 1,
                    text: "scope entrypoints".into(),
                    state: TodoState::Completed,
                    dep: None,
                }],
            },
        },
    );
    golden(
        "node_annotation_title",
        &TreeNode {
            node: NodeId::new("n-11"),
            parent: None,
            kind: NodeKind::Annotation {
                annotation: AnnotationKind::AutoTitle,
                data: serde_json::json!({"title": "Webhook retry backoff"}),
            },
        },
    );
}

#[test]
fn golden_harness_and_hooks() {
    use haider_protocol::hook::*;
    use haider_protocol::state::*;
    golden(
        "harness_starting",
        &HarnessStatus::Starting {
            checks: vec![ReadinessCheck {
                name: "store open".into(),
                ok: true,
                duration_ms: 12,
            }],
        },
    );
    golden(
        "hook_decision_deny",
        &HookDecision {
            hook: "cost-governor".into(),
            point: "tool.authorize".into(),
            verdict: HookVerdict::Deny {
                reason: "budget exceeded".into(),
            },
            op_hash: "op:abc".into(),
        },
    );
    golden(
        "startup_gate_update",
        &StartupGateDecision {
            kind: StartupGateKind::Update,
            choice: "defer".into(),
            decided_at_ms: 1_753_500_000_000,
        },
    );
}

#[test]
fn golden_rpc_family() {
    use haider_protocol::error::*;
    use haider_protocol::rpc::*;
    golden(
        "rpc_hello",
        &ClientHello {
            protocol_version: RPC_VERSION,
            min_supported: 1,
            client_name: "haider-tui".into(),
        },
    );
    golden(
        "rpc_request",
        &RpcRequest {
            id: 7,
            method: "menu.answer".into(),
            command_id: Some("cmd-1".into()),
            params: serde_json::json!({"menu": "m-1"}),
        },
    );
    golden(
        "rpc_error_response",
        &RpcResponse {
            id: 7,
            outcome: RpcOutcome::Error(HaiderError::new(
                ErrorCode::MenuNotFound,
                "no such menu",
                false,
            )),
        },
    );
    golden(
        "rpc_notice_synchronized",
        &SubscriptionNotice::Synchronized { last_seq: 99 },
    );
}

#[test]
fn unknown_error_code_tolerated() {
    let e: haider_protocol::error::ErrorCode =
        serde_json::from_str("\"quantum_flux\"").expect("tolerant");
    assert_eq!(e, haider_protocol::error::ErrorCode::Unknown);
}

#[test]
fn golden_user_message_queue_mode() {
    golden(
        "user_message_queued",
        &EventPayload::UserMessage {
            text: "then document it".into(),
            attachments: vec![],
            mode: haider_protocol::DeliveryMode::Queue,
        },
    );
}
