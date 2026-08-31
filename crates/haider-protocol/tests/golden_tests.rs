//! Golden serialization fixtures — the REAL freeze artifact.
//!
//! Each case serializes a representative value and compares byte-for-byte with
//! `tests/fixtures/<name>.json`. Run with `UPDATE_FIXTURES=1` to (re)write
//! fixtures — doing so in a patch is a schema change and needs the freeze
//! process (version bump, ADR, review). Round-trips are also asserted.
#![allow(clippy::expect_used)] // tests may expect; the lint guards src/ only

use haider_protocol::EventPayload;
use haider_protocol::agent::{
    AgentMessageDelivery, AgentMessaged, AgentMetricsSnapshot, AgentUsageBreakdown,
    AgentUsageMetrics, ChipState,
};
use haider_protocol::branch::{BranchCreated, BranchDescriptor, BranchEventPayload};
use haider_protocol::computer::{ComputerAction, ScreenPoint, ScrollDirection};
use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets};
use haider_protocol::error::{ErrorAction, ErrorCode, ErrorPresentation, ErrorScope};
use haider_protocol::graph::{
    EvidenceAuthority, EvidenceRecorded, EvidenceSlotSpec, EvidenceVerdict, GraphAbandoned,
    GraphAdvanced, GraphAttemptOpened, GraphBlockReason, GraphBlocked, GraphCompleted,
    GraphEvidenceSource, GraphGateKind, GraphGateSatisfied, GraphNodeSpec, GraphPinned,
    GraphStatus, ProcessSignalRecorded, SubjectSelector, evidence_fingerprint,
    process_signal_subject_digest, reduce_graph, ship_loop_digest, ship_loop_nodes,
};
use haider_protocol::hook::{
    HookAttachmentMetadata, HookAttachmentSet, HookEventPayload, HookFired, HookInput, HookOutput,
    HookRuntimeKind,
};
use haider_protocol::ids::*;
use haider_protocol::image::{IMAGE_CREATED_EXTENSION_KIND, ImageCreatedV1};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::menu::{
    AnswerVia, ErrorRecoveryCardKind, Menu, MenuAnswer, MenuCloseReason, MenuKind, MenuOption,
    MenuScope,
};
use haider_protocol::permission::{
    PermissionEventPayload, PermissionGrantAction, PermissionGrantNeeded,
    PermissionGrantResolution, PermissionGrantResolved, SystemPermission,
};
use haider_protocol::project_instructions::{
    ProjectInstructionFileFact, ProjectInstructionsEventPayload, ProjectInstructionsLoaded,
};
use haider_protocol::retry::RunRetryEventPayload;
use haider_protocol::state::{RunState, SessionState, VerifyStep, WaitReason};
use haider_protocol::tool::{
    BoundedResult, DispatchMode, FsSearchMatch, ImageBlockRef, RememberedGrantScope,
    RememberedSessionGrant, ToolInventoryEntry, ToolInventorySnapshot, ToolManifest,
    ToolPermissionDefault, ToolResultData, ToolResultStatus, ToolTruncationReason,
};
use haider_protocol::verify::{Diagnostic, GateReport, Severity, VerifyVerdict};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::path::PathBuf;

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(format!("{name}.json"))
}

fn read_fixture(name: &str) -> String {
    std::fs::read_to_string(fixture_path(name))
        .unwrap_or_else(|_| panic!("missing fixture {name} — run with UPDATE_FIXTURES=1"))
        // Git may materialize text fixtures with CRLF on Windows. The frozen
        // wire representation remains canonical LF JSON on every platform.
        .replace("\r\n", "\n")
}

fn golden<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(name: &str, value: &T) {
    let serialized = serde_json::to_string_pretty(value).expect("serialize");
    let path = fixture_path(name);
    if std::env::var("UPDATE_FIXTURES").is_ok() {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, &serialized).expect("write fixture");
    }
    let expected = read_fixture(name);
    assert_eq!(
        serialized, expected,
        "fixture drift in {name}: schema change requires the freeze process"
    );
    let back: T = serde_json::from_str(&expected).expect("round-trip");
    assert_eq!(&back, value, "round-trip mismatch in {name}");
}

