//! F2c — the composer-top-rule identity and the trimmed status bar.
//!
//! Owner contract: the model / oauth-or-api / reasoning (+ fast) block
//! moves OFF the status bar and onto the composer band's TOP BORDER,
//! right-aligned, right above the talk chip — with NO account alias.
//! The status bar keeps the state badge with token usage DIRECTLY to its
//! right.
//!
//! WIDTH-DEGRADATION LAW: segments drop WHOLE — reasoning (its fast
//! marker riding it) first, then the auth label, then the entire line;
//! never a mid-word truncation.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::state::HarnessStatus;
use haider_tui::app::{AppEvent, AppModel, RuntimeMode};
use haider_tui::mock::{seed_account_rows, seed_provider_summaries};
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

mod common;
use common::launcher_model;

fn seeded_model() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.providers.apply_snapshot(seed_provider_summaries(), 1);
    model.accounts.apply_snapshot(seed_account_rows(), Some(1));
    model
}

fn draw_rows(model: &AppModel, width: u16, height: u16) -> Vec<String> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            render(model, frame);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect()
}

/// MUTATION CHECK (F2c): render the old plain gold rule (drop the
/// identity spans). Expected runtime failure: no rule row carries the
/// model name and the containment below fails.
#[test]
fn composer_top_rule_carries_model_auth_reasoning_and_no_alias() {
    let mut model = seeded_model();
    model.identity.provider = "anthropic".to_owned();
    model.identity.model_short = "fable-5".to_owned();
    model.identity.reasoning = Some("high".to_owned());
    let rows = draw_rows(&model, 100, 30);
    let rule = rows
        .iter()
        .find(|row| row.contains(" fable-5 · oauth · high ") && row.contains('─'))
        .expect("the top rule carries model · auth · reasoning");
    // Right-aligned: the identity sits at the rule's right end.
    assert!(
        rule.trim_end().ends_with("──"),
        "the identity is framed at the right end: {rule:?}"
    );
    // NO alias — the selected anthropic account's alias must not ride it.
    assert!(
        !rows.iter().any(|row| row.contains("personal-max")),
        "the account alias never rides the composer rule"
    );
}

/// The fast-mode marker rides the reasoning segment when active.
#[test]
fn fast_mode_marker_rides_the_reasoning_segment() {
    let mut model = seeded_model();
    model.identity.reasoning = Some("high".to_owned());
    model.identity.fast = true;
    assert_eq!(
        model.composer_identity(60).expect("identity fits"),
        "fable-5 · oauth · high · fast"
    );
}

/// WIDTH-DEGRADATION LAW: reasoning (with fast) drops first, then auth,
/// then the whole line — each candidate a WHOLE-segment string, never a
/// mid-word cut.
///
/// MUTATION CHECK (F2c): truncate instead of dropping (return the full
/// string cut to budget). Expected runtime failure: the mid-budget
/// candidates below stop equalling their whole-segment forms.
#[test]
fn width_degradation_drops_whole_segments_in_order() {
    let mut model = seeded_model();
    model.identity.provider = "anthropic".to_owned();
    model.identity.model_short = "fable-5".to_owned();
    model.identity.reasoning = Some("high".to_owned());
    model.identity.fast = true;
    let full = "fable-5 · oauth · high · fast";
    let no_reasoning = "fable-5 · oauth";
    let model_only = "fable-5";
    assert_eq!(model.composer_identity(80).as_deref(), Some(full));
    assert_eq!(
        model.composer_identity(full.chars().count()).as_deref(),
        Some(full)
    );
    // One short of the full form: reasoning + fast drop TOGETHER.
    assert_eq!(
        model
            .composer_identity(full.chars().count() - 1)
            .as_deref(),
        Some(no_reasoning)
    );
    // One short of model · auth: the auth label drops next.
    assert_eq!(
        model
            .composer_identity(no_reasoning.chars().count() - 1)
            .as_deref(),
        Some(model_only)
    );
    // Below the model name: the line vanishes whole — never "fabl…".
    assert_eq!(
        model.composer_identity(model_only.chars().count() - 1),
        None
    );
    // Every candidate the sweep can produce is a whole-segment form.
    for budget in 0..=80usize {
        if let Some(candidate) = model.composer_identity(budget) {
            assert!(
                [full, no_reasoning, model_only].contains(&candidate.as_str()),
                "unexpected degraded form {candidate:?} at {budget}"
            );
        }
    }
}

