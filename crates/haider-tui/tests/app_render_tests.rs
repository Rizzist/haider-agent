//! Headless screen snapshots (TestBackend) + reducer behavior. These are the
//! sim-parity checks: each screen must show its signature elements in every
//! theme, and the dignity rules must hold under narrow widths.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::state::HarnessStatus;
use haider_tui::app::{AppEvent, AppModel, Screen};
use haider_tui::mock::demo_script;
use haider_tui::render::render;
use haider_tui::runtime::run_demo_plain;
use haider_tui::sanctum::SHAHADA_ARABIC;
use haider_tui::theme::ThemeKey;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn draw(model: &AppModel, width: u16, height: u16) -> (String, Terminal<TestBackend>) {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal.draw(|frame| render(model, frame)).expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let mut text = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            text.push_str(buffer[(x, y)].symbol());
        }
        text.push('\n');
    }
    (text, terminal)
}

fn key(code: KeyCode) -> AppEvent {
    AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn ctrl(c: char) -> AppEvent {
    AppEvent::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
}

fn model_at_boot_mid_checks() -> AppModel {
    let mut model = AppModel::new();
    for payload in demo_script().into_iter().take(3) {
        model.handle(AppEvent::Envelope(Box::new(payload)));
    }
    model
}

fn model_after_full_demo() -> AppModel {
    let mut model = AppModel::new();
    for payload in demo_script() {
        model.handle(AppEvent::Envelope(Box::new(payload)));
    }
    model
}

#[test]
fn boot_screen_shows_mark_word_and_progressing_checks() {
    let model = model_at_boot_mid_checks();
    assert_eq!(model.screen, Screen::Boot);
    let (text, _) = draw(&model, 80, 24);
    assert!(text.contains("حيدر"));
    assert!(text.contains("HAIDER CODE"));
    assert!(text.contains("· starting up"));
    assert!(text.contains("✓ store open · journal replayed"));
    assert!(text.contains("◌"), "current check marker");
    assert!(text.contains("◌ STARTING"), "status badge");
}

#[test]
fn launcher_shows_sanctum_identity_and_composer() {
    let mut model = AppModel::new();
    model.handle(AppEvent::Envelope(Box::new(EventPayload::HarnessStatus(
        HarnessStatus::Ready,
    ))));
    assert_eq!(model.screen, Screen::Launcher);
    let (text, _) = draw(&model, 80, 24);
    assert!(
        text.contains(SHAHADA_ARABIC),
        "sanctum line, Arabic default"
    );
    assert!(text.contains("the lion"));
    assert!(text.contains("provider"));
    assert!(text.contains("anthropic"));
    assert!(text.contains("no sessions yet"));
    assert!(text.contains("❯"));
}

#[test]
fn narrow_launcher_omits_the_sanctum_whole() {
    let mut model = AppModel::new();
    model.handle(AppEvent::Envelope(Box::new(EventPayload::HarnessStatus(
        HarnessStatus::Ready,
    ))));
    let (text, _) = draw(&model, 24, 20);
    // Dignity rule: at 24 columns the shahada cannot fit whole, so NO part
    // of it may appear — never truncated, never ellipsized.
    for word in ["الله", "محمد", "رسول"] {
        assert!(
            !text.contains(word),
            "sanctum fragment leaked into narrow frame"
        );
    }
    assert!(text.contains("HAIDER CODE"), "the rest still renders");
}

#[test]
fn session_screen_shows_transcript_and_meter() {
    let model = model_after_full_demo();
    assert_eq!(model.screen, Screen::Session);
    let (text, _) = draw(&model, 100, 30);
    assert!(text.contains("❯ fix the failing boundary test"));
    assert!(text.contains("⚒ fs_read"));
    assert!(text.contains("± crates/haider-store/src/event_store.rs"));
    assert!(text.contains("IDLE"));
    assert!(text.contains("17% of 200k"));
    assert!(text.contains("claude-fable-5 · anthropic"));
}

#[test]
fn theme_cycle_changes_the_ground_color() {
    let mut model = model_after_full_demo();
    assert_eq!(model.theme, ThemeKey::Dawn);
    let (_, terminal) = draw(&model, 40, 12);
    let dawn_bg = terminal.backend().buffer()[(0, 0)].bg;

    model.handle(ctrl('t'));
    assert_eq!(model.theme, ThemeKey::Ivory);
    model.handle(ctrl('t'));
    assert_eq!(model.theme, ThemeKey::Dark);
    let (_, terminal) = draw(&model, 40, 12);
    let dark_bg = terminal.backend().buffer()[(0, 0)].bg;
    assert_ne!(dawn_bg, dark_bg, "theme actually re-grounds the frame");

    model.handle(ctrl('t'));
    assert_eq!(model.theme, ThemeKey::Dawn, "cycle wraps");
}

#[test]
fn reducer_handles_quit_composer_and_navigation() {
    let mut model = model_after_full_demo();
    model.handle(key(KeyCode::Char('h')));
    model.handle(key(KeyCode::Char('i')));
    assert_eq!(model.composer, "hi");
    model.handle(key(KeyCode::Backspace));
    assert_eq!(model.composer, "h");
    model.handle(AppEvent::Paste("a\nb\r\nc".to_owned()));
    assert_eq!(model.composer, "ha b c", "pasted newlines never submit");
    model.handle(key(KeyCode::Enter));
    assert_eq!(model.composer, "");

    assert_eq!(model.screen, Screen::Session);
    model.handle(key(KeyCode::Esc));
    assert_eq!(model.screen, Screen::Launcher);
    model.handle(key(KeyCode::Enter));
    assert_eq!(model.screen, Screen::Session, "enter re-attaches");

    assert!(!model.should_quit);
    model.handle(ctrl('c'));
    assert!(model.should_quit);
}

#[test]
fn demo_plain_path_is_deterministic_and_complete() {
    let first = run_demo_plain(AppModel::new());
    let second = run_demo_plain(AppModel::new());
    assert_eq!(first, second);
    assert!(first.contains("❯ fix the failing boundary test in haider-store"));
    assert!(first.contains("✓ plan — 3/3 done"));
    assert!(first.lines().last().expect("status").starts_with("IDLE"));
}

#[test]
fn open_menu_replaces_the_composer_and_answers_through_the_outbox() {
    use haider_tui::mock::demo_script;
    let mut model = AppModel::new();
    // Play the demo up to (and including) MenuOpened, but not the script's
    // self-answer.
    for payload in demo_script() {
        let is_answer = matches!(payload, EventPayload::MenuAnswered(_));
        if is_answer {
            break;
        }
        model.handle(AppEvent::Envelope(Box::new(payload)));
    }
    assert!(model.projection.open_menu().is_some());
    let (text, _) = draw(&model, 90, 26);
    assert!(text.contains("Allow fs_patch — event_store.rs?"));
    assert!(text.contains("1. Allow once"));
    assert!(text.contains("2. Deny"));
    assert!(text.contains("? PERMISSION_REQUIRED"));

    // Keys drive the menu, not the composer.
    model.handle(key(KeyCode::Char('x')));
    assert_eq!(
        model.composer, "",
        "composer is unmounted while menu is open"
    );
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Enter));
    let answer = model.outbox.pop().expect("answer produced");
    assert_eq!(answer.option_key.as_deref(), Some("deny"));
    assert_eq!(answer.option_index, 1);

    // Digit quick-answer path.
    model.handle(key(KeyCode::Char('1')));
    let answer = model.outbox.pop().expect("digit answer");
    assert_eq!(answer.option_key.as_deref(), Some("allow"));
}

