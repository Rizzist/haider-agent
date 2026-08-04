//! S2 UI refinements (owner screenshots wave): the rebuilt 24×2 header
//! mark, the composer band's one-line rest height, the transcript/composer
//! breathing rhythm, and the child chip view's user rows.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::agent::{AgentManifest, AgentRole, Grant, Placement};
use haider_protocol::envelope::{EventEnvelope, PromptRender, RawEnvelope, RenderTargets};
use haider_protocol::ids::{AgentId, DeviceId, EventId, LeaseId, SessionId};
use haider_protocol::state::RunState;
use haider_tui::app::{AppEvent, AppModel, Hit, RuntimeMode, Screen};
use haider_tui::mock::demo_script;
use haider_tui::render::render;
use haider_tui::theme::ThemeKey;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};

mod common;
use common::launcher_model;

fn key(code: KeyCode) -> AppEvent {
    AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn session_model() -> AppModel {
    let mut model = AppModel::new();
    for payload in demo_script() {
        model.handle(AppEvent::Envelope(Box::new(payload)));
    }
    model
}

fn draw(
    model: &AppModel,
    width: u16,
    height: u16,
) -> (Vec<String>, Vec<(Rect, Hit)>, Terminal<TestBackend>) {
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
    (rows, hits, terminal)
}

fn row_of(rows: &[String], needle: &str) -> u16 {
    u16::try_from(
        rows.iter()
            .position(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("row containing {needle:?} not rendered")),
    )
    .expect("row fits u16")
}

/// A screen row that reads as a horizontal rule.
fn is_rule(row: &str) -> bool {
    row.chars().filter(|c| *c == '─').count() >= 20
}

fn blank(row: &str) -> bool {
    row.trim().is_empty()
}

fn user_message(text: &str) -> EventPayload {
    EventPayload::UserMessage {
        text: text.to_owned(),
        attachments: Vec::new(),
        mode: haider_protocol::DeliveryMode::Steer,
    }
}

/// A clean session fed only what the test sends — no demo chips, todos or
/// panels between the transcript tail and the band.
fn bare_session(prompts: usize) -> AppModel {
    let mut model = launcher_model();
    for index in 0..prompts {
        model.handle(AppEvent::Envelope(Box::new(user_message(&format!(
            "prompt {index}"
        )))));
    }
    model
}

// ---- Owner item 3: the rebuilt header mark ----

/// MUTATION CHECK: regress `mark::HEADER` to the 16-col block map (or any
/// map that renders different glyph rows) and this fails on the verbatim
/// row pins — the owner's screenshot showed the 16-col mark reading as
/// disconnected squares, so the exact rebuilt art is the contract.
#[test]
fn header_mark_uses_halfblock_glyphs() {
    // The rendered rows, VERBATIM (24 cols × 2 rows): the banner's
    // letterforms at half vertical resolution — `ر`'s body sweeping into
    // its tail, `ـد`'s solid upright, `ـيـ`'s tooth with both dots
    // hanging beneath the `▀` baseline, `حـ`'s head bar thickening into
    // the drop at the right edge.
    let rows = haider_tui::mark::header_rows();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], "   ▄▄  ██   ▄▄  ▀▀▀▀▀██▄");
    assert_eq!(rows[1], "▄▄█▀  ▀▀▀▀██▀▀██▀▀▀▀▀▀▀▀");
    // Half-block ink only — the doubled vertical resolution is the point.
    for row in &rows {
        assert!(row.chars().all(|c| "█▀▄ ".contains(c)));
        assert!(
            row.chars().any(|c| "█▀▄".contains(c)),
            "every terminal row carries ink"
        );
    }
    // Width/floor pins travel with the art.
    assert_eq!(haider_tui::mark::HEADER_COLS, 24);
    assert_eq!(haider_tui::mark::HEADER_ROWS, 2);
}

