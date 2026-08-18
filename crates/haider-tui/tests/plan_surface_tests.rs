//! D4 — the `plan` proposal surface: an open `origin: "plan"` menu renders
//! its full markdown document in the transcript area (scrollable), while the
//! accept/revise/reject decision stays in the composer band through the
//! ordinary blocking-menu machinery.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::ids::MenuId;
use haider_protocol::menu::{Menu, MenuKind, MenuOption, MenuScope};
use haider_tui::app::{AppEvent, AppModel};
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::KeyCode;

mod common;
use common::{key, launcher_model, submit};

fn plan_menu(lines: usize) -> Menu {
    let mut body = vec![
        "# Datacenter build-out".to_owned(),
        String::new(),
        "- edge pops".to_owned(),
        "- core compute".to_owned(),
    ];
    for index in 0..lines {
        body.push(format!("filler line {index}"));
    }
    Menu {
        id: MenuId::new("plan-menu-1"),
        kind: MenuKind::Choice,
        title: "Datacenter build-out".to_owned(),
        body,
        options: vec![
            MenuOption {
                key: "accept".into(),
                label: "Accept".into(),
                detail: None,
                decision: None,
            },
            MenuOption {
                key: "revise".into(),
                label: "Revise".into(),
                detail: None,
                decision: None,
            },
            MenuOption {
                key: "reject".into(),
                label: "Reject".into(),
                detail: None,
                decision: None,
            },
        ],
        blocking: true,
        scope: MenuScope::Session,
        origin: "plan".into(),
        ttl_ms: None,
        timeout_option: None,
    }
}

fn draw(model: &AppModel) -> Vec<String> {
    let backend = TestBackend::new(100, 30);
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
                .map(|x| buffer[(x, y)].symbol().to_owned())
                .collect::<String>()
        })
        .collect()
}

/// MUTATION CHECK: stop routing the plan body to the transcript surface, or
/// let the composer band render the whole document. Expected RUNTIME failure:
/// the header/markdown rows are missing, or the band pointer disappears.
#[test]
fn plan_document_fills_the_transcript_and_options_keep_the_band() {
    let mut model = launcher_model();
    submit(&mut model, "propose the datacenter");
    model.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(
        plan_menu(0),
    ))));

    let rows = draw(&model);
    let all = rows.join("\n");
    assert!(all.contains("◇ PLAN"), "plan header missing:\n{all}");
    assert!(
        all.contains("Datacenter build-out"),
        "plan title missing:\n{all}"
    );
    assert!(all.contains("edge pops"), "markdown body missing:\n{all}");
    // The decision options render through the ordinary menu band.
    assert!(all.contains("Accept"), "accept option missing:\n{all}");
    assert!(all.contains("Revise"), "revise option missing:\n{all}");
    assert!(all.contains("Reject"), "reject option missing:\n{all}");
    // The band carries the pointer, never the whole document again.
    assert!(
        all.contains("proposal above"),
        "band pointer missing:\n{all}"
    );
}

/// MUTATION CHECK: drop the scroll clamp or the ↓ scroll path. Expected
/// RUNTIME failure: scrolling never reveals the deep filler line, or a huge
/// scroll blanks the surface.
#[test]
fn plan_document_scrolls_and_clamps() {
    let mut model = launcher_model();
    submit(&mut model, "propose it");
    model.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(
        plan_menu(80),
    ))));

    let before = draw(&model).join("\n");
    assert!(before.contains("# Datacenter build-out") || before.contains("Datacenter build-out"));
    assert!(
        !before.contains("filler line 79"),
        "the deep line must start off-screen"
    );

    for _ in 0..200 {
        model.handle(key(KeyCode::Down));
    }
    let after = draw(&model).join("\n");
    assert!(
        after.contains("filler line 79"),
        "scrolling must reach the document tail:\n{after}"
    );
    // Clamped: the surface still shows content, never a blank page.
    assert!(after.contains("filler line"), "clamp failed:\n{after}");

    for _ in 0..300 {
        model.handle(key(KeyCode::Up));
    }
    let back = draw(&model).join("\n");
    assert!(
        back.contains("edge pops"),
        "scrolling back must restore the top:\n{back}"
    );
}
