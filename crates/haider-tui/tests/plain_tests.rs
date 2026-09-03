//! `--plain` fallback goldens: the projection rendered as stable text lines.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::ids::ItemId;
use haider_protocol::item::{ItemDelta, ItemEvent, OutputStream, ToolStatus, TurnItem};
use haider_tui::mock::demo_script;
use haider_tui::plain::{render_plain, status_glyph, status_line};
use haider_tui::projection::SessionProjection;

#[test]
fn demo_script_renders_the_full_plain_story() {
    let mut projection = SessionProjection::new();
    for payload in &demo_script() {
        projection.apply(payload);
    }
    let text = render_plain(&projection, 200_000, None);
    assert!(text.contains("❯ fix the failing boundary test in haider-store"));
    assert!(text.contains("the boundary check rejects seq 0"));
    assert!(text.contains("⚒ fs_read ✓"));
    assert!(text.contains("± crates/haider-store/src/event_store.rs +4 -1"));
    assert!(text.contains("✓ plan — 3/3 done"));
    assert!(
        !text.contains("todos —"),
        "finished plan is history, not a pinned panel"
    );
    let last_line = text.lines().last().expect("status line");
    assert!(last_line.starts_with("IDLE · 33k tok · "));
    assert!(last_line.ends_with("17% of 200k"));
}

#[test]
fn streaming_agent_text_shows_the_cursor_block() {
    let mut projection = SessionProjection::new();
    projection.apply(&EventPayload::Item(ItemEvent::Started {
        item_id: ItemId::new("i1"),
        item: TurnItem::AgentMessage {
            text: String::new().into(),
        },
    }));
    projection.apply(&EventPayload::Item(ItemEvent::Delta {
        item_id: ItemId::new("i1"),
        delta: ItemDelta::Text {
            text: "stream".to_owned(),
        },
    }));
    assert!(render_plain(&projection, 0, None).contains("stream▮"));
}

#[test]
fn command_blocks_render_output_tail_and_truncation_notice() {
    use base64::Engine as _;
    let mut projection = SessionProjection::new();
    projection.apply(&EventPayload::Item(ItemEvent::Started {
        item_id: ItemId::new("c1"),
        item: TurnItem::CommandExecution {
            call_id: "call".to_owned(),
            command: "cargo test".to_owned(),
            status: ToolStatus::InProgress,
            exit_code: None,
        },
    }));
    let big = vec![b'x'; haider_tui::projection::OUTPUT_TAIL_MAX + 10];
    projection.apply(&EventPayload::Item(ItemEvent::Delta {
        item_id: ItemId::new("c1"),
        delta: ItemDelta::CommandOutput {
            stream: OutputStream::Stdout,
            chunk_b64: base64::engine::general_purpose::STANDARD.encode(&big),
        },
    }));
    projection.apply(&EventPayload::Item(ItemEvent::Completed {
        item_id: ItemId::new("c1"),
        item: TurnItem::CommandExecution {
            call_id: "call".to_owned(),
            command: "cargo test".to_owned(),
            status: ToolStatus::Failed,
            exit_code: Some(101),
        },
    }));
    let text = render_plain(&projection, 0, None);
    assert!(text.contains("$ cargo test ✗ · exit 101"));
    assert!(!text.contains("! cargo test ✗ · exit 101"));
    assert!(
        !projection.mark_user_command(&haider_protocol::item::UserCommandOriginV1 {
            origin: haider_protocol::item::CommandExecutionOrigin::UserCommand,
            command_item_id: ItemId::new("c1"),
            call_id: "wrong-call".to_owned(),
        }),
        "a mismatched provenance coordinate fails closed"
    );
    assert!(
        projection.mark_user_command(&haider_protocol::item::UserCommandOriginV1 {
            origin: haider_protocol::item::CommandExecutionOrigin::UserCommand,
            command_item_id: ItemId::new("c1"),
            call_id: "call".to_owned(),
        })
    );
    let text = render_plain(&projection, 0, None);
    assert!(text.contains("! cargo test ✗ · exit 101"));
    assert!(text.contains("⋯ earlier output truncated"));
}

#[test]
fn status_glyphs_cover_every_tool_status() {
    assert_eq!(status_glyph(ToolStatus::Pending), "…");
    // Sim ToolRow running glyph (tui.js:3901-3909) — TUI3b parity fix.
    assert_eq!(status_glyph(ToolStatus::InProgress), "◐");
    assert_eq!(status_glyph(ToolStatus::Completed), "✓");
    assert_eq!(status_glyph(ToolStatus::Failed), "✗");
    assert_eq!(status_glyph(ToolStatus::Cancelled), "⊘");
}

#[test]
fn status_line_without_window_omits_the_meter() {
    let projection = SessionProjection::new();
    assert_eq!(status_line(&projection, 0), "IDLE · 0 tok");
}
