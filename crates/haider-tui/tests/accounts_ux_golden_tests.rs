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
        incarnation: 0,
        credential_digest: None,

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
        incarnation: 0,
        credential_digest: None,

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

#[test]
fn a18_action_feedback_and_focus_are_visible_in_the_first_frame() {
    use haider_protocol::credential::CredentialAttentionReason;
    use haider_tui::app::Hit;
    for (width, height) in [(80, 24), (118, 36)] {
        for (provider, method) in [
            ("anthropic-oauth", AuthMethod::OAuth),
            ("openai-oauth", AuthMethod::OAuth),
            ("kimi-oauth", AuthMethod::OAuth),
            ("anthropic", AuthMethod::ApiKey),
        ] {
            for status in [
                CredentialStatus::Expired,
                CredentialStatus::Revoked,
                CredentialStatus::NeedsAttention {
                    reason: CredentialAttentionReason::KeychainMissing,
                },
                CredentialStatus::Ok,
                CredentialStatus::Limited { until_ms: u64::MAX },
            ] {
                for mouse in [false, true] {
                    let mut model = launcher_model();
                    model.screen = Screen::Accounts;
                    let mut rows = (0..24)
                        .map(|i| row(&format!("live-{i}"), provider, CredentialStatus::Ok))
                        .collect::<Vec<_>>();
                    let alias = "last-account-with-a-long-alias-to-force-wrapped-feedback";
                    let mut target = row(alias, provider, status.clone());
                    target.method = method;
                    target.selected = true; // A dead default still needs recovery.
                    rows.push(target);
                    model.accounts.apply_snapshot(rows, Some(1));
                    for _ in 0..24 {
                        model.handle(AppEvent::Key(KeyEvent::new(
                            KeyCode::Down,
                            KeyModifiers::NONE,
                        )));
                        draw(&model, width, height);
                    }
                    let before = draw(&model, width, height);
                    assert!(model.accounts.scroll.get() > 0);
                    if mouse {
                        let (_, hit) = before
                            .hits
                            .iter()
                            .find(|(_, hit)| matches!(hit, Hit::AccountRow(a) if a == alias))
                            .expect("visible row hit");
                        model.accounts.cursor = 0; // The click owns focus, not the old cursor.
                        model.handle_hit(hit.clone());
                    } else {
                        model.handle(AppEvent::Key(KeyEvent::new(
                            KeyCode::Enter,
                            KeyModifiers::NONE,
                        )));
                    }
                    let message = model.accounts.message.as_ref().expect("action feedback");
                    let frame = draw(&model, width, height);
                    let flat = frame
                        .rows
                        .iter()
                        .map(|row| row.trim())
                        .collect::<Vec<_>>()
                        .join(" ");
                    for word in message.split_whitespace() {
                        assert!(
                            flat.contains(word),
                            "feedback word {word:?} hidden: {width}x{height} {provider} {status:?} mouse={mouse}\n{flat}"
                        );
                    }
                    assert!(
                        frame
                            .hits
                            .iter()
                            .any(|(_, hit)| matches!(hit, Hit::AccountRow(a) if a == alias)),
                        "selected row lost after feedback"
                    );
                    assert_eq!(model.accounts.rows[model.accounts.cursor].alias, alias);
                    // Later explicit paging remains possible while the pinned notice stays visible.
                    let scroll = model.accounts.scroll.get();
                    model.handle(AppEvent::Key(KeyEvent::new(
                        KeyCode::PageUp,
                        KeyModifiers::NONE,
                    )));
                    draw(&model, width, height);
                    assert!(model.accounts.scroll.get() < scroll);
                    if method == AuthMethod::OAuth
                        && matches!(provider, "anthropic-oauth" | "openai-oauth")
                        && matches!(
                            status,
                            CredentialStatus::Expired
                                | CredentialStatus::Revoked
                                | CredentialStatus::NeedsAttention { .. }
                        )
                    {
                        // Clicking another usable row disarms the prior recovery,
                        // just like arrow navigation. Returning warns once again.
                        model.accounts.rows[0].selected = true;
                        model.accounts.rows[24].selected = false;
                        model.handle_hit(Hit::AccountRow("live-0".into()));
                        assert!(model.accounts.pending_relogin.is_none());
                        model.handle_hit(Hit::AccountRow(alias.into()));
                        assert!(model.oauth_add.is_none());
                        assert_eq!(model.accounts.pending_relogin.as_deref(), Some(alias));
                    }
                }
            }
        }
    }
}

/// Completion, failure and cancellation paths all share the pinned feedback
/// layout, even when they do not invoke select_account or its follow latch.
#[test]
fn a18_async_action_notice_reconciles_focus_without_losing_explicit_paging() {
    for (width, height) in [(80, 24), (118, 36)] {
        let mut model = launcher_model();
        model.screen = Screen::Accounts;
        model.accounts.apply_snapshot(
            (0..40)
                .map(|i| {
                    row(
                        &format!("account-{i}"),
                        "anthropic-oauth",
                        CredentialStatus::Ok,
                    )
                })
                .collect(),
            Some(1),
        );
        for _ in 0..30 {
            model.handle(AppEvent::Key(KeyEvent::new(
                KeyCode::Down,
                KeyModifiers::NONE,
            )));
            draw(&model, width, height);
        }
        let alias = model.accounts.rows[model.accounts.cursor].alias.clone();
        for notice in [
            "Cancelled sign-in; your current account remains active and you can retry with Enter when ready",
            "Failed to remove the selected account; retry the removal after checking the reported vault error",
            "Completed sign-in and saved the returned account under its existing alias; the original account still needs attention",
        ] {
            model.accounts.set_message(Some(notice.into()));
            let frame = draw(&model, width, height);
            let flat = frame
                .rows
                .iter()
                .map(|row| row.trim())
                .collect::<Vec<_>>()
                .join(" ");
            for word in notice.split_whitespace() {
                assert!(flat.contains(word), "missing notice word {word}: {flat}");
            }
            assert!(
                frame.hits.iter().any(
                    |(_, hit)| matches!(hit, haider_tui::app::Hit::AccountRow(a) if a == &alias)
                ),
                "new feedback must keep focus visible"
            );
            model.handle(AppEvent::Key(KeyEvent::new(
                KeyCode::PageUp,
                KeyModifiers::NONE,
            )));
            draw(&model, width, height);
            model.accounts.set_message(Some(notice.into()));
            let repeated = draw(&model, width, height);
            assert!(
                repeated.hits.iter().any(
                    |(_, hit)| matches!(hit, haider_tui::app::Hit::AccountRow(a) if a == &alias)
                ),
                "repeated feedback must reveal focus again"
            );
        }
    }
}
