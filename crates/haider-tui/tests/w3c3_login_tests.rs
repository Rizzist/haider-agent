//! W3c3 M3 — the `/login … api` masked card and its secret hygiene.
//!
//! Report §6.3's line: "secret typing, paste, redraw, copy, error, quit,
//! and panic-safe teardown never reveal the key". This file is the TUI leg
//! of the sentinel-secret sweep (the daemon legs passed in W3c2): a unique
//! sentinel is typed and pasted into the card, and then every surface that
//! could carry it out of the process is searched for it.
#![allow(clippy::expect_used)]

use haider_tui::app::{
    AccountAddKind, AppModel, AppRequest, Hit, LoginStage, RuntimeMode, Screen, login_recovery,
};
use haider_tui::commands::{PaletteItem, palette_items};
use haider_tui::live::{LiveCommand, LiveDriver, LiveReply};
use haider_tui::mock::seed_provider_summaries;
use haider_tui::render::render;
use haider_tui::runtime::live_pass;
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
    let providers = palette_items("login ", false, &Default::default());
    assert!(
        providers.iter().any(|item| matches!(
            item,
            PaletteItem::Arg { value, .. } if value == "anthropic"
        )),
        "slot 0 offers providers"
    );
    let methods = palette_items("login anthropic ", false, &Default::default());
    let values: Vec<String> = methods.iter().map(PaletteItem::label).collect();
    assert_eq!(values, vec!["api", "oauth"], "slot 1 offers the methods");
    assert!(
        palette_items("login anthropic api ", false, &Default::default()).is_empty(),
        "there is no third slot"
    );
    // A fully-typed `/login` jumps straight to its first slot.
    assert!(matches!(
        palette_items("login", false, &Default::default()).first(),
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
    pasted.handle(haider_tui::app::AppEvent::Paste(SENTINEL.to_owned().into()));
    assert_eq!(pasted.composer.text(), "");
    let text = frame_text(&pasted, 118, 36);
    assert!(!text.contains(SENTINEL), "a pasted key appeared on screen");
    assert!(
        text.contains("stored only in the daemon vault"),
        "the card tells the truth about successful persistence"
    );
    assert!(
        !text.contains("never stored"),
        "a key is deliberately stored in the daemon vault after validation"
    );

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

    // …AND THE RETRY THE RECOVERY TEXT PROMISES ACTUALLY WORKS (review
    // P2-1). `busy`/`overloaded` are the daemon's retryable codes, so this
    // is the most likely path; the card used to refuse both typing and ⏎
    // from `Failed`, making "press ⏎ to try again" a dead end that three
    // separate doc comments and the on-screen hint all promised.
    //
    // MUTATION CHECK: narrow `LoginCard::accepts_input` back to
    // `LoginStage::Entry` only and everything below fails.
    for c in "sk-ant-RETRY-0000".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    assert_eq!(
        model.login.as_ref().expect("open").masked_len(),
        "sk-ant-RETRY-0000".chars().count(),
        "a failed card accepts the retype it asks for"
    );
    model.handle(key(KeyCode::Enter));
    let card = model.login.as_ref().expect("open");
    assert_eq!(card.stage, LoginStage::Submitting, "…and ⏎ re-submits it");
    assert_eq!(
        card.masked_len(),
        0,
        "the retry's key is staged and gone too"
    );
    assert!(
        model
            .requests
            .iter()
            .filter(|request| matches!(request, AppRequest::LoginApi { .. }))
            .count()
            >= 2,
        "the retry is a real second staging attempt"
    );
}

#[test]
fn a_committed_login_shows_the_descriptor_identity_and_no_secret() {
    let mut model = card_with_typed_sentinel();
    model.handle(key(KeyCode::Enter));
    model.login_result(Ok("anthropic · sk-…7f2".to_owned()));
    let text = frame_text(&model, 118, 36);
    assert!(text.contains("signed in"), "the card confirms");
    assert!(!text.contains(SENTINEL));
    // P1 MASK LAW: the committed identity rides the card MASKED-ALWAYS
    // (the card has no reveal loop — its keys belong to the fields).
    assert!(
        text.contains("signed in · a********"),
        "the identity renders masked:\n{text}"
    );
    assert!(
        !text.contains("sk-…7f2"),
        "the raw identity never rides the card:\n{text}"
    );
}

#[test]
fn reset_is_demo_only_and_never_reseeds_a_live_session_list() {
    // REVIEW P1-2. `/reset` reseeds the DEMO world. In live mode the rows
    // are the daemon's: reseeding replaced them with three fabricated
    // `demo-session-N` rows carrying sim transcripts and stranded every
    // attachment the driver held, after which `route_raw` answered
    // `WrongSession` for every live event and the stream was discarded in
    // silence. `RuntimeMode`'s charter always named this as one of the
    // three source-dependent decisions; it had no branch.
    //
    // MUTATION CHECK: delete the `"reset" if self.mode == RuntimeMode::Live`
    // arm from `execute_slash` and every assertion below fails.
    let mut model = live_model();
    model.sessions.clear();
    let live = haider_protocol::ids::SessionId::new("session-real");
    model.upsert_live_session(&live);
    model.open_session(&live);

    run_slash(&mut model, "/reset");

    assert_eq!(model.sessions.len(), 1, "the daemon's rows survive /reset");
    assert_eq!(
        model.sessions[0].id, live,
        "…unchanged, and still the daemon's"
    );
    assert_eq!(
        model.active_session.as_ref(),
        Some(&live),
        "…and the attachment is not stranded"
    );
    assert!(
        model.demo_requests.is_empty(),
        "a live reset never reaches demo persistence (report R11 cut 3)"
    );
    assert!(
        model
            .flash
            .as_deref()
            .is_some_and(|flash| flash.contains("demo only")),
        "…and says so honestly"
    );
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

// ---- TUI6.3 fix 1: the attempt identity (review r3 finding 1) ----

/// Open the card and submit a key, returning the queued attempt id.
fn submit_login(model: &mut AppModel, key_text: &str) -> u64 {
    run_slash(model, "/login anthropic api");
    assert!(model.login.is_some());
    for c in key_text.chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    model
        .requests
        .iter()
        .find_map(|request| match request {
            AppRequest::LoginApi { attempt, .. } => Some(*attempt),
            _ => None,
        })
        .expect("submit queued the attempt-carrying request")
}

/// Walk the owner's exact `+ Haider Code (API)` route while already on
/// Accounts, then submit its masked key card.
fn submit_haider_code_add_on_accounts(model: &mut AppModel, key_text: &str) -> u64 {
    let mut summary = seed_provider_summaries()
        .into_iter()
        .next()
        .expect("seed has a provider summary");
    summary.provider = "haider-code".to_owned();
    model.providers.apply_snapshot(vec![summary], 1);
    run_slash(model, "/accounts");
    model.requests.clear();
    model.handle_hit(Hit::AccountAdd(AccountAddKind::HaiderCodeApi));
    assert_eq!(model.screen, Screen::Accounts);
    assert_eq!(
        model.login.as_ref().map(|card| card.provider.as_str()),
        Some("haider-code")
    );
    for c in key_text.chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    model
        .requests
        .iter()
        .find_map(|request| match request {
            AppRequest::LoginApi { attempt, .. } => Some(*attempt),
            _ => None,
        })
        .expect("Haider Code add queued the attempt-carrying request")
}

/// HAIDERACCTREFRESH success pin.
///
/// MUTATION CHECK: remove the `AccountsRefresh` push from the `Ok` arm of
/// `AppModel::login_result`. Expected RUNTIME failure: the committed
/// Haider Code reply produces no `AccountList` while the model remains on
/// `Screen::Accounts`.
#[test]
fn committed_api_key_add_on_accounts_refreshes_the_roster_once() {
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    let now = std::time::Instant::now();

    let attempt = submit_haider_code_add_on_accounts(&mut model, SENTINEL);
    let _ = live_pass(&mut driver, &mut model, None, now);
    let pass = live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Staged {
            vault_reference: "haider-code-vault-reference".to_owned(),
            provider: "haider-code".to_owned(),
            alias: Some("haider-code-api".to_owned()),
            attempt,
        }),
        now,
    );
    let command_id = pass
        .commands
        .iter()
        .find_map(|command| match command {
            LiveCommand::LoginApi { command_id, .. } => Some(command_id.clone()),
            _ => None,
        })
        .expect("Haider Code login follows the staged key");

    let committed = live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::LoggedIn {
            command_id,
            identity: "haider-code key".to_owned(),
        }),
        now,
    );

    assert_eq!(
        model.screen,
        Screen::Accounts,
        "the user never left the tab"
    );
    assert!(matches!(
        model.login.as_ref().map(|card| &card.stage),
        Some(LoginStage::Done(identity)) if identity == "haider-code key"
    ));
    assert_eq!(
        committed
            .commands
            .iter()
            .filter(|command| matches!(command, LiveCommand::AccountList))
            .count(),
        1,
        "one committed add asks once for the daemon-owned roster"
    );
    assert!(
        !model.accounts.revealed,
        "refreshing after add must preserve the masked state"
    );
}

