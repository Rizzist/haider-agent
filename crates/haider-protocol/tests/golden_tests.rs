//! Golden serialization fixtures — the REAL freeze artifact.
//!
//! Each case serializes a representative value and compares byte-for-byte with
//! `tests/fixtures/<name>.json`. Run with `UPDATE_FIXTURES=1` to (re)write
//! fixtures — doing so in a patch is a schema change and needs the freeze
//! process (version bump, ADR, review). Round-trips are also asserted.
#![allow(clippy::expect_used)] // tests may expect; the lint guards src/ only

use haider_protocol::EventPayload;
use haider_protocol::agent::{AgentMessageDelivery, AgentMessaged, ChipState};
use haider_protocol::branch::{BranchCreated, BranchDescriptor, BranchEventPayload};
use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets};
use haider_protocol::hook::{
    HookAttachmentMetadata, HookAttachmentSet, HookEventPayload, HookFired, HookInput, HookOutput,
    HookRuntimeKind,
};
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

fn additive_golden<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(
    name: &str,
    value: &T,
) {
    let serialized = serde_json::to_string_pretty(value).expect("serialize");
    let expected = std::fs::read_to_string(fixture_path(name))
        .unwrap_or_else(|_| panic!("missing additive fixture {name}"));
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
    let encoded = serde_json::to_string(&decoded).expect("re-encode");
    assert!(!encoded.contains("effort"));
    assert!(!encoded.contains("fast"));

    let tuned = SessionMetadataV1 {
        effort: Some("max".into()),
        fast: true,
        ..decoded
    };
    let encoded = serde_json::to_value(&tuned).expect("tuned encode");
    assert_eq!(encoded["effort"], "max");
    assert_eq!(encoded["fast"], true);
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
    // G2 additive text-file variant: ref + display basename + line count.
    golden(
        "attachment_file",
        &AttachmentBlock::File {
            artifact: ArtifactRef::new("blake3:file"),
            name: "notes.md".into(),
            lines: 42,
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
                        lines_added: 240,
                        lines_removed: 60,
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
                        lines_added: 12,
                        lines_removed: 4,
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
