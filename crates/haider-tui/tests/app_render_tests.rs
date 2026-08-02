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

mod common;
use common::key;

fn draw(model: &AppModel, width: u16, height: u16) -> (String, Terminal<TestBackend>) {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            render(model, frame);
        })
        .expect("draw");
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
    // TUI4 item 4: the mark is half-block art wherever the frame can hold it
    // whole; the Arabic TEXT mark is the narrow-frame tier.
    assert!(
        text.contains(&haider_tui::mark::banner_rows()[2]),
        "the banner's baseline row"
    );
    assert!(text.contains("H A I D E R"));
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
        "sanctum line, Arabic default:\n{text}"
    );
    assert!(text.contains("the lion"));
    assert!(text.contains("provider"));
    assert!(text.contains("anthropic"));
    assert!(text.contains("recent sessions"));
    assert!(text.contains("billing-service"), "sim seed rows present");
    assert!(text.contains("Stripe webhooks + invoice backfill"));
    assert!(text.contains("cellular-pool-fix"));
    assert!(
        text.contains("start a session — describe the task"),
        "composer placeholder"
    );
    assert!(text.contains("◉ talk"), "talk chip");
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
    assert!(text.contains("H A I D E"), "the rest still renders");
    // At 24×20 the block is taller than the frame, so the centered helper
    // sheds from the top — the banner is never drawn here (it needs 32 cols)
    // and the one-line mark is among the rows that yield. The dignity law is
    // upheld either way: what renders is whole.
    assert!(
        !text.contains(&haider_tui::mark::banner_rows()[2]),
        "no banner row at a frame too narrow for it"
    );
}

#[test]
fn session_screen_shows_transcript_and_meter() {
    let model = model_after_full_demo();
    assert_eq!(model.screen, Screen::Session);
    let (text, _) = draw(&model, 100, 30);
    assert!(text.contains("❯ fix the failing boundary test"));
    assert!(text.contains("✓ fs_read"), "tool row: status glyph + name");
    // File changes render as the sim's fs_patch tool-row shape (G30).
    assert!(text.contains("✓ fs_patch crates/haider-store/src/event_store.rs +4 −1"));
    assert!(text.contains("IDLE"));
    assert!(text.contains("17% of 200k"));
    assert!(text.contains("fable-5 · anthropic"));
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
    // Small pastes KEEP their newlines (review r2 P2-4d: real multi-line
    // composer) — and still never submit.
    model.handle(AppEvent::Paste("a\nb\r\nc".to_owned().into()));
    assert_eq!(model.composer, "ha\nb\nc", "pasted newlines never submit");
    model.handle(key(KeyCode::Enter));
    assert_eq!(model.composer, "");

    assert_eq!(model.screen, Screen::Session);
    // Esc mid-turn INTERRUPTS and stays; OWNER DIRECTIVE: esc is
    // SESSION-SCOPED — the idle esc also stays (a hint names the real
    // back affordances); ⌃C remains the keyboard walk-back.
    model.handle(key(KeyCode::Esc));
    assert_eq!(model.screen, Screen::Session, "mid-turn esc interrupts");
    assert!(model.projection.interrupted(), "idle (i) marker set");
    model.handle(key(KeyCode::Esc));
    assert_eq!(model.screen, Screen::Session, "idle esc never navigates");
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|flash| flash.contains("← main")),
        "the hint names the back affordance"
    );
    model.handle(ctrl('c'));
    assert_eq!(model.screen, Screen::Launcher);
    // TUI4c (directed): this transcript came from RAW envelopes — a
    // scratch surface with no session id — so leaving discarded it and
    // empty ⏎ has nothing to re-attach (the law now works by id:
    // `tui4c_session_map_tests` pins it for real typed sessions).
    model.handle(key(KeyCode::Enter));
    assert_eq!(model.screen, Screen::Launcher, "no scratch to re-attach");

    // TUI4b item 10 (directed): ⌃C at the launcher quits; the
    // session-side navigation half lives in tui4b_interaction_tests.
    assert!(!model.should_quit);
    model.handle(ctrl('c'));
    assert!(model.should_quit, "launcher ⌃C quits");
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
    model.handle(AppEvent::Paste("sneaky text".to_owned().into()));
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
    // The header dir is the session's shell working dir (TUI3b §4 — the
    // model default matches the sim launcher dir).
    let model = model_after_full_demo();
    let (text, _) = draw(&model, 100, 30);
    // Header: the 2-row mark spans both header lines (TUI4 items 4+6).
    assert!(text.contains(&haider_tui::mark::header_rows()[0]));
    assert!(text.contains("← main"), "back chip present");
    assert!(text.contains("~/dev/enterprise-suite"));
    assert!(text.contains("← main"));
    assert!(
        text.contains("fix the failing boundary test in haid"),
        "the first user prompt is in the transcript (the header shows the slug NAME since TUI3b)"
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

#[test]
fn slash_palette_filters_completes_and_runs() {
    let mut model = AppModel::new();
    model.handle(AppEvent::Envelope(Box::new(EventPayload::HarnessStatus(
        HarnessStatus::Ready,
    ))));
    for c in "/th".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    assert!(model.palette_open());
    let items = model.palette_items();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].label(), "/theme");
    let (text, _) = draw(&model, 100, 34);
    assert!(text.contains("/theme"), "palette row visible");
    // Sim CmdMenu: the key hint sits at the BOTTOM of the palette.
    assert!(text.contains("↑↓ options · tab complete · ⏎ run · esc dismiss"));

    model.handle(key(KeyCode::Tab));
    assert_eq!(model.composer, "/theme ");
    for c in "dark".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    assert_eq!(model.theme, ThemeKey::Dark, "/theme dark executed");
    assert!(model.composer.is_empty());
}

