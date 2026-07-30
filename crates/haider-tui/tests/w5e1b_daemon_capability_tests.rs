//! W5e-1b — the two live bugs the owner's v0.0.18 run exposed:
//!
//! 1. A stale daemon (two days / five releases old) answered
//!    `invalid_argument — unknown session method` because the TUI offered a
//!    method it never checked was served. Report §4.1: "clients hide/disable
//!    only the methods whose feature is absent."
//! 2. With a CURRENT daemon the card then sat at "starting the loopback
//!    flow…" forever: `account.oauth_start` is non-durable, so its error
//!    reply carries no `command_id` and the generic `Failed` path could not
//!    correlate it to the card.
#![allow(clippy::expect_used)]

use haider_tui::app::{AppModel, Hit, OAuthAddPhase, RuntimeMode};
use haider_tui::link::{CommandContext, map_response};
use haider_tui::live::{LiveCommand, LiveReply};
use haider_tui::mock::seed_account_rows;

mod common;
use common::{launcher_model, run_slash};

fn live_accounts_model(features: &[&str]) -> AppModel {
    let mut model = launcher_model();
    // Live mode: the daemon is the authority, so the feature set gates.
    model.mode = RuntimeMode::Live;
    model.daemon_features = features.iter().map(|name| (*name).to_owned()).collect();
    model.daemon_version = Some("0.0.13".to_owned());
    run_slash(&mut model, "/accounts");
    model.requests.clear();
    model.accounts.apply_snapshot(seed_account_rows(), Some(1));
    model
}

/// MUTATION CHECK (W5e-1b): drop the `daemon_serves(...)` guard from the
/// OAuth arm of the `Hit::AccountAdd` handler (call `open_oauth_add`
/// unconditionally). Expected runtime failure: the card opens against a
/// daemon that never advertised the feature — the exact live failure that
/// produced `unknown session method`.
/// Verified by revert on 2026-07-30.
#[test]
fn a_daemon_without_the_oauth_feature_never_opens_the_card() {
    // A stale daemon: it serves login but not the OAuth PKCE family.
    let mut model = live_accounts_model(&["account_login_api_v1"]);
    model.handle_hit(Hit::AccountAdd(
        haider_tui::app::AccountAddKind::OpenAiOAuth,
    ));

    assert!(
        model.oauth_add.is_none(),
        "no card may open against a daemon that cannot serve the flow"
    );
    assert!(
        model.requests.is_empty(),
        "and no request may be sent — that is what failed obscurely live"
    );
    let message = model.accounts.message.as_deref().unwrap_or_default();
    assert!(
        message.contains("newer daemon") && message.contains("0.0.13"),
        "the refusal must name the stale daemon: {message:?}"
    );
}

/// The same click against a CURRENT daemon opens the card and requests.
#[test]
fn a_capable_daemon_opens_the_card() {
    let mut model = live_accounts_model(&[
        "account_login_api_v1",
        "account_oauth_pkce_v1",
        "account_management_v1",
    ]);
    model.handle_hit(Hit::AccountAdd(
        haider_tui::app::AccountAddKind::AnthropicOAuth,
    ));

    let card = model.oauth_add.as_ref().expect("card opens");
    assert_eq!(card.provider, "anthropic-oauth");
    assert!(matches!(card.phase, OAuthAddPhase::Starting));
    assert!(
        model.requests.iter().any(|request| matches!(
            request,
            haider_tui::app::AppRequest::OAuthAddStart { provider, .. }
                if provider == "anthropic-oauth"
        )),
        "the start request rides"
    );
}

/// Demo mode answers everything locally, so it is never gated.
#[test]
fn demo_mode_is_always_capable() {
    let mut model = launcher_model(); // demo by default
    run_slash(&mut model, "/accounts");
    model.requests.clear();
    assert!(
        model.daemon_features.is_empty(),
        "demo has no daemon features"
    );
    model.handle_hit(Hit::AccountAdd(
        haider_tui::app::AccountAddKind::OpenAiOAuth,
    ));
    assert!(
        model.oauth_add.is_some(),
        "demo must still open its simulated card"
    );
}

/// MUTATION CHECK (W5e-1b): remove the `context.oauth_attempt` branch from
/// `map_response`'s Error arm in `link.rs` (let the reply fall through to the
/// generic `Failed`). Expected runtime failure: the card below stays in
/// `Starting` forever instead of reporting the failure — the exact live
/// symptom ("starting the loopback flow…" with no progress).
/// Verified by revert on 2026-07-30.
#[test]
fn a_failed_start_reports_into_the_card_not_into_the_void() {
    let mut model = live_accounts_model(&["account_oauth_pkce_v1"]);
    model.handle_hit(Hit::AccountAdd(
        haider_tui::app::AccountAddKind::OpenAiOAuth,
    ));
    let attempt = model.oauth_add.as_ref().expect("card").attempt;
    assert!(matches!(
        model.oauth_add.as_ref().expect("card").phase,
        OAuthAddPhase::Starting
    ));

    // What the driver does when the identity-tagged failure lands.
    model.oauth_add_failed(
        attempt,
        "oauth_unavailable — provider registration unavailable",
    );

    match &model.oauth_add.as_ref().expect("card stays open").phase {
        OAuthAddPhase::Failed { message } => {
            assert!(message.contains("oauth_unavailable"));
        }
        other => panic!("the card must report the failure, got {other:?}"),
    }
}

/// MUTATION CHECK (W5e-1b, the one that matters): remove the
/// `context.oauth_attempt` branch from `map_response`'s Error arm in
/// `link.rs`. Expected runtime failure: the error maps to the generic
/// `LiveReply::Failed` (whose `command_id` is `None`, so nothing can
/// correlate it) instead of `OAuthStartFailed` — reproducing the live hang
/// at "starting the loopback flow…".
///
/// This drives `map_response` DIRECTLY. An earlier version of this suite
/// only called `AppModel::oauth_add_failed`, which exercises the model and
/// leaves the link mapping unpinned — the mutation survived. Pinning the
/// seam that actually broke is the point.
/// Verified by revert on 2026-07-30.
#[test]
fn the_link_maps_a_failed_oauth_start_to_an_identity_tagged_reply() {
    let context = CommandContext::of(&LiveCommand::OAuthStart {
        provider: "openai-oauth".into(),
        desired_alias: "openai-oauth".into(),
        attempt_id: "inst-oauth-7".into(),
    });
    assert_eq!(
        context.oauth_attempt.as_deref(),
        Some("inst-oauth-7"),
        "the start's attempt id must ride in its context"
    );

    let replies = map_response(
        &context,
        haider_rpc::ResponseBody::Error {
            code: "oauth_unavailable".into(),
            message: "provider registration unavailable".into(),
            retryable: false,
            data: None,
        },
    );
    match replies.as_slice() {
        [
            LiveReply::OAuthStartFailed {
                attempt_id,
                code,
                message,
            },
        ] => {
            assert_eq!(attempt_id, "inst-oauth-7");
            assert_eq!(code, "oauth_unavailable");
            assert_eq!(message, "provider registration unavailable");
        }
        other => panic!("a failed oauth_start must be identity-tagged, got {other:?}"),
    }
}
