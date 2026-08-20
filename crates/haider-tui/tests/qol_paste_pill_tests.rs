//! QoL wave — the Claude Code paste pill.
//!
//! A large paste never inlines: the draft shows the atomic placeholder
//! `[Pasted text #N +K lines]`, the content parks on the draft's side
//! store, and SUBMIT expands each placeholder back — byte-exact, at its
//! position — into the outgoing text. The pill is atomic for editing
//! (⌫ at its right edge and Delete at its left edge remove it whole, the
//! caret never rests inside), small pastes inline exactly as before, and
//! the masked-card guardrails (login, `/talk` setup) stay first.
#![allow(clippy::expect_used)]

use haider_tui::app::{AppEvent, AppModel, AppRequest, Pasted, RuntimeMode, Screen};
use haider_tui::composer::Composer;
use ratatui::crossterm::event::KeyCode;

mod common;
use common::{key, launcher_model, run_slash, submit};

fn sid() -> haider_protocol::ids::SessionId {
    haider_protocol::ids::SessionId::new("s-pill")
}

/// A live idle session — the pill needs NO daemon feature, so none are
/// advertised on purpose.
fn live_session() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.sessions.clear();
    model.upsert_live_session(&sid());
    model.open_session(&sid());
    assert_eq!(model.screen, Screen::Session);
    model.requests.clear();
    model
}

fn paste(model: &mut AppModel, text: &str) {
    model.handle(AppEvent::Paste(Pasted::new(text.to_owned())));
}

/// The one SubmitText the reducer issued.
fn submitted_text(model: &AppModel) -> String {
    let mut texts = model.requests.iter().filter_map(|request| match request {
        AppRequest::SubmitText { text, .. } => Some(text.clone()),
        _ => None,
    });
    let text = texts.next().expect("a SubmitText was issued");
    assert!(texts.next().is_none(), "exactly one SubmitText");
    text
}

// ---- the pill itself ---------------------------------------------------

/// MUTATION CHECK: make `big_paste` inline the text (drop `insert_paste`).
/// Expected runtime failure: the composer shows the raw five lines
/// instead of the placeholder and the side store stays empty.
#[test]
fn large_paste_becomes_an_atomic_placeholder_not_inline_text() {
    let mut model = live_session();
    paste(&mut model, "one\ntwo\nthree\nfour\nfive");
    assert_eq!(model.composer, "[Pasted text #1 +5 lines]");
    assert_eq!(model.composer.pastes().len(), 1);
    assert_eq!(
        model.composer.pastes()[0].content(),
        "one\ntwo\nthree\nfour\nfive"
    );
    assert!(model.composer.attachments().is_empty(), "no B4b chip");
    assert!(model.requests.is_empty(), "no upload, no request");
}

#[test]
fn very_long_single_line_paste_pills_too() {
    // > 300 UTF-16 units on one line (the sim threshold, unchanged).
    let mut model = live_session();
    paste(&mut model, &"y".repeat(301));
    assert_eq!(model.composer, "[Pasted text #1 +1 lines]");
    assert_eq!(model.composer.pastes()[0].content(), "y".repeat(301));
}

#[test]
fn small_paste_inlines_exactly_as_before() {
    let mut model = live_session();
    paste(&mut model, "a\nb");
    assert_eq!(model.composer, "a\nb");
    assert!(model.composer.pastes().is_empty(), "no store entry");
}

/// MUTATION CHECK: drop the `expand_pastes` call from `submit_composer`.
/// Expected runtime failure: the wire text carries the literal
/// `[Pasted text #1 +5 lines]` instead of the pasted bytes.
#[test]
fn submit_expands_the_placeholder_byte_exact_at_its_position() {
    let mut model = live_session();
    for c in "before ".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    paste(&mut model, "l1\r\nl2\nl3\nl4\nl5");
    for c in " after".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    assert_eq!(model.composer, "before [Pasted text #1 +5 lines] after");
    model.handle(key(KeyCode::Enter));
    // Byte-exact at the placeholder's position; CRLF normalized exactly
    // as the pre-pill inline path normalized.
    assert_eq!(submitted_text(&model), "before l1\nl2\nl3\nl4\nl5 after");
    assert_eq!(model.composer, "");
    assert!(
        model.composer.pastes().is_empty(),
        "submit drained the store"
    );
}

