//! 970 owner bugs — the composer band's extra row, and ⌃V paste-image.
//!
//! BUG 1. The session band carried a `lead_subtree` breathing row on TOP of
//! its closing rule, so `❯ message haider …` reached `▾ subagents` one row
//! later than the SAME band does on the subagent screen. The rule is the
//! separator; the blank is gone.
//!
//! BUG 2. Bracketed paste carries text and only text, so a clipboard IMAGE
//! has to be read from the OS on the keystroke. ⌃V (and ⌘V / ⌃⇧V) reads it,
//! re-encodes it as PNG and hands it to the very pipeline `/attach` uses —
//! gated on whether the session's pair actually accepts pictures, with one
//! typed notice ([`ImageNotice`]) shared by both entry points.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use haider_rpc::ModelDetailWire;
use haider_tui::app::{
    AppEvent, AppModel, AppRequest, ChipModel, ImageNotice, RuntimeMode, Screen,
};
use haider_tui::clipboard::{
    ClipboardContent, ClipboardError, ClipboardSource, FakeClipboard, MAX_CLIPBOARD_IMAGE_BYTES,
};
use haider_tui::composer::PendingKind;
use haider_tui::mock::{seed_account_rows, seed_provider_summaries};
use haider_tui::render::render;
use haider_tui::runtime::{attach_read_effects, clipboard_paste_effects};
use haider_tui::script::{ChipDisplayState, ChipSeed};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

mod common;
use common::launcher_model;

fn sid() -> haider_protocol::ids::SessionId {
    haider_protocol::ids::SessionId::new("w970-session")
}

/// Summaries whose anthropic pair DECLARES a vision answer.
fn summaries_declaring_vision(vision: Option<bool>) -> Vec<haider_rpc::ProviderSummaryWire> {
    let mut summaries = seed_provider_summaries();
    let anthropic = summaries
        .iter_mut()
        .find(|summary| summary.provider == "anthropic")
        .expect("anthropic summary");
    anthropic.model_details = vec![ModelDetailWire {
        name: "claude-opus-5".into(),
        display_name: None,
        context_window: Some(1_000_000),
        supported_efforts: Vec::new(),
        default_effort: None,
        supported_speeds: Vec::new(),
        supports_thinking_type: None,
        supports_vision: vision,
    }];
    summaries
}

/// A live session on the session screen with `artifact.put` advertised and
/// the pair's vision capability set to `vision`.
fn live_model(vision: Option<bool>) -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.daemon_features = [haider_rpc::FEATURE_ARTIFACT_PUT_V1.to_owned()]
        .into_iter()
        .collect();
    model.daemon_version = Some("0.0.970".to_owned());
    model
        .providers
        .apply_snapshot(summaries_declaring_vision(vision), 1);
    model.accounts.apply_snapshot(seed_account_rows(), Some(1));
    model.identity.provider = "anthropic".to_owned();
    model.identity.model_short = "claude-opus-5".to_owned();
    model.identity_pinned = true;
    model.sessions.clear();
    model.upsert_live_session(&sid());
    model.open_session(&sid());
    model.screen = Screen::Session;
    model.requests.clear();
    model
}

fn chord(code: KeyCode, modifiers: KeyModifiers) -> AppEvent {
    AppEvent::Key(KeyEvent::new(code, modifiers))
}

fn rows(model: &AppModel, width: u16, height: u16) -> Vec<String> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| {
            let _ = render(model, frame);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_owned())
                .collect::<String>()
        })
        .collect()
}

/// The band's OPENING rule carries the session identity at its right end
/// (`──── claude-opus-5 · oauth ──`), so it is recognised by its leading
/// glyph rather than by being blank.
fn opens_with_rule(row: &str) -> bool {
    row.starts_with('\u{2500}')
}

fn is_rule(row: &str) -> bool {
    let trimmed = row.trim_end();
    !trimmed.is_empty() && trimmed.chars().all(|c| c == '─' || c == ' ')
}

// ---------------------------------------------------------------------------
// BUG 1 — the band closes with a rule and NOTHING else
// ---------------------------------------------------------------------------

fn model_with_subagents() -> AppModel {
    let mut model = live_model(Some(true));
    model.chips = vec![ChipModel::from_seed(ChipSeed {
        agent: "t1-docs".to_owned(),
        parent: None,
        ros: None,
        callsign: "Husayn".to_owned(),
        hon: "(r)",
        full: "Husayn ibn Ali".to_owned(),
        name: "docs".to_owned(),
        model: "fable-5".to_owned(),
        device: "macbook".to_owned(),
        state: ChipDisplayState::Running,
        tokens: 100,
        prefill: Vec::new(),
    })];
    model
}

