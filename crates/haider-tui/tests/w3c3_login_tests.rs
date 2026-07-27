//! W3c3 M3 — the `/login … api` masked card and its secret hygiene.
//!
//! Report §6.3's line: "secret typing, paste, redraw, copy, error, quit,
//! and panic-safe teardown never reveal the key". This file is the TUI leg
//! of the sentinel-secret sweep (the daemon legs passed in W3c2): a unique
//! sentinel is typed and pasted into the card, and then every surface that
//! could carry it out of the process is searched for it.
#![allow(clippy::expect_used)]

use haider_tui::app::{AppModel, AppRequest, LoginStage, RuntimeMode, login_recovery};
use haider_tui::commands::{PaletteItem, palette_items};
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

mod common;
use common::{key, launcher_model};

/// Type a slash command and RUN it. Enter with the palette open completes
/// the highlighted argument row (sim law), so the palette is dismissed
/// first — exactly what a user who typed the full command does with esc.
fn run_slash(model: &mut AppModel, line: &str) {
    for c in line.chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Esc));
    model.handle(key(KeyCode::Enter));
}

/// A key that exists nowhere else in the process image.
const SENTINEL: &str = "sk-ant-SENTINEL-a7f3c19e04b2-DO-NOT-LEAK";

fn live_model() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model
}

/// Every rendered cell of a frame at one size, as one string.
fn frame_text(model: &AppModel, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            render(model, frame);
        })
        .expect("draw succeeds");
    let buffer = terminal.backend().buffer().clone();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Open the card and type the sentinel one character at a time.
fn card_with_typed_sentinel() -> AppModel {
    let mut model = live_model();
    run_slash(&mut model, "/login anthropic api");
    assert!(
        model.login.is_some(),
        "the masked card opened (flash: {:?})",
        model.flash
    );
    for c in SENTINEL.chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model
}

// ---- the card itself --------------------------------------------------

#[test]
fn login_has_argument_slots_for_provider_then_method() {
    // Report §6.3: "`/login` argument slots". Without them `/login` is a
    // command the palette advertises and cannot complete.
    //
    // MUTATION CHECK: drop `"login"` from `commands::has_arg_slots` and the
    // fully-typed-command case below returns the command row, not slots.
    let providers = palette_items("login ", false);
    assert!(
        providers.iter().any(|item| matches!(
            item,
            PaletteItem::Arg {
                value: "anthropic",
                ..
            }
        )),
        "slot 0 offers providers"
    );
    let methods = palette_items("login anthropic ", false);
    let values: Vec<String> = methods.iter().map(PaletteItem::label).collect();
    assert_eq!(values, vec!["api", "oauth"], "slot 1 offers the methods");
    assert!(
        palette_items("login anthropic api ", false).is_empty(),
        "there is no third slot"
    );
    // A fully-typed `/login` jumps straight to its first slot.
    assert!(matches!(
        palette_items("login", false).first(),
        Some(PaletteItem::Arg { cmd: "login", .. })
    ));
}

#[test]
fn the_card_owns_the_keyboard_and_never_touches_the_composer() {
    // The composer records every submit in its input ring and parks drafts
    // per surface; a key that reached it would survive the card's close.
    //
    // MUTATION CHECK: delete the `self.login.is_some()` interception at the
    // top of `AppModel::handle`'s Key arm and the composer fills with the
    // sentinel — every assertion below fails.
    let model = card_with_typed_sentinel();
    assert_eq!(model.composer.text(), "", "the composer stayed empty");
    assert!(
        !format!("{:?}", model.drafts).contains(SENTINEL),
        "no parked draft holds the key"
    );
    let card = model.login.as_ref().expect("card open");
    assert_eq!(card.masked_len(), SENTINEL.chars().count());
}

