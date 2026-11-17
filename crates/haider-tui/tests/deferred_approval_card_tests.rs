//! Credit safety for the nonblocking, agent-initiated usage-reset card:
//! only local gestures on its visible rows may answer it, and no positional
//! default (Enter, a typed digit, a stale selection) may answer `approve`.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::effect::EffectClass;
use haider_protocol::ids::{CredentialAlias, MenuId, SessionId};
use haider_protocol::menu::{DecisionKind, Menu, MenuKind, MenuOption, MenuScope};
use haider_rpc::SurfaceInjectOp;
use haider_tui::app::{
    AppEvent, AppModel, AppRequest, ChipModel, ChipQuestion, Hit, RuntimeMode, Screen,
};
use haider_tui::clipboard::FakeClipboard;
use haider_tui::live::{LiveDriver, LiveReply};
use haider_tui::runtime::{attach_read_effects, clipboard_paste_effects};
use haider_tui::script::{ChipDisplayState, ChipSeed};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};

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
            binding: None,
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
    model.requests.clear();
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

fn assert_injected_submit_does_not_answer(navigate: bool) {
    let mut model = session_with_card_no_history();
    model.mode = RuntimeMode::Live;
    let session = SessionId::new("synthetic-reset-session");
    model.active_session = Some(session.clone());
    if navigate {
        model.handle(key(KeyCode::Down));
    }
    let mut driver = LiveDriver::new("synthetic-reset-client");
    driver.apply(
        &mut model,
        LiveReply::InputInjected {
            session,
            op: SurfaceInjectOp::Submit,
        },
    );
    assert!(answered_keys(&model).is_empty());
    assert!(
        !model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::SubmitText { .. }))
    );
}

#[test]
fn injected_submit_on_fresh_deny_answers_nothing() {
    assert_injected_submit_does_not_answer(false);
}

#[test]
fn injected_submit_after_local_down_answers_nothing() {
    assert_injected_submit_does_not_answer(true);
}

fn assert_remote_edit_clear_then_submit(edit: SurfaceInjectOp) {
    let mut model = session_with_card_no_history();
    model.mode = RuntimeMode::Live;
    let session = SessionId::new("synthetic-reset-session");
    model.active_session = Some(session.clone());
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Char('x')));
    let mut driver = LiveDriver::new("synthetic-reset-client");
    driver.apply(
        &mut model,
        LiveReply::InputInjected {
            session: session.clone(),
            op: edit,
        },
    );
    driver.apply(
        &mut model,
        LiveReply::InputInjected {
            session: session.clone(),
            op: SurfaceInjectOp::Clear,
        },
    );
    driver.apply(
        &mut model,
        LiveReply::InputInjected {
            session,
            op: SurfaceInjectOp::Submit,
        },
    );
    assert!(answered_keys(&model).is_empty());
    assert_eq!(
        model.menu_display_selection(model.select_menu().expect("card")),
        0
    );
    model.handle(key(KeyCode::Enter));
    assert_eq!(answered_keys(&model), vec![Some("deny".to_owned())]);
}

#[test]
fn injected_clear_after_draft_does_not_restore_apply() {
    assert_remote_edit_clear_then_submit(SurfaceInjectOp::Set { text: "x".into() });
}

#[test]
fn injected_set_empty_after_draft_does_not_restore_apply() {
    let mut model = session_with_card_no_history();
    model.mode = RuntimeMode::Live;
    let session = SessionId::new("synthetic-reset-session");
    model.active_session = Some(session.clone());
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Char('x')));
    let mut driver = LiveDriver::new("synthetic-reset-client");
    driver.apply(
        &mut model,
        LiveReply::InputInjected {
            session: session.clone(),
            op: SurfaceInjectOp::Set {
                text: String::new(),
            },
        },
    );
    driver.apply(
        &mut model,
        LiveReply::InputInjected {
            session,
            op: SurfaceInjectOp::Submit,
        },
    );
    assert!(answered_keys(&model).is_empty());
    assert_eq!(
        model.menu_display_selection(model.select_menu().expect("card")),
        0
    );
}

#[test]
fn injected_insert_then_clear_does_not_restore_apply() {
    assert_remote_edit_clear_then_submit(SurfaceInjectOp::Insert {
        text: "synthetic".into(),
    });
}

