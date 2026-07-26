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

fn dump(model: &AppModel, label: &str) {
    let backend = TestBackend::new(118, 34);
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
}
