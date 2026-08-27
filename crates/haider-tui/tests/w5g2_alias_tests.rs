//! W5g-2 — §5.3: the credential alias is a VISIBLE, EDITABLE field.
//!
//! The API login card grows a prefilled alias row (smallest free
//! `«provider»-api[-N]` against the live snapshot, or the slash token),
//! tab moves the keystrokes between alias and key, and a FAILED OAuth
//! card lets the user edit the alias in place and retry the flow under
//! it — the §5.3 collision recovery.
#![allow(clippy::expect_used)]

use haider_protocol::credential::{AuthMethod, CredentialStatus};
use haider_tui::app::{
    AccountRow, AppEvent, AppModel, AppRequest, Hit, LoginFocus, OAuthAddPhase, RuntimeMode,
    smallest_free_alias,
};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

mod common;
use common::{launcher_model, run_slash};

fn key(model: &mut AppModel, code: KeyCode) {
    model.handle(AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)));
}

fn type_text(model: &mut AppModel, text: &str) {
    for c in text.chars() {
        key(model, KeyCode::Char(c));
    }
}

fn row(alias: &str) -> AccountRow {
    AccountRow {
        alias: alias.to_owned(),
        provider: "openai".to_owned(),
        method: AuthMethod::ApiKey,
        identity: "x".to_owned(),
        account_identity: None,
        created_at_ms: None,
        status: CredentialStatus::Ok,
        selected: false,
        base_url: None,
    }
}

fn live_model() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model
}

/// MUTATION CHECK (W5g-2): make the prefill skip the taken-alias scan
/// (always the bare `«provider»-api`). Expected runtime failure: the card
/// below prefills `openai-api` although that alias is taken.
#[test]
fn the_card_prefills_the_smallest_free_alias() {
    let mut model = live_model();
    model
        .accounts
        .apply_snapshot(vec![row("openai-api"), row("openai-api-2")], None);
    run_slash(&mut model, "/login openai api");
    let card = model.login.as_ref().expect("card open");
    assert_eq!(card.alias, "openai-api-3");
    assert_eq!(card.focus, LoginFocus::Key, "the key is the common path");
    // The helper itself, pinned: base free → base; base taken → -2.
    assert_eq!(smallest_free_alias("a", &[]), "a");
    assert_eq!(smallest_free_alias("a", &[row("a")]), "a-2");
}

/// MUTATION CHECK (W5g-2): route every printable to the KEY regardless of
/// focus. Expected runtime failure: after tab, typing grows the mask
/// instead of the alias — and the secrecy assertion below (alias text
/// never absorbs key bytes) dies with it.
#[test]
fn tab_moves_the_keystrokes_between_fields() {
    let mut model = live_model();
    run_slash(&mut model, "/login openai api");
    type_text(&mut model, "abc");
    {
        let card = model.login.as_ref().expect("card");
        assert_eq!(card.masked_len(), 3, "key field owns the first strokes");
        assert_eq!(card.alias, "openai-api");
    }
    key(&mut model, KeyCode::Tab);
    // Uppercase folds; illegal characters vanish at the keyboard.
    type_text(&mut model, "-X2!");
    key(&mut model, KeyCode::Backspace);
    let card = model.login.as_ref().expect("card");
    assert_eq!(card.alias, "openai-api-x", "grammar-filtered alias editing");
    assert_eq!(card.masked_len(), 3, "the key never absorbs alias strokes");
}

/// MUTATION CHECK (W5g-2): submit `alias: None` (the pre-§5.3 wire shape,
/// letting the daemon fabricate `openai-api-«hash»`). Expected runtime
/// failure: the queued `LoginApi` below carries no alias.
#[test]
fn the_submit_carries_the_edited_alias() {
    let mut model = live_model();
    run_slash(&mut model, "/login openai api");
    key(&mut model, KeyCode::Tab);
    type_text(&mut model, "-work");
    key(&mut model, KeyCode::Tab);
    type_text(&mut model, "sk-test-key");
    model.requests.clear();
    key(&mut model, KeyCode::Enter);
    assert!(
        model.requests.iter().any(|request| matches!(
            request,
            AppRequest::LoginApi { alias: Some(alias), .. } if alias == "openai-api-work"
        )),
        "the submit rides under the user's visible alias"
    );
}

/// MUTATION CHECK (W5g-2): drop the grammar gate on submit. Expected
/// runtime failure: the empty-alias submit below queues a `LoginApi` the
/// daemon would bounce — and wipes the typed key doing it.
#[test]
fn a_bad_alias_refuses_the_submit_and_keeps_the_key() {
    let mut model = live_model();
    run_slash(&mut model, "/login openai api");
    type_text(&mut model, "sk-test-key");
    key(&mut model, KeyCode::Tab);
    for _ in 0.."openai-api".len() {
        key(&mut model, KeyCode::Backspace);
    }
    key(&mut model, KeyCode::Tab);
    model.requests.clear();
    key(&mut model, KeyCode::Enter);
    let card = model.login.as_ref().expect("card stays open");
    assert!(
        !model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::LoginApi { .. })),
        "nothing the daemon would bounce reaches the wire"
    );
    assert_eq!(card.focus, LoginFocus::Alias, "the field to fix is focused");
    assert_eq!(card.masked_len(), "sk-test-key".len(), "the key survives");
}

/// The slash command's optional token still prefills (compat), folded to
/// the grammar's case.
/// MUTATION CHECK (W5g-2): drop the token fold/filter. Expected runtime
/// failure: the card below opens with `WORK-Key` verbatim.
#[test]
fn the_slash_token_prefills_folded() {
    let mut model = live_model();
    run_slash(&mut model, "/login openai api WORK-Key");
    assert_eq!(model.login.as_ref().expect("card").alias, "work-key");
}

/// MUTATION CHECK (W5g-2): make ⏎ on a FAILED OAuth card cancel (the old
/// key map) instead of retrying. Expected runtime failure: no
/// `OAuthAddStart` under the edited alias, and the card is gone.
#[test]
fn a_failed_oauth_card_retries_under_the_edited_alias() {
    let mut model = launcher_model();
    run_slash(&mut model, "/accounts");
    model.requests.clear();
    model.handle_hit(Hit::AccountAdd(
        haider_tui::app::AccountAddKind::OpenAiOAuth,
    ));
    let attempt = model.oauth_add.as_ref().expect("card").attempt;
    model.oauth_add_failed(attempt, "alias is already committed");
    assert!(matches!(
        model.oauth_add.as_ref().expect("card").phase,
        OAuthAddPhase::Failed { .. }
    ));

    // Digits are alias characters now, so `[1]`/`[2]` yield to typing.
    type_text(&mut model, "-2");
    model.requests.clear();
    key(&mut model, KeyCode::Enter);

    let card = model.oauth_add.as_ref().expect("card survives the retry");
    assert_eq!(card.alias, "openai-oauth-2");
    assert!(matches!(card.phase, OAuthAddPhase::Starting));
    assert!(
        card.attempt > attempt,
        "a retry is a NEW issuance — the failed id is dead forever"
    );
    assert!(model.requests.iter().any(|request| matches!(
        request,
        AppRequest::OAuthAddStart { alias, attempt: sent, .. }
            if alias == "openai-oauth-2" && *sent == card.attempt
    )));
}
