//! 973 account-navigation and dead-account recovery goldens.
#![allow(clippy::expect_used)]

use haider_protocol::credential::{AuthMethod, CredentialDescriptor, CredentialStatus};
use haider_protocol::ids::CredentialAlias;
use haider_tui::app::{AccountRow, AppEvent, Screen};
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

    for (width, height) in [(80, 24), (118, 36)] {
        check_golden("accounts_dead_switch_prompt", &draw(&model, width, height));
    }
}
