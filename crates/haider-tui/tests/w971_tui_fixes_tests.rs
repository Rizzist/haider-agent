//! 971 owner-reported desktop TUI defects — the accounts screen's scroll and
//! its one-pending-form law (F1), and the bottom band's placement (F2).
//!
//! F2. The composer sat on a DIFFERENT row on almost every surface: the
//! session screen parked a spacer row UNDER its closing rule (band one row
//! too high), and the loom/workflows tab ran its band to the last row of the
//! body with no closing rule at all (band one row too low). The main menu was
//! the one surface that had it right. `render::bottom_band` is the single
//! authority now, so the opening rule, the composer rows and the closing rule
//! land on identical rows of the body on every screen, and the status line
//! never crowds a composer row.
//!
//! F1. `/accounts` stacked the roster, the enrolled sources, a pending API-key
//! form AND a pending custom-server form in a fixed viewport, so at 120x40 the
//! provider grid and the hint line — the way out of the screen — were pushed
//! off the frame. The body scrolls now, at most one add-form is ever pending,
//! and the hint line is pinned to the last row.
//!
//! MUTATION CHECKS. Revert `bottom_band` to the per-surface layouts and the
//! cross-view equality test fails on the session, loom and workflows rows.
//! Drop the closing-rule row from the loom band and its composer moves down
//! one. Restore the `_gap` row under the session band and the moved-down-by-
//! one assertion fails. Drop the `accounts_replace_pending_add` gate and two
//! forms are pending at once. Remove the pinned hint and the last body row
//! stops being the hint line.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use haider_tui::app::{
    AccountAddKind, AppEvent, AppModel, ChipModel, Hit, LoomPane, RuntimeMode, Screen,
};
use haider_tui::mock::{seed_account_rows, seed_provider_summaries};
use haider_tui::render::render;
use haider_tui::script::{ChipDisplayState, ChipSeed};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;

mod common;
use common::launcher_model;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn key(model: &mut AppModel, code: KeyCode) {
    model.handle(AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)));
}

/// Draw one frame and return its rows plus the hit map.
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
                .map(|x| buffer[(x, y)].symbol().to_owned())
                .collect::<String>()
        })
        .collect();
    (rows, hits)
}

/// The two sizes the owner named: the terminal the defect was reported at
/// and the 80x24 floor.
const SIZES: [(u16, u16); 2] = [(120, 40), (80, 24)];

/// A live model with accounts and providers seeded — the state every screen
/// under test needs to render something real.
fn live_model() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.daemon_version = Some("0.0.971".to_owned());
    model.daemon_features = [
        haider_rpc::FEATURE_PROVIDER_CONFIGURE_V1.to_owned(),
        haider_rpc::FEATURE_ACCOUNT_OAUTH_PKCE_V1.to_owned(),
    ]
    .into_iter()
    .collect();
    model.accounts.apply_snapshot(seed_account_rows(), Some(1));
    model.providers.apply_snapshot(seed_provider_summaries(), 1);
    model.requests.clear();
    model
}

fn on_screen(screen: Screen) -> AppModel {
    let mut model = live_model();
    model.screen = screen;
    model
}

fn loom_model(pane: LoomPane) -> AppModel {
    let mut model = on_screen(Screen::Loom);
    model.loom_pane = pane;
    model
}

fn subagent_model() -> AppModel {
    let mut model = on_screen(Screen::Subagent);
    model.chips.push(ChipModel::from_seed(ChipSeed {
        agent: "t1-audit".to_owned(),
        parent: None,
        ros: None,
        callsign: "Ammar".to_owned(),
        hon: "(r)",
        full: "Ammar ibn Yasir".to_owned(),
        name: "audit".to_owned(),
        model: "fable-5".to_owned(),
        device: "test-lion-box".to_owned(),
        state: ChipDisplayState::Running,
        tokens: 100,
        prefill: Vec::new(),
    }));
    model.view_path = vec!["t1-audit".to_owned()];
    model
}

/// The surfaces that draw an input band with nothing under it. The main menu
/// leads: its placement is the reference the others were brought onto.
///
/// The subagent screen is deliberately absent: its SubTree ledger is drawn
/// BELOW the band by design (TUI6 item 6 — `❯ message …` → rule → `▾
/// subagents`), so its band is anchored above those rows rather than on the
/// last body row. Its shape is pinned by
/// `the_subagent_band_keeps_the_shared_shape_above_its_subtree`, and a
/// session screen with chips shows the same anatomy.
fn banded_views() -> Vec<(&'static str, AppModel)> {
    vec![
        ("main menu", on_screen(Screen::Launcher)),
        ("session", on_screen(Screen::Session)),
        ("loom", loom_model(LoomPane::Types)),
        ("workflows", loom_model(LoomPane::Workflows)),
        ("aura", on_screen(Screen::Aura)),
    ]
}