/// HAIDERACCTREFRESH failure pin.
///
/// MUTATION CHECK: move the `AccountsRefresh` push before the outcome match
/// in `AppModel::login_result`, so failures refresh too. Expected RUNTIME
/// failure: this reply emits `AccountList`; the recovery text assertion
/// also proves the error remains readable.
#[test]
fn failed_api_key_add_on_accounts_does_not_refresh_or_clear_the_error() {
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    let now = std::time::Instant::now();

    let attempt = submit_haider_code_add_on_accounts(&mut model, SENTINEL);
    let _ = live_pass(&mut driver, &mut model, None, now);
    let pass = live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Staged {
            vault_reference: "haider-code-vault-reference".to_owned(),
            provider: "haider-code".to_owned(),
            alias: Some("haider-code-api".to_owned()),
            attempt,
        }),
        now,
    );
    let command_id = pass
        .commands
        .iter()
        .find_map(|command| match command {
            LiveCommand::LoginApi { command_id, .. } => Some(command_id.clone()),
            _ => None,
        })
        .expect("Haider Code login follows the staged key");

    let failed = live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Failed {
            command_id: Some(command_id),
            code: haider_rpc::ERROR_CODE_UNAUTHORIZED.to_owned(),
            message: "rejected test key".to_owned(),
            retryable: false,
            presentation: None,
        }),
        now,
    );

    assert_eq!(
        model.screen,
        Screen::Accounts,
        "the user never left the tab"
    );
    assert!(
        !failed
            .commands
            .iter()
            .any(|command| matches!(command, LiveCommand::AccountList)),
        "a rejected add must not refresh the roster"
    );
    assert!(matches!(
        model.login.as_ref().map(|card| &card.stage),
        Some(LoginStage::Failed(message)) if message.contains("provider rejected this key")
    ));
}

