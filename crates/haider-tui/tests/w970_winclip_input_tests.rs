//! Windows copy/paste conventions through the production input reducer.
//! These tests execute on every host; native OS clipboard tests live separately.
#![allow(clippy::expect_used)]

use haider_protocol::{DeliveryMode, EventPayload};
use haider_tui::app::{AppEvent, AppModel, AppRequest, Pasted, Screen};
use haider_tui::clipboard::FakeClipboard;
use haider_tui::mock::demo_script;
use haider_tui::render::render;
use haider_tui::runtime::{
    clipboard_paste_effects, dispatch_input, rendered_selection_text, terminal_owned_mouse,
};
use haider_tui::select::selection_text;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Rect, Size};

mod common;

fn session() -> AppModel {
    let mut model = AppModel::new();
    for payload in demo_script() {
        model.handle(AppEvent::Envelope(Box::new(payload)));
    }
    model.requests.clear();
    model
}

fn mouse(kind: MouseEventKind, x: u16, y: u16, modifiers: KeyModifiers) -> Event {
    Event::Mouse(MouseEvent {
        kind,
        column: x,
        row: y,
        modifiers,
    })
}

fn draw(model: &AppModel, width: u16) -> (Buffer, Vec<(Rect, haider_tui::app::Hit)>) {
    let mut terminal = Terminal::new(TestBackend::new(width, 36)).expect("test terminal");
    let mut hits = Vec::new();
    terminal
        .draw(|frame| hits = render(model, frame))
        .expect("draw");
    (terminal.backend().buffer().clone(), hits)
}

fn row_text(buffer: &Buffer, y: u16) -> String {
    (0..buffer.area.width)
        .map(|x| buffer[(x, y)].symbol())
        .collect()
}

#[test]
fn transcript_drag_highlights_extends_copies_wrapped_rows_and_clears() {
    let mut model = session();
    let text = "winclip start alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron pi rho sigma tau winclip end";
    model.handle(AppEvent::Envelope(Box::new(EventPayload::UserMessage {
        text: text.to_owned(),
        attachments: vec![],
        mode: DeliveryMode::Steer,
    })));
    let (before, hits) = draw(&model, 50);
    let first = (0..before.area.height)
        .find(|&y| row_text(&before, y).contains("winclip start"))
        .expect("first wrapped row");
    let last = (first..before.area.height)
        .find(|&y| row_text(&before, y).contains("winclip end"))
        .expect("last wrapped row");
    assert!(
        last > first,
        "the production transcript must wrap this message"
    );
    for (kind, x, y) in [
        (MouseEventKind::Down(MouseButton::Left), 1, first),
        (MouseEventKind::Drag(MouseButton::Left), 20, first),
        (MouseEventKind::Drag(MouseButton::Left), 49, last),
    ] {
        dispatch_input(&mut model, &hits, mouse(kind, x, y, KeyModifiers::NONE));
    }
    let selected = model.selection.expect("transcript drag selection");
    assert_eq!(selected.anchor, (1, first));
    assert_eq!(selected.head, (49, last));
    assert!(selected.dragging);
    let (highlighted, _) = draw(&model, 50);
    assert_eq!(
        highlighted[(1, first)].bg,
        model.theme.theme().sel_bg.into()
    );
    assert_eq!(
        highlighted[(0, first + 1)].bg,
        model.theme.theme().sel_bg.into()
    );
    let extracted = rendered_selection_text(&model, Size::new(50, 36), &selected);
    assert_eq!(extracted, selection_text(&before, &selected));
    assert!(extracted.contains("winclip start"));
    assert!(extracted.contains("winclip end"));
    assert!(
        extracted.contains('\n'),
        "copy preserves rendered row boundaries"
    );
    dispatch_input(
        &mut model,
        &hits,
        mouse(
            MouseEventKind::Up(MouseButton::Left),
            49,
            last,
            KeyModifiers::NONE,
        ),
    );
    assert!(
        !model
            .selection
            .expect("highlight survives release")
            .dragging
    );
    assert!(matches!(
        model.requests.as_slice(),
        [AppRequest::CopySelection]
    ));
    dispatch_input(
        &mut model,
        &hits,
        Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
    );
    assert!(model.selection.is_none());
}

#[test]
fn ctrl_shift_c_copies_transcript_for_both_character_cases_without_navigation() {
    for character in ['c', 'C'] {
        let mut model = session();
        model.selection = Some(haider_tui::select::Selection {
            anchor: (1, 3),
            head: (12, 4),
            dragging: false,
        });
        dispatch_input(
            &mut model,
            &[],
            Event::Key(KeyEvent::new(
                KeyCode::Char(character),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            )),
        );
        assert!(matches!(
            model.requests.as_slice(),
            [AppRequest::CopySelection]
        ));
        assert!(model.selection.is_some());
        assert_eq!(model.screen, Screen::Session);
        assert!(!model.should_quit);
    }
}