/// Every full-screen surface, banded or not — the status line is theirs too.
fn all_views() -> Vec<(&'static str, AppModel)> {
    let mut views = banded_views();
    views.push(("subagent", subagent_model()));
    views.push(("accounts", on_screen(Screen::Accounts)));
    views.push(("providers", on_screen(Screen::Providers)));
    views.push(("tools", on_screen(Screen::Tools)));
    views.push(("tree", on_screen(Screen::Tree)));
    views
}

// ---------------------------------------------------------------------------
// F2 — one shared bottom layout
// ---------------------------------------------------------------------------

#[test]
fn every_banded_view_places_the_composer_on_the_same_rows() {
    for (width, height) in SIZES {
        let mut reference: Option<(&str, Rect)> = None;
        for (name, model) in banded_views() {
            let _ = draw(&model, width, height);
            let rect = model
                .composer_rect
                .get()
                .unwrap_or_else(|| panic!("{name} at {width}x{height}: drew no composer"));
            // Fully visible: inside the frame, and clear of the status line.
            let status = model
                .status_rect
                .get()
                .unwrap_or_else(|| panic!("{name} at {width}x{height}: drew no status line"));
            assert!(
                rect.height > 0 && rect.y + rect.height <= height,
                "{name} at {width}x{height}: composer {rect:?} leaves the frame"
            );
            assert!(
                rect.y + rect.height <= status.y,
                "{name} at {width}x{height}: composer {rect:?} reaches the status line {status:?}"
            );
            match reference {
                None => reference = Some((name, rect)),
                Some((first, expected)) => assert_eq!(
                    rect, expected,
                    "{name} at {width}x{height}: composer {rect:?} differs from {first}'s \
                     {expected:?} — the bottom band is one layout for every view"
                ),
            }
        }
    }
}

#[test]
fn every_view_places_the_status_line_on_the_same_row() {
    for (width, height) in SIZES {
        let mut reference: Option<(&str, Rect)> = None;
        for (name, model) in all_views() {
            let _ = draw(&model, width, height);
            let rect = model
                .status_rect
                .get()
                .unwrap_or_else(|| panic!("{name} at {width}x{height}: drew no status line"));
            assert_eq!(
                rect,
                Rect::new(0, height - 1, width, 1),
                "{name} at {width}x{height}: the status line is not the last row"
            );
            match reference {
                None => reference = Some((name, rect)),
                Some((first, expected)) => assert_eq!(
                    rect, expected,
                    "{name} at {width}x{height}: status line {rect:?} differs from {first}'s"
                ),
            }
        }
    }
}

/// The band's anatomy, bottom-up: closing rule on the last body row, the
/// composer above it, the opening rule above that. Pinned on EVERY banded
/// surface, which is what makes the rows above agree.
#[test]
fn the_band_closes_with_a_rule_on_the_last_body_row_everywhere() {
    let is_rule = |row: &str| {
        let trimmed = row.trim_end();
        !trimmed.is_empty() && trimmed.chars().all(|c| c == '─' || c == ' ')
    };
    for (width, height) in SIZES {
        for (name, model) in banded_views() {
            let (rows, _) = draw(&model, width, height);
            let rect = model.composer_rect.get().expect("composer");
            let close = usize::from(rect.y + rect.height);
            assert_eq!(
                close,
                usize::from(height) - 2,
                "{name} at {width}x{height}: the closing rule is not the last body row"
            );
            assert!(
                is_rule(&rows[close]),
                "{name} at {width}x{height}: {:?} is not the band's closing rule",
                rows[close]
            );
        }
    }
}

