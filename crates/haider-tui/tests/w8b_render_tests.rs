//! W8b render laws: a process tool's retained output tail is visible, the
//! live /tools screen renders committed daemon inventory only, and the `!`
//! escape stays out of demo vocabulary.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::ids::ItemId;
use haider_protocol::item::{ItemDelta, ItemEvent, ToolStatus, TurnItem};
use haider_protocol::tool::{
    ToolInventoryEntry, ToolInventorySnapshot, ToolManifest, ToolPermissionDefault,
};
use haider_tui::app::{AppEvent, AppModel, Hit, Screen};
use haider_tui::mock::demo_script;
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::Rect;

mod common;
use common::key;

fn session_model() -> AppModel {
    let mut model = AppModel::new();
    for payload in demo_script() {
        model.handle(AppEvent::Envelope(Box::new(payload)));
    }
    model
}

fn draw(model: &AppModel, width: u16, height: u16) -> (Vec<String>, Vec<(Rect, Hit)>) {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut hits = Vec::new();
    terminal
        .draw(|frame| hits = render(model, frame))
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let mut rows = Vec::new();
    for y in 0..buffer.area.height {
        let mut line = String::new();
        for x in 0..buffer.area.width {
            line.push_str(buffer[(x, y)].symbol());
        }
        rows.push(line);
    }
    (rows, hits)
}

/// MUTATION CHECK (research risk 10): drop the ToolCall output block from
/// the renderer. Expected runtime failure: the tail line assertion below —
/// durably retained process output goes invisible again.
#[test]
fn a_process_tool_calls_retained_output_tail_is_rendered() {
    let mut model = session_model();
    let id = ItemId::new("proc-1");
    model
        .projection
        .apply(&EventPayload::Item(ItemEvent::Started {
            item_id: id.clone(),
            item: TurnItem::ToolCall {
                call_id: "call-proc".into(),
                name: "process_exec".into(),
                args: serde_json::json!({"command": "cargo test"}),
                status: ToolStatus::InProgress,
            },
        }));
    let chunk = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        b"OUTPUT_TAIL_SENTINEL\n",
    );
    model
        .projection
        .apply(&EventPayload::Item(ItemEvent::Delta {
            item_id: id.clone(),
            delta: ItemDelta::CommandOutput {
                stream: haider_protocol::item::OutputStream::Stdout,
                chunk_b64: chunk,
            },
        }));
    model
        .projection
        .apply(&EventPayload::Item(ItemEvent::Completed {
            item_id: id,
            item: TurnItem::ToolCall {
                call_id: "call-proc".into(),
                name: "process_exec".into(),
                args: serde_json::json!({"command": "cargo test"}),
                status: ToolStatus::Completed,
            },
        }));
    let (rows, _) = draw(&model, 118, 40);
    assert!(
        rows.iter().any(|row| row.contains("OUTPUT_TAIL_SENTINEL")),
        "the retained output tail is visible under the tool row"
    );
}

/// MUTATION CHECK: fabricate inventory rows while the read is in flight.
/// Expected runtime failure: the fetching-state assertion below.
#[test]
fn the_tools_screen_renders_committed_snapshot_or_says_fetching() {
    let mut model = session_model();
    model.screen = Screen::Tools;
    model.tools_inventory = None;
    let (rows, _) = draw(&model, 118, 40);
    assert!(
        rows.iter().any(|row| row.contains("fetching")),
        "in-flight read says so — nothing fabricated"
    );
    model.tools_inventory = Some(ToolInventorySnapshot {
        tools: vec![ToolInventoryEntry {
            manifest: ToolManifest {
                name: "process_exec".into(),
                description: "run one supervised shell command".into(),
                effects: vec![haider_protocol::effect::EffectClass::ProcessExec],
                dispatch: haider_protocol::tool::DispatchMode::Await,
                input_schema: serde_json::json!({}),
            },
            default: ToolPermissionDefault::Ask,
        }],
        remembered_grants: Vec::new(),
    });
    let (rows, _) = draw(&model, 118, 40);
    assert!(
        rows.iter()
            .any(|row| row.contains("process_exec") && row.contains("default ask")),
        "the committed snapshot renders name + default"
    );
    assert!(
        rows.iter().any(|row| row.contains("not a sandbox")),
        "honest containment copy (research risk 2)"
    );
    // esc returns to the session.
    model.handle(key(KeyCode::Esc));
    assert_eq!(model.screen, Screen::Session);
}

/// MUTATION CHECK: let demo mode route `!` to the live escape. Expected
/// runtime failure: the flash assertion below (demo has no daemon; the
/// six bare VFS commands remain its only shell).
#[test]
fn demo_bang_flashes_and_stays_out_of_the_vfs() {
    let mut model = session_model();
    for c in "!ls".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|flash| flash.contains("live shell escape")),
        "got {:?}",
        model.flash
    );
    assert!(
        !model
            .projection
            .entries()
            .iter()
            .any(|entry| matches!(entry, haider_tui::projection::TranscriptEntry::Shell { .. })),
        "no fake VFS row for a ! line"
    );
}