#[test]
fn close_retires_the_queued_login_request_before_dispatch() {
    // Review r3 finding 1, the pre-dispatch half: Enter queues the
    // LoginApi request; a close landing while it is still queued must
    // kill it — a cancelled credential never reaches the wire at all.
    //
    // MUTATION CHECK (close-retires-queued): drop the `requests.retain`
    // from close_login_card and this fails — the dead attempt's Stage
    // command is issued after the UI said cancelled. Verified by revert.
    let mut model = live_model();
    let attempt = submit_login(&mut model, SENTINEL);
    model.handle(key(KeyCode::Esc)); // close while the request is queued
    assert!(model.login.is_none());
    assert!(
        !model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::LoginApi { .. })),
        "the queued submit died with the card"
    );
    assert!(
        model.requests.iter().any(|request| matches!(
            request,
            AppRequest::LoginRetired { attempt: retired } if *retired == attempt
        )),
        "and the driver is told to retire the attempt"
    );
    // Draining now issues NO stage for the dead attempt.
    let mut driver = LiveDriver::new("test");
    let pass = live_pass(&mut driver, &mut model, None, std::time::Instant::now());
    assert!(
        !pass
            .commands
            .iter()
            .any(|command| matches!(command, LiveCommand::Stage { .. })),
        "no Stage for a retired attempt: {:?}",
        pass.commands
    );
}

