//! State-projection oracle: envelope sequences → expected badge strings and
//! transcript operations. Badge strings are sim goldens (`BADGE_LABEL`).
#![allow(clippy::expect_used)]

use base64::Engine as _;
use haider_protocol::EventPayload;
use haider_protocol::envelope::{EventEnvelope, PromptRender, RawEnvelope, RenderTargets};
use haider_protocol::history::{TodoItem, TodoState};
use haider_protocol::ids::{DeviceId, EventId, ItemId, SessionId};
use haider_protocol::item::{ItemDelta, ItemEvent, OutputStream, ToolStatus, TurnItem};
use haider_protocol::menu::{AnswerVia, Menu, MenuAnswer, MenuKind, MenuOption, MenuScope};
use haider_protocol::provider::{Usage, UsageSource};
use haider_protocol::state::{HarnessStatus, ReadinessCheck, RunState, VerifyStep, WaitReason};
use haider_tui::projection::{OUTPUT_TAIL_MAX, SessionProjection, TranscriptEntry};

fn item_id(n: u32) -> ItemId {
    ItemId::new(format!("item-{n}"))
}

fn envelope(seq: u64, payload: serde_json::Value) -> RawEnvelope {
    EventEnvelope {
        schema_version: 1,
        event_id: EventId::new(format!("evt-{seq}")),
        seq,
        session_id: SessionId::new("proj-session"),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("proj-device"),
        authority_epoch: 1,
        worker_generation: 1,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload,
    }
}

fn started(n: u32, item: TurnItem) -> EventPayload {
    EventPayload::Item(ItemEvent::Started {
        item_id: item_id(n),
        item,
    })
}

fn delta(n: u32, delta: ItemDelta) -> EventPayload {
    EventPayload::Item(ItemEvent::Delta {
        item_id: item_id(n),
        delta,
    })
}

fn completed(n: u32, item: TurnItem) -> EventPayload {
    EventPayload::Item(ItemEvent::Completed {
        item_id: item_id(n),
        item,
    })
}

fn todo(id: u32, text: &str, state: TodoState) -> TodoItem {
    TodoItem {
        id,
        text: text.to_owned(),
        state,
        dep: None,
    }
}

#[test]
fn badge_walks_the_run_state_machine_with_sim_labels() {
    let mut projection = SessionProjection::new();
    assert_eq!(projection.badge(), "IDLE");

    let expectations: Vec<(RunState, &str)> = vec![
        (RunState::Queued, "◌ QUEUED"),
        (RunState::Thinking, "● THINKING"),
        (RunState::Streaming, "▮ STREAMING"),
        (RunState::RunningTool, "⚒ TOOL_RUNNING"),
        (
            RunState::Waiting {
                reason: WaitReason::Dependency,
            },
            "◔ WAITING · dependency",
        ),
        (
            RunState::Waiting {
                reason: WaitReason::Other {
                    tag: "custom".to_owned(),
                },
            },
            "◔ WAITING · custom",
        ),
        (RunState::Compacting, "⊟ COMPACTING"),
        (
            RunState::Verifying {
                step: VerifyStep::Check,
            },
            "⚙ VERIFYING · check",
        ),
        (RunState::Concluding, "◆ CONCLUDING"),
        (RunState::EffectOutcomeUnknown, "⌁ EFFECT_UNKNOWN"),
        (RunState::Cancelling, "⊘ CANCELLING"),
        (RunState::Done, "IDLE"),
        (RunState::Errored, "✗ ERRORED"),
    ];
    for (state, label) in expectations {
        projection.apply(&EventPayload::RunState(state));
        assert_eq!(projection.badge(), label);
    }
}

#[test]
fn cancelled_turn_shows_interrupted_idle_until_decay() {
    let mut projection = SessionProjection::new();
    projection.apply(&EventPayload::RunState(RunState::Thinking));
    projection.apply(&EventPayload::RunState(RunState::Cancelling));
    projection.apply(&EventPayload::RunState(RunState::Cancelled));
    assert_eq!(projection.badge(), "⏸ IDLE (i)");
    projection.apply(&EventPayload::IdleDecayed);
    assert_eq!(projection.badge(), "IDLE");
    // A fresh turn clears interruption on its own.
    projection.apply(&EventPayload::RunState(RunState::Cancelled));
    assert_eq!(projection.badge(), "⏸ IDLE (i)");
    projection.apply(&EventPayload::RunState(RunState::Thinking));
    assert_eq!(projection.badge(), "● THINKING");
    projection.apply(&EventPayload::RunState(RunState::Done));
    assert_eq!(projection.badge(), "IDLE");
}

