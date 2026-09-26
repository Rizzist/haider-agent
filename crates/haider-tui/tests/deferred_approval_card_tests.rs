//! Credit safety for the nonblocking, agent-initiated usage-reset card: it
//! must never steal composer keystrokes, and no positional default (Enter, a
//! typed digit, a stale selection) may ever answer `approve`.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::effect::EffectClass;
use haider_protocol::ids::{CredentialAlias, MenuId};
use haider_protocol::menu::{DecisionKind, Menu, MenuKind, MenuOption, MenuScope};
use haider_tui::app::{AppEvent, AppModel, ChipModel, ChipQuestion, Screen};
use haider_tui::script::{ChipDisplayState, ChipSeed};
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

/// Approval is explicit: ↓ to `Apply one reset`, then Enter — exactly one
/// answer.
#[test]
fn explicit_navigation_then_enter_approves() {
    let mut model = session_with_card("");
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Enter));
    assert_eq!(answered_keys(&model), vec![Some("approve".to_owned())]);
    assert_eq!(model.outbox[0].answer.option_index, 1);
}

/// The same card with an empty input history (↑ has nothing to recall).
fn session_with_card_no_history() -> AppModel {
    let mut model = session_with_card("");
    model.composer = haider_tui::composer::Composer::default();
    model
}

/// LAW (973-usage-data verify3 S6): ↑ then Enter never approves. On an idle
/// composer ↑ keeps its history-recall meaning — the recalled text hides the
/// card and Enter sends that message — and with nothing to recall ↑ leaves
/// the card on `deny`.
///
/// MUTATION CHECK: route ↑ to the card at `deny` and let it wrap (the
/// verify3 behavior); ↑ lands on `approve` and Enter approves.
#[test]
fn up_then_enter_never_approves_the_reset_card() {
    let mut model = session_with_card("");
    model.handle(key(KeyCode::Up));
    assert_eq!(
        model.composer.text(),
        "work on the repo",
        "↑ on the idle composer recalls history, not the card"
    );
    assert!(
        model.select_menu().is_none(),
        "the recalled text hides the card"
    );
    model.handle(key(KeyCode::Enter));
    assert!(
        answered_keys(&model).is_empty(),
        "↑ then Enter sent the recalled message, not a card answer: {:?}",
        answered_keys(&model)
    );
    assert!(
        model.projection.open_menu().is_some(),
        "the card is still pending"
    );

    let mut model = session_with_card_no_history();
    model.handle(key(KeyCode::Up));
    assert_eq!(
        model.menu_display_selection(model.select_menu().expect("card")),
        0
    );
    model.handle(key(KeyCode::Enter));
    assert_eq!(answered_keys(&model), vec![Some("deny".to_owned())]);
    assert_eq!(model.outbox[0].answer.option_index, 0);
}

/// ↑↑↓ then Enter. With history the arrows walk the composer ring and ↓
/// returns to the empty draft, so the card is back on `deny`; with nothing
/// to recall only the explicit ↓ moves the card, onto `approve`.
#[test]
fn up_up_down_then_enter_follows_the_rule() {
    let mut model = session_with_card("");
    for code in [KeyCode::Up, KeyCode::Up, KeyCode::Down] {
        model.handle(key(code));
    }
    assert!(model.composer.is_empty(), "↓ restored the empty draft");
    model.handle(key(KeyCode::Enter));
    assert_eq!(answered_keys(&model), vec![Some("deny".to_owned())]);

    let mut model = session_with_card_no_history();
    for code in [KeyCode::Up, KeyCode::Up, KeyCode::Down] {
        model.handle(key(code));
    }
    model.handle(key(KeyCode::Enter));
    assert_eq!(answered_keys(&model), vec![Some("approve".to_owned())]);
}

/// The card never wraps: ↓↓ stays on `approve`, and ↑ from `approve` goes
/// back to `deny` (the card owns ↑ only while it is off `deny`); a further
/// ↑ is history recall again.
///
/// MUTATION CHECK: restore wrapping on the card; ↓↓ lands back on `deny`.
#[test]
fn reset_card_selection_never_wraps() {
    let mut model = session_with_card_no_history();
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Down));
    let card = model.select_menu().expect("card");
    assert_eq!(model.menu_display_selection(card), 1, "↓↓ stays on approve");
    model.handle(key(KeyCode::Up));
    let card = model.select_menu().expect("card");
    assert_eq!(model.menu_display_selection(card), 0, "↑ returns to deny");
    model.handle(key(KeyCode::Up));
    assert_eq!(
        model.menu_display_selection(model.select_menu().expect("card")),
        0
    );
    model.handle(key(KeyCode::Enter));
    assert_eq!(answered_keys(&model), vec![Some("deny".to_owned())]);
}

/// A stale index from an earlier menu is neither shown on the card nor
/// answered by Enter.
#[test]
fn stale_selection_is_not_shown_on_the_reset_card() {
    let mut model = session_with_card("");
    model.menu_selection = 1;
    let card = model.select_menu().expect("card");
    assert_eq!(model.menu_display_selection(card), 0);
}

fn chip_view_with_card() -> AppModel {
    let mut model = launcher_model();
    submit(&mut model, "work on the repo");
    model.outbox.clear();
    let mut chip = ChipModel::from_seed(ChipSeed {
        agent: "agent-reset".to_owned(),
        parent: None,
        ros: None,
        callsign: "Husayn".to_owned(),
        hon: "(r)",
        full: "Husayn ibn Ali".to_owned(),
        name: "reset probe".to_owned(),
        model: "fable-5".to_owned(),
        device: "local".to_owned(),
        state: ChipDisplayState::InputRequired,
        tokens: 10,
        prefill: Vec::new(),
    });
    chip.question = Some(ChipQuestion {
        recovery: false,
        text: "Apply a usage reset credit?".to_owned(),
        options: vec!["Deny".to_owned(), "Apply one reset".to_owned()],
        resolved: false,
    });
    chip.transcript
        .apply(&EventPayload::MenuOpened(reset_card()));
    model.chips.push(chip);
    model.screen = Screen::Subagent;
    model.view_path = vec!["agent-reset".to_owned()];
    assert!(
        model
            .viewed_chip()
            .and_then(ChipModel::question_menu)
            .is_some()
    );
    model
}

/// The subagent (chip) view applies the same rules to a spend card: ↑ and
/// digits never approve, Enter defaults to `deny`, only ↓ reaches `approve`.
///
/// MUTATION CHECK: drop the chip view's spend-card arms; ↑ wraps onto
/// `approve` and `2` answers it.
#[test]
fn chip_view_reset_card_needs_an_explicit_down() {
    for keys in [
        vec![KeyCode::Up, KeyCode::Enter],
        vec![KeyCode::Char('2')],
        vec![KeyCode::Enter],
    ] {
        let mut model = chip_view_with_card();
        for code in &keys {
            model.handle(key(*code));
        }
        assert!(
            answered_keys(&model)
                .iter()
                .all(|k| k.as_deref() == Some("deny")),
            "{keys:?} answered {:?}",
            answered_keys(&model)
        );
    }
    let mut model = chip_view_with_card();
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Enter));
    assert_eq!(answered_keys(&model), vec![Some("approve".to_owned())]);
}