/// Auth derivation: the provider key's own `-oauth` encoding wins; then
/// the selected account's method; then a registry row declaring exactly
/// one method; ambiguity renders nothing.
#[test]
fn auth_label_derivation_is_truthful_never_a_guess() {
    let mut model = seeded_model();
    // Key encoding beats everything.
    model.identity.provider = "kimi-oauth".to_owned();
    assert_eq!(model.identity_auth_label(), Some("oauth"));
    // Selected account: anthropic's selected row is OAuth in the seed.
    model.identity.provider = "anthropic".to_owned();
    assert_eq!(model.identity_auth_label(), Some("oauth"));
    // Selected account: gemini's selected row is an API key.
    model.identity.provider = "gemini".to_owned();
    assert_eq!(model.identity_auth_label(), Some("api"));
    // No account, no registry row → honest nothing.
    model.accounts.rows.clear();
    model.providers.providers.clear();
    model.identity.provider = "anthropic".to_owned();
    assert_eq!(model.identity_auth_label(), None);
    assert_eq!(
        model.composer_identity(60).as_deref(),
        Some("fable-5"),
        "an unknown auth flavor degrades to the model alone"
    );
}

/// The status bar: token usage DIRECTLY right of the state badge —
/// nothing but spacing between `[ IDLE ]` and the meter.
///
/// MUTATION CHECK (F2c): reinsert the model · provider block between
/// badge and meter. Expected runtime failure: the between-slice check
/// finds non-space ink.
#[test]
fn status_bar_keeps_tokens_directly_right_of_the_state() {
    let model = seeded_model();
    let rows = draw_rows(&model, 100, 30);
    let bar = rows
        .iter()
        .find(|row| row.contains("[ IDLE ]"))
        .expect("status bar renders");
    let after_badge = bar
        .split("[ IDLE ]")
        .nth(1)
        .expect("content after the badge");
    let meter_at = after_badge.find(" tok ").expect("token meter on the bar");
    let between = &after_badge[..meter_at];
    assert!(
        between
            .chars()
            .all(|c| c == ' ' || c.is_ascii_digit() || c == '~' || c == '.' || c == 'k' || c == 'M'),
        "only the token count sits between state and meter: {between:?}"
    );
    assert!(
        !bar.contains(" · anthropic"),
        "the provider block left the status bar"
    );
}

/// Narrow dignity: when the bar cannot hold the meter beside the badge,
/// the meter yields WHOLE — no partial meter cells, badge untouched.
#[test]
fn meter_yields_whole_when_the_bar_is_narrow() {
    let model = seeded_model();
    let rows = draw_rows(&model, 30, 12);
    let text = rows.join("\n");
    assert!(text.contains("IDLE"), "badge survives narrow widths");
    assert!(
        !text.contains('▰') && !text.contains('▱'),
        "no partial meter at narrow widths"
    );
}

/// The rule identity renders on the launcher too — the default pair a
/// new session will use ("a session should be provider agnostic, just
/// choose model, have default model it works").
#[test]
fn launcher_rule_shows_the_default_pair_identity() {
    let mut model = AppModel::new();
    model.handle(AppEvent::Envelope(Box::new(EventPayload::HarnessStatus(
        HarnessStatus::Ready,
    ))));
    let rows = draw_rows(&model, 90, 28);
    assert!(
        rows.iter()
            .any(|row| row.contains('─') && row.contains(" fable-5 ")),
        "the launcher's composer rule names the default model"
    );
}
