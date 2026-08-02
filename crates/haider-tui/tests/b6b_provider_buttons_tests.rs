//! B6b — the two new providers in the account-add UX. The daemon owns both
//! flows (kimi's device-code grant, gemini's staged API key); the TUI's whole
//! job is the button, the gate, and the exact dispatch the older buttons
//! already perform:
//!
//! * `+ Kimi (OAuth)` mirrors the PKCE pair — the SAME card, the SAME
//!   `account.oauth_start` wire family, gated on the device-flow feature bit
//!   that shipped beside the `kimi-oauth` builtin (v0.0.52).
//! * `+ Gemini (API)` mirrors the API-key pair — the SAME masked login card,
//!   gated on provider-listing truth because the Gemini adapter (B6a,
//!   v0.0.54) shipped with no feature bit of its own.
#![allow(clippy::expect_used)]

use haider_tui::app::{AccountAddKind, AppModel, AppRequest, Hit, RuntimeMode, Screen};
use haider_tui::live::{LiveCommand, LiveDriver};
use haider_tui::mock::{seed_account_rows, seed_provider_summaries};
use haider_tui::render::render;
use haider_tui::runtime::live_pass;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

mod common;
use common::{launcher_model, run_slash};

/// A live model on the LAUNCHER with the given daemon feature set and the
/// seed account/provider snapshots applied (boot always issues
/// `account.list` + `provider.list`, so a connected TUI holds both before
/// any screen can be clicked).
fn live_model(features: &[&str]) -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.daemon_features = features.iter().map(|name| (*name).to_owned()).collect();
    model.daemon_version = Some("0.0.54".to_owned());
    model.accounts.apply_snapshot(seed_account_rows(), Some(1));
    model.providers.apply_snapshot(seed_provider_summaries(), 1);
    model
}

/// The same model, walked onto `/accounts` (the launcher composer owns the
/// slash grammar; the accounts screen itself has none).
fn live_accounts_model(features: &[&str]) -> AppModel {
    let mut model = live_model(features);
    run_slash(&mut model, "/accounts");
    model.requests.clear();
    model
}