#[test]
fn the_session_band_reaches_subagents_through_the_rule_alone() {
    // MUTATION CHECK (970 bug 1): restore `Constraint::Length(lead_subtree)`
    // in `render_session` and this fails at every roomy height — a blank
    // row reappears between the band and `▾ subagents`. Verified by revert.
    let model = model_with_subagents();
    for height in 16..34 {
        let frame = rows(&model, 90, height);
        let Some(band) = frame.iter().position(|row| row.contains("message haider")) else {
            continue;
        };
        let Some(subtree) = frame.iter().position(|row| row.contains("subagents —")) else {
            continue;
        };
        assert!(
            subtree > band,
            "height {height}: the panel sits below the band"
        );
        let between = &frame[band + 1..subtree];
        assert!(
            between.len() <= 1,
            "height {height}: {} rows between the band and ▾ subagents — only the \
             closing rule belongs there: {between:?}",
            between.len()
        );
        assert!(
            between.iter().all(|row| is_rule(row)),
            "height {height}: a NON-rule row separates the band from ▾ subagents: {between:?}"
        );
    }
}

#[test]
fn removing_the_breathing_row_gave_it_to_the_transcript() {
    // The freed row is not lost — `Constraint::Min` on the transcript
    // absorbs it, so the history shows one row MORE than it used to.
    let model = model_with_subagents();
    let frame = rows(&model, 90, 24);
    let band = frame
        .iter()
        .position(|row| row.contains("message haider"))
        .expect("band");
    let subtree = frame
        .iter()
        .position(|row| row.contains("subagents —"))
        .expect("panel");
    assert_eq!(
        subtree - band,
        2,
        "band row, closing rule, panel — three consecutive rows: {:?}",
        &frame[band..=subtree]
    );
}

// ---------------------------------------------------------------------------
// BUG 2 — the clipboard source
// ---------------------------------------------------------------------------

#[test]
fn a_fake_image_clipboard_encodes_a_real_png() {
    let clipboard = FakeClipboard::image(4, 3);
    match clipboard.read().expect("readable") {
        ClipboardContent::Image(image) => {
            assert_eq!((image.width, image.height), (4, 3));
            assert!(
                image.png.starts_with(&[0x89, b'P', b'N', b'G']),
                "the clipboard image is re-encoded as PNG"
            );
            assert!(image.png.len() <= MAX_CLIPBOARD_IMAGE_BYTES);
        }
        other => panic!("expected an image, got {other:?}"),
    }
}

#[test]
fn the_image_debug_never_dumps_pixels() {
    let ClipboardContent::Image(image) = FakeClipboard::image(8, 8).read().expect("readable")
    else {
        panic!("expected an image");
    };
    let rendered = format!("{image:?}");
    assert!(
        rendered.contains("png_bytes"),
        "size, not bytes: {rendered}"
    );
    assert!(
        !rendered.contains("137, 80"),
        "the pixel array must never reach a log line: {rendered}"
    );
}

// ---------------------------------------------------------------------------
// BUG 2 — the paste handler, against a fake clipboard
// ---------------------------------------------------------------------------

#[test]
fn a_clipboard_image_becomes_a_chip() {
    let mut model = live_model(Some(true));
    clipboard_paste_effects(&mut model, &FakeClipboard::image(6, 5));
    let chips = model.composer.attachments();
    assert_eq!(chips.len(), 1, "one chip: {chips:?}");
    assert!(
        matches!(&chips[0].kind, PendingKind::Image { mime } if mime == "image/png"),
        "the chip is a PNG image: {:?}",
        chips[0].kind
    );
    assert!(
        chips[0].label.contains("6×5"),
        "the chip labels the pasted image's size: {}",
        chips[0].label
    );
    assert!(
        model.composer_notice.is_none(),
        "a successful paste raises no notice"
    );
    assert!(
        model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::AttachUpload { .. })),
        "the bytes go up through the SAME upload seam /attach uses"
    );
}

#[test]
fn a_text_clipboard_leaves_the_draft_alone() {
    // The terminal's own bracketed paste owns text; ⌃V must not double it.
    let mut model = live_model(Some(true));
    model.composer.set_text("keep me".to_owned());
    clipboard_paste_effects(&mut model, &FakeClipboard::text());
    assert_eq!(model.composer.text(), "keep me");
    assert!(model.composer.attachments().is_empty(), "no chip for text");
    assert!(model.composer_notice.is_none(), "no notice for text");
}

#[test]
fn an_empty_clipboard_raises_a_notice_and_never_panics() {
    let mut model = live_model(Some(true));
    clipboard_paste_effects(&mut model, &FakeClipboard::empty());
    assert_eq!(model.composer_notice, Some(ImageNotice::ClipboardEmpty));
    assert!(model.composer.attachments().is_empty());
}