/// The mark must look intentional in BOTH redesigned palettes (owner
/// item 3): the session header renders the full art in bold identity ink
/// on light and dark alike.
#[test]
fn header_mark_renders_in_both_new_palettes() {
    let art = haider_tui::mark::header_rows();
    for key in [ThemeKey::Light, ThemeKey::Dark] {
        let mut model = session_model();
        model.theme = key;
        let theme = key.theme();
        let (rows, _, terminal) = draw(&model, 118, 34);
        let buffer = terminal.backend().buffer();
        assert!(
            rows[0].contains(art[0].trim_end()),
            "{}: mark line 1",
            theme.label
        );
        assert!(
            rows[1].contains(art[1].trim_end()),
            "{}: mark line 2",
            theme.label
        );
        // Cell coordinates are CHAR columns, never byte offsets (the art
        // is multi-byte ink).
        let char_x = u16::try_from(
            rows[0]
                .chars()
                .position(|c| c == '█')
                .expect("mark ink cell"),
        )
        .expect("x fits");
        let cell = &buffer[(char_x, 0)];
        assert_eq!(
            cell.fg,
            Color::from(theme.maroon),
            "{}: identity ink",
            theme.label
        );
        assert!(cell.modifier.contains(Modifier::BOLD));
    }
}

// ---- Owner item 4: the band rests at one line ----

/// MUTATION CHECK: restore the TUI6-era `band_pad` row in
/// `render_session`'s ledger/layout and the rest-height half fails (the
/// closing rule moves a row down); collapse `composer_height`'s wrap-row
/// count to a constant 1 and the growth half fails.
#[test]
fn composer_rests_at_one_line_and_grows() {
    let mut model = session_model();
    let theme = model.theme.theme();
    let (rows, _, terminal) = draw(&model, 100, 30);
    let buffer = terminal.backend().buffer();
    let composer_y = row_of(&rows, "message haider");
    // At rest the band is EXACTLY one text row: gold rule above, closing
    // frame rule DIRECTLY below (the owner's screenshot showed a padded
    // two-row band).
    assert_eq!(buffer[(0, composer_y - 1)].fg, Color::from(theme.gold));
    assert!(
        is_rule(&rows[(composer_y + 1) as usize]),
        "closing rule directly under the rest composer, got {:?}",
        rows[(composer_y + 1) as usize]
    );
    assert_eq!(
        buffer[(0, composer_y + 1)].fg,
        Color::from(theme.frame),
        "the closing rule wears frame ink"
    );
    assert_eq!(
        buffer[(0, composer_y + 1)].bg,
        Color::from(theme.bg),
        "the closing rule sits OUTSIDE the band"
    );
    // Growth behavior unchanged: a two-line draft grows the band to two
    // text rows, the closing rule following beneath.
    model.handle(key(KeyCode::Char('a')));
    model.handle(AppEvent::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::ALT,
    )));
    model.handle(key(KeyCode::Char('b')));
    let (rows, _, _) = draw(&model, 100, 30);
    let draft_y = row_of(&rows, "❯ a") as usize;
    assert!(
        !is_rule(&rows[draft_y + 1]),
        "the second draft row lives INSIDE the band"
    );
    assert!(
        is_rule(&rows[draft_y + 2]),
        "the closing rule follows the grown band"
    );
}

// ---- Owner item 5: breathing rhythm ----

/// MUTATION CHECK: drop the transcript stream's trailing blank line in
/// `render_session` and this fails — the bottom-anchored tail sits flush
/// on the band's gold rule (the owner's cramped screenshot).
#[test]
fn one_blank_line_before_composer_band() {
    // Enough prompts to overflow a 12-row frame: the tail bottom-anchors.
    let model = bare_session(12);
    let (rows, _, _) = draw(&model, 90, 12);
    let composer_y = row_of(&rows, "message haider") as usize;
    // Gold rule above the composer; the row above IT is the one blank
    // breathing row; the row above THAT is real output — exactly one.
    assert!(
        rows[composer_y - 1].contains('─'),
        "band top rule above the composer"
    );
    assert!(
        blank(&rows[composer_y - 2]),
        "one blank line between the last output and the band, got {:?}",
        rows[composer_y - 2]
    );
    assert!(
        rows[composer_y - 3].contains("prompt 11"),
        "the last transcript output sits directly above the single blank, got {:?}",
        rows[composer_y - 3]
    );
}

