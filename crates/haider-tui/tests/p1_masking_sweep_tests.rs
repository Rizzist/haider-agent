//! P1 — the masking sweep: the U2 owner addendum ("emails and sensitive
//! identity strings auto-hidden everywhere, streamer-friendly") extended
//! from `/usage` to every other surface that renders an account identity.
//!
//! The laws:
//! * ONE AUTHORITY — every surface masks through `format::mask_identity`;
//!   no second mask dialect exists (`u2_usage_screen_tests.rs` owns the
//!   helper's shape: first char of each part, `*` for the rest,
//!   final `.tld` readable, the full local part never survives);
//! * `/accounts` rows render MASKED by default; `r` reveals for the
//!   CURRENT visit only; esc-close AND ⌃C-out both restore the mask for
//!   the next visit (the U2 Sub-Escape-Lane, baked in from the start);
//! * OAuth completion receipts carry the identity MASKED-ALWAYS — transient
//!   chrome has no reveal loop of its own (the
//!   login card's Done stage is pinned in `w3c3_login_tests.rs`);
//! * the launcher header's `account <label>` segment renders the ALIAS
//!   (grammar `[a-z0-9][a-z0-9._-]{0,63}` — never an email), so it wears
//!   no mask and the raw identity never rides the launcher.
#![allow(clippy::expect_used)]

use haider_protocol::credential::{AuthMethod, CredentialDescriptor, CredentialStatus};
use haider_protocol::ids::CredentialAlias;
use haider_tui::app::{AppModel, AppRequest, Screen};
use haider_tui::mock::seed_account_rows;
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

mod common;
use common::{key, launcher_model, run_slash};

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

/// The demo seed on `/accounts` — identities `you@work.com · ChatGPT`
/// (OAuth email) and `sk-…a91f` (key fragment) among them.
fn accounts_model() -> AppModel {
    let mut model = launcher_model();
    run_slash(&mut model, "/accounts");
    assert_eq!(model.screen, Screen::Accounts);
    model.requests.clear();
    model.accounts.apply_snapshot(seed_account_rows(), None);
    model
}

// ------------------------------------------------------------- the laws --

/// `/accounts` rows mask identities by default (email AND key fragment),
/// `r` reveals for the current visit only, and BOTH exit lanes (esc,
/// ⌃C-to-launcher) restore the mask for the next visit.
///
/// MUTATION CHECK (executed): render `row.identity` verbatim in
/// `render_accounts` (drop the mask branch) — the masked-form asserts and
/// the never-raw assert fail.
/// MUTATION CHECK (executed): drop the `revealed = false` reset from
/// `enter_accounts` — the ⌃C lane's final assert fails (the esc lane
/// alone is covered by `exit_accounts`, the U2 survivor lesson).
#[test]
fn accounts_rows_mask_by_default_and_r_reveals_per_visit() {
    let mut model = accounts_model();
    let frame = draw(&model, 100, 32);
    assert!(
        !frame.contains("you@work.com") && !frame.contains("sk-…a91f"),
        "no raw identity on open:\n{frame}"
    );
    assert!(
        frame.contains("y**@w***.com · ChatGPT") && frame.contains("s*******"),
        "the masked forms render (one authority — the U2 shape):\n{frame}"
    );
    assert!(
        frame.contains("r reveals"),
        "the key map names the reveal:\n{frame}"
    );

    // r reveals for THIS visit.
    model.handle(key(KeyCode::Char('r')));
    let frame = draw(&model, 100, 32);
    assert!(
        frame.contains("you@work.com · ChatGPT") && frame.contains("sk-…a91f"),
        "r reveals the identities:\n{frame}"
    );

    // esc closes; reopening starts masked again.
    model.handle(key(KeyCode::Esc));
    assert_ne!(model.screen, Screen::Accounts, "esc closes the screen");
    run_slash(&mut model, "/accounts");
    let frame = draw(&model, 100, 32);
    assert!(
        !frame.contains("you@work.com"),
        "a new visit always opens masked:\n{frame}"
    );

    // Sub-Escape-Lane (the U2 survivor lesson, baked in): a reveal must
    // not survive an exit that BYPASSES `exit_accounts` — ⌃C walks
    // straight to the launcher, so only the one-door reset in
    // `enter_accounts` can restore the mask for the next visit.
    model.handle(key(KeyCode::Char('r')));
    let frame = draw(&model, 100, 32);
    assert!(
        frame.contains("you@work.com"),
        "revealed again (precondition):\n{frame}"
    );
    model.handle(haider_tui::app::AppEvent::Key(KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
    )));
    assert_eq!(
        model.screen,
        Screen::Launcher,
        "⌃C leaves without exit_accounts"
    );
    run_slash(&mut model, "/accounts");
    let frame = draw(&model, 100, 32);
    assert!(
        !frame.contains("you@work.com"),
        "the visit after a ⌃C exit STILL opens masked:\n{frame}"
    );
}

/// OAuth completion receipts carry the identity MASKED-ALWAYS: transient
/// chrome has no reveal
/// loop of their own — the durable, revealable surface is the account
/// row the chained refresh lands.
///
#[test]
fn oauth_receipts_mask_the_identity_always() {
    let mut model = accounts_model();
    model.handle_hit(haider_tui::app::Hit::AccountAdd(
        haider_tui::app::AccountAddKind::OpenAiOAuth,
    ));
    let attempt = model.oauth_add.as_ref().expect("card open").attempt;
    model.requests.clear();
    model.oauth_add_completed(
        attempt,
        &CredentialDescriptor {
            alias: CredentialAlias::new("openai-oauth"),
            provider: "openai-oauth".into(),
            base_url: None,
            auth_method: AuthMethod::OAuth,
            identity: "person@example.invalid".into(),
            status: CredentialStatus::Ok,
            active: true,
            label: None,
        },
    );
    let message = model.accounts.message.as_deref().expect("receipt named");
    assert!(
        message.contains("p*****@e******.invalid") && !message.contains("person@example.invalid"),
        "the OAuth receipt masks the identity: {message}"
    );
    assert!(
        model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::AccountsRefresh)),
        "the receipt still chains the refresh"
    );
}

/// The launcher header's `account <label>` segment carries the ALIAS —
/// the daemon's alias grammar (`[a-z0-9][a-z0-9._-]{0,63}`, no `@`) means
/// it can never be an email, and U2 shipped `/usage`'s alias chips
/// unmasked: masking it here would be a second dialect, not more safety.
/// The raw identity never rides the launcher.
#[test]
fn launcher_header_carries_the_alias_never_the_identity() {
    let mut model = launcher_model();
    model.accounts.apply_snapshot(seed_account_rows(), None);
    // Selecting the account adopts its ALIAS into the header identity.
    model.apply_account_selected(
        &CredentialDescriptor {
            alias: CredentialAlias::new("work-chatgpt"),
            provider: "openai".into(),
            base_url: None,
            auth_method: AuthMethod::OAuth,
            identity: "you@work.com · ChatGPT".into(),
            status: CredentialStatus::Ok,
            active: true,
            label: None,
        },
        1,
    );
    assert_eq!(model.screen, Screen::Launcher);
    let frame = draw(&model, 118, 36);
    assert!(
        frame.contains("account work-chatgpt"),
        "the header names the alias:\n{frame}"
    );
    assert!(
        !frame.contains("you@work.com"),
        "the raw identity never rides the launcher:\n{frame}"
    );
}
