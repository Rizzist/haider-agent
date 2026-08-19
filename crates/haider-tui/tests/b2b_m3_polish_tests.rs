//! B2b-m3 polish laws:
//!
//! * `/login openai|anthropic oauth` MIRRORS the account-add buttons by
//!   construction — the slash arm routes through the SAME `Hit::AccountAdd`
//!   dispatch (jump to /accounts, then the arm's feature gate and card open
//!   run unchanged), so the pre-W5e "lands after v0.0.12" stub can never
//!   resurface for a flow the buttons already run.
//! * The palette's `/login` slots and `HELP_TEXT` name the REAL rosters —
//!   no `google` provider, no stale oauth description.
//! * A device-code grant reports device-honest card copy: the wire's
//!   `WaitingDevice` status maps to "enter the code at <verification url>",
//!   never the loopback's "your browser opened…" line.
#![allow(clippy::expect_used)]

use haider_tui::app::{
    AccountAddKind, AppModel, AppRequest, Hit, OAuthAddPhase, RuntimeMode, Screen,
};
use haider_tui::commands::{DynamicSlots, HELP_TEXT, palette_items};
use haider_tui::live::{LiveCommand, LiveDriver, LiveReply};
use haider_tui::mock::{seed_account_rows, seed_provider_summaries};
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

mod common;
use common::{key, launcher_model, run_slash};

/// A live model on the LAUNCHER with the given daemon feature set and the
/// seed account/provider snapshots applied (boot always issues
/// `account.list` + `provider.list` before any screen can be clicked).
fn live_model(features: &[&str]) -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.daemon_features = features.iter().map(|name| (*name).to_owned()).collect();
    model.daemon_version = Some("0.0.55".to_owned());
    model.accounts.apply_snapshot(seed_account_rows(), Some(1));
    model.providers.apply_snapshot(seed_provider_summaries(), 1);
    model
}

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

// ---- (a) `/login … oauth` mirrors the buttons -------------------------

/// LAW — `/login openai oauth` and `/login anthropic oauth` run the exact
/// button dispatch: jump to /accounts, feature-gate on the PKCE bit, open
/// the SAME card, issue the SAME `OAuthAddStart`.
///
/// MUTATION CHECK: restore the pre-m3 generic `(_, "oauth")` stub for these
/// providers. Expected RUNTIME failure: no card, no request, and the stale
/// "lands after v0.0.12" flash below.
/// Verified by revert on 2026-08-03.
#[test]
fn slash_login_openai_and_anthropic_oauth_mirror_the_buttons() {
    for (provider, wire) in [("openai", "openai-oauth"), ("anthropic", "anthropic-oauth")] {
        let mut model = live_model(&["account_oauth_pkce_v1"]);
        run_slash(&mut model, &format!("/login {provider} oauth"));
        assert_eq!(
            model.screen,
            Screen::Accounts,
            "{provider}: jumps to /accounts"
        );
        let card = model
            .oauth_add
            .as_ref()
            .unwrap_or_else(|| panic!("{provider}: the oauth card opens via slash"));
        assert_eq!(card.provider, wire);
        assert!(
            model.requests.iter().any(|request| matches!(
                request,
                AppRequest::OAuthAddStart { provider, .. } if provider == wire
            )),
            "{provider}: the slash route issues the button's start request"
        );
        assert!(
            model
                .flash
                .as_deref()
                .is_none_or(|flash| !flash.contains("lands after")),
            "{provider}: the pre-W5e stub flash is gone"
        );
    }
}

/// LAW — the mirror includes the GATE: without the PKCE feature the slash
/// route refuses exactly like the button (stale-daemon note, no card, no
/// request) — mirror-by-construction means one arm, one gate.
#[test]
fn slash_login_oauth_shares_the_buttons_feature_gate() {
    let mut model = live_model(&[]);
    run_slash(&mut model, "/login anthropic oauth");
    assert_eq!(model.screen, Screen::Accounts);
    assert!(model.oauth_add.is_none(), "no card without the feature");
    assert!(
        !model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::OAuthAddStart { .. })),
        "no start request without the feature"
    );
    assert!(
        model
            .accounts
            .message
            .as_deref()
            .is_some_and(|message| message.contains("OAuth sign-in")),
        "the button's stale-daemon note names the gap: {:?}",
        model.accounts.message
    );
}

/// A provider with NO OAuth flow gets an honest, named refusal — not the
/// dead "lands after v0.0.12" promise.
#[test]
fn slash_login_oauth_refuses_a_provider_without_a_flow() {
    let mut model = live_model(&["account_oauth_pkce_v1"]);
    run_slash(&mut model, "/login gemini oauth");
    assert!(model.oauth_add.is_none());
    let flash = model.flash.as_deref().expect("an honest refusal");
    assert!(
        flash.contains("no OAuth flow") && flash.contains("gemini"),
        "the refusal names the provider and the fix: {flash}"
    );
    assert!(!flash.contains("lands after"), "stub copy is dead: {flash}");
}

// ---- palette + help rosters ------------------------------------------

