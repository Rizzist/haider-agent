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
}