#[test]
fn an_unreadable_clipboard_raises_a_notice_and_never_panics() {
    let mut model = live_model(Some(true));
    clipboard_paste_effects(&mut model, &FakeClipboard::broken("no clipboard server"));
    assert_eq!(
        model.composer_notice,
        Some(ImageNotice::ClipboardUnreadable {
            note: "no clipboard server".to_owned()
        })
    );
    assert!(model.composer.attachments().is_empty());
    assert!(
        model
            .composer_notice
            .as_ref()
            .expect("notice")
            .text()
            .contains("no clipboard server"),
        "the notice names what actually went wrong"
    );
}

// ---------------------------------------------------------------------------
// BUG 2 — the vision gate, shared by BOTH image entry points
// ---------------------------------------------------------------------------

#[test]
fn a_pair_without_vision_refuses_the_pasted_image_and_keeps_the_draft() {
    let mut model = live_model(Some(false));
    model.composer.set_text("look at this".to_owned());
    clipboard_paste_effects(&mut model, &FakeClipboard::image(4, 4));
    assert_eq!(
        model.composer_notice,
        Some(ImageNotice::NoVision {
            model: "claude-opus-5".to_owned()
        })
    );
    assert!(
        model.composer.attachments().is_empty(),
        "a refused image is NEVER attached"
    );
    assert_eq!(model.composer.text(), "look at this", "the draft is kept");
    assert!(
        !model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::AttachUpload { .. })),
        "nothing is uploaded behind a refusal"
    );
}

#[test]
fn the_refusal_names_the_model_and_both_ways_out() {
    let notice = ImageNotice::NoVision {
        model: "deepseek-v4".to_owned(),
    };
    let text = notice.text();
    assert!(
        text.starts_with("deepseek-v4 does not accept images"),
        "{text}"
    );
    assert!(text.contains("vision model"), "{text}");
    assert!(text.contains("/attach as text"), "{text}");
}

#[test]
fn attach_of_an_image_uses_the_very_same_notice() {
    // The consistency law: `/attach photo.png` and ⌃V refuse identically.
    let png = {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        bytes.extend_from_slice(&[0; 64]);
        bytes
    };
    let path = std::env::temp_dir().join(format!(
        "haider-w970-{}-{}.png",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::write(&path, &png).expect("temp png");
    let mut model = live_model(Some(false));
    attach_read_effects(&mut model, &path.to_string_lossy());
    let _ = std::fs::remove_file(&path);
    assert_eq!(
        model.composer_notice,
        Some(ImageNotice::NoVision {
            model: "claude-opus-5".to_owned()
        }),
        "/attach raises the SAME typed notice the paste does"
    );
    assert!(model.composer.attachments().is_empty());
}

#[test]
fn an_undeclared_pair_still_attaches_and_lets_the_daemon_answer() {
    // Clients hold no tables: `None` is "the daemon said nothing", and the
    // client must NOT invent a refusal — it attaches, and the daemon's
    // typed `vision_unsupported` remains the authority.
    let mut model = live_model(None);
    assert_eq!(model.pair_accepts_images(), None);
    assert_eq!(model.image_refusal(), None);
    clipboard_paste_effects(&mut model, &FakeClipboard::image(3, 3));
    assert_eq!(model.composer.attachments().len(), 1);
    assert!(model.composer_notice.is_none());
}

// ---------------------------------------------------------------------------
// BUG 2 — the chord
// ---------------------------------------------------------------------------

#[test]
fn ctrl_v_issues_a_clipboard_read() {
    let mut model = live_model(Some(true));
    model.handle(chord(KeyCode::Char('v'), KeyModifiers::CONTROL));
    assert!(
        model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::ClipboardRead)),
        "⌃V asks the SHELL to read the clipboard: {:?}",
        model.requests
    );
    assert_eq!(model.composer.text(), "", "the chord never types a `v`");
}

#[test]
fn the_platform_paste_chords_all_reach_the_same_read() {
    for (code, modifiers) in [
        (KeyCode::Char('v'), KeyModifiers::CONTROL),
        (KeyCode::Char('v'), KeyModifiers::SUPER),
        (
            KeyCode::Char('V'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ),
    ] {
        let mut model = live_model(Some(true));
        model.handle(chord(code, modifiers));
        assert!(
            model
                .requests
                .iter()
                .any(|request| matches!(request, AppRequest::ClipboardRead)),
            "{code:?} + {modifiers:?} reads the clipboard"
        );
    }
}

#[test]
fn the_chord_refuses_before_the_read_on_a_pair_without_vision() {
    let mut model = live_model(Some(false));
    model.handle(chord(KeyCode::Char('v'), KeyModifiers::CONTROL));
    assert!(
        !model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::ClipboardRead)),
        "a declared-no-vision pair never pays for a clipboard round trip"
    );
    assert_eq!(
        model.composer_notice,
        Some(ImageNotice::NoVision {
            model: "claude-opus-5".to_owned()
        })
    );
}