#[test]
fn injected_submit_cannot_answer_a_chip_spend_card() {
    let mut model = chip_view_with_card();
    model.mode = RuntimeMode::Live;
    let session = SessionId::new("synthetic-reset-session");
    model.active_session = Some(session.clone());
    model.handle(key(KeyCode::Down));
    let mut driver = LiveDriver::new("synthetic-reset-client");
    driver.apply(
        &mut model,
        LiveReply::InputInjected {
            session,
            op: SurfaceInjectOp::Submit,
        },
    );
    assert!(answered_keys(&model).is_empty());
}

#[test]
fn remote_clear_and_mirror_replacement_retire_apply_navigation() {
    for mirror in [false, true] {
        let mut model = session_with_card_no_history();
        model.mode = RuntimeMode::Live;
        let session = SessionId::new("synthetic-reset-session");
        model.active_session = Some(session.clone());
        model.handle(key(KeyCode::Down));
        model.handle(key(KeyCode::Char('x')));
        assert!(model.select_menu().is_none());
        if mirror {
            model.handle(AppEvent::SurfaceInputReplace {
                text: String::new(),
            });
        } else {
            let mut driver = LiveDriver::new("synthetic-reset-client");
            driver.apply(
                &mut model,
                LiveReply::InputInjected {
                    session,
                    op: SurfaceInjectOp::Clear,
                },
            );
        }
        assert_eq!(
            model.menu_display_selection(model.select_menu().expect("visible card")),
            0
        );
        model.handle(key(KeyCode::Enter));
        assert_eq!(answered_keys(&model), vec![Some("deny".to_owned())]);
    }
}

#[test]
fn paste_and_newline_chords_keep_the_composer_as_the_target() {
    for event in [
        AppEvent::Paste("synthetic pasted message".to_owned().into()),
        AppEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)),
        AppEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
    ] {
        let mut model = session_with_card_no_history();
        model.handle(key(KeyCode::Down));
        model.handle(event);
        assert!(model.select_menu().is_none());
        assert!(!model.composer.is_empty());
        assert!(answered_keys(&model).is_empty());
        model.handle(key(KeyCode::Enter));
        assert!(answered_keys(&model).is_empty());
    }
}

#[test]
fn bracketed_paste_behind_card_submits_as_a_message() {
    let mut model = session_with_card_no_history();
    model.mode = RuntimeMode::Live;
    model.active_session = Some(SessionId::new("synthetic-reset-session"));
    model.handle(key(KeyCode::Down));
    model.handle(AppEvent::Paste(
        "synthetic bracketed paste".to_owned().into(),
    ));
    model.handle(key(KeyCode::Enter));
    assert!(answered_keys(&model).is_empty());
    assert!(model.requests.iter().any(|request| matches!(request,
        AppRequest::SubmitText { text, .. } if text == "synthetic bracketed paste")));
}

#[test]
fn alt_enter_behind_card_inserts_newline_without_answer() {
    let mut model = session_with_card_no_history();
    model.handle(key(KeyCode::Down));
    model.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::ALT,
    )));
    assert_eq!(model.composer.text(), "\n");
    assert!(answered_keys(&model).is_empty());
}

#[test]
fn shift_enter_behind_card_inserts_newline_without_answer() {
    let mut model = session_with_card_no_history();
    model.handle(key(KeyCode::Down));
    model.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::SHIFT,
    )));
    assert_eq!(model.composer.text(), "\n");
    assert!(answered_keys(&model).is_empty());
}

fn attach_synthetic_file(model: &mut AppModel) -> tempfile::TempDir {
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_ARTIFACT_PUT_V1.to_owned());
    model.daemon_version = Some("0.0.972".to_owned());
    let directory = tempfile::tempdir().expect("synthetic attachment directory");
    let path = directory.path().join("synthetic.txt");
    std::fs::write(&path, b"synthetic attachment body\n").expect("synthetic attachment file");
    submit(model, &format!("/attach {}", path.display()));
    assert!(model.requests.iter().any(|request| matches!(request,
        AppRequest::AttachRead { path: requested } if requested.as_str() == path.to_string_lossy().as_ref())));
    attach_read_effects(model, &path.to_string_lossy());
    assert_eq!(model.composer.attachments().len(), 1);
    directory
}

