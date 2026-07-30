//! W5d `/providers` (report §5.2): owner-directed layout (PROVISIONAL until
//! the v0.0.15 install-probe sign-off), the CAS-fenced default-model law,
//! and honest unavailable rows.
#![allow(clippy::expect_used)]

use haider_tui::app::{AppEvent, AppModel, AppRequest, Screen};
use haider_tui::mock::{seed_account_rows, seed_provider_summaries};
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

mod common;
use common::{launcher_model, run_slash};

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

fn providers_model() -> AppModel {
    let mut model = launcher_model();
    run_slash(&mut model, "/providers");
    assert_eq!(model.screen, Screen::Providers);
    assert!(
        model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::ProvidersRefresh)),
        "entering the screen must request summaries"
    );
    model.requests.clear();
    model
        .providers
        .apply_snapshot(seed_provider_summaries(), 1);
    model
}

/// PROVISIONAL layout (owner design gate, report §5.2): head · provider
/// rows (name + health) · family/endpoint · model chips with `*` default ·
/// account projection + [accounts] · hints. Built-ins appear even when
/// unavailable, with an honest reason.
#[test]
fn providers_screen_renders_the_provisional_layout() {
    let mut model = providers_model();
    // With the accounts snapshot loaded, the account line projects it.
    model.accounts.apply_snapshot(seed_account_rows(), None);
    let frame = draw(&model, 110, 40);

    assert!(frame.contains("PROVIDERS — registry truth · accounts live in /accounts"));
    assert!(frame.contains("openai  ● available"));
    assert!(frame.contains("responses · https://api.openai.com/v1"));
    assert!(frame.contains("models: gpt-5.6  gpt-5.6-codex*  o4-mini"));
    assert!(frame.contains("account: work-chatgpt · oauth · in use"));
    // Unavailable built-in stays visible with its honest reason (§5.2).
    assert!(frame.contains("google  ○ adapter not installed"));
    assert!(frame.contains("models: —"));
    // Custom/local rows render their endpoint truthfully.
    assert!(frame.contains("openai-compatible · http://127.0.0.1:8000/v1"));
    assert!(frame.contains("click a model to make it the provider default"));
}

/// LAW — the default marker moves only on the correlated, revision-gated
/// reply (the /accounts §5.1 law applied to management).
///
/// MUTATION CHECK: make `set_default_model` write
/// `summary.default_model = Some(model)` locally before pushing the
/// request. Expected runtime failure: the "marker must not move before the
/// reply" assertion below.
/// Verified by revert on 2026-07-30.
#[test]
fn default_marker_moves_only_on_the_correlated_reply() {
    let mut model = providers_model();

    model.set_default_model("openai", "o4-mini");
    assert_eq!(
        model.providers.pending_default,
        Some(("openai".to_owned(), "o4-mini".to_owned()))
    );
    assert!(model.requests.iter().any(|request| matches!(
        request,
        AppRequest::SetDefaultModel { provider, model, expected_revision }
            if provider == "openai" && model == "o4-mini" && *expected_revision == 1
    )));
    let openai = model
        .providers
        .providers
        .iter()
        .find(|summary| summary.provider == "openai")
        .expect("openai summary");
    assert_eq!(
        openai.default_model.as_deref(),
        Some("gpt-5.6-codex"),
        "the marker must not move before the reply"
    );
    // Second request while pending is refused.
    model.requests.clear();
    model.set_default_model("anthropic", "claude-sonnet-5");
    assert!(model.requests.is_empty(), "pending default gates re-entry");

    // Correlated reply lands: marker moves, revision stamps.
    let mut committed = seed_provider_summaries()
        .into_iter()
        .find(|summary| summary.provider == "openai")
        .expect("openai seed");
    committed.default_model = Some("o4-mini".to_owned());
    model.apply_default_model_set(committed, 2);
    assert!(model.providers.pending_default.is_none());
    let openai = model
        .providers
        .providers
        .iter()
        .find(|summary| summary.provider == "openai")
        .expect("openai summary");
    assert_eq!(openai.default_model.as_deref(), Some("o4-mini"));
    assert_eq!(model.providers.revision, Some(2));
    assert_eq!(
        model.providers.message.as_deref(),
        Some("✓ openai default → o4-mini")
    );
}

/// LAW — a stale committed reply (older revision) releases the gate but
/// cannot rewrite newer rows.
///
/// MUTATION CHECK: drop the `revision < current` comparison in
/// `ProvidersState::apply_default_set`. Expected runtime failure: the stale
/// reply below rewrites the openai default.
/// Verified by revert on 2026-07-30.
#[test]
fn a_stale_default_result_cannot_regress_newer_rows() {
    let mut model = providers_model();
    model
        .providers
        .apply_snapshot(seed_provider_summaries(), 9);

    model.set_default_model("openai", "o4-mini");
    let mut stale = seed_provider_summaries()
        .into_iter()
        .find(|summary| summary.provider == "openai")
        .expect("openai seed");
    stale.default_model = Some("o4-mini".to_owned());
    model.apply_default_model_set(stale, 3);
    assert!(model.providers.pending_default.is_none(), "gate released");
    let openai = model
        .providers
        .providers
        .iter()
        .find(|summary| summary.provider == "openai")
        .expect("openai summary");
    assert_eq!(
        openai.default_model.as_deref(),
        Some("gpt-5.6-codex"),
        "a stale reply must not rewrite newer rows"
    );
    assert_eq!(model.providers.revision, Some(9));
}

/// Local refusals: an out-of-inventory model and the already-default model
/// never produce an RPC.
#[test]
fn out_of_inventory_and_already_default_refuse_locally() {
    let mut model = providers_model();

    model.requests.clear();
    model.set_default_model("openai", "gpt-9-imaginary");
    assert!(model.requests.is_empty());
    assert_eq!(
        model.providers.message.as_deref(),
        Some("· openai has no model \"gpt-9-imaginary\" configured")
    );

    model.set_default_model("openai", "gpt-5.6-codex");
    assert!(model.requests.is_empty());
    assert_eq!(
        model.providers.message.as_deref(),
        Some("· gpt-5.6-codex is already openai's default")
    );
    assert!(model.providers.pending_default.is_none());
}

/// A revision_conflict failure releases the gate AND refreshes the snapshot
/// (the CAS proved it stale).
#[test]
fn revision_conflict_releases_the_gate_and_refreshes() {
    let mut model = providers_model();
    model.set_default_model("openai", "o4-mini");
    model.requests.clear();

    model.default_model_failed("openai", "expected management revision 1, current revision is 4", true);
    assert!(model.providers.pending_default.is_none());
    assert!(
        model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::ProvidersRefresh)),
        "a conflict must refresh the stale snapshot"
    );
    assert_eq!(
        model.providers.message.as_deref(),
        Some("· openai: expected management revision 1, current revision is 4")
    );
}

/// Esc routing matches /accounts: no session → launcher.
#[test]
fn esc_returns_to_launcher() {
    let mut model = providers_model();
    model.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Esc,
        KeyModifiers::NONE,
    )));
    assert_eq!(model.screen, Screen::Launcher);
}