#[test]
fn multiple_pastes_number_per_draft_and_all_expand() {
    let mut model = live_session();
    paste(&mut model, "a1\na2\na3\na4");
    for c in " and ".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    paste(&mut model, "b1\nb2\nb3\nb4\nb5");
    assert_eq!(
        model.composer,
        "[Pasted text #1 +4 lines] and [Pasted text #2 +5 lines]"
    );
    model.handle(key(KeyCode::Enter));
    assert_eq!(
        submitted_text(&model),
        "a1\na2\na3\na4 and b1\nb2\nb3\nb4\nb5"
    );
}

/// MUTATION CHECK (rev934 P3): record the pre-expansion draft in
/// `take_for_submit` again. Expected runtime failure: the recalled entry
/// below reads `before [Pasted text #1 +5 lines] after` and resubmitting it
/// ships that literal token — the store drained at the first submit.
#[test]
fn history_recall_reproduces_the_sent_bytes_not_the_placeholder() {
    let mut model = live_session();
    for c in "before ".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    paste(&mut model, "l1\nl2\nl3\nl4\nl5");
    for c in " after".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    let sent = submitted_text(&model);
    assert_eq!(sent, "before l1\nl2\nl3\nl4\nl5 after");
    model.requests.clear();

    assert!(model.composer.history_prev());
    assert_eq!(model.composer, sent.as_str(), "recall shows what was SENT");
    model.handle(key(KeyCode::Enter));
    assert_eq!(
        submitted_text(&model),
        sent,
        "resubmit ships the same bytes"
    );
}

/// MUTATION CHECK (rev934 P3): mint pill numbers from the live store's max
/// alone. Expected runtime failure: the fresh paste below mints a token
/// byte-identical to the orphan, first-occurrence resolution expands the
/// ORPHAN's position, and the submitted text swaps the two segments.
#[test]
fn fresh_paste_mints_above_orphan_placeholder_tokens() {
    let mut model = live_session();
    // An orphan token in the draft (a recalled/foreign draft whose store is
    // gone) — typed literally, so the side store stays empty.
    for c in "[Pasted text #1 +5 lines] ".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    assert!(model.composer.pastes().is_empty());
    paste(&mut model, "b1\nb2\nb3\nb4\nb5");
    assert_eq!(
        model.composer,
        "[Pasted text #1 +5 lines] [Pasted text #2 +5 lines]"
    );
    model.handle(key(KeyCode::Enter));
    assert_eq!(
        submitted_text(&model),
        "[Pasted text #1 +5 lines] b1\nb2\nb3\nb4\nb5",
        "the orphan stays literal; only the fresh pill expands"
    );
}

// ---- atomic editing ----------------------------------------------------

/// MUTATION CHECK: drop the pill arm from `Composer::backspace`. Expected
/// runtime failure: ⌫ peels one `]` off the placeholder, the containment
/// GC drops the store entry, and the draft keeps a broken token.
#[test]
fn backspace_at_the_right_edge_removes_the_whole_pill_and_its_store() {
    let mut model = live_session();
    paste(&mut model, "x1\nx2\nx3\nx4\nx5");
    model.handle(key(KeyCode::Backspace));
    assert_eq!(model.composer, "");
    assert!(
        model.composer.pastes().is_empty(),
        "the content died with the pill"
    );
}

/// MUTATION CHECK: drop the pill arm from `Composer::delete_forward`.
/// Expected runtime failure: Delete eats the pill's `[` and leaves a
/// broken token with no store behind it.
#[test]
fn delete_forward_at_the_left_edge_removes_the_whole_pill() {
    let mut composer = Composer::new();
    composer.insert_paste("d1\nd2\nd3".to_owned(), 3);
    composer.line_home(false);
    composer.delete_forward();
    assert_eq!(composer.text(), "");
    assert!(composer.pastes().is_empty());
}

