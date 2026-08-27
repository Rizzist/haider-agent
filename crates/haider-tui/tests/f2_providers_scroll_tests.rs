//! F2b — the providers page scrolls; the add-login buttons pin at the
//! bottom.
//!
//! Owner contract: "the providers page I should be able to scroll it and
//! bottom should have the add login buttons". Long rosters reach every
//! row (wheel, PageUp/PageDown, Home/End, cursor-follow); the add-login
//! actions — the SAME flows as before, just relocated — stay pinned in a
//! bottom footer, always reachable.
#![allow(clippy::expect_used)]

use haider_tui::app::{AccountAddKind, AppModel, AppRequest, Hit, Screen};
use haider_tui::mock::seed_provider_summaries;
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::Rect;

mod common;
use common::{key, launcher_model, run_slash};

fn draw(model: &AppModel, width: u16, height: u16) -> (Vec<String>, Vec<(Rect, Hit)>) {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut hits = Vec::new();
    terminal
        .draw(|frame| hits = render(model, frame))
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let rows = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect();
    (rows, hits)
}

/// A roster long enough to overflow any test viewport: the seed registry
/// plus synthetic providers, each a full block (header · family · models
/// · account · spacer).
fn long_roster_model() -> AppModel {
    let mut model = launcher_model();
    run_slash(&mut model, "/providers");
    assert_eq!(model.screen, Screen::Providers);
    model.requests.clear();
    let mut summaries = seed_provider_summaries();
    let template = summaries[0].clone();
    for index in 0..12 {
        let mut synthetic = template.clone();
        synthetic.provider = format!("synth-{index:02}");
        synthetic.default_model = None;
        summaries.push(synthetic);
    }
    model.providers.apply_snapshot(summaries, 1);
    model
}

/// MUTATION CHECK (F2b): pin the roster's scroll to zero (drop the
/// `.scroll` call). Expected runtime failure: `synth-11` never enters the
/// viewport and the End-key assertion fails.
#[test]
fn long_rosters_scroll_to_reach_every_row() {
    let mut model = long_roster_model();
    let (rows, _) = draw(&model, 100, 24);
    let text = rows.join("\n");
    assert!(text.contains("openai"), "the roster head renders first");
    assert!(
        !text.contains("synth-11"),
        "the tail overflows a 24-row frame (precondition)"
    );
    // End jumps to the bottom of the roster.
    model.handle(key(KeyCode::End));
    let (rows, _) = draw(&model, 100, 24);
    let text = rows.join("\n");
    assert!(
        text.contains("synth-11"),
        "End reaches the roster's last provider"
    );
    // Home returns to the head.
    model.handle(key(KeyCode::Home));
    let (rows, _) = draw(&model, 100, 24);
    assert!(
        rows.join("\n").contains("PROVIDERS"),
        "Home restores the head"
    );
}

/// PageDown/PageUp move the viewport in page steps and clamp at the true
/// range; the wheel steps by 3 (the transcript's step).
#[test]
fn page_keys_and_wheel_move_and_clamp() {
    let mut model = long_roster_model();
    let (_, _) = draw(&model, 100, 24);
    assert_eq!(model.providers.scroll.get(), 0);
    model.handle(key(KeyCode::PageDown));
    let after_page = model.providers.scroll.get();
    assert_eq!(after_page, 8, "PageDown steps by 8");
    model.handle_wheel(false);
    assert_eq!(
        model.providers.scroll.get(),
        after_page + 3,
        "wheel steps by 3"
    );
    model.handle_wheel(true);
    assert_eq!(
        model.providers.scroll.get(),
        after_page,
        "wheel up steps back"
    );
    // Clamp: hammering PageDown never exceeds the frame-written max.
    for _ in 0..50 {
        model.handle(key(KeyCode::PageDown));
    }
    draw(&model, 100, 24);
    let max = model.providers.scroll_max.get();
    assert!(max > 0, "the long roster really overflows");
    assert_eq!(
        model.providers.scroll.get(),
        max,
        "scroll clamps at the render-authored max"
    );
    model.handle(key(KeyCode::PageUp));
    assert_eq!(model.providers.scroll.get(), max.saturating_sub(8));
}

/// MUTATION CHECK (F2b): drop the follow-cursor reconciliation. Expected
/// runtime failure: after walking the cursor to the last provider the
/// hover-band row is off-screen and this law's visibility check fails.
#[test]
fn cursor_walk_keeps_the_selected_provider_visible() {
    let mut model = long_roster_model();
    let count = model.providers.providers.len();
    for _ in 0..count {
        model.handle(key(KeyCode::Down));
        let (rows, _) = draw(&model, 100, 24);
        let name = model.providers.providers[model.providers.cursor]
            .provider
            .clone();
        assert!(
            rows.iter().any(|row| row.contains(&name)),
            "the cursor's provider {name:?} must stay in view"
        );
    }
    assert_eq!(model.providers.cursor, count - 1);
}