#[test]
fn narrow_status_bar_keeps_the_badge_and_drops_the_meter() {
    let model = model_after_full_demo();
    let (text, _) = draw(&model, 30, 12);
    assert!(text.contains("IDLE"), "badge survives narrow widths");
    assert!(!text.contains('▰'), "meter yields before the badge clips");
}

#[test]
fn stream_ended_never_dirties_the_frame() {
    let mut model = model_after_full_demo();
    model.dirty = false;
    model.handle(AppEvent::StreamEnded);
    assert!(!model.dirty, "post-stream no-ops must not spin redraws");
}

#[test]
fn command_decode_error_is_visible_in_the_session_view() {
    use haider_protocol::ids::ItemId;
    use haider_protocol::item::{ItemDelta, ItemEvent, OutputStream, ToolStatus, TurnItem};
    let mut model = model_after_full_demo();
    model.handle(AppEvent::Envelope(Box::new(EventPayload::Item(
        ItemEvent::Started {
            item_id: ItemId::new("cmd-err"),
            item: TurnItem::CommandExecution {
                call_id: "c".to_owned(),
                command: "cat log.bin".to_owned(),
                status: ToolStatus::InProgress,
                exit_code: None,
            },
        },
    ))));
    model.handle(AppEvent::Envelope(Box::new(EventPayload::Item(
        ItemEvent::Delta {
            item_id: ItemId::new("cmd-err"),
            delta: ItemDelta::CommandOutput {
                stream: OutputStream::Stdout,
                chunk_b64: "*** not base64 ***".to_owned(),
            },
        },
    ))));
    let (text, _) = draw(&model, 90, 30);
    assert!(text.contains("⚠ some output could not be decoded"));
}