/// MUTATION CHECK: drop the `snap_pill_*` calls from the movement seams.
/// Expected runtime failure: ← from the right edge rests INSIDE the
/// placeholder (one grapheme in) instead of jumping to its left edge.
#[test]
fn the_caret_never_rests_inside_the_pill() {
    let mut composer = Composer::new();
    composer.insert_str("hi ");
    composer.insert_paste("m1\nm2\nm3\nm4".to_owned(), 4);
    let placeholder = "[Pasted text #1 +4 lines]";
    let start = "hi ".len();
    let end = start + placeholder.len();
    // ← from the right edge crosses the pill whole.
    assert_eq!(composer.cursor(), end);
    composer.move_left(false);
    assert_eq!(composer.cursor(), start);
    // → from the left edge crosses it whole again.
    composer.move_right(false);
    assert_eq!(composer.cursor(), end);
    // A click inside snaps to the nearer edge.
    composer.press_at(start + 2);
    assert_eq!(composer.cursor(), start);
    composer.press_at(end - 2);
    assert_eq!(composer.cursor(), end);
    // ⌥← / ⌥→ may not stop on the words inside the token.
    composer.line_end_key(false);
    composer.word_left(false);
    assert!(
        composer.cursor() <= start,
        "word-left stopped inside the pill at {}",
        composer.cursor()
    );
    composer.line_home(false);
    composer.word_right(false); // "hi" — its end is before the pill
    assert_eq!(composer.cursor(), "hi".len());
    composer.word_right(false); // the next word is INSIDE the token
    assert!(
        composer.cursor() >= end,
        "word-right stopped inside the pill at {}",
        composer.cursor()
    );
}

#[test]
fn word_backspace_at_the_edge_swallows_the_pill_whole() {
    let mut composer = Composer::new();
    composer.insert_paste("w1\nw2\nw3\nw4".to_owned(), 4);
    composer.word_backspace();
    assert_eq!(composer.text(), "");
    assert!(composer.pastes().is_empty());
}

#[test]
fn clearing_the_draft_drops_the_store() {
    let mut composer = Composer::new();
    composer.insert_paste("c1\nc2\nc3\nc4".to_owned(), 4);
    composer.clear();
    assert!(composer.pastes().is_empty());
    // The next paste counts from #1 again — the counter is per-draft.
    composer.insert_paste("n1\nn2\nn3\nn4".to_owned(), 4);
    assert_eq!(composer.text(), "[Pasted text #1 +4 lines]");
}

#[test]
fn placeholders_survive_editing_around_them() {
    let mut model = live_session();
    paste(&mut model, "s1\ns2\ns3\ns4");
    for c in " tail".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    // Edit around the pill: kill the tail, retype, the pill and its
    // store survive every after_edit GC pass.
    for _ in 0..5 {
        model.handle(key(KeyCode::Backspace));
    }
    for c in " kept".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    assert_eq!(model.composer, "[Pasted text #1 +4 lines] kept");
    assert_eq!(model.composer.pastes().len(), 1);
}

// ---- guardrails: masked cards stay FIRST -------------------------------

/// MUTATION CHECK: move the login-card arm below the pill thresholds in
/// the `AppEvent::Paste` arm. Expected runtime failure: a five-line paste
/// while the card is open mints a pill into the parked composer instead
/// of landing in the masked buffer.
#[test]
fn login_card_paste_lands_in_the_masked_buffer_never_a_pill() {
    let mut model = live_session();
    run_slash(&mut model, "/login anthropic api");
    assert!(model.login.is_some(), "the card opened");
    paste(&mut model, "sk-l1\nsk-l2\nsk-l3\nsk-l4\nsk-l5");
    assert_eq!(model.composer, "");
    assert!(model.composer.pastes().is_empty(), "no pill store");
    assert!(
        model.login.as_ref().expect("card open").masked_len() > 0,
        "the paste landed in the masked buffer"
    );
}

#[test]
fn talk_setup_key_paste_lands_in_the_card_never_a_pill() {
    let mut model = live_session();
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_TRANSCRIPTION_V1.to_owned());
    submit(&mut model, "/talk setup");
    assert!(model.talk_setup.is_some(), "the card opened");
    model.handle_talk(haider_tui::talk::TalkEvent::SetupSnapshot {
        snapshot: haider_tui::talk::TalkSetupSnapshot {
            config: Ok(haider_stt::config::TranscriptionConfig::default()),
            whisper_dir: Some("/tmp/DiffForge/whisper".to_owned()),
            installed: vec![],
            selected_hint: None,
            runtime: None,
            runtime_hint: "brew install whisper-cpp".to_owned(),
        },
    });
    // Engine picker → deepgram → the masked key stage.
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Enter));
    assert_eq!(
        model.talk_setup.as_ref().expect("open").stage,
        haider_tui::talk::SetupStage::DeepgramKey
    );
    paste(&mut model, "dg-1\ndg-2\ndg-3\ndg-4\ndg-5");
    assert_eq!(model.composer, "");
    assert!(model.composer.pastes().is_empty(), "no pill store");
    assert!(
        model.talk_setup.as_ref().expect("open").masked_len() > 0,
        "the paste landed in the card's key buffer"
    );
}
