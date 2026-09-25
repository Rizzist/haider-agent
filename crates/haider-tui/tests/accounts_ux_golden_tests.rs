//! 973 account-navigation and dead-account recovery goldens.
#![allow(clippy::expect_used)]

use haider_protocol::credential::{AuthMethod, CredentialDescriptor, CredentialStatus};
use haider_protocol::ids::CredentialAlias;
use haider_tui::app::{AccountRow, AppEvent, AppRequest, Screen};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

mod tuivirt_common;
use tuivirt_common::{check_golden, draw, launcher_model};

fn row(alias: &str, provider: &str, status: CredentialStatus) -> AccountRow {
    AccountRow::from_descriptor(&CredentialDescriptor {
        alias: CredentialAlias::new(alias),
        provider: provider.to_owned(),
        base_url: None,
        auth_method: AuthMethod::OAuth,
        identity: format!("{alias}@example.test"),
        status,
        active: alias == "anthropic-dead",
        label: None,
        account_identity: None,
        created_at_ms: Some(1),
    })
}

#[test]
fn grouped_row_navigation_and_dead_account_switch_prompt() {
    let mut model = launcher_model();
    model.screen = Screen::Accounts;
    model.accounts.apply_snapshot(
        vec![
            row("openai-live", "openai-oauth", CredentialStatus::Ok),
            row(
                "anthropic-dead",
                "anthropic-oauth",
                CredentialStatus::Expired,
            ),
            row("openai-second", "openai-oauth", CredentialStatus::Ok),
            row("anthropic-live", "anthropic-oauth", CredentialStatus::Ok),
        ],
        Some(1),
    );
    model.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Down,
        KeyModifiers::NONE,
    )));
    model.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Down,
        KeyModifiers::NONE,
    )));
    assert_eq!(
        model.accounts.rows[model.accounts.cursor].alias,
        "anthropic-dead"
    );
    model.select_account("anthropic-dead");

    assert_eq!(
        model.accounts.pending_relogin.as_deref(),
        Some("anthropic-dead")
    );

    for (width, height) in [(80, 24), (118, 36)] {
        check_golden("accounts_dead_switch_prompt", &draw(&model, width, height));
    }

    // A second Enter on the same dead row opens its in-place re-login card
    // (the requested alias IS the dead row), never a fresh-alias add.
    model.requests.clear();
    model.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
    let card = model.oauth_add.as_ref().expect("re-login card");
    assert_eq!(card.alias, "anthropic-dead");
    assert_eq!(card.provider, "anthropic-oauth");
    assert!(model.requests.iter().any(|request| matches!(
        request,
        AppRequest::OAuthAddStart { provider, alias, .. }
            if provider == "anthropic-oauth" && alias == "anthropic-dead"
    )));
    assert!(model.accounts.pending_relogin.is_none());
}

#[test]
fn moving_off_a_dead_row_disarms_its_relogin() {
    let mut model = launcher_model();
    model.screen = Screen::Accounts;
    model.accounts.apply_snapshot(
        vec![
            row(
                "anthropic-dead",
                "anthropic-oauth",
                CredentialStatus::Revoked,
            ),
            row("anthropic-live", "anthropic-oauth", CredentialStatus::Ok),
        ],
        Some(1),
    );
    model.select_account("anthropic-dead");
    assert!(model.accounts.pending_relogin.is_some());
    model.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Down,
        KeyModifiers::NONE,
    )));
    model.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Up,
        KeyModifiers::NONE,
    )));
    assert!(model.accounts.pending_relogin.is_none());
    model.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
    assert!(
        model.oauth_add.is_none(),
        "one Enter after re-arming only warns"
    );
}

/// S1: a re-login through `X` that the daemon saved under another alias
/// says so, and says `X` was not changed and still needs attention.
#[test]
fn relogin_saved_elsewhere_names_the_saved_alias_and_the_untouched_row() {
    let mut model = launcher_model();
    model.screen = Screen::Accounts;
    model.accounts.apply_snapshot(
        vec![
            row(
                "anthropic-dead",
                "anthropic-oauth",
                CredentialStatus::Expired,
            ),
            row("anthropic-live", "anthropic-oauth", CredentialStatus::Ok),
        ],
        Some(1),
    );
    let saved = |alias: &str| CredentialDescriptor {
        alias: CredentialAlias::new(alias),
        provider: "anthropic-oauth".to_owned(),
        base_url: None,
        auth_method: AuthMethod::OAuth,
        identity: "other@example.test".to_owned(),
        status: CredentialStatus::Ok,
        active: true,
        label: None,
        account_identity: None,
        created_at_ms: Some(2),
    };
    for (target, expected) in [
        (
            "anthropic-oauth",
            "saved as new alias `anthropic-oauth` (now active)",
        ),
        (
            "anthropic-live",
            "updated existing `anthropic-live` (now active)",
        ),
    ] {
        model.select_account("anthropic-dead");
        model.select_account("anthropic-dead");
        let attempt = model.oauth_add.as_ref().expect("re-login card").attempt;
        model.oauth_add_completed(attempt, &saved(target));
        let message = model.accounts.message.clone().expect("notice");
        assert!(
            message.contains(&format!(
                "a different account than `anthropic-dead`: {expected} · `anthropic-dead` unchanged and still needs attention"
            )),
            "{message}"
        );
        // The notice is word-wrapped, not clipped: every clause is on screen
        // even at 80 columns.
        let frame = draw(&model, 80, 24);
        let flat = frame
            .rows
            .iter()
            .map(|row| row.trim_end())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            flat.contains("`anthropic-dead` unchanged and still needs attention"),
            "{flat}"
        );
    }
}