/// The subagent screen keeps rows BELOW its band (the SubTree ledger), so its
/// band is anchored above them — the same three-row anatomy the shared layout
/// draws everywhere: opening rule, composer rows, closing rule, then whatever
/// the surface keeps underneath.
#[test]
fn the_subagent_band_keeps_the_shared_shape_above_its_subtree() {
    let is_rule = |row: &str| {
        let trimmed = row.trim_end();
        !trimmed.is_empty() && trimmed.chars().all(|c| c == '\u{2500}' || c == ' ')
    };
    for (width, height) in SIZES {
        let model = subagent_model();
        let (rows, _) = draw(&model, width, height);
        let rect = model.composer_rect.get().expect("subagent composer");
        let opening = usize::from(rect.y) - 1;
        let closing = usize::from(rect.y + rect.height);
        // The OPENING rule carries the surface identity at its right end
        // (`──── fable-5 ──`), so it is recognised by its leading glyph.
        assert!(
            rows[opening].starts_with('\u{2500}'),
            "subagent at {width}x{height}: no opening rule above the band: {:?}",
            rows[opening]
        );
        assert!(
            is_rule(&rows[closing]),
            "subagent at {width}x{height}: no closing rule under the band: {:?}",
            rows[closing]
        );
        assert!(
            rows[closing + 1].contains("subagents"),
            "subagent at {width}x{height}: the SubTree follows the closing rule: {:?}",
            rows[closing + 1]
        );
        assert!(
            usize::from(rect.y + rect.height) < usize::from(height) - 1,
            "subagent at {width}x{height}: the band leaves the frame"
        );
    }
}

/// The owner's measurement: the session band was ONE row too high. Reverting
/// the `_gap` row to its old slot under the closing rule reproduces the
/// pre-fix tops below, so the +1 here is the fix itself.
#[test]
fn the_session_composer_moved_down_exactly_one_row() {
    // Pre-fix session layout: body height − (closing rule + trailing gap) −
    // composer rows. 120x40 → 39 − 2 − 1 = 36; 80x24 → 23 − 2 − 1 = 20.
    const PRE_FIX_TOP: [(u16, u16, u16); 2] = [(120, 40, 36), (80, 24, 20)];
    for (width, height, pre_fix) in PRE_FIX_TOP {
        let model = on_screen(Screen::Session);
        let _ = draw(&model, width, height);
        let rect = model.composer_rect.get().expect("session composer");
        assert_eq!(
            rect.y,
            pre_fix + 1,
            "session at {width}x{height}: the composer's top row must sit exactly one \
             below the pre-fix row {pre_fix}"
        );
        // And the main menu — the reference — coincides with it.
        let menu = on_screen(Screen::Launcher);
        let _ = draw(&menu, width, height);
        assert_eq!(
            menu.composer_rect.get().expect("menu composer").y,
            rect.y,
            "session at {width}x{height}: the corrected row is the main menu's row"
        );
    }
}

// ---------------------------------------------------------------------------
// F1 — the accounts screen scrolls, and pins its way out
// ---------------------------------------------------------------------------

/// Enough accounts that the roster cannot fit — the owner's screenshot state.
fn crowded_accounts() -> AppModel {
    let mut model = live_model();
    let seeded = seed_account_rows();
    for round in 0..6 {
        for row in &seeded {
            let mut row = row.clone();
            row.alias = format!("{}-{round}", row.alias);
            row.selected = false;
            model.accounts.rows.push(row);
        }
    }
    model.screen = Screen::Accounts;
    model
}

#[test]
fn the_accounts_hint_line_stays_pinned_on_the_last_body_row() {
    for (width, height) in SIZES {
        let model = crowded_accounts();
        let (rows, _) = draw(&model, width, height);
        let last_body = usize::from(height) - 2;
        assert!(
            rows[last_body].contains("esc back"),
            "accounts at {width}x{height}: the hint line is not pinned to the last body \
             row, got {:?}",
            rows[last_body]
        );
        assert!(
            model.accounts.scroll_max.get() > 0,
            "accounts at {width}x{height}: a roster this tall must report scrollable rows"
        );
    }
}

#[test]
fn the_accounts_body_scrolls_under_the_keys_and_the_wheel() {
    let mut model = crowded_accounts();
    let _ = draw(&model, 120, 40);
    assert_eq!(model.accounts.scroll.get(), 0, "a visit opens at the top");

    key(&mut model, KeyCode::PageDown);
    let _ = draw(&model, 120, 40);
    let paged = model.accounts.scroll.get();
    assert!(paged > 0, "PgDn scrolls the body, got {paged}");

    key(&mut model, KeyCode::PageUp);
    let _ = draw(&model, 120, 40);
    assert_eq!(model.accounts.scroll.get(), 0, "PgUp scrolls back");

    key(&mut model, KeyCode::End);
    let _ = draw(&model, 120, 40);
    assert_eq!(
        model.accounts.scroll.get(),
        model.accounts.scroll_max.get(),
        "End reaches the foot of the body"
    );

    model.handle_wheel(true);
    let _ = draw(&model, 120, 40);
    assert!(
        model.accounts.scroll.get() < model.accounts.scroll_max.get(),
        "the wheel scrolls back up"
    );

    key(&mut model, KeyCode::Home);
    let _ = draw(&model, 120, 40);
    assert_eq!(model.accounts.scroll.get(), 0, "Home returns to the top");
}