#[test]
fn ctrl_shift_c_copies_composer_and_is_harmless_without_selection() {
    for character in ['c', 'C'] {
        let mut model = common::launcher_model();
        model.composer.set_text("café 🪟".to_owned());
        model.composer.press_at(0);
        model.composer.drag_to(model.composer.text().len());
        let key = Event::Key(KeyEvent::new(
            KeyCode::Char(character),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ));
        dispatch_input(&mut model, &[], key.clone());
        assert!(
            matches!(model.requests.as_slice(), [AppRequest::CopyText(text)] if text == "café 🪟")
        );
        model.requests.clear();
        dispatch_input(&mut model, &[], key);
        assert!(model.requests.is_empty());
        assert!(!model.should_quit);
    }
}

#[test]
fn shift_modified_mouse_is_terminal_owned_without_app_selection_scroll_click_or_paste() {
    let mut model = session();
    let (_, hits) = draw(&model, 80);
    model.dirty = false;
    let scroll = model.scroll_back.get();
    for modifiers in [
        KeyModifiers::SHIFT,
        KeyModifiers::SHIFT | KeyModifiers::CONTROL,
    ] {
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
            MouseEventKind::Moved,
            MouseEventKind::ScrollUp,
            MouseEventKind::ScrollDown,
            MouseEventKind::Down(MouseButton::Right),
            MouseEventKind::Up(MouseButton::Right),
        ] {
            let event = mouse(kind, 10, 6, modifiers);
            let Event::Mouse(report) = &event else {
                unreachable!()
            };
            assert!(terminal_owned_mouse(report));
            dispatch_input(&mut model, &hits, event);
            assert!(model.selection.is_none());
            assert!(model.mouse_down.is_none());
            assert!(!model.composer_drag);
            assert!(model.requests.is_empty());
            assert_eq!(model.scroll_back.get(), scroll);
            assert!(!model.dirty);
        }
    }
}

#[test]
fn shift_handoff_cancels_a_pending_app_drag_without_copy_or_click() {
    let mut model = session();
    for (kind, modifiers) in [
        (MouseEventKind::Down(MouseButton::Left), KeyModifiers::NONE),
        (MouseEventKind::Drag(MouseButton::Left), KeyModifiers::SHIFT),
        (MouseEventKind::Up(MouseButton::Left), KeyModifiers::NONE),
    ] {
        dispatch_input(&mut model, &[], mouse(kind, 9, 5, modifiers));
    }
    assert!(model.mouse_down.is_none());
    assert!(model.selection.is_none());
    assert!(model.requests.is_empty());
}

#[test]
fn a_captured_right_click_requests_one_paste_and_never_a_left_click_action() {
    let mut model = session();
    let (_, hits) = draw(&model, 80);
    for kind in [
        MouseEventKind::Down(MouseButton::Right),
        MouseEventKind::Drag(MouseButton::Right),
        MouseEventKind::Up(MouseButton::Right),
    ] {
        dispatch_input(&mut model, &hits, mouse(kind, 2, 1, KeyModifiers::NONE));
    }
    assert!(matches!(
        model.requests.as_slice(),
        [AppRequest::ClipboardRead]
    ));
    assert_eq!(model.screen, Screen::Session);
    assert!(model.selection.is_none());
    clipboard_paste_effects(&mut model, &FakeClipboard::text_with("right\r\nclick 🪟"));
    assert_eq!(model.composer.text(), "right\nclick 🪟");
}

#[test]
fn terminal_paste_inserts_once_without_requesting_an_os_read() {
    let mut model = session();
    dispatch_input(&mut model, &[], Event::Paste("one\r\ntwo 🪟".to_owned()));
    assert_eq!(model.composer.text(), "one\ntwo 🪟");
    assert!(model.requests.is_empty());
}

#[test]
fn clipboard_and_terminal_pastes_normalize_crlf_and_preserve_utf16_round_trip() {
    for original in [
        "café\r\nفارسی 日本語 🪟\rtail".to_owned(),
        "row 🪟\r\n".repeat(80),
    ] {
        let utf16: Vec<u16> = original.encode_utf16().collect();
        let decoded = String::from_utf16(&utf16).expect("valid Windows Unicode text");
        let expected = original.replace("\r\n", "\n").replace('\r', "\n");
        for direct in [true, false] {
            let mut model = common::launcher_model();
            if direct {
                clipboard_paste_effects(&mut model, &FakeClipboard::text_with(&decoded));
            } else {
                model.handle(AppEvent::Paste(Pasted::new(decoded.clone())));
            }
            let display = model.composer.text().to_owned();
            assert_eq!(model.composer.expand_pastes(&display), expected);
            assert!(
                model.requests.is_empty(),
                "newlines paste; they do not submit"
            );
        }
    }
}