#[test]
fn typing_pasting_and_redrawing_never_put_the_key_on_screen() {
    // The renderer is handed a LENGTH, never the text — so no frame, and
    // therefore no snapshot, scrollback, drag-selection or ⌃C copy, can
    // carry it.
    //
    // MUTATION CHECK: render `card.secret` instead of the mask in
    // `render::login_lines` and every size below fails.
    let mut model = card_with_typed_sentinel();
    for (width, height) in [(118, 36), (90, 10), (90, 5), (40, 3)] {
        let text = frame_text(&model, width, height);
        assert!(
            !text.contains(SENTINEL),
            "the typed key appeared at {width}x{height}"
        );
        assert!(
            !text.contains("SENTINEL"),
            "even a fragment of the key appeared at {width}x{height}"
        );
    }
    // The masked run is CAPPED, so a long key does not advertise its
    // length across the terminal.
    let masked = frame_text(&model, 118, 36);
    assert!(masked.contains('•'), "the field shows a mask");
    assert!(
        !masked.contains(&"•".repeat(SENTINEL.chars().count())),
        "the mask must not be a character-exact length readout"
    );

    // A PASTE lands in the same masked buffer and nowhere else — no pill
    // token, no draft, no ring.
    let mut pasted = live_model();
    run_slash(&mut pasted, "/login anthropic api");
    pasted.handle(haider_tui::app::AppEvent::Paste(SENTINEL.to_owned()));
    assert_eq!(pasted.composer.text(), "");
    let text = frame_text(&pasted, 118, 36);
    assert!(!text.contains(SENTINEL), "a pasted key appeared on screen");

    // A REDRAW after the card closes leaves nothing behind.
    model.handle(key(KeyCode::Esc));
    assert!(model.login.is_none(), "esc closed the card");
    let after = frame_text(&model, 118, 36);
    assert!(!after.contains(SENTINEL), "the key survived the redraw");
}

#[test]
fn the_key_is_redacted_in_debug_and_absent_from_the_persisted_dto() {
    // Panic teardown prints `{:?}` of whatever it holds; the demo store
    // serializes whatever its DTO enumerates. Neither may see the key.
    //
    // MUTATION CHECK: derive `Debug` on `LoginCard` instead of the manual
    // redacted impl and the first assertion fails.
    let model = card_with_typed_sentinel();
    let debug = format!("{model:?}");
    assert!(
        !debug.contains(SENTINEL),
        "the model's Debug leaked the key (this is what a panic prints)"
    );
    assert!(
        debug.contains("<redacted>"),
        "…and it says so, rather than silently omitting the field"
    );
    // The request that carries the key to the driver is Debug-safe too.
    let mut submitting = card_with_typed_sentinel();
    submitting.handle(key(KeyCode::Enter));
    let request = submitting
        .requests
        .iter()
        .find(|request| matches!(request, AppRequest::LoginApi { .. }))
        .expect("the card submitted");
    assert!(
        !format!("{request:?}").contains(SENTINEL),
        "AppRequest's derived Debug leaked the key"
    );

    // Persistence: the DTO is a whitelist, and the key is not on it.
    let dto = serde_json::to_string(&haider_tui::demo_store::snapshot(&model))
        .expect("snapshot serializes");
    assert!(!dto.contains(SENTINEL), "the demo store persisted the key");
    assert!(
        !dto.contains("login"),
        "…and carries no login key at all (a field named for it is a future leak)"
    );
}

#[test]
fn submitting_wipes_the_local_copy_before_the_request_is_even_drained() {
    // Between the card and the wire there is exactly ONE live copy. If the
    // card kept its own, a failure that leaves the card open would leave a
    // key sitting in the model for as long as the user stares at it.
    //
    // MUTATION CHECK: change `LoginCard::take_secret` to CLONE instead of
    // `mem::replace` and the masked length below stays non-zero.
    let mut model = card_with_typed_sentinel();
    model.handle(key(KeyCode::Enter));
    let card = model
        .login
        .as_ref()
        .expect("the card stays open while it works");
    assert_eq!(card.stage, LoginStage::Submitting);
    assert_eq!(
        card.masked_len(),
        0,
        "the card's copy of the key is gone the moment it is staged"
    );
    assert!(
        !format!("{model:?}").contains(SENTINEL),
        "…and nothing in the model can print it"
    );
}

#[test]
fn an_empty_card_cannot_be_submitted_and_esc_cancels_without_a_request() {
    let mut model = live_model();
    run_slash(&mut model, "/login anthropic api");
    model.handle(key(KeyCode::Enter));
    assert!(
        !model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::LoginApi { .. })),
        "an empty key is not a login"
    );
    assert!(model.login.is_some(), "…and the card stays open");
    model.handle(key(KeyCode::Esc));
    assert!(model.login.is_none());
    assert!(
        !model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::LoginApi { .. })),
        "cancelling never stages anything"
    );
}

