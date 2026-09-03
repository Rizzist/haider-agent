//! v0.0.970 client-memory laws: lazy image state and bounded idle caches.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::envelope::{EventEnvelope, PromptRender, RawEnvelope, RenderTargets};
use haider_protocol::ids::{DeviceId, EventId, ItemId, SessionId};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::state::RunState;
use haider_rpc::AttachmentId;
use haider_tui::app::{AppEvent, AppModel, RuntimeMode};
use haider_tui::live::{LiveDriver, LiveReply};
use haider_tui::render::render;
use haider_tui::session::{PROMPT_RECALL_MAX_BYTES, PROMPT_RECALL_MAX_ENTRIES};
use haider_tui::wordmark::Wordmark;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui_image::picker::{Picker, ProtocolType};

mod common;
use common::launcher_model;

fn raw_prompt(session: &SessionId, seq: u64, text: String) -> RawEnvelope {
    EventEnvelope {
        schema_version: 1,
        event_id: EventId::new(format!("memclient2-{seq}")),
        seq,
        session_id: session.clone(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("memclient2-device"),
        authority_epoch: 1,
        worker_generation: 1,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: seq,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::to_value(EventPayload::UserMessage {
            text,
            attachments: vec![],
            mode: haider_protocol::DeliveryMode::Steer,
        })
        .expect("prompt serializes"),
    }
}

fn draw(model: &AppModel) {
    let backend = TestBackend::new(118, 36);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            let _ = render(model, frame);
        })
        .expect("frame renders");
}

#[test]
fn wordmark_allocates_on_first_real_render_and_reuses_one_protocol() {
    let mut picker = Picker::halfblocks();
    picker.set_protocol_type(ProtocolType::Sixel);
    let mut wordmark = Wordmark::from_picker(picker).expect("Sixel is a graphics protocol");
    let mut buffer = Buffer::empty(Rect::new(0, 0, 28, 4));

    wordmark.render_into(Rect::new(0, 0, 0, 4), &mut buffer);
    assert!(!wordmark.is_initialized(), "empty areas allocate nothing");

    wordmark.render_into(Rect::new(0, 0, 24, 2), &mut buffer);
    assert!(wordmark.is_initialized(), "the first real draw initializes");
    wordmark.render_into(Rect::new(0, 0, 24, 2), &mut buffer);
    assert!(
        wordmark.is_initialized(),
        "later frames retain that protocol"
    );
}

#[test]
fn background_prompt_replay_keeps_exactly_the_newest_bounded_window() {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    let session = model.sessions[0].id.clone();
    for index in 0..PROMPT_RECALL_MAX_ENTRIES + 7 {
        let seq = u64::try_from(index + 1).expect("small test index");
        let _ = model.route_raw(&raw_prompt(&session, seq, format!("prompt-{index}")));
    }

    let history = &model.sessions[0].prompt_history;
    assert_eq!(history.len(), PROMPT_RECALL_MAX_ENTRIES);
    assert_eq!(
        history.front().map(|entry| entry.text.as_str()),
        Some("prompt-134")
    );
    assert_eq!(
        history.back().map(|entry| entry.text.as_str()),
        Some("prompt-7")
    );
}

#[test]
fn attached_prompt_recall_caps_bytes_and_skips_one_oversized_prompt() {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    let session = model.sessions[0].id.clone();
    model.open_session(&session);
    let chunk = "x".repeat(PROMPT_RECALL_MAX_BYTES / 2);
    for seq in 1..=3 {
        let _ = model.route_raw(&raw_prompt(&session, seq, chunk.clone()));
    }
    assert_eq!(model.prompt_history.len(), 2);
    let before = model.prompt_history.clone();

    let _ = model.route_raw(&raw_prompt(
        &session,
        4,
        "z".repeat(PROMPT_RECALL_MAX_BYTES + 1),
    ));
    assert_eq!(model.prompt_history, before);
}

#[test]
fn oversized_transcript_retains_geometry_without_duplicating_payload_text() {
    let mut model = launcher_model();
    let session = model.sessions[0].id.clone();
    model.open_session(&session);
    model.handle(AppEvent::Envelope(Box::new(EventPayload::Item(
        ItemEvent::Completed {
            item_id: ItemId::new("large-answer"),
            item: TurnItem::AgentMessage {
                text: "x".repeat(700 * 1024),
            },
        },
    ))));

    draw(&model);

    let (entries, retained_bytes, bounded) = model.transcript_cache_stats();
    assert!(entries <= 128);
    assert!(bounded);
    assert!(retained_bytes > 0);
    assert!(retained_bytes <= 512 * 1024);
}