fn assert_attachment_only_draft(ready: bool) {
    let mut model = session_with_card_no_history();
    model.mode = RuntimeMode::Live;
    model.active_session = Some(SessionId::new("synthetic-reset-session"));
    model.handle(key(KeyCode::Down));
    let _directory = attach_synthetic_file(&mut model);
    if ready {
        let upload = model.composer.attachments()[0].upload;
        let surface = model.surface_key();
        let mut driver = LiveDriver::new("synthetic-reset-client");
        driver.apply(
            &mut model,
            LiveReply::ArtifactUploaded {
                upload,
                surface,
                artifact: haider_protocol::ids::ArtifactRef::new("synthetic-cas-ref"),
            },
        );
    }
    assert!(model.select_menu().is_none());
    model.handle(key(KeyCode::Enter));
    assert!(answered_keys(&model).is_empty());
    if ready {
        assert!(model.requests.iter().any(|request| matches!(request,
            AppRequest::SubmitText { text, attachments, .. }
                if text.is_empty() && attachments.len() == 1)));
    } else {
        assert!(
            !model
                .requests
                .iter()
                .any(|request| matches!(request, AppRequest::SubmitText { .. }))
        );
    }
}

#[test]
fn inflight_file_attachment_hides_card_without_submission() {
    assert_attachment_only_draft(false);
}

#[test]
fn ready_file_attachment_submits_without_answering_card() {
    assert_attachment_only_draft(true);
}

#[test]
fn pending_image_without_text_hides_spend_card() {
    let mut model = session_with_card_no_history();
    model.mode = RuntimeMode::Live;
    model.active_session = Some(SessionId::new("synthetic-reset-session"));
    model
        .daemon_features
        .insert(haider_rpc::FEATURE_ARTIFACT_PUT_V1.to_owned());
    model.daemon_version = Some("0.0.972".to_owned());
    model.handle(key(KeyCode::Down));
    model.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Char('v'),
        KeyModifiers::CONTROL,
    )));
    assert!(
        model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::ClipboardRead))
    );
    clipboard_paste_effects(&mut model, &FakeClipboard::image(2, 2));
    assert_eq!(model.composer.attachments().len(), 1);
    assert!(model.select_menu().is_none());
    model.handle(key(KeyCode::Enter));
    assert!(answered_keys(&model).is_empty());
}

#[test]
fn stale_spending_card_hit_and_hover_do_not_act_after_draft_hides_it() {
    let mut model = session_with_card_no_history();
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Char('x')));
    let hit = Hit::MenuOption {
        menu: reset_card().id,
        index: 1,
    };
    model.handle_hover(Some(hit.clone()));
    model.handle_hit(hit);
    assert!(answered_keys(&model).is_empty());
    assert_eq!(
        model.menu_selection, 1,
        "the prior raw row may remain, but hover cannot change it"
    );
    model.handle(key(KeyCode::Backspace));
    assert_eq!(
        model.menu_display_selection(model.select_menu().expect("visible card")),
        0
    );
}

#[test]
fn hidden_card_hit_does_not_change_the_prior_raw_selection() {
    let mut model = session_with_card_no_history();
    model.handle(key(KeyCode::Down));
    assert_eq!(model.menu_selection, 1);
    model.handle(key(KeyCode::Char('x')));
    model.handle_hit(Hit::MenuOption {
        menu: reset_card().id,
        index: 0,
    });
    assert_eq!(model.menu_selection, 1);
    assert!(answered_keys(&model).is_empty());
}

#[test]
fn chip_up_without_down_cannot_grant_hover_selection_ownership() {
    let mut model = chip_view_with_card();
    model.handle(key(KeyCode::Up));
    model.handle_hover(Some(Hit::MenuOption {
        menu: reset_card().id,
        index: 1,
    }));
    model.handle(key(KeyCode::Enter));
    assert!(
        answered_keys(&model)
            .iter()
            .all(|answer| answer.as_deref() != Some("approve"))
    );
}

#[test]
fn foreign_empty_replacement_while_card_is_navigated_retires_apply() {
    let mut model = session_with_card_no_history();
    model.handle(key(KeyCode::Down));
    model.handle(AppEvent::SurfaceInputReplace {
        text: String::new(),
    });
    assert_eq!(
        model.menu_display_selection(model.select_menu().expect("card")),
        0
    );
    model.handle(key(KeyCode::Enter));
    assert_eq!(answered_keys(&model), vec![Some("deny".to_owned())]);
}

#[test]
fn pasting_then_deleting_to_empty_restores_deny() {
    let mut model = session_with_card_no_history();
    model.handle(key(KeyCode::Down));
    model.handle(AppEvent::Paste("x".to_owned().into()));
    model.handle(key(KeyCode::Backspace));
    assert_eq!(
        model.menu_display_selection(model.select_menu().expect("card")),
        0
    );
    model.handle(key(KeyCode::Enter));
    assert_eq!(answered_keys(&model), vec![Some("deny".to_owned())]);
}