fn draw(model: &AppModel, width: u16, height: u16) -> (String, Vec<(ratatui::layout::Rect, Hit)>) {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut hits = Vec::new();
    terminal
        .draw(|frame| {
            hits = render(model, frame);
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
    (text, hits)
}

/// LAW — the new buttons dispatch the daemon-owned flows through the exact
/// seams the older buttons use: kimi through `AppRequest::OAuthAddStart` →
/// `LiveCommand::OAuthStart` under provider `kimi-oauth`, gemini through the
/// masked login card under provider `gemini` (whose submit is the existing
/// vault.stage → account.login_api transaction).
///
/// MUTATION CHECK: map `AccountAddKind::KimiOAuth` to `"openai-oauth"` in
/// `open_oauth_add` (a plausible copy-paste). Expected RUNTIME failure: the
/// issued `OAuthStart` below carries the wrong provider.
/// Verified by revert on 2026-08-02.
#[test]
fn kimi_and_gemini_buttons_dispatch_the_daemon_flows() {
    // Kimi: the device-flow feature is served → the card opens and the
    // start request rides, exactly like the PKCE pair.
    let mut model = live_accounts_model(&["account_oauth_device_v1"]);
    let mut driver = LiveDriver::new("test");
    model.handle_hit(Hit::AccountAdd(AccountAddKind::KimiOAuth));
    let card = model.oauth_add.as_ref().expect("kimi card opens");
    assert_eq!(card.provider, "kimi-oauth");
    assert_eq!(card.alias, "kimi-oauth", "smallest free alias, §5.3");
    assert!(
        model.requests.iter().any(|request| matches!(
            request,
            AppRequest::OAuthAddStart { provider, alias, .. }
                if provider == "kimi-oauth" && alias == "kimi-oauth"
        )),
        "the start request rides: {:?}",
        model.requests
    );
    let issued = live_pass(&mut driver, &mut model, None, std::time::Instant::now()).commands;
    assert!(
        issued.iter().any(|command| matches!(
            command,
            LiveCommand::OAuthStart { provider, desired_alias, .. }
                if provider == "kimi-oauth" && desired_alias == "kimi-oauth"
        )),
        "the exact wire command the PKCE buttons issue, under the kimi id: {issued:?}"
    );

    // Gemini: the provider is listed → the masked login card opens under
    // `gemini` with the §5.3 alias prefill, exactly like the API-key pair.
    let mut model = live_accounts_model(&[]);
    model.handle_hit(Hit::AccountAdd(AccountAddKind::GeminiApi));
    let card = model.login.as_ref().expect("gemini login card opens");
    assert_eq!(card.provider, "gemini");
    assert_eq!(card.alias, "gemini-api", "smallest free `gemini-api[-N]`");
    assert!(
        model.oauth_add.is_none(),
        "an API add never opens the OAuth card"
    );
}

/// LAW — `/login kimi oauth` and `/login gemini api` parse into the same
/// dispatches as the buttons (the slash grammar is the buttons' keyboard
/// twin, report §6.3).
///
/// MUTATION CHECK: drop the `("kimi", "oauth")` arm from the `/login` match
/// (fall through to the generic oauth flash). Expected RUNTIME failure: no
/// card, no request, and the stale "lands after v0.0.12" flash below.
/// Verified by revert on 2026-08-02.
#[test]
fn slash_login_kimi_oauth_and_gemini_api_parse() {
    // `/login kimi oauth` jumps to /accounts (the card renders and owns
    // keys there) and runs the button's exact gated dispatch.
    let mut model = live_model(&["account_oauth_device_v1"]);
    run_slash(&mut model, "/login kimi oauth");
    assert_eq!(model.screen, Screen::Accounts);
    let card = model.oauth_add.as_ref().expect("kimi card opens via slash");
    assert_eq!(card.provider, "kimi-oauth");
    assert!(
        model.requests.iter().any(|request| matches!(
            request,
            AppRequest::OAuthAddStart { provider, .. } if provider == "kimi-oauth"
        )),
        "the slash route issues the same start request"
    );

    // `/login gemini api` opens the masked key card under `gemini` — the
    // generic `<provider> api` grammar now names a provider the daemon
    // actually serves (B6a).
    let mut model = live_model(&[]);
    run_slash(&mut model, "/login gemini api");
    let card = model.login.as_ref().expect("gemini card opens via slash");
    assert_eq!(card.provider, "gemini");
    assert_eq!(card.alias, "gemini-api");

    // The optional alias token still rides (§5.3 prefill precedence).
    let mut model = live_model(&[]);
    run_slash(&mut model, "/login gemini api work-gemini");
    assert_eq!(
        model.login.as_ref().expect("card").alias,
        "work-gemini",
        "the slash command's alias token wins over the derived prefill"
    );
}

/// LAW — a button whose provider the connected daemon cannot serve is
/// HONEST: no card, no request, the stale-daemon note names the running
/// version (report §4.1 for the feature-gated kimi flow; provider-listing
/// truth for the bit-less gemini adapter). Demo mode fabricates locally and
/// is never gated — the sim's own behavior.
///
/// MUTATION CHECK: make `daemon_lists_provider` return `true`
/// unconditionally. Expected RUNTIME failure: the gemini arm below opens a
/// card against a daemon that never listed the provider.
/// Verified by revert on 2026-08-02.
#[test]
fn ungated_provider_button_is_honest() {
    // A pre-B6k daemon: no device flow served.
    let mut model = live_accounts_model(&["account_oauth_pkce_v1"]);
    model.handle_hit(Hit::AccountAdd(AccountAddKind::KimiOAuth));
    assert!(
        model.oauth_add.is_none(),
        "no kimi card without the feature"
    );
    assert!(model.requests.is_empty(), "and nothing reaches the wire");
    let message = model.accounts.message.as_deref().unwrap_or_default();
    assert!(
        message.contains("newer daemon") && message.contains("0.0.54"),
        "the refusal names the stale daemon: {message:?}"
    );

    // A pre-B6a daemon: its provider.list never mentions gemini.
    let mut model = live_accounts_model(&[]);
    let registry: Vec<_> = seed_provider_summaries()
        .into_iter()
        .filter(|summary| summary.provider != "gemini")
        .collect();
    model.providers.apply_snapshot(registry, 2);
    model.handle_hit(Hit::AccountAdd(AccountAddKind::GeminiApi));
    assert!(model.login.is_none(), "no gemini card without the listing");
    let message = model.accounts.message.as_deref().unwrap_or_default();
    assert!(
        message.contains("newer daemon") && message.contains("0.0.54"),
        "the refusal names the stale daemon: {message:?}"
    );

    // Demo mode mirrors the sim: both buttons work with no daemon at all,
    // and kimi's `[1]` authorize lands the fabricated account under the
    // daemon-truth provider id.
    let mut model = launcher_model(); // demo by default
    run_slash(&mut model, "/accounts");
    model.requests.clear();
    model.handle_hit(Hit::AccountAdd(AccountAddKind::KimiOAuth));
    assert!(
        model.oauth_add.is_some(),
        "demo opens the kimi card ungated"
    );
    let attempt = model.oauth_add.as_ref().expect("card").attempt;
    model.oauth_add_phase(
        attempt,
        haider_tui::app::OAuthAddPhase::WaitingBrowser {
            url: "http://localhost:1455/callback (demo)".to_owned(),
            origin: "auth.kimi.com".to_owned(),
        },
    );
    model.handle(common::key(ratatui::crossterm::event::KeyCode::Char('1')));
    assert!(model.oauth_add.is_none(), "the simulated authorize closes");
    let row = model
        .accounts
        .rows
        .iter()
        .find(|row| row.provider == "kimi-oauth")
        .expect("the fabricated kimi account lands");
    assert_eq!(row.alias, "kimi-oauth");
    assert!(row.selected, "and is active for its provider");

    let mut model = launcher_model();
    run_slash(&mut model, "/accounts");
    model.requests.clear();
    model.handle_hit(Hit::AccountAdd(AccountAddKind::GeminiApi));
    assert!(
        model
            .login
            .as_ref()
            .is_some_and(|card| card.provider == "gemini"),
        "demo opens the gemini key card ungated"
    );
}

/// LAW — `/providers` offers the SAME add row (owner ask): the two new
/// buttons render there with live hit regions, and a click jumps home to
/// `/accounts` before opening its flow — identical to the older buttons.
///
/// MUTATION CHECK: drop the `+ Kimi (OAuth)` / `+ Gemini (API)` entries from
/// `push_account_add_buttons`. Expected RUNTIME failure: the render and
/// hit-region assertions below (on BOTH screens — the row is shared).
/// Verified by revert on 2026-08-02.
#[test]
fn providers_screen_shares_the_same_buttons() {
    let mut model = live_model(&["account_oauth_device_v1"]);
    run_slash(&mut model, "/providers");
    assert_eq!(model.screen, Screen::Providers);
    model.requests.clear();

    let (text, hits) = draw(&model, 130, 45);
    assert!(
        text.contains("[+ Kimi (OAuth)]"),
        "kimi renders on /providers"
    );
    assert!(
        text.contains("[+ Gemini (API)]"),
        "gemini renders on /providers"
    );
    assert!(
        hits.iter()
            .any(|(_, hit)| matches!(hit, Hit::AccountAdd(AccountAddKind::KimiOAuth))),
        "kimi carries a hit region"
    );
    assert!(
        hits.iter()
            .any(|(_, hit)| matches!(hit, Hit::AccountAdd(AccountAddKind::GeminiApi))),
        "gemini carries a hit region"
    );

    // A providers-screen click jumps home and opens the flow (the cards
    // and their keyboard ownership live on /accounts).
    model.handle_hit(Hit::AccountAdd(AccountAddKind::KimiOAuth));
    assert_eq!(model.screen, Screen::Accounts, "the add flow jumps home");
    assert!(
        model
            .oauth_add
            .as_ref()
            .is_some_and(|card| card.provider == "kimi-oauth"),
        "and opens the kimi card"
    );

    // The same row renders on /accounts — one shared function, and the
    // kimi card header names the honest grant (device code, not PKCE).
    let (text, _) = draw(&model, 130, 45);
    assert!(
        text.contains("[+ Kimi (OAuth)]"),
        "the row is shared with /accounts"
    );
    assert!(text.contains("[+ Gemini (API)]"));
    assert!(
        text.contains("authorize Kimi — Moonshot — OAuth (device code)"),
        "the card names the device-code grant"
    );
}
