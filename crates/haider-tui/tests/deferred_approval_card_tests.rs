//! Credit safety for the nonblocking, agent-initiated usage-reset card: it
//! must never steal composer keystrokes, and no positional default (Enter, a
//! typed digit, a stale selection) may ever answer `approve`.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::effect::EffectClass;
use haider_protocol::ids::{CredentialAlias, MenuId};
use haider_protocol::menu::{DecisionKind, Menu, MenuKind, MenuOption, MenuScope};
use haider_tui::app::{AppEvent, AppModel};
use ratatui::crossterm::event::KeyCode;

mod common;
use common::{key, launcher_model, submit};

/// Mirrors the daemon's `usage_reset::approval_menu` (deny is option 0).
fn reset_card() -> Menu {
    Menu {
        id: MenuId::new("deferred-usage-reset-card-test"),
        kind: MenuKind::DeferredApproval {
            approval_id: "usage-reset-card-test".into(),
            effect: EffectClass::UsageResetApply,
            account: Some(CredentialAlias::new("work-chatgpt")),
        },
        title: "Apply a usage reset credit?".into(),
        body: vec!["Account: work-chatgpt".into()],
        options: vec![
            MenuOption {
                key: "deny".into(),
                label: "Deny".into(),
                detail: None,
                decision: Some(DecisionKind::RejectOnce),
            },
            MenuOption {
                key: "approve".into(),
                label: "Apply one reset".into(),
                detail: None,
                decision: Some(DecisionKind::AllowOnce),
            },
        ],
        blocking: false,
        scope: MenuScope::Session,
        origin: "deferred_approval_v1".into(),
        ttl_ms: None,
        timeout_option: None,
    }
}

fn session_with_card(typed: &str) -> AppModel {
    let mut model = launcher_model();
    submit(&mut model, "work on the repo");
    model.outbox.clear();
    for c in typed.chars() {
        model.handle(key(KeyCode::Char(c)));
    }
    model.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(
        reset_card(),
    ))));
    model
}

fn answered_keys(model: &AppModel) -> Vec<Option<String>> {
    model
        .outbox
        .iter()
        .map(|answer| answer.answer.option_key.clone())
        .collect()
}

/// LAW: text being typed when the card arrives stays a composer message —
/// the card does not replace the composer and Enter does not answer it.
///
/// MUTATION CHECK: let the card capture keys regardless of the composer
/// (restore the unconditional SELECT-menu gate); Enter then answers the card.
#[test]
fn typing_in_the_composer_is_never_captured_by_the_reset_card() {
    let mut model = session_with_card("steer: keep going");
    assert!(
        model.select_menu().is_none(),
        "the card yields to typed text"
    );
    model.handle(key(KeyCode::Char('1')));
    model.handle(key(KeyCode::Enter));
    assert!(
        answered_keys(&model).is_empty(),
        "Enter/1 while typing never answer the card: {:?}",
        answered_keys(&model)
    );
    assert!(
        model.projection.open_menu().is_some(),
        "the card is still pending after the message was sent"
    );
}

/// LAW: on an idle composer, Enter answers the default — `deny`.
///
/// MUTATION CHECK: put `approve` at option 0 or drop the Enter default
/// guard; the answer key becomes `approve`.
#[test]
fn enter_on_the_reset_card_declines() {
    let mut model = session_with_card("");
    assert!(model.select_menu().is_some());
    model.handle(key(KeyCode::Enter));
    assert_eq!(answered_keys(&model), vec![Some("deny".to_owned())]);
    assert_eq!(model.outbox[0].answer.option_index, 0);
}

/// LAW: a typed digit never answers the card; it starts a composer message.
///
/// MUTATION CHECK: route digits to the card; `2` then answers `approve`.
#[test]
fn digits_never_answer_the_reset_card() {
    for digit in ['1', '2'] {
        let mut model = session_with_card("");
        model.handle(key(KeyCode::Char(digit)));
        assert!(answered_keys(&model).is_empty(), "digit {digit} answered");
        assert_eq!(model.composer.text(), digit.to_string());
        assert!(model.select_menu().is_none());
    }
}

/// LAW: a stale selection left by an earlier menu cannot pre-select
/// `approve`; only arrow navigation on THIS card can.
///
/// MUTATION CHECK: drop the per-card selection owner check.
#[test]
fn stale_selection_cannot_preselect_approve() {
    let mut model = session_with_card("");
    model.menu_selection = 1;
    model.handle(key(KeyCode::Enter));
    assert_eq!(answered_keys(&model), vec![Some("deny".to_owned())]);
}

/// Approval is explicit: arrow to `Apply one reset`, then Enter.
#[test]
fn explicit_navigation_then_enter_approves() {
    let mut model = session_with_card("");
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Enter));
    assert_eq!(answered_keys(&model), vec![Some("approve".to_owned())]);
    assert_eq!(model.outbox[0].answer.option_index, 1);
}