#[test]
fn stub_commands_flash_honestly_and_help_opens() {
    let mut model = AppModel::new();
    model.handle(AppEvent::Envelope(Box::new(EventPayload::HarnessStatus(
        HarnessStatus::Ready,
    ))));
    for c in "/fork".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    assert!(
        model.flash.as_deref().unwrap_or("").contains("lands with"),
        "stubs name their wave"
    );

    for c in "/help".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    assert!(model.help_open);
    let (text, _) = draw(&model, 100, 34);
    assert!(text.contains("/queue <steer|turn>"), "help panel body");
    model.handle(key(KeyCode::Esc));
    assert!(!model.help_open);
}

#[test]
fn typed_text_starts_a_session_and_requests_a_turn() {
    use haider_tui::app::AppRequest;
    let mut model = AppModel::new();
    model.handle(AppEvent::Envelope(Box::new(EventPayload::HarnessStatus(
        HarnessStatus::Ready,
    ))));
    for c in "refactor the parser".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    assert_eq!(model.screen, Screen::Session);
    // TUI4c (directed): `new_session` cancels NOTHING — the session left
    // behind keeps running (sim tui.js:1617-1650); only the submit itself
    // is requested. The micro-call still names the session 1.5 s later.
    assert_eq!(
        model.requests,
        vec![AppRequest::SubmitText {
            text: "refactor the parser".to_owned(),
            voice: false,
            title: true,
            branch: None,
            attachments: vec![],
        }]
    );
    // Sim autoBlurb (G47) is the BLURB, applied by the 1.5 s callback; the
    // header + window title show the session's slug NAME right away (sim
    // tui.js:2014-2016, TUI3b §4 step 7).
    assert_eq!(model.session_title, None, "not titled until the callback");
    assert_eq!(model.session_name.as_deref(), Some("refactor-the-parser"));
    assert_eq!(
        model.window_title(),
        format!(
            "haider — refactor-the-parser · {}",
            haider_tui::app::local_device_name()
        )
    );
}

#[test]
fn digit_attaches_a_sample_and_autoplay_is_one_shot() {
    use haider_tui::app::AppRequest;
    let mut model = AppModel::new();
    model.handle(AppEvent::Envelope(Box::new(EventPayload::HarnessStatus(
        HarnessStatus::Ready,
    ))));
    model.handle(key(KeyCode::Char('2')));
    // TUI4c (directed): attaching asks for NOTHING — no canned turn (item
    // 1) and no teardown either: the sim's `openSession` (tui.js:1606)
    // never cancels; the previous session keeps running in its slot.
    assert_eq!(model.requests, vec![]);
    let _ = AppRequest::Quit;
    assert_eq!(model.session_head, ("Fatima".to_owned(), "(a)".to_owned()));
    assert!(!model.turn_active, "no turn was started");
    assert_eq!(model.screen, Screen::Session);
}