#[test]
fn ctrl_c_closes_the_card_instead_of_navigating_with_a_key_in_memory() {
    // ⌃C is navigation everywhere else. While a key is being typed it must
    // first mean "drop this" — walking to the launcher with the card (and
    // its buffer) still alive is the leak.
    let mut model = card_with_typed_sentinel();
    model.handle(haider_tui::app::AppEvent::Key(KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
    )));
    assert!(model.login.is_none(), "⌃C dropped the card");
    assert!(!format!("{model:?}").contains(SENTINEL));
}

// ---- typed result handling -------------------------------------------

#[test]
fn every_stable_login_code_gets_its_own_recovery_text() {
    // Report §6.3: "stage/login result handling (typed restage_required /
    // busy recovery text)". The STABLE CODE decides what to say; the
    // human message is never load-bearing.
    //
    // MUTATION CHECK: collapse `login_recovery`'s arms into the `_` fallback
    // and every distinctness assertion below fails.
    let cases = [
        (haider_rpc::ERROR_CODE_RESTAGE_REQUIRED, "type it again"),
        (haider_rpc::ERROR_CODE_BUSY, "busy"),
        (haider_rpc::ERROR_CODE_UNAUTHORIZED, "rejected this key"),
        (haider_rpc::ERROR_CODE_PERMISSION_DENIED, "may not stage"),
        (
            haider_rpc::ERROR_CODE_VAULT_UNSUPPORTED,
            "no credential vault",
        ),
        (
            haider_rpc::ERROR_CODE_CREDENTIAL_MISSING,
            "stored credential is gone",
        ),
    ];
    let mut seen: Vec<String> = Vec::new();
    for (code, expected) in cases {
        let text = login_recovery(code, "detail");
        assert!(
            text.contains(expected),
            "`{code}` must recover with its own text, got {text:?}"
        );
        assert!(
            !text.contains("detail"),
            "`{code}` is typed — the human message must not be load-bearing"
        );
        assert!(!seen.contains(&text), "`{code}` reuses another code's text");
        seen.push(text);
    }
    // An unknown future code degrades honestly instead of pretending.
    let unknown = login_recovery("from_the_future", "why");
    assert!(unknown.contains("from_the_future") && unknown.contains("why"));
}

#[test]
fn a_failed_login_returns_the_card_to_entry_with_nothing_retained() {
    // A retry costs a retype BY DESIGN: holding the key across a failure
    // is exactly the window a `busy` retry would widen.
    let mut model = card_with_typed_sentinel();
    model.handle(key(KeyCode::Enter));
    model.login_result(Err((
        haider_rpc::ERROR_CODE_BUSY.to_owned(),
        "account actor mid-transaction".to_owned(),
    )));
    let card = model
        .login
        .as_ref()
        .expect("the card stays open to explain");
    assert!(matches!(card.stage, LoginStage::Failed(_)));
    assert_eq!(card.masked_len(), 0, "nothing was retained for the retry");
    let text = frame_text(&model, 118, 36);
    assert!(text.contains("busy"), "the recovery text is on screen");
    assert!(!text.contains(SENTINEL));
}

#[test]
fn a_committed_login_shows_the_descriptor_identity_and_no_secret() {
    let mut model = card_with_typed_sentinel();
    model.handle(key(KeyCode::Enter));
    model.login_result(Ok("anthropic · sk-…7f2".to_owned()));
    let text = frame_text(&model, 118, 36);
    assert!(text.contains("signed in"), "the card confirms");
    assert!(!text.contains(SENTINEL));
}

#[tokio::test(start_paused = true)]
async fn the_demo_refuses_login_honestly_and_drops_the_key() {
    // `haider tui --demo` has no daemon and no vault. It must say so — and
    // the request's secret must die at that boundary, not linger.
    let mut model = launcher_model();
    run_slash(&mut model, "/login anthropic api");
    for c in SENTINEL.chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    let (mut driver, _rx) = common::driver_for(&model);
    let requests: Vec<AppRequest> = model.requests.drain(..).collect();
    for request in requests {
        driver.handle_request(&mut model, request);
    }
    assert!(model.login.is_none(), "the demo closed the card");
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|flash| flash.contains("needs the daemon")),
        "…honestly"
    );
    assert!(!format!("{model:?}").contains(SENTINEL));
}