#[test]
fn paste_behind_a_blocking_menu_is_dropped() {
    use haider_tui::mock::demo_script;
    let mut model = AppModel::new();
    for payload in demo_script() {
        if matches!(payload, EventPayload::MenuAnswered(_)) {
            break;
        }
        model.handle(AppEvent::Envelope(Box::new(payload)));
    }
    assert!(model.projection.open_menu().is_some());
    model.handle(AppEvent::Paste("sneaky text".to_owned()));
    assert_eq!(
        model.composer, "",
        "paste has no target while the menu replaces the composer"
    );
}

#[test]
fn honesty_notices_survive_long_bottom_anchored_output() {
    use base64::Engine as _;
    use haider_protocol::ids::ItemId;
    use haider_protocol::item::{ItemDelta, ItemEvent, OutputStream, ToolStatus, TurnItem};
    let mut model = model_after_full_demo();
    model.handle(AppEvent::Envelope(Box::new(EventPayload::Item(
        ItemEvent::Started {
            item_id: ItemId::new("cmd-long"),
            item: TurnItem::CommandExecution {
                call_id: "c".to_owned(),
                command: "yes".to_owned(),
                status: ToolStatus::InProgress,
                exit_code: None,
            },
        },
    ))));
    // A multi-line tail far taller than the viewport, with the cap exceeded.
    let long = "line\n".repeat(4000);
    model.handle(AppEvent::Envelope(Box::new(EventPayload::Item(
        ItemEvent::Delta {
            item_id: ItemId::new("cmd-long"),
            delta: ItemDelta::CommandOutput {
                stream: OutputStream::Stdout,
                chunk_b64: base64::engine::general_purpose::STANDARD.encode(long.as_bytes()),
            },
        },
    ))));
    let (text, _) = draw(&model, 90, 24);
    assert!(
        text.contains("earlier output truncated"),
        "the truncation notice must be visible at the bottom-anchored viewport"
    );
}

#[test]
fn session_screen_has_header_and_composer_gap() {
    let mut model = model_after_full_demo();
    model.identity.dir = "~/dev/enterprise-suite".to_owned();
    let (text, _) = draw(&model, 100, 30);
    // Header: mark + product line + session line (owner ask: sim-parity header).
    assert!(text.contains("حيدر"));
    assert!(text.contains("· haider v"), "mark · product separator");
    assert!(text.contains("~/dev/enterprise-suite"));
    assert!(text.contains("← esc"));
    assert!(
        text.contains("fix the failing boundary test in haid"),
        "session title from first message"
    );
    // Agent blocks carry the ■ haider name header.
    assert!(text.contains("■ haider"));
    // Gap row: the line directly above the status bar is empty — the
    // composer must never sit ON the bottom bar (owner ask).
    let lines: Vec<&str> = text.lines().collect();
    let above_status = lines[lines.len() - 2];
    assert_eq!(
        above_status.trim(),
        "",
        "spacer row between composer and status bar"
    );
    assert!(lines[lines.len() - 1].contains("IDLE"));
}