#[test]
fn harness_starting_wins_the_badge_and_exposes_checks() {
    let mut projection = SessionProjection::new();
    projection.apply(&EventPayload::RunState(RunState::Thinking));
    projection.apply(&EventPayload::HarnessStatus(HarnessStatus::Starting {
        checks: vec![ReadinessCheck {
            name: "store open · journal replayed".to_owned(),
            ok: true,
            duration_ms: 12,
        }],
    }));
    assert_eq!(projection.badge(), "◌ STARTING");
    let checks = projection.boot_checks().expect("starting exposes checks");
    assert_eq!(checks.len(), 1);
    assert!(checks[0].ok);

    projection.apply(&EventPayload::HarnessStatus(HarnessStatus::Ready));
    assert_eq!(projection.boot_checks(), None);
    assert_eq!(projection.badge(), "● THINKING");
}

#[test]
fn agent_message_streams_then_completed_replaces() {
    let mut projection = SessionProjection::new();
    projection.apply(&started(
        1,
        TurnItem::AgentMessage {
            text: String::new(),
        },
    ));
    projection.apply(&delta(
        1,
        ItemDelta::Text {
            text: "Hel".to_owned(),
        },
    ));
    projection.apply(&delta(
        1,
        ItemDelta::Text {
            text: "lo…".to_owned(),
        },
    ));
    let entries = projection.entries();
    assert_eq!(entries.len(), 1);
    let TranscriptEntry::Item(block) = &entries[0] else {
        panic!("agent message renders as an item block");
    };
    assert!(block.streaming);
    assert_eq!(
        block.item,
        TurnItem::AgentMessage {
            text: "Hello…".to_owned()
        }
    );

    // Completed is authoritative and REPLACES accumulated deltas.
    projection.apply(&completed(
        1,
        TurnItem::AgentMessage {
            text: "Hello, corrected.".to_owned(),
        },
    ));
    let TranscriptEntry::Item(block) = &projection.entries()[0] else {
        panic!("still one block");
    };
    assert!(!block.streaming);
    assert_eq!(
        block.item,
        TurnItem::AgentMessage {
            text: "Hello, corrected.".to_owned()
        }
    );
}