/// LAW — the palette's `/login` slot 0 names the real named-provider roster
/// and slot 1's oauth row no longer promises a post-v0.0.12 landing.
///
/// MUTATION CHECK: restore the anthropic-only slot-0 table in
/// `commands::login_args`. Expected RUNTIME failure: the roster assertion.
/// Verified by revert on 2026-08-03.
#[test]
fn palette_login_slots_name_the_real_roster() {
    let slots = DynamicSlots::default();
    let providers: Vec<String> = palette_items("login ", true, &slots)
        .iter()
        .map(haider_tui::commands::PaletteItem::label)
        .collect();
    assert_eq!(
        providers,
        [
            "anthropic",
            "openai",
            "gemini",
            "kimi",
            "grok",
            "xai",
            "deepseek",
        ]
    );
    let methods = palette_items("login anthropic ", true, &slots);
    let oauth = methods
        .iter()
        .find(|item| item.label() == "oauth")
        .expect("oauth method row");
    assert!(
        !oauth.desc().contains("lands after"),
        "the stale landing promise is gone: {}",
        oauth.desc()
    );
    assert!(
        oauth.desc().contains("sign-in"),
        "the row describes the real flow: {}",
        oauth.desc()
    );
}

/// LAW — HELP_TEXT names the real provider roster: `google` was never a
/// provider id (the adapter is `gemini`), and `kimi` shipped in v0.0.52.
#[test]
fn help_text_names_the_real_provider_roster() {
    let provider_line = HELP_TEXT
        .iter()
        .find(|line| line.contains("/provider "))
        .expect("the /provider help line");
    assert!(
        provider_line.contains("gemini") && provider_line.contains("kimi"),
        "the real roster is named: {provider_line}"
    );
    assert!(
        !HELP_TEXT.iter().any(|line| line.contains("google")),
        "no help line names the `google` ghost provider"
    );
}

// ---- (c) device-honest WaitingDevice ---------------------------------

/// LAW — a Kimi DEVICE flow shows device-honest copy: the `WaitingDevice`
/// status maps onto its own card phase carrying the verification URL, the
/// card renders "enter the code at <url>", and `[1]` re-opens that URL.
///
/// MUTATION CHECK: let `WaitingDevice` keep falling through the tolerant
/// `_ => {}` arm in `LiveDriver`'s `OAuthFlowStatus` match. Expected
/// RUNTIME failure: the phase stays `WaitingBrowser` and the render below
/// still shows the loopback's "your browser opened…" line.
/// Verified by revert on 2026-08-03.
#[test]
fn waiting_device_maps_to_device_honest_copy() {
    let mut model = live_model(&["account_oauth_device_v1"]);
    run_slash(&mut model, "/accounts");
    model.requests.clear();
    let mut driver = LiveDriver::new("test");
    model.handle_hit(Hit::AccountAdd(AccountAddKind::KimiOAuth));
    let start = model
        .requests
        .drain(..)
        .find(|request| matches!(request, AppRequest::OAuthAddStart { .. }))
        .expect("the start request");
    let commands = driver.handle_request(&mut model, start);
    let Some(LiveCommand::OAuthStart { attempt_id, .. }) = commands.first() else {
        panic!("expected an OAuthStart command, got {commands:?}");
    };
    let attempt_id = attempt_id.clone();
    driver.apply(
        &mut model,
        LiveReply::OAuthStarted {
            attempt_id,
            availability: haider_rpc::OAuthAvailabilityWire {
                available: true,
                reason: None,
            },
            flow_id: Some(haider_rpc::OAuthFlowId::new("flow-1")),
            authorization_url: Some("https://auth.kimi.com/device?code=ABCD-1234".to_owned()),
            provider_origin: Some("auth.kimi.com".to_owned()),
        },
    );
    // The status poll reports the DEVICE waiting phase.
    driver.apply(
        &mut model,
        LiveReply::OAuthFlowStatus {
            flow_id: haider_rpc::OAuthFlowId::new("flow-1"),
            status: haider_rpc::OAuthFlowStatusWire::WaitingDevice,
        },
    );
    let card = model.oauth_add.as_ref().expect("card open");
    let OAuthAddPhase::WaitingDevice { url, origin } = &card.phase else {
        panic!(
            "WaitingDevice must map to its own phase, got {:?}",
            card.phase
        );
    };
    assert_eq!(url, "https://auth.kimi.com/device?code=ABCD-1234");
    assert_eq!(origin, "auth.kimi.com");
    let frame = draw(&model, 120, 36);
    assert!(
        frame.contains("enter the code at https://auth.kimi.com/device?code=ABCD-1234"),
        "device-honest copy renders"
    );
    assert!(
        !frame.contains("your browser opened"),
        "the loopback copy must not dress a device flow"
    );
    // `[1]` re-opens the VERIFICATION url.
    model.requests.clear();
    model.handle(key(ratatui::crossterm::event::KeyCode::Char('1')));
    assert!(
        model.requests.iter().any(|request| matches!(
            request,
            AppRequest::OpenUrl { url } if url == "https://auth.kimi.com/device?code=ABCD-1234"
        )),
        "[1] re-opens the verification url"
    );
}