#[test]
fn newline_then_deleting_to_empty_restores_deny() {
    let mut model = session_with_card_no_history();
    model.handle(key(KeyCode::Down));
    model.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::ALT,
    )));
    model.handle(key(KeyCode::Backspace));
    assert_eq!(
        model.menu_display_selection(model.select_menu().expect("card")),
        0
    );
    model.handle(key(KeyCode::Enter));
    assert_eq!(answered_keys(&model), vec![Some("deny".to_owned())]);
}

#[test]
fn removed_attachment_restores_deny_without_apply_navigation() {
    let mut model = session_with_card_no_history();
    model.mode = RuntimeMode::Live;
    model.active_session = Some(SessionId::new("synthetic-reset-session"));
    model.handle(key(KeyCode::Down));
    let _directory = attach_synthetic_file(&mut model);
    assert!(model.select_menu().is_none());
    model.handle(key(KeyCode::Backspace));
    assert_eq!(
        model.menu_display_selection(model.select_menu().expect("card")),
        0
    );
    model.handle(key(KeyCode::Enter));
    assert_eq!(answered_keys(&model), vec![Some("deny".to_owned())]);
}

#[test]
fn replaced_card_drops_old_frame_hit_and_hover() {
    let mut model = session_with_card_no_history();
    let old_id = reset_card().id;
    let mut replacement = reset_card();
    replacement.id = MenuId::new("replacement-reset-card");
    model.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(
        replacement,
    ))));
    let old_hit = Hit::MenuOption {
        menu: old_id,
        index: 1,
    };
    model.handle_hover(Some(old_hit.clone()));
    model.handle_hit(old_hit);
    assert!(answered_keys(&model).is_empty());
    assert_eq!(
        model.menu_display_selection(model.select_menu().expect("new card")),
        0
    );
}

#[test]
fn overlay_drops_stale_apply_hit() {
    let mut model = session_with_card_no_history();
    model.handle(key(KeyCode::Down));
    model.help_open = true;
    model.handle_hit(Hit::MenuOption {
        menu: reset_card().id,
        index: 1,
    });
    assert!(answered_keys(&model).is_empty());
    model.help_open = false;
    assert_eq!(
        model.menu_display_selection(model.select_menu().expect("card")),
        0
    );
}

fn assert_clipboard_text_stays_a_message(chord: Option<KeyModifiers>) {
    let mut model = session_with_card_no_history();
    model.mode = RuntimeMode::Live;
    model.active_session = Some(SessionId::new("synthetic-reset-session"));
    model.handle(key(KeyCode::Down));
    if let Some(modifiers) = chord {
        model.handle(AppEvent::Key(KeyEvent::new(KeyCode::Char('v'), modifiers)));
    } else {
        // The runtime's captured right-click dispatches this same read.
        model.request_clipboard_paste();
    }
    assert!(
        model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::ClipboardRead))
    );
    clipboard_paste_effects(
        &mut model,
        &FakeClipboard::text_with("synthetic clipboard text"),
    );
    assert!(model.select_menu().is_none());
    model.handle(key(KeyCode::Enter));
    assert!(answered_keys(&model).is_empty());
    assert!(model.requests.iter().any(|request| matches!(request,
        AppRequest::SubmitText { text, .. } if text == "synthetic clipboard text")));
}

#[test]
fn ctrl_v_on_visible_spend_card_inserts_clipboard_text() {
    assert_clipboard_text_stays_a_message(Some(KeyModifiers::CONTROL));
}

#[test]
fn cmd_v_on_visible_spend_card_inserts_clipboard_text() {
    assert_clipboard_text_stays_a_message(Some(KeyModifiers::SUPER));
}

#[test]
fn ctrl_shift_v_on_visible_spend_card_inserts_clipboard_text() {
    assert_clipboard_text_stays_a_message(Some(KeyModifiers::CONTROL | KeyModifiers::SHIFT));
}

#[test]
fn cmd_shift_v_on_visible_spend_card_inserts_clipboard_text() {
    assert_clipboard_text_stays_a_message(Some(KeyModifiers::SUPER | KeyModifiers::SHIFT));
}

#[test]
fn captured_right_click_on_visible_spend_card_inserts_clipboard_text() {
    assert_clipboard_text_stays_a_message(None);
}

#[test]
fn large_multiline_paste_behind_card_submits_its_full_body() {
    let mut model = session_with_card_no_history();
    model.mode = RuntimeMode::Live;
    model.active_session = Some(SessionId::new("synthetic-reset-session"));
    model.handle(key(KeyCode::Down));
    let payload = format!("{}\nsecond\nthird\nfourth", "🔹".repeat(160));
    model.handle(AppEvent::Paste(payload.clone().into()));
    assert!(model.select_menu().is_none());
    model.handle(key(KeyCode::Enter));
    assert!(answered_keys(&model).is_empty());
    assert!(model.requests.iter().any(|request| matches!(request,
        AppRequest::SubmitText { text, .. } if text == &payload)));
}