#[test]
fn tool_call_accumulates_arg_fragments_for_display() {
    let mut projection = SessionProjection::new();
    projection.apply(&started(
        2,
        TurnItem::ToolCall {
            call_id: "call-1".to_owned(),
            name: "fs_read".to_owned(),
            args: serde_json::Value::Null,
            status: ToolStatus::InProgress,
        },
    ));
    projection.apply(&delta(
        2,
        ItemDelta::ToolArgs {
            fragment: r#"{"path":"#.to_owned(),
        },
    ));
    projection.apply(&delta(
        2,
        ItemDelta::ToolArgs {
            fragment: r#""a.rs"}"#.to_owned(),
        },
    ));
    let TranscriptEntry::Item(block) = &projection.entries()[0] else {
        panic!("tool call block");
    };
    assert_eq!(block.args_fragments, r#"{"path":"a.rs"}"#);
}

#[test]
fn command_output_is_decoded_bounded_and_lossy() {
    let mut projection = SessionProjection::new();
    projection.apply(&started(
        3,
        TurnItem::CommandExecution {
            call_id: "call-2".to_owned(),
            command: "yes".to_owned(),
            status: ToolStatus::InProgress,
            exit_code: None,
        },
    ));
    let chunk = |bytes: &[u8]| ItemDelta::CommandOutput {
        stream: OutputStream::Stdout,
        chunk_b64: base64::engine::general_purpose::STANDARD.encode(bytes),
    };
    projection.apply(&delta(3, chunk(b"head-marker|")));
    projection.apply(&delta(3, chunk(&vec![b'y'; OUTPUT_TAIL_MAX])));
    let TranscriptEntry::Item(block) = &projection.entries()[0] else {
        panic!("command block");
    };
    assert_eq!(block.output_tail.len(), OUTPUT_TAIL_MAX);
    assert!(block.output_truncated, "front dropped once over the cap");
    assert!(!block.output_text().contains("head-marker"));

    // Invalid base64 sets the honesty flag and appends nothing.
    projection.apply(&delta(
        3,
        ItemDelta::CommandOutput {
            stream: OutputStream::Stderr,
            chunk_b64: "*** not base64 ***".to_owned(),
        },
    ));
    let TranscriptEntry::Item(block) = &projection.entries()[0] else {
        panic!("command block");
    };
    assert!(block.output_decode_error);
    assert_eq!(block.output_tail.len(), OUTPUT_TAIL_MAX);
}

#[test]
fn completed_without_started_lands_as_finished_block() {
    let mut projection = SessionProjection::new();
    projection.apply(&completed(
        4,
        TurnItem::FileChange {
            path: "src/lib.rs".to_owned(),
            added: 3,
            removed: 1,
        },
    ));
    let entries = projection.entries();
    assert_eq!(entries.len(), 1);
    let TranscriptEntry::Item(block) = &entries[0] else {
        panic!("file change block");
    };
    assert!(!block.streaming);
}

#[test]
fn orphan_deltas_are_counted_never_fatal() {
    let mut projection = SessionProjection::new();
    projection.apply(&delta(
        9,
        ItemDelta::Text {
            text: "ghost".to_owned(),
        },
    ));
    assert_eq!(projection.orphan_deltas(), 1);
    assert!(projection.entries().is_empty());
}

#[test]
fn plan_pins_updates_and_unpins_into_transcript_when_done() {
    let mut projection = SessionProjection::new();
    projection.apply(&started(
        5,
        TurnItem::Plan {
            items: vec![
                todo(0, "write theme tokens", TodoState::Processing),
                todo(1, "boot screen", TodoState::Listed),
            ],
        },
    ));
    let todos = projection.todos().expect("plan pins");
    assert!(todos.pinned);
    assert_eq!(todos.done_count(), 0);
    assert_eq!(
        todos.current().map(|t| t.text.as_str()),
        Some("write theme tokens")
    );
    assert!(
        projection.entries().is_empty(),
        "pinned plan is not history"
    );

    // Mid-turn update: one done, one processing — still pinned.
    projection.apply(&completed(
        5,
        TurnItem::Plan {
            items: vec![
                todo(0, "write theme tokens", TodoState::Completed),
                todo(1, "boot screen", TodoState::Processing),
            ],
        },
    ));
    let todos = projection.todos().expect("plan still present");
    assert!(todos.pinned);
    assert_eq!(todos.done_count(), 1);

    // All done: unpins and lands in the transcript as history.
    projection.apply(&completed(
        5,
        TurnItem::Plan {
            items: vec![
                todo(0, "write theme tokens", TodoState::Completed),
                todo(1, "boot screen", TodoState::Completed),
            ],
        },
    ));
    let todos = projection.todos().expect("plan record kept");
    assert!(!todos.pinned);
    assert_eq!(projection.entries().len(), 1);
}

#[test]
fn user_messages_and_menus_project() {
    let mut projection = SessionProjection::new();
    projection.apply(&EventPayload::UserMessage {
        text: "run the tests".to_owned(),
        attachments: vec![],
        mode: haider_protocol::DeliveryMode::Steer,
    });
    assert_eq!(
        projection.entries(),
        &[TranscriptEntry::User {
            text: "run the tests".to_owned(),
            attachments: 0
        }]
    );

    let menu = Menu {
        id: haider_protocol::ids::MenuId::new("menu-1"),
        kind: MenuKind::Permission {
            effect_summary: "write src/lib.rs".to_owned(),
        },
        title: "Allow fs_patch?".to_owned(),
        body: vec![],
        options: vec![MenuOption {
            key: "allow".to_owned(),
            label: "Allow".to_owned(),
            detail: None,
            decision: None,
        }],
        blocking: true,
        scope: MenuScope::Session,
        origin: "fs_patch".to_owned(),
        ttl_ms: None,
        timeout_option: None,
    };
    projection.apply(&EventPayload::MenuOpened(menu.clone()));
    assert!(projection.open_menu().is_some());

    // An answer for a DIFFERENT menu does not clear the open one.
    projection.apply(&EventPayload::MenuAnswered(MenuAnswer {
        menu: haider_protocol::ids::MenuId::new("menu-0"),
        option_key: Some("allow".to_owned()),
        option_index: 0,
        value: None,
        via: AnswerVia::Tui,
    }));
    assert!(projection.open_menu().is_some());

    projection.apply(&EventPayload::MenuAnswered(MenuAnswer {
        menu: haider_protocol::ids::MenuId::new("menu-1"),
        option_key: Some("allow".to_owned()),
        option_index: 0,
        value: None,
        via: AnswerVia::Tui,
    }));
    assert!(projection.open_menu().is_none());
}

#[test]
fn usage_drives_the_context_meter() {
    let mut projection = SessionProjection::new();
    assert_eq!(projection.context_tokens(), 0);
    projection.apply(&EventPayload::Usage(Usage {
        input: 1000,
        output: 200,
        reasoning: 50,
        cached: 300,
        source: UsageSource::ProviderReported,
        account: None,
    }));
    assert_eq!(projection.context_tokens(), 1550);
}

#[test]
fn raw_stream_tracks_gaps_duplicates_and_unknown_payloads() {
    let mut projection = SessionProjection::new();
    let thinking = serde_json::to_value(EventPayload::RunState(RunState::Thinking))
        .expect("payload serializes");
    let streaming = serde_json::to_value(EventPayload::RunState(RunState::Streaming))
        .expect("payload serializes");

    projection.apply_raw(&envelope(1, thinking.clone()));
    assert_eq!(projection.badge(), "● THINKING");
    assert!(!projection.gap_seen());

    // Duplicate seq: skipped entirely.
    projection.apply_raw(&envelope(1, streaming.clone()));
    assert_eq!(projection.badge(), "● THINKING");

    // Unknown payload kind: counted, stream continues (forward-compat law).
    projection.apply_raw(&envelope(
        2,
        serde_json::json!({"type": "from_the_future", "x": 1}),
    ));
    assert_eq!(projection.unknown_payloads(), 1);

    // Gap: recorded, event still applied.
    projection.apply_raw(&envelope(5, streaming));
    assert!(projection.gap_seen());
    assert_eq!(projection.badge(), "▮ STREAMING");
}

#[test]
fn command_tail_capacity_stays_bounded_after_huge_chunks() {
    // Rider #4: append-then-drain retained a chunk-sized capacity high-water
    // mark. The tail must bound BEFORE appending.
    let mut projection = SessionProjection::new();
    projection.apply(&started(
        30,
        TurnItem::CommandExecution {
            call_id: "cap".to_owned(),
            command: "yes".to_owned(),
            status: ToolStatus::InProgress,
            exit_code: None,
        },
    ));
    let huge = vec![b'z'; OUTPUT_TAIL_MAX * 8];
    projection.apply(&delta(
        30,
        ItemDelta::CommandOutput {
            stream: OutputStream::Stdout,
            chunk_b64: base64::engine::general_purpose::STANDARD.encode(&huge),
        },
    ));
    let TranscriptEntry::Item(block) = &projection.entries()[0] else {
        panic!("command block");
    };
    assert_eq!(block.output_tail.len(), OUTPUT_TAIL_MAX);
    assert!(block.output_truncated);
    assert!(
        block.output_tail.capacity() < OUTPUT_TAIL_MAX * 4,
        "capacity {} retains the huge chunk",
        block.output_tail.capacity()
    );
}

#[test]
fn completed_tool_call_releases_the_fragment_accumulation() {
    // Rider #3: the completed item carries authoritative args; the raw
    // fragment duplicate must be released.
    let mut projection = SessionProjection::new();
    projection.apply(&started(
        31,
        TurnItem::ToolCall {
            call_id: "frag".to_owned(),
            name: "fs_patch".to_owned(),
            args: serde_json::Value::Null,
            status: ToolStatus::InProgress,
        },
    ));
    projection.apply(&delta(
        31,
        ItemDelta::ToolArgs {
            fragment: "{\"big\":\"payload\"}".to_owned(),
        },
    ));
    projection.apply(&completed(
        31,
        TurnItem::ToolCall {
            call_id: "frag".to_owned(),
            name: "fs_patch".to_owned(),
            args: serde_json::json!({"big": "payload"}),
            status: ToolStatus::Completed,
        },
    ));
    let TranscriptEntry::Item(block) = &projection.entries()[0] else {
        panic!("tool block");
    };
    assert!(block.args_fragments.is_empty());
}

#[test]
fn context_tokens_saturate_on_adversarial_usage() {
    let mut projection = SessionProjection::new();
    projection.apply(&EventPayload::Usage(Usage {
        input: u64::MAX,
        output: 10,
        reasoning: 10,
        cached: 10,
        source: UsageSource::ProviderReported,
        account: None,
    }));
    assert_eq!(projection.context_tokens(), u64::MAX);
}

#[test]
fn non_ui_envelopes_advance_seq_without_display_mutation() {
    // §6.1: render targets are law — ui:false events must not paint.
    let mut projection = SessionProjection::new();
    let mut hidden = envelope(
        1,
        serde_json::to_value(EventPayload::RunState(RunState::Thinking)).expect("serializes"),
    );
    hidden.render.ui = false;
    projection.apply_raw(&hidden);
    assert_eq!(projection.badge(), "IDLE", "non-ui event painted the badge");

    // Seq accounting DID advance: a duplicate of seq 1 is still skipped…
    let visible_dup = envelope(
        1,
        serde_json::to_value(EventPayload::RunState(RunState::Streaming)).expect("serializes"),
    );
    projection.apply_raw(&visible_dup);
    assert_eq!(projection.badge(), "IDLE");
    // …and seq 2 applies without recording a gap.
    let next = envelope(
        2,
        serde_json::to_value(EventPayload::RunState(RunState::Streaming)).expect("serializes"),
    );
    projection.apply_raw(&next);
    assert!(!projection.gap_seen());
    assert_eq!(projection.badge(), "▮ STREAMING");
}

#[test]
fn item_lifecycle_is_idempotent_under_redelivery() {
    let mut projection = SessionProjection::new();
    let message = TurnItem::AgentMessage {
        text: "final".to_owned(),
    };
    projection.apply(&started(
        40,
        TurnItem::AgentMessage {
            text: String::new(),
        },
    ));
    projection.apply(&completed(40, message.clone()));
    // Re-delivered Completed under a fresh seq: no duplicate block.
    projection.apply(&completed(40, message.clone()));
    // Stale Started for a closed id: no revival.
    projection.apply(&started(
        40,
        TurnItem::AgentMessage {
            text: String::new(),
        },
    ));
    // Double Started for an OPEN id: no double block.
    projection.apply(&started(
        41,
        TurnItem::AgentMessage {
            text: String::new(),
        },
    ));
    projection.apply(&started(
        41,
        TurnItem::AgentMessage {
            text: String::new(),
        },
    ));

    assert_eq!(projection.entries().len(), 2, "one block per item id, ever");
    assert_eq!(projection.duplicate_items(), 3);
}

#[test]
fn finished_plan_redelivery_does_not_duplicate_history() {
    let mut projection = SessionProjection::new();
    let done_plan = TurnItem::Plan {
        items: vec![todo(0, "a", TodoState::Completed)],
    };
    projection.apply(&completed(50, done_plan.clone()));
    projection.apply(&completed(50, done_plan));
    assert_eq!(projection.entries().len(), 1, "one history entry per plan");
    assert_eq!(projection.duplicate_items(), 1);
}

#[test]
fn stale_plan_started_cannot_overwrite_a_progressed_plan() {
    // Review r2 P2: active plans live in todos, not entries — Started
    // idempotency must cover them.
    let mut projection = SessionProjection::new();
    projection.apply(&started(
        60,
        TurnItem::Plan {
            items: vec![todo(0, "a", TodoState::Processing)],
        },
    ));
    projection.apply(&completed(
        60,
        TurnItem::Plan {
            items: vec![
                todo(0, "a", TodoState::Completed),
                todo(1, "b", TodoState::Processing),
            ],
        },
    ));
    // Stale re-delivered Started for the same plan id: ignored, counted.
    projection.apply(&started(
        60,
        TurnItem::Plan {
            items: vec![todo(0, "a", TodoState::Processing)],
        },
    ));
    let todos_panel = projection.todos().expect("plan pinned");
    assert!(todos_panel.pinned);
    assert_eq!(todos_panel.done_count(), 1, "progressed state survives");
    assert_eq!(todos_panel.items.len(), 2);
    assert_eq!(projection.duplicate_items(), 1);
}
