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

/// CU-2: an in-flight screen-CONTROL action raises the sacred banner and
/// renders the action readably; a passive screenshot does neither.
/// MUTATION CHECK: treat screenshot as control, or drop the banner. Expected
/// runtime failure: the banner presence/absence assertions below.
#[test]
fn computer_control_raises_the_sacred_banner_and_screenshot_does_not() {
    // A control action mid-flight: banner + readable description.
    let mut model = session_model();
    model
        .projection
        .apply(&EventPayload::Item(ItemEvent::Started {
            item_id: ItemId::new("cu-1"),
            item: TurnItem::ToolCall {
                call_id: "c1".into(),
                name: "computer".into(),
                args: serde_json::json!({"action": "left_click", "x": 840, "y": 220}),
                status: ToolStatus::InProgress,
            },
        }));
    let (rows, _) = draw(&model, 118, 40);
    assert!(
        rows.iter()
            .any(|r| r.contains("controlling your screen") && r.contains("esc to stop")),
        "an in-flight click raises the sacred control banner"
    );
    assert!(
        rows.iter()
            .any(|r| r.contains("computer") && r.contains("left_click (840, 220)")),
        "the action row reads the exact click target"
    );

    // A passive screenshot mid-flight: NO banner (observation is not control).
    let mut obs = session_model();
    obs.projection
        .apply(&EventPayload::Item(ItemEvent::Started {
            item_id: ItemId::new("cu-2"),
            item: TurnItem::ToolCall {
                call_id: "c2".into(),
                name: "computer".into(),
                args: serde_json::json!({"action": "screenshot"}),
                status: ToolStatus::InProgress,
            },
        }));
    let (rows, _) = draw(&obs, 118, 40);
    assert!(
        !rows.iter().any(|r| r.contains("controlling your screen")),
        "a screenshot observes, it does not control — no banner"
    );
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

/// MUTATION CHECK: render a live `!` draft with the ordinary `❯` sigil, or
/// leak the `$` glyph into demo mode. Expected runtime failure: the sigil
/// assertions below — command mode promises the exact `$` row the
/// transcript will commit, and only where the escape actually runs.
#[test]
fn live_bang_draft_flips_the_sigil_to_the_command_glyph() {
    let mut model = session_model();
    model.mode = haider_tui::app::RuntimeMode::Live;
    model.handle(key(KeyCode::Char('!')));
    let (rows, _) = draw(&model, 118, 40);
    assert!(
        rows.iter()
            .any(|row| row.contains("$ !") && row.contains("workspace shell · ⏎ run")),
        "bare ! shows the $ sigil and says where it runs"
    );
    for c in "cargo test".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    let (rows, _) = draw(&model, 118, 40);
    assert!(
        rows.iter().any(|row| row.contains("$ !cargo test")),
        "the command draft keeps the $ sigil"
    );
    assert!(
        !rows
            .iter()
            .any(|row| row.contains("workspace shell · ⏎ run")),
        "the hint yields once the command exists"
    );
    // Demo keeps the ordinary prompt — the escape only flashes there.
    let mut demo = session_model();
    for c in "!ls".chars() {
        demo.handle(key(KeyCode::Char(c)));
    }
    let (rows, _) = draw(&demo, 118, 40);
    assert!(
        rows.iter().any(|row| row.contains("❯ !ls")),
        "demo drafts keep ❯"
    );
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