#[test]
fn chip_spend_card_paste_uses_the_composer() {
    let mut model = chip_view_with_card();
    model.handle(key(KeyCode::Down));
    model.handle(AppEvent::Paste("chip synthetic paste".to_owned().into()));
    assert_eq!(model.composer.text(), "chip synthetic paste");
    assert!(!model.composer.is_empty());
    model.handle(key(KeyCode::Enter));
    assert!(answered_keys(&model).is_empty());
}

fn assert_chip_newline(modifiers: KeyModifiers) {
    let mut model = chip_view_with_card();
    model.handle(key(KeyCode::Down));
    model.handle(AppEvent::Key(KeyEvent::new(KeyCode::Enter, modifiers)));
    assert!(model.composer.text().contains('\n'));
    assert!(!model.composer.is_empty());
    assert!(answered_keys(&model).is_empty());
}

#[test]
fn chip_spend_card_alt_enter_is_a_newline() {
    assert_chip_newline(KeyModifiers::ALT);
}

#[test]
fn chip_spend_card_shift_enter_is_a_newline() {
    assert_chip_newline(KeyModifiers::SHIFT);
}

#[test]
fn chip_spend_card_attachment_only_hides_card() {
    let mut model = chip_view_with_card();
    model.mode = RuntimeMode::Live;
    model.active_session = Some(SessionId::new("synthetic-reset-session"));
    model.handle(key(KeyCode::Down));
    model.screen = Screen::Session;
    let _directory = attach_synthetic_file(&mut model);
    model.screen = Screen::Subagent;
    assert!(!model.composer.is_empty());
    model.handle(key(KeyCode::Enter));
    assert!(answered_keys(&model).is_empty());
}

#[test]
fn chip_spend_card_stale_hit_and_hover_are_dropped() {
    let mut model = chip_view_with_card();
    model.handle(key(KeyCode::Down));
    model.handle(key(KeyCode::Char('x')));
    let hit = Hit::MenuOption {
        menu: reset_card().id,
        index: 1,
    };
    model.handle_hover(Some(hit.clone()));
    model.handle_hit(hit);
    assert!(answered_keys(&model).is_empty());
    model.handle(key(KeyCode::Backspace));
    assert_eq!(
        model.menu_display_selection(
            model
                .viewed_chip()
                .and_then(ChipModel::question_menu)
                .expect("card")
        ),
        0
    );
}

#[test]
fn replayed_pending_card_in_fresh_model_paints_and_answers_deny() {
    let mut before_restart = session_with_card_no_history();
    before_restart.handle(key(KeyCode::Down));
    let pending = before_restart
        .projection
        .open_menu()
        .expect("pending card")
        .clone();

    let mut resumed = launcher_model();
    submit(&mut resumed, "resume synthetic session");
    resumed.outbox.clear();
    resumed.composer = haider_tui::composer::Composer::default();
    resumed.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(
        pending,
    ))));
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("terminal");
    terminal
        .draw(|frame| {
            haider_tui::render::render(&resumed, frame);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer();
    let painted = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(painted.contains("❯ 1. Deny"), "{painted}");
    resumed.handle(key(KeyCode::Enter));
    assert_eq!(answered_keys(&resumed), vec![Some("deny".to_owned())]);
}

#[test]
fn live_reply_retires_hidden_apply_navigation_before_the_next_key() {
    let mut model = session_with_card_no_history();
    let card = model.projection.open_menu().expect("card").clone();
    model.handle(key(KeyCode::Down));
    assert_eq!(model.menu_display_selection(&card), 1);
    let mut driver = LiveDriver::new("synthetic-reply");

    // A background overlay change and a daemon reply can both occur between
    // keyboard events. The reply epilogue must retire ownership right away.
    model.help_open = true;
    driver.apply(
        &mut model,
        LiveReply::Draining {
            reason: "synthetic".into(),
        },
    );
    model.help_open = false;
    assert_eq!(model.menu_display_selection(&card), 0);

    // A reply that leaves the card painted keeps a deliberate selection.
    model.handle(key(KeyCode::Down));
    assert_eq!(model.menu_display_selection(&card), 1);
    driver.apply(
        &mut model,
        LiveReply::Draining {
            reason: "synthetic".into(),
        },
    );
    assert_eq!(model.menu_display_selection(&card), 1);
}