#[test]
fn cancelled_attempt_never_mints_and_a_stale_result_never_lands() {
    // The reviewer's exact r3 probe: abort → immediate re-/login. The
    // OLD attempt's Staged reply must not mint the login command (its
    // credential must never commit after the UI said cancelled), and a
    // stale LoggedIn must not mark the NEW card successful.
    //
    // MUTATION CHECK (staged-attempt-gate): force the Staged arm's
    // `live` to true and this fails — the cancelled attempt's reference
    // mints a login command. Verified by revert.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    let start = std::time::Instant::now();
    // Attempt 1: submit, drain (Stage in flight), then abort.
    let first = submit_login(&mut model, SENTINEL);
    let pass = live_pass(&mut driver, &mut model, None, start);
    assert!(
        pass.commands
            .iter()
            .any(|command| matches!(command, LiveCommand::Stage { .. })),
        "attempt 1 staged"
    );
    model.handle(key(KeyCode::Esc)); // abort while the stage is in flight
    // Attempt 2 opens immediately (the race window).
    let second = submit_login(&mut model, "sk-ant-SECOND-attempt-key");
    assert_ne!(first, second, "attempts are never reused");
    let pass = live_pass(&mut driver, &mut model, None, start);
    assert!(
        pass.commands
            .iter()
            .any(|command| matches!(command, LiveCommand::Stage { .. })),
        "attempt 2 staged"
    );
    // The OLD Staged reply lands first, carrying ITS OWN attempt (the
    // link's context tags every stage reply — TUI6.4): it must die whole.
    let pass = live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Staged {
            vault_reference: "old-cancelled-ref".to_owned(),
            provider: "anthropic".to_owned(),
            alias: None,
            attempt: first,
        }),
        start,
    );
    assert!(
        pass.commands.is_empty(),
        "the cancelled attempt's reference minted something: {:?}",
        pass.commands
    );
    // The NEW Staged reply mints the login for the LIVE attempt.
    let pass = live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Staged {
            vault_reference: "new-live-ref".to_owned(),
            provider: "anthropic".to_owned(),
            alias: None,
            attempt: second,
        }),
        start,
    );
    let command_id = pass
        .commands
        .iter()
        .find_map(|command| match command {
            LiveCommand::LoginApi {
                command_id,
                vault_reference,
                ..
            } => {
                assert_eq!(vault_reference, "new-live-ref");
                Some(command_id.clone())
            }
            _ => None,
        })
        .expect("the live attempt logs in");
    // A STALE LoggedIn (a command id the driver does not own) touches
    // nothing: the card stays Submitting, no success lands.
    let _ = live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::LoggedIn {
            command_id: haider_rpc::CommandId::new("cmd-stale-ghost"),
            identity: "ghost@old-attempt".to_owned(),
        }),
        start,
    );
    assert_eq!(
        model.login.as_ref().expect("card open").stage,
        LoginStage::Submitting,
        "a stale result never marks the new card"
    );
    // The REAL LoggedIn commits the live attempt.
    let _ = live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::LoggedIn {
            command_id,
            identity: "team@anthropic".to_owned(),
        }),
        start,
    );
    assert_eq!(
        model.login.as_ref().expect("card open").stage,
        LoginStage::Done("team@anthropic".to_owned()),
        "the live attempt's own result lands"
    );
}

#[test]
fn switch_abort_retires_the_inflight_attempt_too() {
    // The asynchronous close (the TUI6.2c chokepoint) retires exactly
    // like Esc: a key-changing switch under a Submitting card kills the
    // attempt end-to-end, and its late Staged reply mints nothing.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    let start = std::time::Instant::now();
    let aborted = submit_login(&mut model, SENTINEL);
    let pass = live_pass(&mut driver, &mut model, None, start);
    assert_eq!(pass.commands.len(), 1, "stage in flight");
    // The daemon's Created reply forces open_session — the switch aborts
    // the card (TUI6.2c) and must ALSO retire the attempt (TUI6.3).
    let id = common::session_named(&model, "billing-service");
    model.open_session(&id);
    assert!(model.login.is_none(), "chokepoint aborted the card");
    let pass = live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Staged {
            vault_reference: "post-abort-ref".to_owned(),
            provider: "anthropic".to_owned(),
            alias: None,
            attempt: aborted,
        }),
        start,
    );
    assert!(
        !pass.commands.iter().any(|command| matches!(
            command,
            LiveCommand::LoginApi { .. } | LiveCommand::Stage { .. }
        )),
        "the aborted attempt's reference commits nothing (the Attach is \
         sync_selection for the opened session, not the login): {:?}",
        pass.commands
    );
}