#[test]
fn status_bar_has_boxed_chips_and_hint() {
    let mut model = AppModel::new();
    model.handle(AppEvent::Envelope(Box::new(EventPayload::HarnessStatus(
        HarnessStatus::Ready,
    ))));
    let (text, _) = draw(&model, 118, 34);
    assert!(text.contains("[ IDLE ]"), "boxed state chip");
    assert!(text.contains("fable-5 · anthropic"));
    assert!(text.contains("[ ◉ voice · whisper→openai ]"), "voice chip");
    assert!(text.contains("/help · theme desert dawn"), "launcher hint");
}

// ---- TUI2 review r1 + mouse regressions ----

fn draw_with_hits(
    model: &AppModel,
    width: u16,
    height: u16,
) -> (String, Vec<(ratatui::layout::Rect, haider_tui::app::Hit)>) {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut hits = Vec::new();
    terminal
        .draw(|frame| hits = render(model, frame))
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let mut text = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            text.push_str(buffer[(x, y)].symbol());
        }
        text.push('\n');
    }
    (text, hits)
}

#[test]
fn one_turn_at_a_time_across_digits_and_submits() {
    use haider_tui::app::AppRequest;
    let mut model = AppModel::new();
    model.handle(AppEvent::Envelope(Box::new(EventPayload::HarnessStatus(
        HarnessStatus::Ready,
    ))));
    model.handle(key(KeyCode::Char('1')));
    // TUI4c (directed): attaching requests nothing and starts no turn.
    assert_eq!(model.requests.len(), 0);
    assert!(!model.turn_active, "attach starts no turn");
    // A second digit attaches the OTHER seeded session — there is no turn to
    // conflict with any more (TUI4 item 1).
    // A digit inside a session is ordinary text, not another attach.
    model.handle(key(KeyCode::Char('2')));
    assert_eq!(model.session_name.as_deref(), Some("billing-service"));
    assert!(!model.turn_active);
    let _ = AppRequest::Quit;
}

#[test]
fn same_length_prompts_produce_distinct_turn_items() {
    use haider_tui::script::{Beat, respond_beats};
    let counter = std::sync::atomic::AtomicU64::new;
    let mut projection = haider_tui::projection::SessionProjection::new();
    let (generic, roster) = (counter(0), counter(3));
    for (text, turn) in [("aaa", 1_u64), ("bbb", 2)] {
        let beats = respond_beats(
            text,
            false,
            haider_protocol::DeliveryMode::Steer,
            turn,
            &generic,
            &roster,
        );
        for beat in beats {
            if let Beat::Emit(payload) = beat {
                projection.apply(&payload);
            }
        }
    }
    assert_eq!(projection.duplicate_items(), 0, "per-turn id namespace");
    assert_eq!(projection.orphan_deltas(), 0);
    let users = projection
        .entries()
        .iter()
        .filter(|entry| matches!(entry, haider_tui::projection::TranscriptEntry::User { .. }))
        .count();
    assert_eq!(users, 2, "both turns landed");
}

#[test]
fn window_title_strips_control_characters() {
    // The window title comes from the session NAME; the slug law already
    // filters, but the OSC seam must strip control characters for ANY
    // name source (review r1 P1).
    let mut model = AppModel::new();
    model.screen = Screen::Session;
    model.session_name = Some("evil\u{1b}]2;pwned\u{7}title".to_owned());
    let title = model.window_title();
    assert!(!title.contains('\u{1b}'), "no ESC in OSC titles");
    assert!(!title.contains('\u{7}'), "no BEL in OSC titles");
    assert!(
        title.contains("pwned") || title.contains("evil"),
        "text kept"
    );
}

#[test]
fn clear_starts_fresh_and_launcher_typing_never_leaks_old_turns() {
    let mut model = model_after_full_demo();
    assert!(!model.projection.entries().is_empty());
    for c in "/clear".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    assert_eq!(model.screen, Screen::Launcher);
    assert!(model.projection.entries().is_empty(), "fresh projection");
    assert!(model.session_title.is_none());
}