#[test]
fn the_notice_clears_on_the_next_keystroke() {
    let mut model = live_model(Some(false));
    model.handle(chord(KeyCode::Char('v'), KeyModifiers::CONTROL));
    assert!(model.composer_notice.is_some());
    model.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Char('a'),
        KeyModifiers::NONE,
    )));
    assert!(
        model.composer_notice.is_none(),
        "the notice answers ONE gesture"
    );
}

#[test]
fn the_loom_tab_keeps_its_own_ctrl_v() {
    let mut model = live_model(Some(true));
    model.screen = Screen::Loom;
    model.requests.clear();
    model.handle(chord(KeyCode::Char('v'), KeyModifiers::CONTROL));
    assert!(
        !model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::ClipboardRead)),
        "Loom's ⌃V validates a document; the paste chord must not shadow it"
    );
}

// ---------------------------------------------------------------------------
// BUG 2 — the notice ROW in the band
// ---------------------------------------------------------------------------

#[test]
fn the_notice_renders_as_its_own_row_inside_the_band() {
    let mut model = live_model(Some(false));
    model.composer.set_text("look".to_owned());
    model.handle(chord(KeyCode::Char('v'), KeyModifiers::CONTROL));
    for width in [80_u16, 118, 160] {
        let frame = rows(&model, width, 30);
        let notice = frame
            .iter()
            .position(|row| row.contains("does not accept images"))
            .unwrap_or_else(|| panic!("the notice row renders at {width} cols: {frame:?}"));
        let draft = frame
            .iter()
            .position(|row| row.contains("look"))
            .expect("the draft is still on screen");
        assert!(
            notice < draft,
            "the notice sits ABOVE the draft it preserved (width {width})"
        );
        assert!(
            frame[notice].contains("claude-opus-5"),
            "the row names the model: {}",
            frame[notice]
        );
    }
}

#[test]
fn the_band_grows_by_exactly_one_row_for_the_notice() {
    // The band is anchored at the BOTTOM, so a notice does not move the
    // draft — the band grows UPWARD and the transcript pays the row. The
    // observable is the band's opening rule: it climbs by exactly one.
    let mut quiet = live_model(Some(false));
    quiet.composer.set_text("look".to_owned());
    let before = rows(&quiet, 90, 30);
    let draft_before = before
        .iter()
        .position(|row| row.contains("look"))
        .expect("draft");
    assert!(
        opens_with_rule(&before[draft_before - 1]),
        "with no notice the opening rule sits directly above the draft: {:?}",
        &before[draft_before - 1]
    );

    let mut noticed = live_model(Some(false));
    noticed.composer.set_text("look".to_owned());
    noticed.handle(chord(KeyCode::Char('v'), KeyModifiers::CONTROL));
    let after = rows(&noticed, 90, 30);
    let draft_after = after
        .iter()
        .position(|row| row.contains("look"))
        .expect("draft");
    assert_eq!(
        draft_after, draft_before,
        "the draft holds its row — the band grows upward, not downward"
    );
    assert!(
        after[draft_after - 1].contains("does not accept images"),
        "the notice is the row DIRECTLY above the draft: {:?}",
        &after[draft_after - 1]
    );
    assert!(
        opens_with_rule(&after[draft_after - 2]),
        "and the opening rule has climbed one row to make space: {:?}",
        &after[draft_after - 2]
    );
    // The row came out of the transcript, so the band is one row taller.
    assert_eq!(
        (draft_before - 1) - (draft_after - 2),
        1,
        "exactly ONE row was claimed"
    );
}

// ---------------------------------------------------------------------------
// The shared trait — a bespoke source works, so the seam is real
// ---------------------------------------------------------------------------

struct AlwaysEmpty;

impl ClipboardSource for AlwaysEmpty {
    fn read(&self) -> Result<ClipboardContent, ClipboardError> {
        Ok(ClipboardContent::Empty)
    }
}

#[test]
fn any_clipboard_source_drives_the_same_handler() {
    let mut model = live_model(Some(true));
    clipboard_paste_effects(&mut model, &AlwaysEmpty);
    assert_eq!(model.composer_notice, Some(ImageNotice::ClipboardEmpty));
}