/// MUTATION CHECK: drop the `Line::default()` push above the thinking
/// badge (session or chip arm) and the matching surface's assertion fails
/// — the badge sits flush on the last output line.
#[test]
fn one_blank_line_above_thinking_badge() {
    // Session view.
    let mut model = bare_session(3);
    model.handle(AppEvent::Envelope(Box::new(EventPayload::RunState(
        RunState::Thinking,
    ))));
    let (rows, _, _) = draw(&model, 90, 30);
    let badge_y = row_of(&rows, "thinking…") as usize;
    assert!(
        blank(&rows[badge_y - 1]),
        "session: one blank line above the badge, got {:?}",
        rows[badge_y - 1]
    );
    assert!(
        rows[badge_y - 2].contains("prompt 2"),
        "session: output directly above the single blank"
    );
    // Chip view (the badge is the chip's own thinking tail).
    let mut model = launcher_model();
    let mut chip =
        haider_tui::app::ChipModel::from_manifest(&manifest("t1-audit", "audit the toolset"));
    chip.state = haider_tui::script::ChipDisplayState::Thinking;
    chip.transcript.apply(&user_message("child prompt"));
    model.chips.push(chip);
    model.screen = Screen::Subagent;
    model.view_path = vec!["t1-audit".to_owned()];
    let (rows, _, _) = draw(&model, 90, 30);
    let badge_y = row_of(&rows, "thinking…") as usize;
    assert!(
        blank(&rows[badge_y - 1]),
        "chip view: one blank line above the badge, got {:?}",
        rows[badge_y - 1]
    );
    assert!(
        rows[badge_y - 2].contains("child prompt"),
        "chip view: output directly above the single blank"
    );
}

// ---- Owner item 6: the child chip view renders user messages ----

fn manifest(agent: &str, task: &str) -> AgentManifest {
    AgentManifest {
        agent: AgentId::new(agent),
        role: AgentRole::Subagent,
        task: task.to_owned(),
        callsign: Some("Ammar".to_owned()),
        model_profile: "fable-5".to_owned(),
        grant: Grant {
            tools: vec![],
            effect_ceiling: vec![],
        },
        budget_tokens: None,
        placement: Placement::Local,
        lease: LeaseId::new("lease-s2"),
        fencing_epoch: 1,
        attempt: 0,
        parent: None,
        coordinates: None,
    }
}

fn sid() -> SessionId {
    SessionId::new("s-s2")
}

fn raw(seq: u64, agent: Option<&str>, payload: &EventPayload) -> RawEnvelope {
    EventEnvelope {
        schema_version: 1,
        event_id: EventId::new(format!("evt-s2-{seq}")),
        seq,
        session_id: sid(),
        branch_id: None,
        run_id: None,
        agent_id: agent.map(AgentId::new),
        device_id: DeviceId::new("s2-device"),
        authority_epoch: 1,
        worker_generation: 7,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::to_value(payload).expect("payload serializes"),
    }
}

/// MUTATION CHECK: make `session::classify`'s catch-all ignore the
/// envelope's `agent_id` (route everything to the session) and this fails
/// — the chip view shows neither user row while the session transcript
/// swallows both. The FIRST message is the load-bearing case: the spawn
/// prompt arrives before any other chip content, so a display gate that
/// only renders rows after an agent item would drop it (daemon ui-flag
/// fix lands in a parallel lane — this pin renders whatever arrives,
/// fabricating nothing locally).
#[test]
fn child_view_renders_user_messages() {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.sessions.clear();
    model.upsert_live_session(&sid());
    model.open_session(&sid());
    // The chip exists from its journal manifest…
    model.route_raw(&raw(
        1,
        None,
        &EventPayload::AgentSpawned(manifest("t1-audit", "audit the toolset")),
    ));
    // …and its FIRST stream content is a user message on the chip scope.
    model.route_raw(&raw(
        2,
        Some("t1-audit"),
        &user_message("audit the toolset and report gaps"),
    ));
    model.screen = Screen::Subagent;
    model.view_path = vec!["t1-audit".to_owned()];
    let (rows, _, _) = draw(&model, 100, 30);
    let first_y = row_of(&rows, "audit the toolset and report gaps");
    // HONEST FLIP (S3): an agent-scoped user row is parent-authored by
    // construction, so the chip renders it with the → `from main` sigil
    // instead of the plain ❯ user row it wore when this pin was written.
    assert!(
        rows[first_y as usize].contains('→'),
        "the chip's first user message renders as a from-main sigiled row"
    );
    // A mid-run steer arrives later on the same scope and renders too.
    model.route_raw(&raw(3, Some("t1-audit"), &user_message("focus on fs_edit")));
    let (rows, _, _) = draw(&model, 100, 30);
    row_of(&rows, "focus on fs_edit");
    // Neither leaked into the SESSION transcript.
    model.screen = Screen::Session;
    model.view_path.clear();
    let (rows, _, _) = draw(&model, 100, 30);
    assert!(
        !rows
            .iter()
            .any(|row| row.contains("audit the toolset and report gaps")),
        "the chip's user rows stay scoped to the chip view"
    );
}