// ---- TUI6.3 fix 2: paste hygiene (review r3 finding 2) ----

#[test]
fn pasted_debug_is_redacted_end_to_end() {
    // A pasted API key must never be printable through Debug — not from
    // the wrapper, not from the event that carries it (panic teardown
    // prints events). The W3c2 SecretWire discipline, applied at the TUI
    // ingress, UNIVERSALLY: pasted text is user content either way, and a
    // secret-aware split path would reopen a printable window every time
    // a new consumer picked the wrong lane.
    use haider_tui::app::{AppEvent, Pasted};
    let secret = "sk-ant-SENTINEL-paste-99ab";
    let event = AppEvent::Paste(Pasted::new(secret.to_owned()));
    let printed = format!("{event:?}");
    assert!(
        !printed.contains(secret) && !printed.contains("SENTINEL"),
        "Debug leaked the paste: {printed}"
    );
    assert!(printed.contains("redacted"), "honest redaction marker");
}

#[test]
fn pasted_buffer_is_zeroizing_by_type() {
    // MUTATION CHECK (paste-zeroize-on-drop): the wipe-on-drop guarantee
    // IS the field's type. Change `Pasted`'s field to a plain `String`
    // and this fails to COMPILE at the accessor — could-not-compile is
    // the gate's loudest failure. Verified by revert.
    use haider_tui::app::Pasted;
    let pasted = Pasted::new("sk-wipe-me".to_owned());
    let zeroizing: &zeroize::Zeroizing<String> = pasted.zeroizing_inner();
    assert_eq!(zeroizing.as_str(), "sk-wipe-me");
    assert_eq!(pasted.as_str(), "sk-wipe-me");
}

// ---- TUI6.3b: the correlation queue survives every failure path ----

#[test]
fn stage_error_pops_its_tag_and_the_next_attempt_is_not_wedged() {
    // TUI6.4 re-scope (directed, review r4): this pin was born on 6.3b's
    // positional no-id consumption, which r4 proved unsound (an
    // unrelated no-id frame could shift the FIFO and cross-bind a
    // cancelled vault reference). The LIVENESS LAW it pinned is
    // unchanged — a stage-level error must reach the live card
    // immediately and must not wedge the next attempt — but the stage
    // error now arrives IDENTITY-TAGGED (`StageFailed { attempt }`, the
    // link's request context) instead of being guessed from queue
    // position.
    //
    // MUTATION CHECK (stage-error-identity): remove the StageFailed
    // arm's live delivery and this fails — the card never leaves
    // Submitting. Verified by revert.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    let start = std::time::Instant::now();
    let first = submit_login(&mut model, SENTINEL);
    let pass = live_pass(&mut driver, &mut model, None, start);
    assert_eq!(pass.commands.len(), 1, "attempt 1 staged");
    // The stage fails at the wire — identity-tagged by the link.
    let _ = live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::StageFailed {
            attempt: first,
            code: "vault_unavailable".to_owned(),
            message: "the vault is sealed".to_owned(),
        }),
        start,
    );
    assert!(
        matches!(
            model.login.as_ref().expect("card open").stage,
            LoginStage::Failed(_)
        ),
        "the live card takes the recovery immediately, not at the deadline"
    );
    // Attempt 2 must correlate cleanly — no stale tag in front.
    model.handle(key(KeyCode::Esc));
    let second = submit_login(&mut model, "sk-ant-SECOND-key-after-error");
    let pass = live_pass(&mut driver, &mut model, None, start);
    assert!(
        pass.commands
            .iter()
            .any(|command| matches!(command, LiveCommand::Stage { .. })),
        "attempt {second} staged"
    );
    let pass = live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Staged {
            vault_reference: "second-ref".to_owned(),
            provider: "anthropic".to_owned(),
            alias: None,
            attempt: second,
        }),
        start,
    );
    assert!(
        pass.commands
            .iter()
            .any(|command| matches!(command, LiveCommand::LoginApi { .. })),
        "attempt 2's genuine Staged mints — nothing wedged: {:?}",
        pass.commands
    );
}

