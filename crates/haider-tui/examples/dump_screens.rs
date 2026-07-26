//! Dev tool: dump each screen as plain text at a fixed size for visual
//! review without a live terminal. `cargo run -p haider-tui --example
//! dump_screens`.
#![allow(clippy::expect_used)]

use haider_tui::app::{AppEvent, AppModel};
use haider_tui::mock::demo_script;
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn dump_at(model: &AppModel, label: &str, width: u16, height: u16) {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| {
            render(model, frame);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    println!("──── {label} ────");
    for y in 0..buffer.area.height {
        let mut line = String::new();
        for x in 0..buffer.area.width {
            line.push_str(buffer[(x, y)].symbol());
        }
        println!("{}", line.trim_end());
    }
    println!();
}

fn dump(model: &AppModel, label: &str) {
    dump_at(model, label, 118, 34);
}

fn main() {
    let mut model = AppModel::new();
    let script = demo_script();
    // Boot mid-checks.
    for payload in script.iter().take(3) {
        model.handle(AppEvent::Envelope(Box::new(payload.clone())));
    }
    dump(&model, "boot");
    // Launcher.
    for payload in script.iter().skip(3).take(3) {
        model.handle(AppEvent::Envelope(Box::new(payload.clone())));
    }
    dump(&model, "launcher");
    // Palette open.
    model.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Char('/'),
        KeyModifiers::NONE,
    )));
    dump(&model, "launcher + palette");
    for _ in 0..2 {
        model.handle(AppEvent::Key(KeyEvent::new(
            KeyCode::Backspace,
            KeyModifiers::NONE,
        )));
    }
    // Blocking menu (a separate model: the demo turn up to, not including,
    // its self-answer — the main model must not see events twice).
    let mut menu_model = AppModel::new();
    for payload in &script {
        if matches!(payload, haider_protocol::EventPayload::MenuAnswered(_)) {
            break;
        }
        menu_model.handle(AppEvent::Envelope(Box::new(payload.clone())));
    }
    dump(&menu_model, "session + blocking menu");
    // Sacred options at short heights (review r3 P2-1b): hint + body shed,
    // options never.
    dump_at(&menu_model, "session + blocking menu @ 90×10", 90, 10);
    // Chrome yields to the blocking card below 90×7 (review r5 P2-1):
    // status row + session line shed, both options intact.
    dump_at(&menu_model, "session + blocking menu @ 90×5", 90, 5);
    // The menu-close transition (review r6 P2-1): the composer inherits
    // the ladder — the answered card gives way to an EDITABLE composer.
    let mut answered_model = menu_model;
    answered_model.handle(AppEvent::Envelope(Box::new(
        haider_protocol::EventPayload::MenuAnswered(haider_protocol::menu::MenuAnswer {
            menu: haider_protocol::ids::MenuId::new("t0-menu-1"),
            option_key: Some("allow".to_owned()),
            option_index: 0,
            value: None,
            via: haider_protocol::menu::AnswerVia::Tui,
        }),
    )));
    dump_at(&answered_model, "session @ 90×5 · menu answered", 90, 5);
    // Full session.
    for payload in script.iter().skip(6) {
        model.handle(AppEvent::Envelope(Box::new(payload.clone())));
    }
    dump(&model, "session (end of demo)");
    // Session palette (session-only commands included) — the ghost
    // completion trails the cursor.
    for c in "/t".chars() {
        model.handle(AppEvent::Key(KeyEvent::new(
            KeyCode::Char(c),
            KeyModifiers::NONE,
        )));
    }
    dump(&model, "session + palette");
    // /theme argument slot (G12 slice).
    for c in "heme ".chars() {
        model.handle(AppEvent::Key(KeyEvent::new(
            KeyCode::Char(c),
            KeyModifiers::NONE,
        )));
    }
    dump(&model, "session + theme args");
    // Multi-line composer (⇧⏎/⌥⏎ newlines, review r2 P2-4).
    for _ in 0..7 {
        model.handle(AppEvent::Key(KeyEvent::new(
            KeyCode::Backspace,
            KeyModifiers::NONE,
        )));
    }
    for (index, line) in [
        "draft the migration plan",
        "then apply it to staging",
        "and verify",
    ]
    .iter()
    .enumerate()
    {
        if index > 0 {
            model.handle(AppEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::ALT,
            )));
        }
        for c in line.chars() {
            model.handle(AppEvent::Key(KeyEvent::new(
                KeyCode::Char(c),
                KeyModifiers::NONE,
            )));
        }
    }
    dump(&model, "session + multi-line composer");

    // ---- TUI3b turn-engine frames ----
    let ready =
        haider_protocol::EventPayload::HarnessStatus(haider_protocol::state::HarnessStatus::Ready);
    let submit = |model: &mut AppModel, text: &str| {
        for c in text.chars() {
            model.handle(AppEvent::Key(KeyEvent::new(
                KeyCode::Char(c),
                KeyModifiers::NONE,
            )));
        }
        model.handle(AppEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
    };

    // Todos pinned mid-chain: the plan-todo branch's beats applied up to
    // the second completed work tool.
    let mut todos_model = AppModel::new();
    todos_model.handle(AppEvent::Envelope(Box::new(ready.clone())));
    submit(&mut todos_model, "plan todo the harness work");
    todos_model.requests.clear();
    let (mut generic, mut roster) = (0, 3);
    let beats = haider_tui::script::respond_beats(
        "plan todo the harness work",
        false,
        haider_protocol::DeliveryMode::Steer,
        1,
        &mut generic,
        &mut roster,
    );
    let mut tools_done = 0;
    for beat in &beats {
        if let haider_tui::script::Beat::Emit(payload) = beat {
            todos_model.handle(AppEvent::Envelope(Box::new(payload.clone())));
            if matches!(
                payload,
                haider_protocol::EventPayload::Item(haider_protocol::item::ItemEvent::Completed {
                    item: haider_protocol::item::TurnItem::ToolCall { .. },
                    ..
                })
            ) {
                tools_done += 1;
                if tools_done == 2 {
                    break;
                }
            }
        }
    }
    dump(&todos_model, "session + todos pinned (dep chain)");

    // The ⧗ queue panel between the todos and the composer.
    todos_model.queue_mode = true;
    todos_model
        .msg_queue
        .push("and then re-run the whole suite".to_owned());
    todos_model
        .msg_queue
        .push("finally draft the release notes".to_owned());
    dump(&todos_model, "session + ⧗ queue panel (q:turn)");
    // The ledger at 90×10: todos shed first, the panel holds if it fits.
    todos_model.msg_queue.truncate(1);
    dump_at(&todos_model, "session + ⧗ queue @ 90×10", 90, 10);

    // Shell rows, a voice turn and the compaction numbers row.
    let mut engine_model = AppModel::new();
    engine_model.handle(AppEvent::Envelope(Box::new(ready.clone())));
    submit(&mut engine_model, "walk the harness with me");
    engine_model.requests.clear();
    engine_model.turn_active = false;
    submit(&mut engine_model, "cd web");
    submit(&mut engine_model, "ls");
    engine_model
        .projection
        .push_user_voice("walk me through the harness entrypoints".to_owned());
    engine_model
        .projection
        .push_note("◉ heard · whisper-large-v3".to_owned());
    engine_model.projection.set_voice_live(true);
    engine_model.handle(AppEvent::Envelope(Box::new(
        haider_protocol::EventPayload::Item(haider_protocol::item::ItemEvent::Completed {
            item_id: haider_protocol::ids::ItemId::new("spoken-1"),
            item: haider_protocol::item::TurnItem::AgentMessage {
                text: "Starting at the run loop — the harness owns every state write.".to_owned(),
            },
        }),
    )));
    engine_model.projection.set_voice_live(false);
    engine_model.projection.push_note(
        "· context at 85% — compacting (dead branches first, live path last)".to_owned(),
    );
    engine_model.handle(AppEvent::Envelope(Box::new(
        haider_protocol::EventPayload::Item(haider_protocol::item::ItemEvent::Completed {
            item_id: haider_protocol::ids::ItemId::new("compact-demo"),
            item: haider_protocol::item::TurnItem::ContextCompaction {
                summary_artifact: haider_protocol::ids::ArtifactRef::new("blake3:demo"),
                tokens_before: Some(170_000),
                tokens_after: Some(12_000),
            },
        }),
    )));
    engine_model.turn_active = false;
    dump(&engine_model, "session — shell · voice · ⊟ compaction rows");

    // The /voice and /tools command cards (◉/⚒ glyphs via origin).
    submit(&mut engine_model, "/voice");
    dump(&engine_model, "session + /voice card");
    engine_model.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Esc,
        KeyModifiers::NONE,
    )));
    submit(&mut engine_model, "/tools");
    dump(&engine_model, "session + /tools card");

    // The launcher .shellout block under the recent list.
    let mut shell_launcher = AppModel::new();
    shell_launcher.handle(AppEvent::Envelope(Box::new(ready)));
    submit(&mut shell_launcher, "ls");
    dump(&shell_launcher, "launcher + shellout");
}