#[test]
fn palette_enter_runs_the_highlighted_command() {
    let mut model = AppModel::new();
    model.handle(AppEvent::Envelope(Box::new(EventPayload::HarnessStatus(
        HarnessStatus::Ready,
    ))));
    // Enter a session so session-only commands are offered.
    for c in "hello".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    for c in "/t".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    // matches: theme, tree, tokens, tools — Down selects the second.
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Enter));
    assert_eq!(
        model.screen,
        Screen::Tree,
        "highlighted command ran, not the raw query: {:?}",
        model.flash
    );
}

#[test]
fn invalid_theme_and_unknown_commands_are_told_apart() {
    let mut model = AppModel::new();
    model.handle(AppEvent::Envelope(Box::new(EventPayload::HarnessStatus(
        HarnessStatus::Ready,
    ))));
    let before = model.theme;
    for c in "/theme sepia".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    assert_eq!(model.theme, before, "invalid theme never cycles");
    assert!(
        model
            .flash
            .as_deref()
            .unwrap_or("")
            .contains("unknown theme")
    );

    for c in "/frobnicate".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    assert!(
        model
            .flash
            .as_deref()
            .unwrap_or("")
            .contains("unknown command"),
        "typos are not presented as planned features"
    );
}

#[test]
fn boot_swallows_ordinary_input() {
    let mut model = AppModel::new();
    assert_eq!(model.screen, Screen::Boot);
    for c in "sneaky".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    assert_eq!(model.composer, "");
    assert!(model.requests.is_empty(), "boot cannot start turns");
}

#[test]
fn short_launcher_keeps_the_composer_visible() {
    let mut model = AppModel::new();
    model.handle(AppEvent::Envelope(Box::new(EventPayload::HarnessStatus(
        HarnessStatus::Ready,
    ))));
    let (text, _) = draw(&model, 90, 10);
    assert!(
        text.contains("start a session"),
        "tail-visible centering keeps the input on screen"
    );
}

#[test]
fn clicks_attach_sessions_and_wheel_scrolls() {
    use haider_tui::app::Hit;
    let mut model = AppModel::new();
    model.handle(AppEvent::Envelope(Box::new(EventPayload::HarnessStatus(
        HarnessStatus::Ready,
    ))));
    let (_, hits) = draw_with_hits(&model, 118, 34);
    let billing = common::session_named(&model, "billing-service");
    assert!(
        hits.iter()
            .any(|(_, h)| *h == Hit::AttachSession(billing.clone())),
        "sample rows are clickable"
    );
    assert!(hits.iter().any(|(_, h)| *h == Hit::TalkChip));
    assert!(hits.iter().any(|(_, h)| *h == Hit::HelpHint));

    common::hit_session_named(&mut model, "l1-remote-projects");
    // TUI4 item 1: attaching opens the SEEDED session and starts no turn.
    assert!(!model.turn_active);
    assert_eq!(model.screen, Screen::Session);
    assert_eq!(model.session_head, ("Ali".to_owned(), "(a)".to_owned()));

    // The script's UserMessage envelope flips to the session view.
    model.handle(AppEvent::Envelope(Box::new(EventPayload::UserMessage {
        text: "L1 remote-projects contract read".to_owned(),
        attachments: vec![],
        mode: haider_protocol::DeliveryMode::Steer,
    })));
    assert_eq!(model.screen, Screen::Session);

    // Wheel scroll-back only in session — reconcile-then-apply (review r5
    // P2-2): every notch clamps to the last known truth, so a short
    // transcript never scrolls, even before any redraw.
    model.handle_wheel(true);
    assert_eq!(model.scroll_back.get(), 0, "no debt, even pre-frame");
    let (_, _) = draw_with_hits(&model, 118, 34);
    assert_eq!(model.scroll_max.get(), 0, "everything visible → max 0");
    model.handle_wheel(true);
    model.handle_wheel(false);
    assert_eq!(model.scroll_back.get(), 0);
    model.handle_hit(Hit::BackChip);
    assert_eq!(model.screen, Screen::Launcher);
}