#[test]
fn retire_then_disconnect_clears_the_tags_and_the_next_attempt_works() {
    // TUI6.4 re-scope (directed, review r4): born as 6.3b's
    // clear-before-early-return pin for the positional queue; the queue
    // is gone, but the LAW stands — retire-then-disconnect must leave
    // NOTHING that eats the next attempt's correlation. Under the
    // identity mechanism the pin asserts the same end-to-end outcome:
    // the post-reconnect attempt stages and mints cleanly.
    //
    // MUTATION CHECK (abandon-clears-binding): move abandon_login's
    // `login_attempt = None` below its early-return and the SIBLING law
    // (a ghost reply gated on a dead binding) weakens; this pin holds
    // the liveness half — attempt 2 mints after the reconnect. Verified
    // by revert (executed on the 6.4 identity gate below).
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    let start = std::time::Instant::now();
    submit_login(&mut model, SENTINEL);
    let pass = live_pass(&mut driver, &mut model, None, start);
    assert_eq!(pass.commands.len(), 1, "attempt 1 staged");
    model.handle(key(KeyCode::Esc)); // retire: command + deadline cleared
    let _ = live_pass(&mut driver, &mut model, None, start); // drain the retire
    let _ = live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Disconnected {
            reason: "socket died".to_owned(),
        }),
        start,
    );
    let _ = live_pass(&mut driver, &mut model, Some(LiveReply::Reconnected), start);
    // Attempt 2 on the fresh connection.
    let second = submit_login(&mut model, "sk-ant-SECOND-after-reconnect");
    let pass = live_pass(&mut driver, &mut model, None, start);
    assert!(
        pass.commands
            .iter()
            .any(|command| matches!(command, LiveCommand::Stage { .. })),
        "attempt 2 staged post-reconnect"
    );
    let pass = live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Staged {
            vault_reference: "fresh-connection-ref".to_owned(),
            provider: "anthropic".to_owned(),
            alias: None,
            attempt: second,
        }),
        start,
    );
    assert!(
        pass.commands
            .iter()
            .any(|command| matches!(command, LiveCommand::LoginApi { .. })),
        "nothing stale survived the disconnect: {:?}",
        pass.commands
    );
}

// ---- TUI6.4: identity-matched stage correlation (review r4) ----