#[test]
fn nested_json_container_capacity_cannot_escape_transcript_cache_cap() {
    let mut model = launcher_model();
    let session = model.sessions[0].id.clone();
    model.open_session(&session);
    model.handle(AppEvent::Envelope(Box::new(EventPayload::Item(
        ItemEvent::Completed {
            item_id: ItemId::new("large-tool-args"),
            item: TurnItem::ToolCall {
                call_id: "large-tool-call".to_owned(),
                name: "large-tool".to_owned(),
                args: serde_json::Value::Array(vec![serde_json::Value::Null; 128 * 1024]),
                status: haider_protocol::item::ToolStatus::Completed,
            },
        },
    ))));

    draw(&model);

    let (entries, retained_bytes, bounded) = model.transcript_cache_stats();
    assert!(entries > 0);
    assert!(!bounded);
    assert!(
        retained_bytes < 128 * 1024,
        "the multi-megabyte source container is not cloned into the cache"
    );
}

#[test]
fn bounded_transcript_reformats_only_the_changed_entry() {
    let mut model = launcher_model();
    let session = model.sessions[0].id.clone();
    model.open_session(&session);
    for index in 0..140 {
        model.projection.push_note(format!("history row {index}"));
    }
    let transcript_entries = model.projection.entries().len() as u64;

    draw(&model);
    let initial = model.transcript_cache_bounded_format_count();
    assert!(initial >= transcript_entries);
    assert!(initial <= transcript_entries + 40);

    model.projection.apply(&EventPayload::IdleDecayed);
    draw(&model);
    assert_eq!(model.transcript_cache_bounded_format_count(), initial);

    model.projection.push_note("one appended row".to_owned());
    draw(&model);
    let appended = model.transcript_cache_bounded_format_count();
    assert!(
        (1..=2).contains(&appended.saturating_sub(initial)),
        "only new geometry and its newly visible row may format"
    );
}

#[test]
fn large_background_replay_carries_transient_pressure_to_caught_up() {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    let session = model.sessions[0].id.clone();
    let attachment = AttachmentId::new("memclient2-attachment");
    let mut driver = LiveDriver::new("memclient2-driver");
    let _ = driver.apply(
        &mut model,
        LiveReply::Attached {
            session: session.clone(),
            attachment: attachment.clone(),
            worker_generation: 1,
            replay_through_seq: 1,
        },
    );
    let replay = raw_prompt(&session, 1, "history".repeat(48 * 1024));
    let _ = driver.apply(
        &mut model,
        LiveReply::Event {
            attachment: attachment.clone(),
            session,
            envelope: Box::new(replay),
        },
    );
    // Model the runtime painting the final replay event before CaughtUp.
    model.dirty = false;
    assert_eq!(model.finish_frame_memory_maintenance(), 0);
    let _ = driver.apply(
        &mut model,
        LiveReply::CaughtUp {
            attachment,
            high_water_seq: 1,
        },
    );

    assert!(model.dirty, "CaughtUp schedules the post-replay frame");
    assert!(
        model.finish_frame_memory_maintenance() >= 256 * 1024,
        "large replay crosses the coalesced allocator-relief threshold"
    );
}

#[test]
fn terminal_frame_releases_cache_once_and_next_idle_frame_reuses_rebuild() {
    let mut model = launcher_model();
    let session = model.sessions[0].id.clone();
    model.open_session(&session);
    model.handle(AppEvent::Envelope(Box::new(EventPayload::UserMessage {
        text: "remember this frame".into(),
        attachments: vec![],
        mode: haider_protocol::DeliveryMode::Steer,
    })));
    model.handle(AppEvent::Envelope(Box::new(EventPayload::RunState(
        RunState::Done,
    ))));

    draw(&model);
    assert!(
        model.transcript_cache_stats().0 > 0,
        "terminal frame has geometry"
    );
    assert!(model.finish_frame_memory_maintenance() > 0);
    assert_eq!(model.transcript_cache_stats(), (0, 0, false));

    draw(&model);
    let rebuilt = model.transcript_cache_stats();
    assert!(rebuilt.0 > 0);
    assert_eq!(model.finish_frame_memory_maintenance(), 0);
    draw(&model);
    assert_eq!(model.transcript_cache_stats(), rebuilt);
}

#[test]
fn background_settlement_does_not_evict_a_streaming_active_transcript() {
    let mut model = launcher_model();
    let session = model.sessions[0].id.clone();
    model.open_session(&session);
    model.handle(AppEvent::Envelope(Box::new(EventPayload::UserMessage {
        text: "active cache must survive".into(),
        attachments: vec![],
        mode: haider_protocol::DeliveryMode::Steer,
    })));
    model.handle(AppEvent::Envelope(Box::new(EventPayload::RunState(
        RunState::Streaming,
    ))));

    draw(&model);
    let active_cache = model.transcript_cache_stats();
    assert!(active_cache.0 > 0);

    // A background attachment reaching CaughtUp schedules the same global
    // maintenance pass. It may return transient pages to the allocator, but
    // it must not discard a still-live active session's view cache.
    model.request_client_memory_settle_after(256 * 1024);
    assert!(model.finish_frame_memory_maintenance() >= 256 * 1024);
    assert_eq!(model.transcript_cache_stats(), active_cache);
}