/// The pending form is total-modal, so the frame must keep it on screen even
/// when the roster above it is far longer than the viewport — pinned above
/// the grid while the frame affords it, and SCROLLED INTO VIEW once the pin
/// ladder has to let it go.
#[test]
fn an_open_add_form_is_always_on_screen() {
    let mut model = crowded_accounts();
    let _ = draw(&model, 120, 40);
    model.handle_hit(Hit::AccountAdd(AccountAddKind::DeepSeekApi));
    assert!(model.login.is_some(), "the DeepSeek key card opens");

    // Roomy frame: the form is pinned, so no scrolling is needed for it and
    // the roster keeps its own offset.
    let (rows, _) = draw(&model, 120, 40);
    assert!(
        rows.iter().any(|row| row.contains("deepseek · API key")),
        "the pinned form is on screen: {rows:?}"
    );
    assert_eq!(
        model.accounts.scroll.get(),
        0,
        "a pinned form costs the roster no scrolling"
    );

    // Short frame: the ladder unpins the form into the scrolling body, and
    // the frame scrolls to it rather than leaving an invisible modal card
    // eating keystrokes (the W5g-5 trap).
    model.accounts.scroll.set(0);
    let (rows, _) = draw(&model, 120, 16);
    assert!(
        rows.iter().any(|row| row.contains("deepseek · API key")),
        "the unpinned form is scrolled into view: {rows:?}"
    );
    assert!(
        model.accounts.scroll.get() > 0,
        "the frame scrolled the body to reach it"
    );
}

#[test]
fn choosing_a_provider_replaces_an_untouched_pending_form() {
    let mut model = crowded_accounts();
    let _ = draw(&model, 120, 40);
    model.handle_hit(Hit::AccountAdd(AccountAddKind::DeepSeekApi));
    assert!(model.login.is_some());

    // Nothing typed → the pending form is discarded silently.
    model.handle_hit(Hit::AccountAdd(AccountAddKind::Custom));
    assert!(
        model.login.is_none(),
        "the untouched key card is discarded, not stacked"
    );
    assert!(
        model.custom_add.is_some(),
        "the custom card takes its place"
    );
    assert!(
        model.accounts.pending_replace.is_none(),
        "an untouched form asks nothing"
    );

    let (rows, _) = draw(&model, 120, 40);
    let forms = rows
        .iter()
        .filter(|row| row.contains("· API key") || row.contains("add custom server"))
        .count();
    assert_eq!(forms, 1, "exactly ONE pending form is on screen: {rows:?}");
}

#[test]
fn a_pending_form_holding_a_typed_key_asks_before_it_is_replaced() {
    let mut model = crowded_accounts();
    let _ = draw(&model, 120, 40);
    model.handle_hit(Hit::AccountAdd(AccountAddKind::DeepSeekApi));
    for character in "SECRET-971".chars() {
        key(&mut model, KeyCode::Char(character));
    }
    assert!(
        model.login.as_ref().expect("card").masked_len() > 0,
        "the card holds typed bytes"
    );

    model.handle_hit(Hit::AccountAdd(AccountAddKind::Custom));
    assert_eq!(
        model.accounts.pending_replace,
        Some(AccountAddKind::Custom),
        "the discard is held for a confirm"
    );
    assert!(model.login.is_some(), "nothing is discarded yet");
    assert!(model.custom_add.is_none(), "and nothing new opened");
    let (rows, _) = draw(&model, 120, 40);
    assert!(
        rows.iter().any(|row| row.contains("discard the pending")),
        "one line asks: {rows:?}"
    );

    // esc keeps what is there — the typed key survives.
    key(&mut model, KeyCode::Esc);
    assert!(model.accounts.pending_replace.is_none());
    assert!(
        model.login.as_ref().expect("card").masked_len() > 0,
        "esc keeps the pending key"
    );

    // ⏎ authorises the discard and opens the chosen form.
    model.handle_hit(Hit::AccountAdd(AccountAddKind::Custom));
    key(&mut model, KeyCode::Enter);
    assert!(
        model.login.is_none(),
        "the confirmed discard closed the card"
    );
    assert!(model.custom_add.is_some(), "the chosen form opened");
}