fn additive_golden<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(
    name: &str,
    value: &T,
) {
    let serialized = serde_json::to_string_pretty(value).expect("serialize");
    let expected = read_fixture(name);
    let expected = expected.strip_suffix('\n').unwrap_or(&expected);
    assert_eq!(serialized, expected, "additive fixture drift in {name}");
    let back: T = serde_json::from_str(expected).expect("round-trip additive fixture");
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

/// MUTATION CHECK: remove an image location, producer identity, or image
/// metadata field, or rename the extension kind. Expected runtime failure:
/// the additive durable image-event fixture differs or no longer round-trips.
#[test]
fn golden_image_created_extension() {
    let image = ImageCreatedV1 {
        path: "/workspace/output/chart.png".into(),
        display_path: "output/chart.png".into(),
        media_type: "image/png".into(),
        byte_len: 12_345,
        width: Some(640),
        height: Some(480),
        call_id: "call-image-1".into(),
        tool: "process_exec".into(),
    };
    additive_golden(
        "item_completed_image_created",
        &ItemEvent::Completed {
            item_id: ItemId::new("it-image-created-1"),
            item: TurnItem::Extension {
                kind: IMAGE_CREATED_EXTENSION_KIND.into(),
                data: serde_json::to_value(image).expect("serialize image-created payload"),
            },
        },
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

/// LAW (G2 fact golden): the `session_renamed` config fact rides the
/// additive `SessionConfigEventPayload` union — a titled rename pins its
/// exact shape, a CLEAR keeps `title` OFF the wire, and the core
/// [`EventPayload`] enum still treats the kind as unknown (raw-tolerated),
/// exactly like `model_selected`.
///
/// MUTATION CHECK: rename the `session_renamed` tag, serialize
/// `title: null` for a clear, or decode the fact through the core enum.
/// Expected RUNTIME failure: fixture drift or the tolerance asserts below.
#[test]
fn golden_session_renamed_fact() {
    use haider_protocol::session::SessionConfigEventPayload;
    golden(
        "session_renamed",
        &SessionConfigEventPayload::SessionRenamed {
            title: Some("Parser rewrite".into()),
        },
    );
    // A CLEAR keeps `title` off the wire entirely.
    let cleared = serde_json::to_value(SessionConfigEventPayload::SessionRenamed { title: None })
        .expect("encode cleared rename");
    assert_eq!(cleared, serde_json::json!({"type": "session_renamed"}));
    assert_eq!(
        SessionConfigEventPayload::session_renamed_from_value(&cleared),
        Some(None)
    );
    let titled = serde_json::json!({"type": "session_renamed", "title": "Parser rewrite"});
    assert_eq!(
        SessionConfigEventPayload::session_renamed_from_value(&titled),
        Some(Some("Parser rewrite".to_owned()))
    );
    // A model_selected fact is NOT a rename, and the decoder returns None
    // for it rather than lying.
    let selected = serde_json::json!({
        "type": "model_selected", "provider": "fake", "model": "fake-v1"
    });
    assert_eq!(
        SessionConfigEventPayload::session_renamed_from_value(&selected),
        None
    );
    // The CORE payload enum does not decode the config fact — raw
    // tolerance is what keeps exhaustive consumers source-compatible.
    assert!(serde_json::from_value::<EventPayload>(titled).is_err());
}

/// G3 goldens for the two session-config tuning facts, plus their F3
/// contract: BOTH decode as `SessionConfigEventPayload`, which is exactly
/// what lets the compaction head CAS tolerate a mid-compaction effort or
/// fast change (the classifier decodes this union).
///
/// MUTATION CHECK: rename either tag, drop the `effort` skip, or remove a
/// variant from the union. Expected RUNTIME failure: the fixture bytes
/// differ, the revert payload grows an `effort` key, or the classifier
/// decode below fails.
#[test]
fn golden_session_config_effort_and_fast_facts() {
    use haider_protocol::session::{EffortSelected, FastModeSelected, SessionConfigEventPayload};

    golden(
        "session_config_effort_selected",
        &SessionConfigEventPayload::EffortSelected(EffortSelected {
            effort: Some("xhigh".into()),
        }),
    );
    golden(
        "session_config_fast_selected",
        &SessionConfigEventPayload::FastModeSelected(FastModeSelected { enabled: true }),
    );

    // A revert-to-default selection carries NO effort key at all.
    let revert = EffortSelected { effort: None }
        .to_payload_value()
        .expect("revert payload");
    assert_eq!(revert, serde_json::json!({"type": "effort_selected"}));

    // The F3 classifier contract: every tuning fact IS a session-config
    // payload, and each helper decodes its own fact and refuses the others'.
    let effort_value = EffortSelected {
        effort: Some("low".into()),
    }
    .to_payload_value()
    .expect("effort payload");
    let fast_value = FastModeSelected { enabled: false }
        .to_payload_value()
        .expect("fast payload");
    for value in [&effort_value, &fast_value] {
        serde_json::from_value::<SessionConfigEventPayload>(value.clone())
            .expect("tuning facts decode as session-config payloads");
    }
    assert_eq!(
        EffortSelected::from_payload_value(&effort_value),
        Some(EffortSelected {
            effort: Some("low".into())
        })
    );
    assert_eq!(EffortSelected::from_payload_value(&fast_value), None);
    assert_eq!(
        FastModeSelected::from_payload_value(&fast_value),
        Some(FastModeSelected { enabled: false })
    );
    assert_eq!(FastModeSelected::from_payload_value(&effort_value), None);
}

/// G3 metadata additivity: pre-G3 metadata JSON (no tuning keys) decodes
/// with `effort: None` / `fast: false`, and a metadata row with the tuning
/// unset serializes WITHOUT the keys — pre-G3 rows stay byte-identical.
///
/// MUTATION CHECK: drop either serde default/skip attribute. Expected
/// RUNTIME failure: the legacy decode errors or the re-encoded JSON grows
/// an `effort`/`fast` key.
#[test]
fn session_metadata_tuning_fields_are_additive_and_skip_defaults() {
    use haider_protocol::session::SessionMetadataV1;

    let legacy = r#"{"cwd":"/tmp","provider":"anthropic","model":"claude-test","max_tokens":4096,"created_at_ms":1}"#;
    let decoded: SessionMetadataV1 = serde_json::from_str(legacy).expect("legacy metadata decodes");
    assert_eq!(decoded.effort, None);
    assert!(!decoded.fast);
    assert_eq!(
        decoded.interaction_mode,
        haider_protocol::session::SessionInteractionModeV1::Interactive
    );
    assert_eq!(
        decoded.cache_policy,
        haider_protocol::cache::CachePolicySettingsV1::default()
    );
    let encoded = serde_json::to_string(&decoded).expect("re-encode");
    assert!(!encoded.contains("effort"));
    assert!(!encoded.contains("fast"));
    assert!(!encoded.contains("cache_policy"));
    assert!(!encoded.contains("interaction_mode"));

    let tuned = SessionMetadataV1 {
        effort: Some("max".into()),
        fast: true,
        ..decoded
    };
    let encoded = serde_json::to_value(&tuned).expect("tuned encode");
    assert_eq!(encoded["effort"], "max");
    assert_eq!(encoded["fast"], true);

    let mobile = SessionMetadataV1 {
        cache_policy: haider_protocol::cache::CachePolicySettingsV1 {
            mode: haider_protocol::cache::CachePolicyMode::Mobility,
            cold_cost_threshold_microusd: 9_000,
        },
        ..tuned
    };
    let encoded = serde_json::to_value(&mobile).expect("policy encode");
    assert_eq!(encoded["cache_policy"]["mode"], "mobility");
    assert_eq!(
        encoded["cache_policy"]["cold_cost_threshold_microusd"],
        9_000
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
        origin: "fs_edit".into(),
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

/// E2/E3/E4 additive wire fixtures. Pre-E2 goldens remain byte-identical;
/// these pin only the new optional shapes.
#[test]
fn golden_error_presentation_contract() {
    let presentation = ErrorPresentation::new(
        "rate-limited",
        "Provider rate limit reached",
        "Wait for the provider limit to reset, then retry.",
        ErrorScope::Account,
        [
            ErrorAction::Wait,
            ErrorAction::Retry,
            ErrorAction::SwitchAccount,
        ],
    )
    .with_http_status(429)
    .with_request_id(Some("req-safe-42"))
    .with_retry_after(Some(30_000), 1_753_500_000_000);
    additive_golden(
        "run_failed_typed",
        &EventPayload::RunFailed {
            code: ErrorCode::ProviderError,
            message: "legacy safe fallback".into(),
            retryable: true,
            presentation: Some(presentation.clone()),
        },
    );
    additive_golden(
        "tool_result_failed_typed",
        &EventPayload::ToolResult {
            call_id: "call-typed-error".into(),
            result: BoundedResult {
                preview: "tool failed".into(),
                truncated: false,
                data: None,
                artifact: None,
                images: Vec::new(),
                cursor: None,
                status: ToolResultStatus::Failed,
                reason: Some("legacy safe fallback".into()),
                presentation: Some(ErrorPresentation::new(
                    "tool-failed",
                    "Tool execution failed",
                    "The tool did not complete successfully.",
                    ErrorScope::Tool,
                    [ErrorAction::Retry],
                )),
            },
        },
    );
    additive_golden(
        "menu_error_recovery",
        &Menu {
            id: MenuId::new("m-error-1"),
            kind: MenuKind::ErrorRecovery {
                card: ErrorRecoveryCardKind::RateLimit,
                presentation: presentation.clone(),
                option_actions: vec![ErrorAction::Wait, ErrorAction::Retry],
                provider: Some("openai".into()),
                account: Some(CredentialAlias::new("openai-work")),
                source_run: Some(RunId::new("run-typed-1")),
                source_item: None,
            },
            title: presentation.title.clone(),
            body: vec![presentation.detail.clone()],
            options: vec![
                MenuOption {
                    key: "wait".into(),
                    label: "Wait".into(),
                    detail: Some("Wait for the displayed reset time.".into()),
                    decision: None,
                },
                MenuOption {
                    key: "retry".into(),
                    label: "Retry".into(),
                    detail: None,
                    decision: None,
                },
            ],
            blocking: false,
            scope: MenuScope::Session,
            origin: "error-recovery".into(),
            ttl_ms: None,
            timeout_option: None,
        },
    );
    additive_golden(
        "item_incomplete_agent_message",
        &EventPayload::Item(ItemEvent::Completed {
            item_id: ItemId::new("item-partial-1"),
            item: TurnItem::IncompleteAgentMessage {
                text: "A response prefix".into(),
                interruption: ErrorPresentation::new(
                    "stream-interrupted",
                    "Response stream interrupted",
                    "The provider connection ended after part of the response was received.",
                    ErrorScope::Turn,
                    [ErrorAction::ContinuePartial, ErrorAction::RetryFresh],
                ),
            },
        }),
    );
}

#[test]
fn golden_image_bearing_tool_result_is_additive_and_legacy_decodes_empty() {
    additive_golden(
        "tool_result_with_image",
        &EventPayload::ToolResult {
            call_id: "call-screenshot".into(),
            result: BoundedResult {
                preview: "captured dashboard".into(),
                truncated: false,
                data: None,
                artifact: None,
                images: vec![ImageBlockRef {
                    artifact: ArtifactRef::new(
                        "blake3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    ),
                    media_type: "image/png".into(),
                    width: 1280,
                    height: 720,
                    byte_len: 42_000,
                }],
                cursor: None,
                status: ToolResultStatus::Completed,
                reason: None,
                presentation: None,
            },
        },
    );

    let legacy: BoundedResult = serde_json::from_value(serde_json::json!({
        "preview": "legacy text-only result",
        "truncated": false
    }))
    .expect("pre-CU-1 bounded result decodes");
    assert!(legacy.images.is_empty());
    assert_eq!(
        serde_json::to_value(&legacy).expect("legacy result encodes"),
        serde_json::json!({"preview": "legacy text-only result", "truncated": false})
    );
}

/// MUTATION CHECK: remove a kept search field, rename its additive kind, or
/// make legacy `data` mandatory. Expected failure: exact wire JSON drifts or
/// the pre-E result stops decoding.
#[test]
fn structured_search_result_wire_is_additive_and_legacy_decodes() {
    let result = BoundedResult {
        preview: "src/lib.rs:7:needle\n".into(),
        truncated: true,
        data: Some(ToolResultData::FsSearch {
            matches: vec![FsSearchMatch {
                path: "src/lib.rs".into(),
                line: 7,
                column: 3,
                text: "needle".into(),
                context_before: vec!["before".into()],
                context_after: vec!["after".into()],
            }],
            truncated_reason: Some(ToolTruncationReason::MatchLimit),
            binary_files_skipped: 2,
            skipped_sensitive: 1,
            files_scanned: 9,
            bytes_scanned: 4096,
        }),
        artifact: Some(ArtifactRef::new("blake3:search")),
        images: Vec::new(),
        cursor: None,
        status: ToolResultStatus::Completed,
        reason: None,
        presentation: None,
    };
    assert_eq!(
        serde_json::to_value(&result).expect("structured search wire"),
        serde_json::json!({
            "preview": "src/lib.rs:7:needle\n",
            "truncated": true,
            "data": {
                "kind": "fs_search",
                "matches": [{
                    "path": "src/lib.rs",
                    "line": 7,
                    "column": 3,
                    "text": "needle",
                    "context_before": ["before"],
                    "context_after": ["after"]
                }],
                "truncated_reason": "match_limit",
                "binary_files_skipped": 2,
                "skipped_sensitive": 1,
                "files_scanned": 9,
                "bytes_scanned": 4096
            },
            "artifact": "blake3:search"
        })
    );
    let legacy: BoundedResult = serde_json::from_value(serde_json::json!({
        "preview": "old",
        "truncated": false
    }))
    .expect("legacy bounded result");
    assert!(legacy.data.is_none());
}

/// MUTATION CHECK: rename `run_retried` or omit any source coordinate.
/// Expected runtime failure: restart recovery and transcript clients can no
/// longer bind a fresh run to its failed run and original committed user turn.
#[test]
fn golden_run_retried_contract() {
    let fact = RunRetryEventPayload::RunRetried {
        failed_run_id: RunId::new("run-failed-7"),
        prompt_run_id: RunId::new("run-original-3"),
        user_seq: 42,
    };
    additive_golden("run_retried", &fact);
    let value = serde_json::to_value(&fact).expect("retry fact value");
    assert!(
        serde_json::from_value::<EventPayload>(value).is_err(),
        "the additive fact must not break exhaustive core-event consumers"
    );
}

/// MUTATION CHECK: fold task facts into the closed EventPayload enum, rename
/// their additive kinds, or re-type a field. Expected RUNTIME failure: the
/// independent golden changes or an older typed reducer unexpectedly accepts
/// the task payload.
#[test]
fn golden_additive_task_facts_and_unknown_kind_tolerance() {
    use haider_protocol::task::{
        TaskCompleted, TaskCompletionDelivery, TaskEventPayload, TaskStarted, TaskTerminalState,
    };
    let started = TaskEventPayload::TaskStarted(TaskStarted {
        task: TaskId::new("task-3f9a"),
        name: "cargo".into(),
        command: "cargo watch -x test".into(),
        pid: 4242,
        started_at_ms: 1_753_500_000_000,
    });
    golden("task_started", &started);
    let completed = TaskEventPayload::TaskCompleted(TaskCompleted {
        task: TaskId::new("task-3f9a"),
        name: "cargo".into(),
        state: TaskTerminalState::Completed { exit_code: Some(0) },
        elapsed_ms: 61_000,
        output_bytes: 700_000,
        tail: "test result: ok\n".into(),
        artifact: Some(ArtifactRef::new(
            "blake3:9c1185a5c5e9fc54612808977ee8f548b2258d31",
        )),
        truncated: true,
        full_output_unavailable: false,
        delivery: TaskCompletionDelivery::DeliveredQueued,
        workspace_mutation: None,
    });
    golden("task_completed", &completed);
    for fact in [&started, &completed] {
        let raw = fact.to_payload_value().expect("task payload");
        assert!(
            serde_json::from_value::<EventPayload>(raw.clone()).is_err(),
            "raw tolerance is what keeps exhaustive consumers source-compatible"
        );
        let mut envelope_value =
            serde_json::to_value(envelope(EventPayload::IdleDecayed)).expect("envelope value");
        envelope_value["payload"] = raw.clone();
        let decoded: haider_protocol::envelope::RawEnvelope =
            serde_json::from_value(envelope_value).expect("raw envelope tolerates additive kind");
        assert_eq!(decoded.payload, raw);
    }
}

/// MUTATION CHECK: fold hook facts into the closed EventPayload enum or rename
/// their additive kind. Expected RUNTIME failure: the independent golden
/// changes or an older typed reducer unexpectedly accepts the hook payload.
#[test]
fn golden_additive_hook_fired_fact_and_unknown_kind_tolerance() {
    let fact = HookEventPayload::HookFired(HookFired {
        hook: "notify".into(),
        digest: "a".repeat(64),
        kind: HookRuntimeKind::Exec,
        observed_seq: 41,
        exit_code: Some(0),
        timed_out: false,
        stdout: HookOutput {
            preview: "done\n".into(),
            bytes: 5,
            truncated: false,
            artifact: None,
        },
        stderr: HookOutput {
            preview: String::new(),
            bytes: 0,
            truncated: false,
            artifact: None,
        },
        proposed_decision: None,
        menu_id: None,
        decision_applied: false,
    });
    additive_golden("hook_fired", &fact);
    let raw = fact.to_payload_value().expect("hook payload");
    assert!(serde_json::from_value::<EventPayload>(raw.clone()).is_err());
    let mut envelope =
        serde_json::to_value(envelope(EventPayload::IdleDecayed)).expect("envelope value");
    envelope["payload"] = raw.clone();
    let decoded: haider_protocol::envelope::RawEnvelope =
        serde_json::from_value(envelope).expect("raw envelope tolerates additive kind");
    assert_eq!(decoded.payload, raw);
}

/// WIRE-GAPS hook additions stay satellite facts: the current writer may
/// name the decision menu and trust revision, while a legacy field reader
/// ignores `menu_id` and the closed core event union preserves either fact
/// through `RawEnvelope`.
#[test]
fn golden_hook_menu_coordinate_and_trust_revision_are_additive() {
    let output = HookOutput {
        preview: String::new(),
        bytes: 0,
        truncated: false,
        artifact: None,
    };
    let fired = HookEventPayload::HookFired(HookFired {
        hook: "permission-guard".into(),
        digest: "b".repeat(64),
        kind: HookRuntimeKind::Decision,
        observed_seq: 73,
        exit_code: Some(0),
        timed_out: false,
        stdout: output.clone(),
        stderr: output,
        proposed_decision: Some(haider_protocol::hook::HookDecisionKind::Allow),
        menu_id: Some(MenuId::new("menu-permission-73")),
        decision_applied: true,
    });
    additive_golden("hook_fired_decision_menu", &fired);

    #[derive(Deserialize)]
    struct LegacyHookFired {
        #[serde(rename = "type")]
        event_type: String,
        hook: String,
        decision_applied: bool,
    }
    let legacy: LegacyHookFired = serde_json::from_value(
        fired
            .to_payload_value()
            .expect("serialize decision hook fact"),
    )
    .expect("legacy decoder ignores additive menu_id");
    assert_eq!(legacy.event_type, "hook_fired");
    assert_eq!(legacy.hook, "permission-guard");
    assert!(legacy.decision_applied);

    let trust = HookEventPayload::HookTrustChanged {
        digest: "c".repeat(64),
        trusted: false,
        revision: 7,
    };
    additive_golden("hook_trust_changed", &trust);
    for fact in [fired, trust] {
        let raw = fact.to_payload_value().expect("serialize hook fact");
        assert!(serde_json::from_value::<EventPayload>(raw.clone()).is_err());
        let mut raw_envelope =
            serde_json::to_value(envelope(EventPayload::IdleDecayed)).expect("envelope value");
        raw_envelope["payload"] = raw.clone();
        let decoded: haider_protocol::envelope::RawEnvelope =
            serde_json::from_value(raw_envelope).expect("legacy raw envelope keeps hook fact");
        assert_eq!(decoded.payload, raw);
    }
}

/// MUTATION CHECK: erase hook provenance or reuse RPC provenance. Expected
/// RUNTIME failure: the exact answer-authority golden changes.
#[test]
fn golden_hook_menu_answer_authority() {
    additive_golden(
        "menu_answer_hook",
        &MenuAnswer {
            menu: MenuId::new("m-perm-hook"),
            option_key: Some("allow_once".into()),
            option_index: 0,
            value: None,
            via: AnswerVia::Hook,
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
                name: "fs_edit".into(),
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
    let origin = UserCommandOriginV1 {
        origin: CommandExecutionOrigin::UserCommand,
        command_item_id: ItemId::new("it-user-command"),
        call_id: "shell-command-1".into(),
    };
    additive_golden(
        "item_completed_user_command_origin",
        &ItemEvent::Completed {
            item_id: ItemId::new("it-user-command-origin"),
            item: origin.extension_item().expect("serialize origin marker"),
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
                tokens_estimated: false,
            },
        },
    );
    // G1: the live plan lifecycle the actor now emits for `todo_write` —
    // existing shapes (`TurnItem::Plan`, `TodoItem`), NEW fixtures.
    golden(
        "item_started_plan",
        &ItemEvent::Started {
            item_id: ItemId::new("it-5"),
            item: TurnItem::Plan {
                items: vec![
                    haider_protocol::history::TodoItem {
                        id: 0,
                        text: "scope entrypoints".into(),
                        state: haider_protocol::history::TodoState::Processing,
                        dep: None,
                    },
                    haider_protocol::history::TodoItem {
                        id: 1,
                        text: "patch run loop".into(),
                        state: haider_protocol::history::TodoState::Listed,
                        dep: Some(0),
                    },
                ],
            },
        },
    );
    golden(
        "item_started_tool_call_todo_write",
        &ItemEvent::Started {
            item_id: ItemId::new("it-6"),
            item: TurnItem::ToolCall {
                call_id: "c-3".into(),
                name: "todo_write".into(),
                args: serde_json::json!({
                    "items": [
                        {"id": 0, "text": "scope entrypoints", "state": "processing"},
                        {"id": 1, "text": "patch run loop", "state": "listed", "dep": 0},
                    ]
                }),
                status: ToolStatus::InProgress,
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
            workspace_mutation: None,
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
            workspace_mutation: None,
        },
    );
    golden(
        "effect_outcome_unknown",
        &EffectPhase::Outcome {
            effect: EffectId::new("ef-1"),
            outcome: EffectOutcome::Unknown,
            freshness: None,
            workspace_mutation: None,
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
            workspace_mutation: None,
        },
    );
}

/// CU-2 additive wire law: every computer action keeps its exact top-level
/// tagged JSON shape so native provider adapters need no second envelope.
#[test]
fn golden_computer_actions() {
    additive_golden(
        "computer_actions",
        &vec![
            ComputerAction::Screenshot,
            ComputerAction::CursorPosition,
            ComputerAction::Inspect { x: 400, y: 225 },
            ComputerAction::LeftClick { x: 120, y: 240 },
            ComputerAction::RightClick,
            ComputerAction::MiddleClick,
            ComputerAction::DoubleClick,
            ComputerAction::LeftMouseDown,
            ComputerAction::LeftMouseUp,
            ComputerAction::MouseMove { x: 320, y: 180 },
            ComputerAction::LeftClickDrag {
                from: ScreenPoint { x: 10, y: 20 },
                to: ScreenPoint { x: 300, y: 400 },
            },
            ComputerAction::Type {
                text: "Hello, Haider".into(),
            },
            ComputerAction::Key {
                keys: "cmd+shift+4".into(),
            },
            ComputerAction::Scroll {
                x: 640,
                y: 360,
                direction: ScrollDirection::Down,
                amount: 3,
            },
            ComputerAction::Wait { ms: 250 },
        ],
    );
}

/// The historical reserved class remains readable while CU-2 appends the
/// separately grantable observe/control keys.
#[test]
fn golden_computer_effect_classes() {
    use haider_protocol::effect::EffectClass;
    additive_golden(
        "computer_effect_classes",
        &vec![
            EffectClass::GuiAct,
            EffectClass::ScreenObserve,
            EffectClass::ScreenControl,
        ],
    );
}

#[test]
fn golden_attachments() {
    use haider_protocol::tool::{AttachmentBlock, PdfDeliveryMode};
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
    // G2 additive text-file variant: ref + display basename + line count.
    golden(
        "attachment_file",
        &AttachmentBlock::File {
            artifact: ArtifactRef::new("blake3:file"),
            name: "notes.md".into(),
            lines: 42,
        },
    );
    additive_golden(
        "attachment_pdf",
        &AttachmentBlock::Pdf {
            artifact: ArtifactRef::new("blake3:pdf"),
            name: "report.pdf".into(),
            pages: 12,
            delivery: PdfDeliveryMode::NativeDocument,
        },
    );
    let legacy_pdf: AttachmentBlock = serde_json::from_value(serde_json::json!({
        "kind": "pdf",
        "artifact": "blake3:legacy-pdf",
        "name": "legacy.pdf",
        "pages": 3
    }))
    .expect("pre-delivery PDF block decodes");
    assert!(matches!(
        legacy_pdf,
        AttachmentBlock::Pdf {
            delivery: PdfDeliveryMode::ExtractedText,
            ..
        }
    ));
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
            label: None,
            account_identity: None,
            created_at_ms: None,
        },
    );
    golden(
        "credential_descriptor_keychain_locked",
        &CredentialDescriptor {
            alias: CredentialAlias::new("claude-code"),
            provider: "anthropic-oauth".into(),
            base_url: None,
            auth_method: AuthMethod::OAuth,
            identity: "Claude Max subscription · Linked to Claude Code".into(),
            status: CredentialStatus::NeedsAttention {
                reason: CredentialAttentionReason::KeychainLocked,
            },
            active: true,
            label: None,
            account_identity: None,
            created_at_ms: None,
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

/// LAW (golden_usage_report_v1): the U1 `usage.report` payload shape is
/// frozen — provider/alias/identity/plan coordinates, tagged meter state,
/// normalized windows (utilization is a 0–1 fraction on the wire), and the
/// local counters — and it round-trips byte-for-byte. Secrets have no field
/// to hide in.
/// MUTATION CHECK: rename a window coordinate, collapse the tagged meter
/// state, or serialize utilization as a percentage. Expected RUNTIME failure:
/// this fixture differs byte-for-byte or no longer round-trips.
#[test]
fn golden_usage_report_v1() {
    use haider_protocol::usage::*;
    golden(
        "usage_report_v1",
        &UsageReportV1 {
            generated_at_ms: 1_753_500_000_000,
            accounts: vec![
                AccountUsageReportV1 {
                    provider: "anthropic-oauth".into(),
                    alias: CredentialAlias::new("personal-max"),
                    identity: Some("user@example.com".into()),
                    plan: None,
                    auth_method: haider_protocol::credential::AuthMethod::OAuth,
                    meter: AccountMeterStateV1::Metered {
                        windows: vec![
                            UsageWindowV1 {
                                window: "five_hour".into(),
                                utilization: 0.6,
                                resets_at_ms: Some(1_753_507_200_000),
                                label: None,
                            },
                            UsageWindowV1 {
                                window: "seven_day".into(),
                                utilization: 0.12,
                                resets_at_ms: Some(1_753_900_000_000),
                                label: None,
                            },
                        ],
                    },
                    local: LocalUsageStatsV1 {
                        sessions: 3,
                        total_duration_ms: 5_400_000,
                        input_tokens: 120_000,
                        output_tokens: 9_500,
                        reasoning_tokens: 2_000,
                        cached_tokens: 80_000,
                        est_cost_usd: None,
                        api_equivalent_est_cost_usd: Some(0.42),
                        lines_added: 240,
                        lines_removed: 60,
                        cache: CacheUsageStatsV1::default(),
                    },
                },
                AccountUsageReportV1 {
                    provider: "openai".into(),
                    alias: CredentialAlias::new("billing-key"),
                    identity: Some("metered".into()),
                    plan: None,
                    auth_method: haider_protocol::credential::AuthMethod::ApiKey,
                    meter: AccountMeterStateV1::LocalOnly,
                    local: LocalUsageStatsV1 {
                        sessions: 1,
                        total_duration_ms: 600_000,
                        input_tokens: 40_000,
                        output_tokens: 3_000,
                        reasoning_tokens: 0,
                        cached_tokens: 0,
                        est_cost_usd: Some(0.08),
                        api_equivalent_est_cost_usd: Some(0.08),
                        lines_added: 12,
                        lines_removed: 4,
                        cache: CacheUsageStatsV1::default(),
                    },
                },
            ],
        },
    );
    golden(
        "usage_meter_unavailable",
        &AccountMeterStateV1::Unavailable {
            reason: "http_status_429".into(),
        },
    );
}

/// LAW (usage_report_fields_are_tolerant_and_additive): an old-wire account
/// entry without the additive optional fields still decodes, and unknown
/// future fields are ignored — the report is safe to extend.
#[test]
fn usage_report_fields_are_tolerant_and_additive() {
    use haider_protocol::usage::*;
    let old_wire = r#"{
        "provider": "kimi-oauth",
        "alias": "kimi-main",
        "auth_method": "oauth",
        "meter": {"state": "unavailable", "reason": "not_yet_polled", "future": 1},
        "local": {
            "sessions": 0,
            "total_duration_ms": 0,
            "input_tokens": 0,
            "output_tokens": 0
        },
        "future_hint": true
    }"#;
    let entry: AccountUsageReportV1 = serde_json::from_str(old_wire).expect("tolerant decode");
    assert_eq!(entry.identity, None);
    assert_eq!(entry.plan, None);
    assert_eq!(
        entry.meter,
        AccountMeterStateV1::Unavailable {
            reason: "not_yet_polled".into()
        }
    );
    assert_eq!(entry.local.reasoning_tokens, 0);
    assert_eq!(entry.local.est_cost_usd, None);
    assert!(
        !serde_json::to_value(&entry)
            .expect("serialize")
            .as_object()
            .is_some_and(|object| object.contains_key("plan")),
        "absent plan stays absent on the wire"
    );
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
            normalized: Some(NormalizedUsage {
                logical_input: 1_800,
                uncached_input: 1_000,
                cache_read_input: 800,
                cache_write_input: 200,
                cache_write_5m_input: 200,
                cache_write_1h_input: 0,
                billed_output: 200,
                reasoning_detail: 50,
                reasoning_accounting: ReasoningAccounting::SubsetOfOutput,
                cache_status: CacheStatAvailability::Present,
                cache_write_status: CacheStatAvailability::Present,
                cache_write_ttl_status: CacheStatAvailability::Present,
                cache_telemetry_input: 1_800,
                explicit_cache_storage_token_hours: None,
            }),
            scope: Some(UsageScope {
                provider: "anthropic-oauth".into(),
                model: "claude-sonnet-5".into(),
                account_scope: Some(CredentialAlias::new("personal-max")),
                auth_scope: "oauth_subscription".into(),
                api_family: None,
                effort: None,
                speed: None,
                cache_epoch: "epoch-7".into(),
                stable_prefix_tokens: 1_700,
                cache_boundaries: None,
                request_kind: UsageRequestKind::MainTurn,
                run: Some(RunId::new("run-7")),
                agent: None,
                prefix_digests: Some(PrefixDigests {
                    system: "system-digest".into(),
                    tools: "tools-digest".into(),
                    immutable_history: "history-digest".into(),
                    model: "model-digest".into(),
                    auth_mode: "auth-digest".into(),
                    reasoning_settings: "reasoning-digest".into(),
                }),
            }),
            cache_cost: Some(CacheCostEstimate {
                input_with_cache_usd: 0.001,
                input_without_cache_usd: 0.0054,
                estimated_savings_usd: 0.0044,
                explicit_storage_usd: 0.0,
            }),
            request: None,
        },
    );
}

/// CM1 protocol extension law: every normalized/cache-domain field is
/// additive, so a pre-CM1 usage payload still decodes with no invented cache
/// telemetry or cost.
#[test]
fn cm1_normalized_usage_fields_are_additive() {
    use haider_protocol::provider::{Usage, UsageSource};

    let usage: Usage = serde_json::from_str(
        r#"{"input":42,"output":7,"reasoning":3,"cached":0,"source":"provider_reported"}"#,
    )
    .expect("pre-CM1 usage decodes");
    assert_eq!(usage.input, 42);
    assert_eq!(usage.source, UsageSource::ProviderReported);
    assert_eq!(usage.normalized, None);
    assert_eq!(usage.scope, None);
    assert_eq!(usage.cache_cost, None);
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
        accounting: None,
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
                tools: vec!["fs_read".into(), "fs_edit".into()],
                effect_ceiling: vec![haider_protocol::effect::EffectClass::FsWrite],
            },
            budget_tokens: Some(500_000),
            placement: Placement::Local,
            lease: LeaseId::new("l-1"),
            fencing_epoch: 2,
            attempt: 1,
            parent: Some(AgentId::new("a-head")),
            coordinates: None,
            cli_scope: None,
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

/// WIRE-GAPS S3: the manifest advertises the exact daemon-generated handoff
/// path as one additive coordinate. A legacy coordinate reader keeps all of
/// its known fields and ignores the new one.
#[test]
fn golden_agent_spawned_handoff_coordinate_is_additive() {
    use haider_protocol::agent::*;

    let fact = EventPayload::AgentSpawned(AgentManifest {
        agent: AgentId::new("agent-child-handoff"),
        role: AgentRole::Subagent,
        task: "inspect parser".into(),
        callsign: None,
        model_profile: "gpt-5.6".into(),
        grant: Grant {
            tools: vec!["fs_read".into()],
            effect_ceiling: Vec::new(),
        },
        budget_tokens: Some(8_192),
        placement: Placement::Local,
        lease: LeaseId::new("lease-child-handoff"),
        fencing_epoch: 9,
        attempt: 0,
        parent: None,
        coordinates: Some(serde_json::json!({
            "parent_session_id": "session-parent",
            "parent_run_id": "run-parent",
            "call_id": "call-spawn",
            "tool_item_id": "item-spawn",
            "child_session_id": "session-child-handoff",
            "handoff_dir": "/work/project/.haider/handoff/0123456789abcdef",
        })),
        cli_scope: None,
    });
    additive_golden("agent_spawned_handoff", &fact);

    #[derive(Deserialize)]
    struct LegacyCoordinates {
        parent_session_id: SessionId,
        child_session_id: SessionId,
    }
    let EventPayload::AgentSpawned(mut manifest) = fact else {
        unreachable!("constructed agent_spawned")
    };
    manifest
        .coordinates
        .as_mut()
        .expect("coordinates")
        .as_object_mut()
        .expect("coordinate map")
        .insert("provider".into(), serde_json::json!("openai"));
    assert_eq!(manifest.provider(), Some("openai"));
    let legacy: LegacyCoordinates =
        serde_json::from_value(manifest.coordinates.expect("coordinates"))
            .expect("legacy coordinates ignore additive handoff_dir");
    assert_eq!(legacy.parent_session_id.as_str(), "session-parent");
    assert_eq!(legacy.child_session_id.as_str(), "session-child-handoff");
}

/// MUTATION CHECK: remove/rename the additive fact or collapse either
/// delivery receipt. Expected RUNTIME failure: the canonical parent-timeline
/// JSON differs and no longer round-trips through the satellite union.
#[test]
fn golden_agent_messaged_fact() {
    let fact = AgentMessaged {
        agent: AgentId::new("agent-child-7"),
        preview: "check the non-degenerate parser fixture".into(),
        delivery: AgentMessageDelivery::DeliveredSteer,
    };
    additive_golden(
        "agent_messaged",
        &haider_protocol::agent::AgentEventPayload::AgentMessaged(fact.clone()),
    );
    let value = fact
        .to_payload_value()
        .expect("serialize additive agent fact");
    assert!(serde_json::from_value::<EventPayload>(value.clone()).is_err());
    assert_eq!(AgentMessaged::from_payload_value(&value), Some(fact));
}

/// LAW (metrics wire): the compact snapshot is an additive raw agent fact,
/// carries separate real/API-equivalent costs, and never enters the closed
/// `EventPayload` union.
#[test]
fn golden_agent_metrics_snapshot_fact() {
    let snapshot = AgentMetricsSnapshot {
        agent: Some(AgentId::new("agent-child-7")),
        session_id: SessionId::new("session-child-7"),
        head_seq: 42,
        started_at_ms: 1_000,
        terminal_at_ms: None,
        live: true,
        tool_attempts: 2,
        usage: Some(AgentUsageMetrics {
            logical_input_tokens: 10_000,
            billed_output_tokens: 500,
            additional_reasoning_tokens: 25,
            cache_read_tokens: 7_000,
            cache_write_tokens: 1_000,
            cache_hit_basis_points: Some(7_000),
            cache_reread_hit_basis_points: None,
            metered_cost_microusd: None,
            api_equivalent_cost_microusd: Some(27_000),
            all_lanes_priced: true,
            has_metered_lanes: false,
            has_oauth_lanes: true,
            breakdowns: vec![AgentUsageBreakdown {
                provider: "openai".into(),
                model: "gpt-5.6-terra".into(),
                cache_epoch: "epoch-a".into(),
                request_kind: haider_protocol::provider::UsageRequestKind::DelegatedAgent,
                auth_method: Some(haider_protocol::credential::AuthMethod::OAuth),
                logical_input_tokens: 10_000,
                billed_output_tokens: 500,
                additional_reasoning_tokens: 25,
                cache_read_tokens: 7_000,
                cache_write_tokens: 1_000,
                metered_cost_microusd: None,
                api_equivalent_cost_microusd: Some(27_000),
                priced: true,
            }],
        }),
    };
    additive_golden(
        "agent_metrics_snapshot",
        &haider_protocol::agent::AgentEventPayload::AgentMetrics(snapshot.clone()),
    );
    let value = snapshot.to_payload_value().expect("serialize metrics fact");
    assert!(serde_json::from_value::<EventPayload>(value.clone()).is_err());
    assert_eq!(
        AgentMetricsSnapshot::from_payload_value(&value),
        Some(snapshot)
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

/// ST1 LAW. MUTATION CHECK: remove/rename `Subturn`, reorder the default to
/// it, or serialize it as either legacy mode. Expected runtime failure: the
/// additive bytes or the default assertion differ.
#[test]
fn delivery_mode_subturn_round_trips_and_preserves_steer_default() {
    assert_eq!(
        haider_protocol::DeliveryMode::default(),
        haider_protocol::DeliveryMode::Steer
    );
    additive_golden(
        "user_message_subturn",
        &EventPayload::UserMessage {
            text: "revise before the next tool".into(),
            attachments: vec![],
            mode: haider_protocol::DeliveryMode::Subturn,
        },
    );
}

/// MUTATION CHECK: expose a surface tag/resolved attachment bytes, rename a
/// coordinate, or omit the count/truncation contract. Expected RUNTIME failure:
/// the additive hook-input golden differs byte-for-byte.
#[test]
fn golden_user_message_hook_event_projection() {
    additive_golden(
        "hook_user_message_event",
        &HookInput::UserMessage {
            session: SessionId::new("s-billing"),
            run: RunId::new("r-7"),
            branch: Some(BranchId::new("b-main")),
            mode: haider_protocol::DeliveryMode::Queue,
            text: "then document it".into(),
            truncated: false,
            attachments: HookAttachmentSet {
                count: 1,
                items: vec![HookAttachmentMetadata {
                    mime: "image/png".into(),
                    bytes: 8,
                    artifact: ArtifactRef::new(
                        "blake3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    ),
                }],
            },
        },
    );
}

/// CG-M1 LAW: all eight durable fact discriminants and fields are frozen in
/// one append-only fixture. A pre-graph typed reader may reject the new union
/// variants, but its RawEnvelope journal path must retain their JSON exactly.
#[test]
fn golden_convergence_graph_facts_and_old_decoder_tolerance() {
    let graph_id = GraphId::new("graph-ship-loop-1");
    let legacy_nodes = vec![
        GraphNodeSpec {
            name: haider_protocol::graph::build_node(),
            gate: GraphGateKind::CommandGreen,
            executor: haider_protocol::graph::GraphExecutorShape::Inline,
            max_attempts: 8,
            max_evidence_per_attempt: Some(8),
            depends_on: Vec::new(),
            red_target: None,
            verify_slots: Vec::new(),
        },
        GraphNodeSpec {
            name: haider_protocol::graph::verify_node(),
            gate: GraphGateKind::AllOfN { n: 3 },
            executor: haider_protocol::graph::GraphExecutorShape::FanOut,
            max_attempts: 8,
            max_evidence_per_attempt: Some(8),
            depends_on: vec![haider_protocol::graph::build_node()],
            red_target: None,
            verify_slots: Vec::new(),
        },
        GraphNodeSpec {
            name: haider_protocol::graph::ship_node(),
            gate: GraphGateKind::HumanConfirm,
            executor: haider_protocol::graph::GraphExecutorShape::Human,
            max_attempts: 8,
            max_evidence_per_attempt: None,
            depends_on: vec![haider_protocol::graph::verify_node()],
            red_target: None,
            verify_slots: Vec::new(),
        },
    ];
    let facts = vec![
        EventPayload::GraphPinned(GraphPinned {
            graph_id: graph_id.clone(),
            template: "ship-loop".into(),
            digest: "63cee264d2a430b21d32c5f8b71c390e0bb825e88073571d19e7dbf2084820eb".into(),
            template_version: 0,
            start_node: None,
            nodes: legacy_nodes,
        }),
        EventPayload::GraphAttemptOpened(GraphAttemptOpened {
            graph_id: graph_id.clone(),
            node: haider_protocol::graph::build_node(),
            attempt: 2,
        }),
        EventPayload::EvidenceRecorded(EvidenceRecorded {
            graph_id: graph_id.clone(),
            node: haider_protocol::graph::verify_node(),
            attempt: 2,
            verdict: EvidenceVerdict::Red,
            detail: "cargo test failed".into(),
            fingerprint: evidence_fingerprint("cargo test failed"),
            slot: None,
            subject_digest: None,
            source: GraphEvidenceSource::Model {
                run_id: RunId::new("run-graph-2"),
                call_id: "call-evidence-2".into(),
            },
        }),
        EventPayload::GraphGateSatisfied(GraphGateSatisfied {
            graph_id: graph_id.clone(),
            node: haider_protocol::graph::verify_node(),
            attempt: 2,
        }),
        EventPayload::GraphAdvanced(GraphAdvanced {
            graph_id: graph_id.clone(),
            from_node: haider_protocol::graph::verify_node(),
            to_node: haider_protocol::graph::ship_node(),
        }),
        EventPayload::GraphBlocked(GraphBlocked {
            graph_id: graph_id.clone(),
            node: haider_protocol::graph::verify_node(),
            reason: GraphBlockReason::NoProgress,
        }),
        EventPayload::GraphCompleted(GraphCompleted {
            graph_id: graph_id.clone(),
        }),
        EventPayload::GraphAbandoned(GraphAbandoned {
            graph_id,
            why: "operator chose a different release".into(),
        }),
    ];
    golden("convergence_graph_facts", &facts);

    // M2b mutation guard: populating any defaulted M2b status field for this
    // legacy journal, or changing legacy START/current-node semantics, changes
    // these exact pre-M2b reducer bytes.
    let raw = facts
        .iter()
        .cloned()
        .map(|fact| {
            serde_json::from_value(serde_json::to_value(envelope(fact)).expect("envelope value"))
                .expect("raw envelope")
        })
        .collect::<Vec<_>>();
    let status = reduce_graph(&raw).status.expect("legacy status");
    assert_eq!(
        serde_json::to_string(&status).expect("legacy status JSON"),
        r#"{"graph_id":"graph-ship-loop-1","template":"ship-loop","digest":"63cee264d2a430b21d32c5f8b71c390e0bb825e88073571d19e7dbf2084820eb","phase":"abandoned","current_node":"VERIFY","attempt":2,"nodes":[{"node":"BUILD","attempts_opened":1,"current_attempt":2,"evidence":{"green":0,"red":0,"effective_green":0,"standing_red":0},"satisfied":false},{"node":"VERIFY","attempts_opened":0,"current_attempt":null,"evidence":{"green":0,"red":0,"effective_green":0,"standing_red":0},"satisfied":false},{"node":"SHIP","attempts_opened":0,"current_attempt":null,"evidence":{"green":0,"red":0,"effective_green":0,"standing_red":0},"satisfied":false}],"blocked_reason":"no-progress"}"#
    );

    #[derive(Debug, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum PreGraphPayload {
        IdleDecayed,
    }

    for fact in facts {
        let raw = serde_json::to_value(&fact).expect("graph fact JSON");
        assert!(
            serde_json::from_value::<PreGraphPayload>(raw.clone()).is_err(),
            "an old closed union rejects an additive typed variant"
        );
        let mut value = serde_json::to_value(envelope(EventPayload::IdleDecayed))
            .expect("raw envelope template");
        value["payload"] = raw.clone();
        let decoded: haider_protocol::envelope::RawEnvelope =
            serde_json::from_value(value).expect("RawEnvelope tolerates the new fact");
        assert_eq!(decoded.payload, raw);
    }
}

/// CG-M2a additive wire: the new template identity, trusted process fact,
/// slotted subject, and signal provenance are frozen separately so the M1
/// fixture above remains byte-for-byte legacy truth.
#[test]
fn additive_golden_convergence_graph_m2a_authority() {
    let graph_id = GraphId::new("graph-ship-loop-m2a");
    let command_arg_digest = "blake3:command-args".to_owned();
    let transcript_digest = "blake3:captured-output".to_owned();
    let subject_digest =
        process_signal_subject_digest(&command_arg_digest, &transcript_digest, None);
    let signal = ProcessSignalRecorded {
        run_id: RunId::new("run-m2a"),
        call_id: "call-process-tests".into(),
        effect_id: EffectId::new("effect-process-tests"),
        command_arg_digest,
        exit_code: Some(0),
        transcript_digest,
        workspace_revision: None,
        subject_digest: subject_digest.clone(),
        artifact: Some(ArtifactRef::new("blake3:transcript-artifact")),
    };
    additive_golden(
        "convergence_graph_m2a_authority",
        &vec![
            EventPayload::GraphPinned(GraphPinned {
                graph_id: graph_id.clone(),
                template: "ship-loop".into(),
                digest: ship_loop_digest(),
                template_version: 0,
                start_node: None,
                nodes: ship_loop_nodes(),
            }),
            EventPayload::ProcessSignalRecorded(signal.clone()),
            EventPayload::EvidenceRecorded(EvidenceRecorded {
                graph_id,
                node: haider_protocol::graph::verify_node(),
                attempt: 1,
                verdict: EvidenceVerdict::Green,
                detail: "cargo test passed".into(),
                fingerprint: evidence_fingerprint("cargo test passed"),
                slot: Some("tests".into()),
                subject_digest: Some(subject_digest),
                source: GraphEvidenceSource::ProcessSignal {
                    run_id: signal.run_id,
                    call_id: signal.call_id,
                    effect_id: signal.effect_id,
                },
            }),
        ],
    );
}

/// CG-M1 LAW: the SHIP confirmation is an ordinary durable, session-scoped,
/// nonblocking menu. Its new MenuKind and both semantic answer keys are
/// frozen independently from the eight graph fact discriminants above.
#[test]
fn golden_graph_human_confirm_menu() {
    golden(
        "graph_human_confirm_menu",
        &EventPayload::MenuOpened(Menu {
            id: MenuId::new("graph-confirm-graph-ship-loop-1-2"),
            kind: MenuKind::GraphHumanConfirm {
                graph_id: GraphId::new("graph-ship-loop-1"),
                node: haider_protocol::graph::ship_node(),
                attempt: 2,
            },
            title: "Ship this graph?".into(),
            body: vec!["BUILD and VERIFY are green for the current attempt epoch.".into()],
            options: vec![
                MenuOption {
                    key: "confirm".into(),
                    label: "Confirm ship".into(),
                    detail: Some("Satisfy SHIP and complete the graph.".into()),
                    decision: None,
                },
                MenuOption {
                    key: "hold".into(),
                    label: "Hold".into(),
                    detail: Some("Park the graph for explicit re-pin or abandon.".into()),
                    decision: None,
                },
            ],
            blocking: false,
            scope: MenuScope::Session,
            origin: "convergence-graph".into(),
            ttl_ms: None,
            timeout_option: None,
        }),
    );
}

#[test]
fn ship_loop_template_and_graph_brief_are_bounded_contracts() {
    assert_eq!(
        ship_loop_nodes(),
        vec![
            GraphNodeSpec {
                name: haider_protocol::graph::build_node(),
                gate: GraphGateKind::CommandGreen,
                executor: haider_protocol::graph::GraphExecutorShape::Inline,
                max_attempts: 8,
                max_evidence_per_attempt: Some(8),
                depends_on: Vec::new(),
                red_target: None,
                verify_slots: Vec::new(),
            },
            GraphNodeSpec {
                name: haider_protocol::graph::verify_node(),
                gate: GraphGateKind::AllOfN { n: 3 },
                executor: haider_protocol::graph::GraphExecutorShape::FanOut,
                max_attempts: 8,
                max_evidence_per_attempt: Some(8),
                depends_on: vec![haider_protocol::graph::build_node()],
                red_target: None,
                verify_slots: ["tests", "lint", "typecheck"]
                    .into_iter()
                    .map(|id| EvidenceSlotSpec {
                        id: id.into(),
                        authority: EvidenceAuthority::DaemonVerified,
                        subject_selector: SubjectSelector::Command,
                    })
                    .collect(),
            },
            GraphNodeSpec {
                name: haider_protocol::graph::ship_node(),
                gate: GraphGateKind::HumanConfirm,
                executor: haider_protocol::graph::GraphExecutorShape::Human,
                max_attempts: 8,
                max_evidence_per_attempt: None,
                depends_on: vec![haider_protocol::graph::verify_node()],
                red_target: None,
                verify_slots: Vec::new(),
            },
        ]
    );
    let graph_id = GraphId::new("graph-brief");
    let facts = vec![
        EventPayload::GraphPinned(GraphPinned {
            graph_id: graph_id.clone(),
            template: "ship-loop".into(),
            digest: ship_loop_digest(),
            template_version: 0,
            start_node: None,
            nodes: ship_loop_nodes(),
        }),
        EventPayload::GraphAttemptOpened(GraphAttemptOpened {
            graph_id,
            node: haider_protocol::graph::build_node(),
            attempt: 2,
        }),
    ];
    let envelopes = facts
        .into_iter()
        .take(2)
        .map(|fact| {
            serde_json::from_value(serde_json::to_value(envelope(fact)).expect("envelope JSON"))
                .expect("raw envelope")
        })
        .collect::<Vec<haider_protocol::envelope::RawEnvelope>>();
    let status: GraphStatus = haider_protocol::graph::reduce_graph(&envelopes)
        .status
        .expect("pinned graph reduces");
    let brief = status.graph_brief().expect("active graph brief");
    assert!(brief.len() <= haider_protocol::graph::GRAPH_BRIEF_MAX_BYTES);
}

#[test]
fn additive_permission_grant_needed_event_shape() {
    additive_golden(
        "permission_grant_needed",
        &PermissionEventPayload::PermissionGrantNeeded(PermissionGrantNeeded {
            request_id: "computer-permission-effect-7-screen_recording".into(),
            menu_id: MenuId::new("computer-permission-effect-7-screen_recording"),
            request_seq: 41,
            opening_generation: 3,
            call_id: "call-screen-1".into(),
            effect_id: EffectId::new("effect-7"),
            permission: SystemPermission::ScreenRecording,
            pane_name: "System Settings > Privacy & Security > Screen Recording".into(),
            settings_url:
                "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
                    .into(),
            actions: vec![
                PermissionGrantAction::OpenSettings,
                PermissionGrantAction::Retry,
                PermissionGrantAction::RestartDaemon,
            ],
            auto_restart_pending: false,
            poll_timeout_ms: 120_000,
        }),
    );
}

#[test]
fn additive_permission_grant_resolved_event_shape() {
    additive_golden(
        "permission_grant_resolved",
        &PermissionEventPayload::PermissionGrantResolved(PermissionGrantResolved {
            request_id: "computer-permission-effect-7-screen_recording".into(),
            permission: SystemPermission::ScreenRecording,
            resolution: PermissionGrantResolution::Granted,
            retrying_parked_action: true,
        }),
    );
}
