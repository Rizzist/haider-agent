//! W5e-1: the OAuth add card — alias derivation, attempt gating, the demo
//! simulate path (sim confirmAuth), cancel routing, and the bottom-anchored
//! add row + hover chrome (owner asks, 2026-07-30).
#![allow(clippy::expect_used)]

use haider_protocol::credential::{AuthMethod, CredentialDescriptor, CredentialStatus};
use haider_protocol::ids::CredentialAlias;
use haider_tui::app::{AppEvent, AppModel, AppRequest, Hit, OAuthAddPhase};
use haider_tui::mock::seed_account_rows;
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

mod common;
use common::{launcher_model, run_slash};

fn key(model: &mut AppModel, code: KeyCode) {
    model.handle(AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)));
}

fn accounts_model() -> AppModel {
    let mut model = launcher_model();
    run_slash(&mut model, "/accounts");
    model.requests.clear();
    model.accounts.apply_snapshot(seed_account_rows(), None);
    model
}

fn draw(model: &AppModel, width: u16, height: u16) -> String {
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
    text
}

fn open_card(model: &mut AppModel) {
    model.handle_hit(Hit::AccountAdd(haider_tui::app::AccountAddKind::OpenAiOAuth));
}

/// The add row + hints anchor to the BOTTOM of the screen (owner ask): even
/// with zero accounts the buttons sit just above the hints at the bottom,
/// not floating high under the empty-state line.
#[test]
fn add_row_is_bottom_anchored_even_when_empty() {
    let mut model = launcher_model();
    run_slash(&mut model, "/accounts");
    model.requests.clear(); // empty rows: the live first-run state
    let frame = draw(&model, 100, 24);
    let rows: Vec<&str> = frame.lines().collect();
    let button_row = rows
        .iter()
        .position(|row| row.contains("[+ OpenAI (OAuth)]"))
        .expect("add row rendered");
    let hint_row = rows
        .iter()
        .position(|row| row.contains("click an account to make it active"))
        .expect("hints rendered");
    // Bottom block: buttons(2) + blank + hints, then the status row.
    assert!(
        button_row >= rows.len() - 6,
        "add row must sit at the bottom (row {button_row} of {})",
        rows.len()
    );
    assert!(hint_row > button_row);
}

/// MUTATION CHECK (W5e-1): make `open_oauth_add` skip the taken-alias scan
/// (always use the bare provider name). Expected runtime failure: the second
/// card below derives `openai-oauth` again instead of `openai-oauth-2`.
/// Verified by revert on 2026-07-30.
#[test]
fn oauth_alias_derives_the_smallest_free_suffix() {
    let mut model = accounts_model();
    open_card(&mut model);
    let card = model.oauth_add.as_ref().expect("card open");
    assert_eq!(card.provider, "openai-oauth");
    assert_eq!(card.alias, "openai-oauth");
    assert!(matches!(card.phase, OAuthAddPhase::Starting));
    assert!(model.requests.iter().any(|request| matches!(
        request,
        AppRequest::OAuthAddStart { provider, alias, .. }
            if provider == "openai-oauth" && alias == "openai-oauth"
    )));

    // A row already holds the bare alias → the next card takes -2.
    key(&mut model, KeyCode::Esc); // cancel the first card
    let mut rows = seed_account_rows();
    rows.push(haider_tui::app::AccountRow {
        alias: "openai-oauth".into(),
        provider: "openai".into(),
        method: AuthMethod::OAuth,
        identity: "x".into(),
        status: CredentialStatus::Ok,
        selected: false,
        base_url: None,
    });
    model.accounts.apply_snapshot(rows, None);
    open_card(&mut model);
    assert_eq!(
        model.oauth_add.as_ref().expect("second card").alias,
        "openai-oauth-2"
    );
}

/// The demo `[1]` simulate lands the account exactly like the sim's
/// confirmAuth: row added under the sim provider name, selected for its
/// provider, card closed, ✓ message.
#[test]
fn demo_simulate_lands_the_account_and_selects_it() {
    let mut model = accounts_model();
    open_card(&mut model);
    // The demo driver would answer the start; emulate its phase flip.
    let attempt = model.oauth_add.as_ref().expect("card").attempt;
    model.oauth_add_phase(
        attempt,
        OAuthAddPhase::WaitingBrowser {
            url: "http://localhost:1455/callback (demo)".into(),
            origin: "auth.openai.com".into(),
        },
    );
    key(&mut model, KeyCode::Char('1'));
    assert!(model.oauth_add.is_none(), "card closes on simulate");
    let added = model
        .accounts
        .rows
        .iter()
        .find(|row| row.alias == "openai-oauth")
        .expect("row added");
    assert!(added.selected);
    assert_eq!(added.provider, "openai");
    assert!(
        !model
            .accounts
            .rows
            .iter()
            .any(|row| row.alias == "work-chatgpt" && row.selected),
        "the provider's previous selection yields"
    );
    assert_eq!(
        model.accounts.message.as_deref(),
        Some("✓ openai → openai-oauth · oauth · active")
    );
}

/// Esc/`[2]` cancels: the card closes, the cancel request carries the
/// attempt, and a LATE phase reply for that attempt touches nothing
/// (attempt gate — the login-card law).
#[test]
fn cancel_closes_and_late_replies_are_ghosts() {
    let mut model = accounts_model();
    open_card(&mut model);
    let attempt = model.oauth_add.as_ref().expect("card").attempt;
    model.requests.clear();
    key(&mut model, KeyCode::Esc);
    assert!(model.oauth_add.is_none());
    assert!(model.requests.iter().any(|request| matches!(
        request,
        AppRequest::OAuthAddCancel { attempt: cancelled } if *cancelled == attempt
    )));
    // Late replies for the retired attempt: silent.
    model.oauth_add_phase(attempt, OAuthAddPhase::Exchanging);
    assert!(model.oauth_add.is_none());
    model.oauth_add_failed(attempt, "late failure");
    assert!(model.oauth_add.is_none());
    // A late COMPLETION for the retired attempt must not repaint either.
    let descriptor = CredentialDescriptor {
        alias: CredentialAlias::new("openai-oauth"),
        provider: "openai-oauth".into(),
        base_url: None,
        auth_method: AuthMethod::OAuth,
        identity: "person@example.invalid".into(),
        status: CredentialStatus::Ok,
        active: true,
    };
    let message_before = model.accounts.message.clone();
    model.oauth_add_completed(attempt, &descriptor);
    assert_eq!(model.accounts.message, message_before);
}

/// The card renders with the bottom chrome and shows the waiting copy.
#[test]
fn card_renders_waiting_copy_above_the_add_row() {
    let mut model = accounts_model();
    open_card(&mut model);
    let attempt = model.oauth_add.as_ref().expect("card").attempt;
    model.oauth_add_phase(
        attempt,
        OAuthAddPhase::WaitingBrowser {
            url: "https://auth.openai.com/x".into(),
            origin: "auth.openai.com".into(),
        },
    );
    let frame = draw(&model, 100, 30);
    assert!(frame.contains("authorize OpenAI — ChatGPT — OAuth (loopback PKCE)"));
    assert!(frame.contains("your browser opened auth.openai.com"));
    assert!(frame.contains("[1] open the link again · [2] cancel"));
    let card_row = frame
        .lines()
        .position(|row| row.contains("authorize OpenAI"))
        .expect("card row");
    let button_row = frame
        .lines()
        .position(|row| row.contains("[+ OpenAI (OAuth)]"))
        .expect("buttons");
    assert!(card_row < button_row, "card sits above the add row");
}