/// The add-login buttons pin at the BOTTOM: visible without any
/// scrolling even when the roster overflows, wearing the same
/// value-carrying AccountAdd hits as before.
///
/// MUTATION CHECK (F2b/Q): flow the buttons back into the scroll body, omit
/// the custom-server row, or retag that row with a different `AccountAddKind`.
/// Expected runtime failure: the overflowing roster hides a pinned label, or
/// the label-to-action assertion loses the custom add flow.
#[test]
fn add_login_buttons_pin_at_the_bottom() {
    let model = long_roster_model();
    let (rows, hits) = draw(&model, 100, 24);
    let text = rows.join("\n");
    assert!(
        !text.contains("synth-11"),
        "the roster overflows (precondition)"
    );
    for (button, kind) in [
        ("+ OpenAI (OAuth)", AccountAddKind::OpenAiOAuth),
        ("+ Kimi (OAuth)", AccountAddKind::KimiOAuth),
        ("+ Add custom server", AccountAddKind::Custom),
    ] {
        assert!(
            text.contains(button),
            "{button:?} must be visible without scrolling"
        );
        assert!(
            hits.iter()
                .any(|(_, hit)| matches!(hit, Hit::AccountAdd(candidate) if *candidate == kind)),
            "the pinned {button:?} button keeps its add flow"
        );
    }
    let custom_row = rows
        .iter()
        .position(|row| row.contains("+ Add custom server"))
        .expect("custom button row");
    let custom_column = rows[custom_row]
        .find("+ Add custom server")
        .expect("custom button column");
    let custom_hit = hits
        .iter()
        .find_map(|(rect, hit)| {
            matches!(hit, Hit::AccountAdd(AccountAddKind::Custom)).then_some(rect)
        })
        .expect("custom button hit rectangle");
    assert!(
        custom_row >= usize::from(custom_hit.y)
            && custom_row < usize::from(custom_hit.y + custom_hit.height)
            && custom_column >= usize::from(custom_hit.x)
            && custom_column < usize::from(custom_hit.x + custom_hit.width),
        "the custom caption must lie inside its Custom hit rectangle"
    );
    // The buttons sit in the BOTTOM footer rows.
    let oauth_row = rows
        .iter()
        .position(|row| row.contains("+ OpenAI (OAuth)"))
        .expect("button row");
    // U1 widened the footer to FOUR button rows (OpenCode Zen/Go joined the
    // HF row's band); G4a added the local-preset row (Ollama/LM Studio) and
    // split the key map into action + preset hint lines; G4b added the
    // enterprise row (Azure/Bedrock/Vertex) and its own hint line; 940 added
    // Haider Code, which did not fit on the six-button API row at 100 cols
    // (~117 chars), so that row split 6 -> 4+3. Bottom band is now
    // footer(7) + hints(3) + status = 11.
    assert!(
        oauth_row >= rows.len() - 11,
        "buttons pin at the bottom band (footer + status bar), not mid-page (row {oauth_row})"
    );
    // Scrolling the roster leaves the footer put.
    let mut model = model;
    model.handle(key(KeyCode::End));
    let (rows, _) = draw(&model, 100, 24);
    assert!(
        rows.iter().any(|row| row.contains("+ OpenAI (OAuth)"))
            && rows.iter().any(|row| row.contains("+ Kimi (OAuth)"))
            && rows.iter().any(|row| row.contains("+ Add custom server")),
        "the complete footer survives scrolling to the end"
    );
}

/// The `⋮` gutter marks hidden content on the edge rows (the menu's
/// vocabulary), and vanish when everything fits.
#[test]
fn scroll_indicator_marks_hidden_content_honestly() {
    let mut model = long_roster_model();
    let (rows, _) = draw(&model, 100, 24);
    assert!(!rows[0].starts_with('⋮'), "nothing hidden above at the top");
    assert!(
        rows[..rows.len() - 4]
            .iter()
            .any(|row| row.starts_with('⋮')),
        "content hidden below wears the bottom mark"
    );
    model.handle(key(KeyCode::End));
    let (rows, _) = draw(&model, 100, 24);
    assert!(rows[0].starts_with('⋮'), "content hidden above at the end");

    // A short roster (the seed alone, including the named DeepSeek, xAI,
    // and Grok OAuth cards) at a tall frame: no marks at all.
    let mut short = launcher_model();
    run_slash(&mut short, "/providers");
    short.requests.clear();
    short.providers.apply_snapshot(seed_provider_summaries(), 1);
    let (rows, _) = draw(&short, 100, 64);
    assert!(
        !rows.iter().any(|row| row.starts_with('⋮')),
        "no phantom scroll marks when everything fits"
    );
}

/// Model-chip hits follow the scroll: after scrolling, a chip's rect
/// lands on the row that actually shows its provider block — value-
/// carrying hits stay aligned with the pixels.
#[test]
fn chip_hits_follow_the_scrolled_rows() {
    let mut model = long_roster_model();
    model.handle(key(KeyCode::End));
    let (rows, hits) = draw(&model, 100, 24);
    let (rect, provider) = hits
        .iter()
        .find_map(|(rect, hit)| match hit {
            Hit::ProviderModel { provider, .. } => Some((*rect, provider.clone())),
            _ => None,
        })
        .expect("a model chip is hittable after scrolling");
    // The chip row belongs to its provider's block: the header renders at
    // or above the chip row, within the block's height.
    let visible = (0..=rect.y)
        .rev()
        .take(5)
        .any(|y| rows[y as usize].contains(&provider));
    assert!(
        visible,
        "the chip rect (y={}) must sit inside {provider:?}'s visible block",
        rect.y
    );
}

/// Entering the providers screen still requests a refresh — the scroll
/// rework must not disturb the entry contract.
#[test]
fn entry_contract_still_requests_summaries() {
    let mut model = launcher_model();
    run_slash(&mut model, "/providers");
    assert!(
        model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::ProvidersRefresh)),
        "entering the screen requests summaries"
    );
}
