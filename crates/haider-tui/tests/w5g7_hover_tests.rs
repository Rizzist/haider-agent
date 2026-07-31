//! W5g-7 — hover truth under a moving frame (owner report #2: "hovering
//! location is still off, mainly the main tui menu").
//!
//! Root cause pins: the compact-banner hit-row shift lives in
//! `hit_alignment_tests::launcher_row_hits_align_on_the_compact_banner_path`;
//! this file pins the LOOP laws — a redraw that moves targets under a
//! stationary pointer must not keep painting the old highlight, and must
//! never let the pointer steal keyboard navigation.
#![allow(clippy::expect_used)]

use haider_tui::app::{AccountAddKind, Hit};
use haider_tui::runtime::settle_hover_after_draw;
use ratatui::layout::Rect;

mod common;
use common::launcher_model;

fn rect(y: u16) -> Rect {
    Rect {
        x: 0,
        y,
        width: 80,
        height: 1,
    }
}

/// MUTATION CHECK (W5g-7): revert `settle_hover_after_draw` to the old
/// identity-vanish cleanup (keep hover whenever the identity exists
/// anywhere in the map). Expected runtime failure: the moved-target case
/// below keeps the stale highlight — the owner's mismatched hover.
#[test]
fn a_target_that_moved_under_a_stationary_pointer_loses_its_highlight() {
    let mut model = launcher_model();
    let target = Hit::AccountAdd(AccountAddKind::OpenAiOAuth);
    model.handle_hover(Some(target.clone()));
    model.dirty = false;

    // The redraw moved the target from the pointer's row (5) to row 8.
    let map = vec![(rect(8), target)];
    settle_hover_after_draw(&mut model, &map, Some((10, 5)));
    assert!(model.hovered.is_none(), "the painted highlight was a lie");
    assert!(model.dirty, "and the next frame must drop it");
}

/// A pointer still over its target keeps the highlight — settling is not
/// a hover kill-switch.
#[test]
fn a_target_still_under_the_pointer_keeps_its_highlight() {
    let mut model = launcher_model();
    let target = Hit::AccountAdd(AccountAddKind::OpenAiOAuth);
    model.handle_hover(Some(target.clone()));
    model.dirty = false;

    let map = vec![(rect(5), target.clone())];
    settle_hover_after_draw(&mut model, &map, Some((10, 5)));
    assert_eq!(model.hovered, Some(target));
    assert!(!model.dirty, "nothing changed — no repaint");
}

/// MUTATION CHECK (W5g-7): make the settle ADOPT the newly resolved hit
/// instead of clearing. Expected runtime failure: the assertion below —
/// adoption would impose pointer selection on every keyboard-driven
/// redraw, stealing palette/menu navigation from the keys.
#[test]
fn settling_never_adopts_the_new_target_under_the_pointer() {
    let mut model = launcher_model();
    let old = Hit::AccountAdd(AccountAddKind::OpenAiOAuth);
    let new = Hit::AccountAdd(AccountAddKind::AnthropicOAuth);
    model.handle_hover(Some(old));
    model.dirty = false;

    // The redraw put a DIFFERENT target under the stationary pointer.
    let map = vec![(rect(5), new)];
    settle_hover_after_draw(&mut model, &map, Some((10, 5)));
    assert!(
        model.hovered.is_none(),
        "cleared, never adopted — real motion re-arms hover"
    );
}
