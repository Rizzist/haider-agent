//! 971 owner-reported desktop TUI defects — the accounts screen's scroll and
//! its one-pending-form law (F1), and the bottom band's placement (F2).
//!
//! F2. The composer sat on a DIFFERENT row on almost every surface: the
//! session screen parked a spacer row UNDER its closing rule (band one row
//! too high), and the loom/workflows tab ran its band to the last row of the
//! body with no closing rule at all (band one row too low). The main menu was
//! the one surface that had it right. `render::bottom_band` is the single
//! authority now, so the opening rule, the slot and the closing rule land on
//! identical rows of the body on every screen, and the status line never
//! crowds a slot row.
//!
//! Verify round 1 (Astra) closed the two exemptions round 0 left: the SubTree
//! ledger, which used to be drawn BELOW the band and lifted the session and
//! child views three rows off everyone else, is ordinary content above the
//! band now; and `/accounts`, which has no composer at all, draws the same
//! band with its pinned key-map line in the slot.
//!
//! F1. `/accounts` stacked the roster, the enrolled sources, a pending API-key
//! form AND a pending custom-server form in a fixed viewport, so at 120x40 the
//! provider grid and the hint line — the way out of the screen — were pushed
//! off the frame. The body scrolls now, at most one add-form is ever pending,
//! and the hint line is pinned to the last row.
//!
//! F2 AMENDMENT (971-tui-collapse, owner 2026-09-08). The band grew a fourth
//! part: a live background-task line between the slot and the closing rule
//! (`▸▸ bypass permissions on · 6 shells, 14 monitors`). `bottom_band` draws
//! it itself, precisely so no surface can place it differently or omit it —
//! the F2 law's own logic, extended.
//!
//! It is STATE-DEPENDENT, so the cross-view equality here is now stated over
//! the band's ANATOMY rather than over one absolute row: every view puts the
//! slot the same distance above the bottom GIVEN the same background-task
//! state. That is the property the owner's complaint was about — switching
//! views must not move the composer — and it is checked directly by
//! `one_model_places_the_band_identically_on_every_screen`, which walks one
//! model (one background-task state, as a running session actually has)
//! across every screen. The fixtures below deliberately differ in that state
//! (`session with ledger` runs a child), which is why the absolute row
//! differs by exactly the line each model carries.
//!
//! MUTATION CHECKS. Revert `bottom_band` to the per-surface layouts and the
//! cross-view equality test fails on the session, loom and workflows rows.
//! Drop the closing-rule row from the loom band and its composer moves down
//! one. Restore the `_gap` row under the session band and the moved-down-by-
//! one assertion fails. Put the SubTree ledger back under the band and the
//! ledger views leave the equality set. Drop the `accounts_replace_pending_add`
//! gate and two forms are pending at once. Remove the pinned band and the last
//! body rows stop being rule/hint/rule. Move the accounts paging gestures back
//! behind form modality and PgDn/wheel die inside a pending card again. Let a
//! repeated grid hit answer the confirm and a typed key is dropped with no ⏎.
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

fn chip(agent: &str, name: &str) -> ChipModel {
    ChipModel::from_seed(ChipSeed {
        agent: agent.to_owned(),
        parent: None,
        ros: None,
        callsign: "Ammar".to_owned(),
        hon: "(r)",
        full: "Ammar ibn Yasir".to_owned(),
        name: name.to_owned(),
        model: "fable-5".to_owned(),
        device: "test-lion-box".to_owned(),
        state: ChipDisplayState::Running,
        tokens: 100,
        prefill: Vec::new(),
    })
}

fn subagent_model() -> AppModel {
    let mut model = on_screen(Screen::Subagent);
    model.chips.push(chip("t1-audit", "audit"));
    model.view_path = vec!["t1-audit".to_owned()];
    model
}

/// A session with the SubTree ledger open. Verify round 1 (Astra) found this
/// shape three rows off everyone else's, because the ledger used to be drawn
/// BELOW the band; it is ordinary content above the band now, so this view
/// belongs in the equality set like any other.
fn session_with_ledger() -> AppModel {
    let mut model = on_screen(Screen::Session);
    model.chips.push(chip("t1-docs", "docs"));
    model
}

/// The surfaces that draw the shared bottom band with a COMPOSER in its slot.
/// The main menu leads: its placement is the reference the others were
/// brought onto.
fn composer_views() -> Vec<(&'static str, AppModel)> {
    vec![
        ("main menu", on_screen(Screen::Launcher)),
        ("session", on_screen(Screen::Session)),
        ("session with ledger", session_with_ledger()),
        ("subagent", subagent_model()),
        ("loom", loom_model(LoomPane::Types)),
        ("workflows", loom_model(LoomPane::Workflows)),
        ("aura", on_screen(Screen::Aura)),
    ]
}

