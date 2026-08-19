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

/// Review round 2 MUTATION CHECK: drop the state-side clamp, the render-time
/// new-proposal reset, or the reconnect-independent scroll ceiling. Expected
/// RUNTIME failures: one ↑ after heavy overscroll does not move the view, or
/// plan B opens at plan A's offset before any keypress.
#[test]
fn overscroll_clamps_in_state_and_new_plans_open_at_the_top() {
    let mut model = launcher_model();
    submit(&mut model, "propose it");
    model.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(
        plan_menu(80),
    ))));
    // First paint installs the scroll ceiling for the key handler.
    drop(draw(&model));

    for _ in 0..200 {
        model.handle(key(KeyCode::PageDown));
    }
    let bottom = draw(&model).join("\n");
    assert!(bottom.contains("filler line 79"), "at the tail:\n{bottom}");
    // ONE ↑ must move the view — overscroll never accumulates in state.
    model.handle(key(KeyCode::Up));
    let up_one = draw(&model).join("\n");
    assert_ne!(bottom, up_one, "a single ↑ after overscroll must scroll");

    // A NEW proposal opens at the top at first PAINT — no keypress needed.
    let mut second = plan_menu(80);
    second.id = MenuId::new("plan-menu-2");
    second.title = "Second proposal".to_owned();
    model.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(
        second,
    ))));
    let fresh = draw(&model).join("\n");
    assert!(
        fresh.contains("edge pops"),
        "plan B must open at its top:\n{fresh}"
    );
    assert!(
        !fresh.contains("filler line 79"),
        "plan B must not inherit plan A's scroll:\n{fresh}"
    );
}

/// Review round 2 MUTATION CHECK: let the sticky prompt band render while a
/// plan owns the transcript. Expected RUNTIME failure: the pinned history
/// prompt paints over the document's first row.
#[test]
fn an_open_plan_suppresses_the_sticky_prompt_band() {
    let mut model = launcher_model();
    for index in 0..40 {
        submit(&mut model, &format!("history prompt number {index}"));
    }
    // Scrolled into history: the sticky band pins the producing prompt.
    model.scroll_back.set(8);
    let without_plan = draw(&model).join("\n");
    assert!(
        without_plan.contains("history prompt"),
        "harness: history prompts must be on screen:\n{without_plan}"
    );

    model.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(
        plan_menu(0),
    ))));
    let with_plan = draw(&model).join("\n");
    assert!(with_plan.contains("◇ PLAN"), "plan owns the transcript");
    assert!(
        !with_plan.contains("history prompt"),
        "the sticky band must not overlay an open plan:\n{with_plan}"
    );
}
