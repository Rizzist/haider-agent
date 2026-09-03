//! v0.0.970 client-memory laws after transcript virtualization.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::envelope::{EventEnvelope, PromptRender, RawEnvelope, RenderTargets};
use haider_protocol::ids::{DeviceId, EventId, SessionId};
use haider_tui::app::RuntimeMode;
use haider_tui::projection::TranscriptEntry;
use haider_tui::session::{PROMPT_RECALL_MAX_BYTES, PROMPT_RECALL_MAX_ENTRIES};
use haider_tui::wordmark::Wordmark;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui_image::picker::{Picker, ProtocolType};

mod common;
use common::launcher_model;

fn raw_prompt(session: &SessionId, seq: u64, text: String) -> RawEnvelope {
    EventEnvelope {
        schema_version: 1,
        event_id: EventId::new(format!("memclient3-{seq}")),
        seq,
        session_id: session.clone(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("memclient3-device"),
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

#[test]
fn wordmark_allocates_only_for_a_real_supported_draw_and_reuses_fixed_protocols() {
    for kind in [
        ProtocolType::Sixel,
        ProtocolType::Kitty,
        ProtocolType::Iterm2,
    ] {
        let mut picker = Picker::halfblocks();
        picker.set_protocol_type(kind);
        let mut wordmark = Wordmark::from_picker(picker).expect("a graphics protocol is accepted");
        let mut buffer = Buffer::empty(Rect::new(0, 0, 28, 4));

        wordmark.render_into(Rect::new(0, 0, 0, 4), &mut buffer);
        wordmark.render_into(Rect::new(0, 0, 23, 2), &mut buffer);
        assert!(
            !wordmark.is_initialized(),
            "empty or undersized {kind:?} image slots allocate nothing"
        );

        wordmark.render_into(Rect::new(0, 0, 24, 2), &mut buffer);
        assert!(
            wordmark.is_initialized(),
            "the first real {kind:?} draw initializes"
        );
        assert!(
            buffer
                .content()
                .iter()
                .any(|cell| cell.symbol().contains('\u{1b}')),
            "the fixed {kind:?} header protocol emitted terminal graphics"
        );
        wordmark.render_into(Rect::new(0, 0, 28, 4), &mut buffer);
        wordmark.render_into(Rect::new(0, 0, 24, 2), &mut buffer);
        assert!(
            wordmark.is_initialized(),
            "{kind:?} banner and header redraws reuse the fixed protocols"
        );
    }
}

#[test]
fn halfblock_picker_never_constructs_a_graphics_wordmark() {
    assert!(Wordmark::from_picker(Picker::halfblocks()).is_none());
}

#[test]
fn background_prompt_replay_keeps_only_the_newest_bounded_window() {
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
    assert_eq!(history.front().and_then(|entry| entry.seq), Some(135));
    assert_eq!(
        history.back().map(|entry| entry.text.as_str()),
        Some("prompt-7")
    );
    assert_eq!(history.back().and_then(|entry| entry.seq), Some(8));
    assert!(
        history
            .iter()
            .map(|entry| entry.text.capacity())
            .sum::<usize>()
            <= PROMPT_RECALL_MAX_BYTES
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

    let oversized_len = PROMPT_RECALL_MAX_BYTES + 1;
    let _ = model.route_raw(&raw_prompt(&session, 4, "z".repeat(oversized_len)));
    assert_eq!(model.prompt_history, before);
    assert!(
        matches!(
            model.projection.entries().last(),
            Some(TranscriptEntry::User { text, .. }) if text.len() == oversized_len
        ),
        "the authoritative transcript retains a prompt omitted from recall"
    );
    assert!(
        model
            .prompt_history
            .iter()
            .map(|entry| entry.text.capacity())
            .sum::<usize>()
            <= PROMPT_RECALL_MAX_BYTES
    );
}