#[test]
fn non_stage_noid_failure_never_shifts_stage_correlation() {
    // The r4 reviewer's exact probe: after abort→re-login, 6.3b's FIFO
    // queue was [retired N, live N+1]; a NON-stage no-id failure (a
    // List/Detach error, an uncorrelated ProtocolError) consumed N, the
    // OLD attempt's Staged then consumed N+1, passed both live gates,
    // and minted LoginApi with the CANCELLED vault reference. Under
    // identity correlation there is no position to shift: the old reply
    // carries its own retired attempt and dies, whatever arrives around
    // it.
    //
    // MUTATION CHECK (staged-identity-gate): weaken the Staged arm's
    // gate to `let live = true;` and this fails — the cancelled
    // reference mints LoginApi. Verified by revert.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    let start = std::time::Instant::now();
    let first = submit_login(&mut model, SENTINEL);
    let pass = live_pass(&mut driver, &mut model, None, start);
    assert_eq!(pass.commands.len(), 1, "attempt 1 staged");
    model.handle(key(KeyCode::Esc)); // abort — attempt 1 retired
    let second = submit_login(&mut model, "sk-ant-SECOND-live-key");
    let pass = live_pass(&mut driver, &mut model, None, start);
    assert!(
        pass.commands
            .iter()
            .any(|command| matches!(command, LiveCommand::Stage { .. })),
        "attempt 2 staged"
    );
    // The unrelated no-id failure lands between them — exactly what
    // shifted 6.3b's queue. It must touch no login state.
    let _ = live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Failed {
            command_id: None,
            code: "list_failed".to_owned(),
            message: "roster unavailable".to_owned(),
            retryable: true,
            presentation: None,
        }),
        start,
    );
    // The OLD attempt's Staged, identity-tagged with the retired attempt.
    let pass = live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Staged {
            vault_reference: "OLD-CANCELLED-VAULT-REFERENCE".to_owned(),
            provider: "anthropic".to_owned(),
            alias: None,
            attempt: first,
        }),
        start,
    );
    assert!(
        pass.commands.is_empty(),
        "the cancelled attempt's reply minted something: {:?}",
        pass.commands
    );
    // The TUI-side guarantee stands alone (no daemon backstop assumed):
    // the cancelled single-use reference is never emitted in ANY command.
    let pass = live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Staged {
            vault_reference: "new-live-ref".to_owned(),
            provider: "anthropic".to_owned(),
            alias: None,
            attempt: second,
        }),
        start,
    );
    let minted = pass
        .commands
        .iter()
        .find_map(|command| match command {
            LiveCommand::LoginApi {
                vault_reference, ..
            } => Some(vault_reference.clone()),
            _ => None,
        })
        .expect("the live attempt mints");
    assert_eq!(minted, "new-live-ref");
    assert!(
        !format!("{pass:?}").contains("OLD-CANCELLED-VAULT-REFERENCE"),
        "the cancelled reference never leaves the driver"
    );
}

#[test]
fn retired_failed_reply_is_silent_no_flash() {
    // Review r4 finding 2 (P2): a late `Failed(Some(old_login_id))` for
    // a RETIRED attempt left the card untouched but painted
    // `model.flash = "· provider_rejected — …"` — a misleading global
    // flash for an attempt the user already cancelled. Retired replies
    // are now consumed SILENTLY: the retire remembers the command id
    // precisely so its late reply can be told apart from a genuinely
    // unrelated failure (which still deserves its flash).
    //
    // MUTATION CHECK (retired-silence): remove the
    // `retired_logins.remove(id)` consumption from the Failed arm and
    // this fails — the ghost failure paints the flash. Verified by
    // revert.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    let start = std::time::Instant::now();
    let first = submit_login(&mut model, SENTINEL);
    let _ = live_pass(&mut driver, &mut model, None, start);
    // The stage answers; the login command mints.
    let pass = live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Staged {
            vault_reference: "ref-1".to_owned(),
            provider: "anthropic".to_owned(),
            alias: None,
            attempt: first,
        }),
        start,
    );
    let old_login_id = pass
        .commands
        .iter()
        .find_map(|command| match command {
            LiveCommand::LoginApi { command_id, .. } => Some(command_id.clone()),
            _ => None,
        })
        .expect("login command in flight");
    // Abort while the login is in flight, then open a NEW card (the r4
    // shape: the ghost must not touch it OR the global flash).
    model.handle(key(KeyCode::Esc));
    run_slash(&mut model, "/login anthropic api");
    let _ = live_pass(&mut driver, &mut model, None, start);
    model.flash = None;
    let _ = live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Failed {
            command_id: Some(old_login_id),
            code: "provider_rejected".to_owned(),
            message: "old attempt failed".to_owned(),
            retryable: false,
            presentation: None,
        }),
        start,
    );
    assert_eq!(
        model.flash, None,
        "a retired attempt's late failure paints NOTHING"
    );
    assert_eq!(
        model.login.as_ref().expect("new card open").stage,
        LoginStage::Entry,
        "and the new card is untouched"
    );
}

// ---- TUI6.5: stage-issuance identity + deadline ordering (review r5) ----

