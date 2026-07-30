//! W4a4: the mutation-approval card renders its patch preview DIFF-AWARE —
//! `-` preimage lines in the error tone, `+` replacement lines in the ok
//! tone, `---`/`+++` headers faint — instead of one undifferentiated dim
//! block. The daemon's `approval_preview` (haider-tools filesystem.rs)
//! emits exactly those prefixes.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::menu::{Menu, MenuKind, MenuOption, MenuScope};
use haider_tui::app::AppModel;
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

mod common;
use common::{launcher_model, submit};

fn approval_menu() -> Menu {
    Menu {
        id: haider_protocol::ids::MenuId::new("permission-w4a4-1"),
        kind: MenuKind::Permission {
            effect_summary: "patch src/lib.rs".to_owned(),
        },
        title: "Allow patch src/lib.rs?".to_owned(),
        body: vec![
            "Target: src/lib.rs".to_owned(),
            "Structured exact-preimage hunk:\n--- expected\n+++ replacement\n-    let x = 1;\n+    let x = 2;".to_owned(),
            "Effect class: FsWrite".to_owned(),
        ],
        options: vec![
            MenuOption {
                key: "approve_once".to_owned(),
                label: "Approve once".to_owned(),
                detail: None,
                decision: None,
            },
            MenuOption {
                key: "deny".to_owned(),
                label: "Deny".to_owned(),
                detail: None,
                decision: None,
            },
        ],
        blocking: true,
        scope: MenuScope::Session,
        origin: "effect_broker".to_owned(),
        ttl_ms: None,
        timeout_option: None,
    }
}

/// Draw and return (text grid, per-row first-content-cell fg color).
fn draw_with_styles(model: &AppModel) -> (Vec<String>, Vec<Option<ratatui::style::Color>>) {
    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            render(model, frame);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let mut rows = Vec::new();
    let mut row_fg = Vec::new();
    for y in 0..buffer.area.height {
        let mut text = String::new();
        for x in 0..buffer.area.width {
            text.push_str(buffer[(x, y)].symbol());
        }
        // The first non-space cell's fg is the row's body tone.
        let fg = (0..buffer.area.width)
            .find(|&x| buffer[(x, y)].symbol() != " ")
            .and_then(|x| buffer[(x, y)].style().fg);
        rows.push(text);
        row_fg.push(fg);
    }
    (rows, row_fg)
}

/// MUTATION CHECK: collapse `DiffTone::of` to always `Body` (or delete the
/// prefix classification). Expected runtime failure: the `-`/`+` rows below
/// share one fg color and the distinct-tone assertions fail.
/// Verified by revert on 2026-07-30.
#[test]
fn approval_card_colors_the_patch_preview_by_diff_prefix() {
    let mut model = launcher_model();
    submit(&mut model, "patch the file");
    model.handle(haider_tui::app::AppEvent::Envelope(Box::new(
        EventPayload::MenuOpened(approval_menu()),
    )));

    let (rows, row_fg) = draw_with_styles(&model);
    let find_row = |needle: &str| {
        rows.iter()
            .position(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("row containing {needle:?} not rendered"))
    };

    let del = find_row("-    let x = 1;");
    let add = find_row("+    let x = 2;");
    let meta = find_row("--- expected");
    let body = find_row("Target: src/lib.rs");

    let fg =
        |index: usize| row_fg[index].unwrap_or_else(|| panic!("row {index} has no content fg"));
    assert_ne!(
        fg(del),
        fg(add),
        "preimage and replacement rows must carry DIFFERENT tones"
    );
    assert_ne!(
        fg(add),
        fg(body),
        "replacement rows must not blend into the dim body tone"
    );
    assert_ne!(
        fg(del),
        fg(body),
        "preimage rows must not blend into the dim body tone"
    );
    assert_ne!(
        fg(meta),
        fg(add),
        "diff headers stay meta-faint, not add-green"
    );
    // The options and title still render (the card stays a first-class
    // blocking menu — coloring must not break the card chrome).
    assert!(
        rows.iter()
            .any(|row| row.contains("Allow patch src/lib.rs?"))
    );
    assert!(rows.iter().any(|row| row.contains("1. Approve once")));
    assert!(rows.iter().any(|row| row.contains("2. Deny")));
}