#[test]
fn clipboard_gestures_never_read_or_copy_hidden_composer_text() {
    for screen in [
        Screen::Boot,
        Screen::Sessions,
        Screen::Tree,
        Screen::Tools,
        Screen::Accounts,
        Screen::Providers,
        Screen::Hooks,
        Screen::Usage,
        Screen::Fleet,
        Screen::Graph,
    ] {
        let mut model = session();
        model.screen = screen;
        model.composer.set_text("parked draft".to_owned());
        model.composer.press_at(0);
        model.composer.drag_to(model.composer.text().len());
        for event in [
            mouse(
                MouseEventKind::Down(MouseButton::Right),
                4,
                5,
                KeyModifiers::NONE,
            ),
            Event::Key(KeyEvent::new(
                KeyCode::Char('C'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            )),
            Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL)),
        ] {
            dispatch_input(&mut model, &[], event);
            assert!(
                model.requests.is_empty(),
                "hidden clipboard target on {screen:?}"
            );
            assert_eq!(model.composer.text(), "parked draft");
        }
    }
    let mut model = session();
    model.lockdown_overlay = true;
    dispatch_input(
        &mut model,
        &[],
        mouse(
            MouseEventKind::Down(MouseButton::Right),
            2,
            3,
            KeyModifiers::NONE,
        ),
    );
    assert!(model.requests.is_empty());
}

#[test]
fn forwarded_paste_and_captured_right_click_reach_the_masked_login_field_only() {
    for event in [
        Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL)),
        mouse(
            MouseEventKind::Down(MouseButton::Right),
            2,
            3,
            KeyModifiers::NONE,
        ),
    ] {
        let mut model = common::launcher_model();
        model.mode = haider_tui::app::RuntimeMode::Live;
        common::submit(&mut model, "/login anthropic api");
        assert!(model.login.is_some(), "masked login card");
        model.requests.clear();
        dispatch_input(&mut model, &[], event);
        assert!(matches!(
            model.requests.as_slice(),
            [AppRequest::ClipboardRead]
        ));
        model.requests.clear();
        clipboard_paste_effects(&mut model, &FakeClipboard::text_with("winclip-private-key"));
        assert!(model.composer.text().is_empty());
        assert!(model.requests.is_empty());
        let (frame, _) = draw(&model, 80);
        assert!(!(0..36).any(|y| row_text(&frame, y).contains("winclip-private-key")));
        clipboard_paste_effects(&mut model, &FakeClipboard::image(2, 2));
        assert!(model.composer.attachments().is_empty());
        assert!(model.requests.is_empty());
        model.handle(common::key(KeyCode::Enter));
        assert!(model.requests.iter().any(|request| matches!(request,
            AppRequest::LoginApi { secret, .. } if secret.expose_secret() == "winclip-private-key"
        )));
    }
}

#[test]
fn forwarded_paste_in_talk_setup_never_types_a_literal_v_or_edits_the_draft() {
    use haider_tui::talk::SetupStage;
    let mut model = session();
    model.mode = haider_tui::app::RuntimeMode::Live;
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_TRANSCRIPTION_V1.to_owned());
    common::submit(&mut model, "/talk setup");
    model.talk_setup.as_mut().expect("talk card").stage = SetupStage::DeepgramKey;
    model.requests.clear();
    dispatch_input(
        &mut model,
        &[],
        Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL)),
    );
    assert!(
        model.talk_setup.as_ref().expect("talk card").key_is_empty(),
        "Ctrl+V is a chord, not a typed v"
    );
    assert!(matches!(
        model.requests.as_slice(),
        [AppRequest::ClipboardRead]
    ));
    model.requests.clear();
    clipboard_paste_effects(&mut model, &FakeClipboard::text_with("talk-private-key"));
    assert_eq!(
        model
            .talk_setup
            .as_mut()
            .expect("talk card")
            .take_key()
            .expose_secret(),
        "talk-private-key"
    );
    assert!(model.composer.text().is_empty());
    let card = model.talk_setup.as_mut().expect("talk card");
    card.stage = SetupStage::Language;
    card.language.clear();
    dispatch_input(
        &mut model,
        &[],
        mouse(
            MouseEventKind::Down(MouseButton::Right),
            3,
            4,
            KeyModifiers::NONE,
        ),
    );
    assert!(matches!(
        model.requests.as_slice(),
        [AppRequest::ClipboardRead]
    ));
    model.requests.clear();
    clipboard_paste_effects(&mut model, &FakeClipboard::text_with("fa-IR"));
    assert_eq!(
        model.talk_setup.as_ref().expect("talk card").language,
        "fa-IR"
    );
    assert!(model.composer.text().is_empty());
    assert!(model.requests.is_empty());
}