/// Every surface that draws the band.
///
/// OWNER RULING (verify round 2, accepted): `/accounts` gets the band's
/// GEOMETRY AND FRAMING with its key-map line in the slot, and no composer.
/// The screen is a single-key surface — `r` reveals, `x` removes, every bare
/// printable key is a command — so a composer there would take the keyboard
/// away from the screen's whole vocabulary. The invariant this suite holds it
/// to is therefore `band_rect`, identical to every other view's, while
/// `composer_rect` stays `None` here.
fn banded_views() -> Vec<(&'static str, AppModel)> {
    let mut views = composer_views();
    views.push(("accounts", on_screen(Screen::Accounts)));
    views
}

/// Every full-screen surface, banded or not — the status line is theirs too.
///
/// DOCUMENTED EXCLUSION (verify round 2, accepted for this lane): the
/// read-only report screens — `/providers`, `/usage`, `/tools`, `/hooks`,
/// `/tree`, the fleet, graph and session browsers — are asserted on the
/// STATUS ROW only, not on the band. They draw their own footers, and those
/// footers are not one row: `/providers` pins the provider grid plus three
/// hint lines (actions, presets, enterprise), which cannot occupy a one-row
/// band slot without redesigning the footer itself. Bringing them onto the
/// band is a separate lane, not a gap in this one.
fn all_views() -> Vec<(&'static str, AppModel)> {
    let mut views = banded_views();
    views.push(("providers", on_screen(Screen::Providers)));
    views.push(("tools", on_screen(Screen::Tools)));
    views.push(("tree", on_screen(Screen::Tree)));
    views
}

// ---------------------------------------------------------------------------
// F2 — one shared bottom layout
// ---------------------------------------------------------------------------

/// EVERY view that draws the band — a session with a running child and
/// `/accounts` included — puts its slot on the same rows.
#[test]
fn every_banded_view_places_the_band_on_the_same_rows() {
    for (width, height) in SIZES {
        let mut reference: Option<(&str, Rect)> = None;
        for (name, model) in banded_views() {
            let _ = draw(&model, width, height);
            let rect = model
                .band_rect
                .get()
                .unwrap_or_else(|| panic!("{name} at {width}x{height}: drew no band"));
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
            // The slot's distance from the bottom, with the band's own
            // background-task line discounted (see the F2 AMENDMENT): every
            // view places the slot identically GIVEN the same task state.
            let line = model.tasks_line_rect.get().map_or(0, |line| line.height);
            let anatomy = Rect {
                y: rect.y + line,
                ..rect
            };
            match reference {
                None => reference = Some((name, anatomy)),
                Some((first, expected)) => assert_eq!(
                    anatomy, expected,
                    "{name} at {width}x{height}: band slot {rect:?} (+{line} task rows) \
                     differs from {first}'s {expected:?} — the bottom band is one \
                     layout for every view"
                ),
            }
        }
    }
}

/// The property the owner's F2 complaint was actually about, stated on ONE
/// model — one background-task state, as a running session actually has —
/// walked across every screen: switching views must not move the composer.
#[test]
fn one_model_places_the_band_identically_on_every_screen() {
    for (width, height) in SIZES {
        let mut reference: Option<(Screen, Rect)> = None;
        for screen in [
            Screen::Launcher,
            Screen::Session,
            Screen::Loom,
            Screen::Aura,
            Screen::Accounts,
        ] {
            let mut model = session_with_ledger();
            model.screen = screen;
            let _ = draw(&model, width, height);
            let rect = model
                .band_rect
                .get()
                .unwrap_or_else(|| panic!("{screen:?} at {width}x{height}: drew no band"));
            match reference {
                None => reference = Some((screen, rect)),
                Some((first, expected)) => assert_eq!(
                    rect, expected,
                    "{screen:?} at {width}x{height}: band slot {rect:?} differs from \
                     {first:?}'s {expected:?} — one model must place the band on the \
                     same rows on every screen"
                ),
            }
        }
    }
}