#[test]
fn timeout_retype_late_old_stage_never_mints() {
    // The r5 reviewer's exact probe: card-scoped identity let a timeout
    // clear the driver binding while the RETYPE revived the SAME id, so
    // the timed-out stage's late reply passed both gates and minted
    // LoginApi{vault_reference: "OLD-TIMED-OUT-VAULT-REFERENCE"}. Every
    // submit is now a fresh issuance: the old id is dead forever the
    // moment the retype mints.
    //
    // MUTATION CHECK (fresh-issuance-identity): remove the
    // `login_attempt_seq += 1; card.attempt = …` re-mint from login_key's
    // Enter arm and this fails — the old reference mints. Verified by
    // revert.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    let start = std::time::Instant::now();
    let first = submit_login(&mut model, SENTINEL);
    let _ = live_pass(&mut driver, &mut model, None, start);
    // The stage never answers; the deadline abandons it.
    let after_deadline =
        start + haider_tui::live::LOGIN_STAGE_TIMEOUT + std::time::Duration::from_secs(1);
    let _ = live_pass(&mut driver, &mut model, None, after_deadline);
    assert!(
        matches!(
            model.login.as_ref().expect("card open").stage,
            LoginStage::Failed(_)
        ),
        "timed out to the retype recovery"
    );
    // Retype on the SAME card: a NEW issuance.
    for c in "sk-ant-RETYPED-key".chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(key(KeyCode::Enter));
    let second = model
        .requests
        .iter()
        .find_map(|request| match request {
            AppRequest::LoginApi { attempt, .. } => Some(*attempt),
            _ => None,
        })
        .expect("retype queued");
    assert_ne!(first, second, "a retype is a NEW issuance, never a revival");
    let _ = live_pass(&mut driver, &mut model, None, after_deadline);
    // The TIMED-OUT stage's late reply, carrying its dead issuance.
    let pass = live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Staged {
            vault_reference: "OLD-TIMED-OUT-VAULT-REFERENCE".to_owned(),
            provider: "anthropic".to_owned(),
            alias: None,
            attempt: first,
        }),
        after_deadline,
    );
    assert!(
        pass.commands.is_empty(),
        "the timed-out issuance minted: {:?}",
        pass.commands
    );
    assert!(
        !format!("{pass:?}").contains("OLD-TIMED-OUT-VAULT-REFERENCE"),
        "the stale reference never leaves live_pass"
    );
    // The live issuance completes normally.
    let pass = live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Staged {
            vault_reference: "retyped-ref".to_owned(),
            provider: "anthropic".to_owned(),
            alias: None,
            attempt: second,
        }),
        after_deadline,
    );
    assert!(
        pass.commands
            .iter()
            .any(|command| matches!(command, LiveCommand::LoginApi { .. })),
        "the retype's own issuance mints"
    );
}

#[test]
fn at_deadline_stage_mints_nothing_in_its_own_pass() {
    // The r5 sibling: live_pass applied an inbound Staged BEFORE expiring
    // the deadline, so a stage arriving in the very pass its deadline
    // elapsed still minted — and expiry then retired internal state but
    // not the already-returned command. Expiry now runs FIRST: at the
    // boundary, expiry wins and the reply dies at the gates.
    //
    // MUTATION CHECK (deadline-before-apply): swap live_pass back to
    // apply-then-expire and this fails — the at-deadline stage mints.
    // Verified by revert.
    let mut model = live_model();
    let mut driver = LiveDriver::new("test");
    let start = std::time::Instant::now();
    let first = submit_login(&mut model, SENTINEL);
    let _ = live_pass(&mut driver, &mut model, None, start);
    // The reply arrives in the SAME pass the deadline elapses.
    let at_deadline = start + haider_tui::live::LOGIN_STAGE_TIMEOUT;
    let pass = live_pass(
        &mut driver,
        &mut model,
        Some(LiveReply::Staged {
            vault_reference: "at-deadline-ref".to_owned(),
            provider: "anthropic".to_owned(),
            alias: None,
            attempt: first,
        }),
        at_deadline,
    );
    assert!(
        pass.commands.is_empty(),
        "expiry wins the tie — the at-deadline stage minted: {:?}",
        pass.commands
    );
    assert!(
        matches!(
            model.login.as_ref().expect("card open").stage,
            LoginStage::Failed(_)
        ),
        "the card took the honest timeout recovery"
    );
}