/// And on every view whose slot holds a COMPOSER, the composer IS that slot.
#[test]
fn every_composer_view_fills_the_shared_band_slot() {
    for (width, height) in SIZES {
        for (name, model) in composer_views() {
            let _ = draw(&model, width, height);
            let composer = model
                .composer_rect
                .get()
                .unwrap_or_else(|| panic!("{name} at {width}x{height}: drew no composer"));
            let band = model.band_rect.get().expect("band");
            assert_eq!(
                composer, band,
                "{name} at {width}x{height}: the composer is not the band's slot"
            );
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
            let rect = model.band_rect.get().expect("band");
            // The band's live background-task line, when it drew one, sits
            // between the slot and the closing rule (F2 AMENDMENT).
            let line = model.tasks_line_rect.get().map_or(0, |line| line.height);
            let close = usize::from(rect.y + rect.height + line);
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

/// The SubTree ledger is ordinary content ABOVE the band now — the seam it
/// once had below the band is what put the child view's composer three rows
/// off everyone else's (verify round 1). The ledger still reads straight into
/// the band with a rule between and no blank row.
#[test]
fn the_subtree_ledger_reads_into_the_band_from_above() {
    for (width, height) in SIZES {
        for (name, model) in [
            ("subagent", subagent_model()),
            ("session with ledger", session_with_ledger()),
        ] {
            let (rows, _) = draw(&model, width, height);
            let rect = model.band_rect.get().expect("band");
            let opening = usize::from(rect.y) - 1;
            let ledger = rows
                .iter()
                .position(|row| row.contains("subagents"))
                .unwrap_or_else(|| panic!("{name} at {width}x{height}: no ledger: {rows:?}"));
            assert!(
                ledger < opening,
                "{name} at {width}x{height}: the ledger sits above the band"
            );
            // The OPENING rule carries the surface identity at its right end
            // (`──── fable-5 ──`), so it is recognised by its leading glyph.
            assert!(
                rows[opening].starts_with('\u{2500}'),
                "{name} at {width}x{height}: no opening rule above the band: {:?}",
                rows[opening]
            );
            assert!(
                rows[ledger..opening]
                    .iter()
                    .all(|row| !row.trim().is_empty()),
                "{name} at {width}x{height}: a blank row splits the ledger from the band: {:?}",
                &rows[ledger..opening]
            );
        }
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
fn the_accounts_hint_line_stays_pinned_in_the_band_slot() {
    for (width, height) in SIZES {
        let model = crowded_accounts();
        let (rows, _) = draw(&model, width, height);
        // Verify round 1: the key map lives in the SHARED band's slot, so it
        // is the row above the closing rule — the row a composer occupies on
        // every other view.
        let slot = model.band_rect.get().expect("accounts band");
        assert_eq!(usize::from(slot.y), usize::from(height) - 3);
        assert!(
            rows[usize::from(slot.y)].contains("esc back"),
            "accounts at {width}x{height}: the hint line is not in the band slot, got {:?}",
            rows[usize::from(slot.y)]
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

// ---------------------------------------------------------------------------
// Verify round 1 (Astra) — the gestures a pending form used to swallow, and
// the click that used to answer its own question
// ---------------------------------------------------------------------------

/// A pending add-form is modal for TEXT, never for navigation. PageUp/PageDown
/// and the wheel are screen gestures and reach the roster with a DeepSeek key
/// card or a custom-server card open — Astra measured byte-identical frames
/// before and after each of them at both sizes.
#[test]
fn a_pending_form_never_swallows_the_paging_gestures() {
    for kind in [AccountAddKind::DeepSeekApi, AccountAddKind::Custom] {
        for (width, height) in SIZES {
            let mut model = crowded_accounts();
            let _ = draw(&model, width, height);
            model.handle_hit(Hit::AccountAdd(kind));
            assert!(
                model.login.is_some() || model.custom_add.is_some(),
                "{kind:?}: the form opens"
            );
            let _ = draw(&model, width, height);
            let rest = model.accounts.scroll.get();

            key(&mut model, KeyCode::PageDown);
            let (paged_rows, _) = draw(&model, width, height);
            let paged = model.accounts.scroll.get();
            assert!(
                paged > rest,
                "{kind:?} at {width}x{height}: PgDn died inside the form ({rest} → {paged})"
            );
            assert!(
                model.login.is_some() || model.custom_add.is_some(),
                "{kind:?} at {width}x{height}: paging must not close the form"
            );
            // The focused field is still on screen — paging the roster under
            // a pinned form never hides what the keystrokes are landing in.
            assert!(
                paged_rows
                    .iter()
                    .any(|row| row.contains("API key") || row.contains("add custom server")),
                "{kind:?} at {width}x{height}: the form left the frame: {paged_rows:?}"
            );

            key(&mut model, KeyCode::PageUp);
            let _ = draw(&model, width, height);
            assert!(
                model.accounts.scroll.get() < paged,
                "{kind:?} at {width}x{height}: PgUp died inside the form"
            );

            model.handle_wheel(false);
            let _ = draw(&model, width, height);
            let wheeled = model.accounts.scroll.get();
            model.handle_wheel(true);
            let _ = draw(&model, width, height);
            assert!(
                model.accounts.scroll.get() < wheeled,
                "{kind:?} at {width}x{height}: the wheel died inside the form"
            );
        }
    }
}

/// Typed secret bytes are dropped by the explicit ⏎ and by NOTHING else. A
/// repeated grid click only re-aims the question, however many times it lands
/// (Astra: two clicks on `+ Add custom server` discarded a typed DeepSeek key
/// with no Enter between them).
#[test]
fn a_repeated_grid_click_never_answers_the_discard_confirm() {
    let mut model = crowded_accounts();
    let _ = draw(&model, 120, 40);
    model.handle_hit(Hit::AccountAdd(AccountAddKind::DeepSeekApi));
    for character in "SECRET-971".chars() {
        key(&mut model, KeyCode::Char(character));
    }
    let typed = model.login.as_ref().expect("card").masked_len();
    assert!(typed > 0);

    for click in 1..=4 {
        model.handle_hit(Hit::AccountAdd(AccountAddKind::Custom));
        assert_eq!(
            model.accounts.pending_replace,
            Some(AccountAddKind::Custom),
            "click {click}: the question stays pending"
        );
        assert_eq!(
            model.login.as_ref().map(|card| card.masked_len()),
            Some(typed),
            "click {click}: the typed key survives an unanswered click"
        );
        assert!(model.custom_add.is_none(), "click {click}: nothing opened");
    }

    // A click on a DIFFERENT provider re-aims the same question.
    model.handle_hit(Hit::AccountAdd(AccountAddKind::GeminiApi));
    assert_eq!(
        model.accounts.pending_replace,
        Some(AccountAddKind::GeminiApi),
        "the question follows the latest choice"
    );
    assert!(model.login.is_some(), "and still nothing was discarded");

    // Only ⏎ drops the bytes, and it opens exactly the last choice.
    key(&mut model, KeyCode::Enter);
    assert!(model.accounts.pending_replace.is_none());
    assert_eq!(
        model.login.as_ref().map(|card| card.provider.clone()),
        Some("gemini".to_owned()),
        "the confirmed choice is the one that opened"
    );
}

/// Verify round 1 (Astra): the scroll CLAMP needed coverage that a mutation
/// removing it would fail. A banked offset is reconciled against the frame
/// that actually measured the body — a taller frame or a shorter roster both
/// shrink the true maximum, and an offset kept past it scrolls the reader
/// into blank space below the last account.
#[test]
fn a_banked_accounts_offset_folds_to_the_frame_that_measured_it() {
    let has_rows = |rows: &[String]| {
        rows.iter()
            .any(|row| row.contains("[api key]") || row.contains("[oauth]"))
    };

    // (a) The frame grows.
    let mut model = crowded_accounts();
    let _ = draw(&model, 80, 24);
    key(&mut model, KeyCode::End);
    let _ = draw(&model, 80, 24);
    let deep = model.accounts.scroll.get();
    assert!(deep > 0, "a deep offset is banked at the short frame");

    let (rows, _) = draw(&model, 80, 70);
    let max = model.accounts.scroll_max.get();
    assert!(
        deep > max,
        "the banked offset really is past the taller frame's end"
    );
    assert!(
        model.accounts.scroll.get() <= max,
        "the offset folds to the frame's own max ({} > {max})",
        model.accounts.scroll.get()
    );
    assert!(
        has_rows(&rows),
        "the taller frame shows roster rows, not blank space past the end: {rows:?}"
    );

    // (b) The roster shrinks under a banked offset.
    let mut model = crowded_accounts();
    let _ = draw(&model, 80, 24);
    key(&mut model, KeyCode::End);
    let _ = draw(&model, 80, 24);
    let deep = model.accounts.scroll.get();
    model.accounts.apply_snapshot(seed_account_rows(), Some(2));
    let (rows, _) = draw(&model, 80, 24);
    let max = model.accounts.scroll_max.get();
    assert!(deep > max, "the shorter roster really moves the end up");
    assert!(
        model.accounts.scroll.get() <= max,
        "the offset folds when the daemon replaces the rows"
    );
    assert!(
        has_rows(&rows),
        "the shorter roster is on screen, not scrolled past: {rows:?}"
    );
}
