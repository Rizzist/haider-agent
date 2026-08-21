//! W-flow inline identity — the session agent-type binding in the TUI:
//! the /loom Types pane's fixed-head `∅ none` row and `p` bind over the
//! receipted `session.select_agent_type`, receipt/refusal flash grammar,
//! the `agent_type_selected` fact moving `identity.agent_type` (and a
//! clearing fact reverting it), the session-accent surfaces painting from
//! the bound type's registry color, and the roster-row summary join.
#![allow(clippy::expect_used)]

use haider_protocol::envelope::{EventEnvelope, PromptRender, RawEnvelope, RenderTargets};
use haider_protocol::ids::{DeviceId, EventId, SessionId};
use haider_protocol::loom::LoomAgentType;
use haider_protocol::session::AgentTypeSelected;
use haider_rpc::{AttachmentId, RequestBody, ResponseBody};
use haider_tui::app::{AppModel, AppRequest, LoomPane, RuntimeMode, Screen, TypeRow};
use haider_tui::link::{CommandContext, map_response, request_body};
use haider_tui::live::{LiveCommand, LiveDriver, LiveReply};
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::KeyCode;

mod common;
use common::{ctrl, launcher_model, submit};

fn sid() -> SessionId {
    SessionId::new("s-agent-type")
}

/// The daemon's boot seed (loom_seed.rs): scout #7aa2f7 ⌖.
fn scout() -> LoomAgentType {
    LoomAgentType {
        id: "scout".into(),
        name: "Scout".into(),
        job: "Read-only reconnaissance.".into(),
        in_type: "Brief".into(),
        out_type: "Map".into(),
        clis: Vec::new(),
        apis: Vec::new(),
        skills: Vec::new(),
        scripts: Vec::new(),
        color: "#7aa2f7".into(),
        glyph: "⌖".into(),
        rev: 0,
    }
}

const SCOUT_ACCENT: Option<ratatui::style::Color> =
    Some(ratatui::style::Color::Rgb(0x7a, 0xa2, 0xf7));

/// A live model with the Loom + agent-type-select features served and one
/// bound session.
fn live_bound_model() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.daemon_features = [
        haider_rpc::FEATURE_LOOM_V1.to_owned(),
        haider_rpc::FEATURE_SESSION_AGENT_TYPE_SELECT_V1.to_owned(),
    ]
    .into_iter()
    .collect();
    model.daemon_version = Some("0.0.933".to_owned());
    model.sessions.clear();
    model.upsert_live_session(&sid());
    model.open_session(&sid());
    model.loom_loaded = true;
    model.loom_types = vec![scout()];
    model
}

fn draw(model: &AppModel) -> (Vec<String>, Vec<Vec<Option<ratatui::style::Color>>>) {
    let backend = TestBackend::new(110, 32);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            render(model, frame);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let mut rows = Vec::new();
    let mut colors = Vec::new();
    for y in 0..buffer.area.height {
        let mut text = String::new();
        let mut row_colors = Vec::new();
        for x in 0..buffer.area.width {
            text.push_str(buffer[(x, y)].symbol());
            row_colors.push(buffer[(x, y)].style().fg);
        }
        rows.push(text);
        colors.push(row_colors);
    }
    (rows, colors)
}

fn drain(driver: &mut LiveDriver, model: &mut AppModel) -> Vec<LiveCommand> {
    let requests: Vec<AppRequest> = model.requests.drain(..).collect();
    let mut commands = Vec::new();
    for request in requests {
        commands.extend(driver.handle_request(model, request));
    }
    commands
}

/// A journaled session-config fact envelope for the bound session.
fn config_fact(seq: u64, payload: &AgentTypeSelected) -> RawEnvelope {
    EventEnvelope {
        schema_version: 1,
        event_id: EventId::new(format!("evt-at-{seq}")),
        seq,
        session_id: sid(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("agent-type-device"),
        authority_epoch: 1,
        worker_generation: 7,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: false,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: payload.to_payload_value().expect("fact payload"),
    }
}

// ---- item A: the Types pane's fixed head --------------------------------

/// MUTATION CHECK: reorder the /loom Types rows (registered before the
/// head), drop the synthetic `∅ none` row, or source it from the registry
/// (making it deletable). Expected RUNTIME failure: the order assertion,
/// the empty-registry survival assertion, or the detail/footer copy below.
#[test]
fn types_pane_leads_with_none_and_none_is_synthetic() {
    let mut model = launcher_model();
    submit(&mut model, "open the loom");
    model.loom_types = vec![scout()];
    model.screen = Screen::Loom;
    model.loom_pane = LoomPane::Types;

    let (rows, _) = draw(&model);
    let position = |needle: &str| {
        rows.iter()
            .position(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("`{needle}` missing:\n{}", rows.join("\n")))
    };
    let none = position("∅ none — plain session · default");
    let scout_row = position("@scout");
    assert!(
        none < scout_row,
        "the synthetic default leads the registered types:\n{}",
        rows.join("\n")
    );
    // The verbs sit on ⌃: the pane carries a live composer that owns every
    // bare letter (owner 2026-08-22).
    assert!(
        rows.iter()
            .any(|row| row.contains("⌃P bind") && row.contains("⌃N new")),
        "types footer missing ⌃P/⌃N:\n{}",
        rows.join("\n")
    );

    // The row-space authority agrees with the paint.
    assert_eq!(model.type_row_count(), 2);
    assert_eq!(model.type_row(0), Some(TypeRow::None));
    assert_eq!(model.type_row(1), Some(TypeRow::Registered(0)));

    // ⏎ detail on `none` renders the plain-session line.
    model.loom_selection = 0;
    model.loom_detail = true;
    let (rows, _) = draw(&model);
    assert!(
        rows.iter().any(
            |row| row.contains("every session starts plain — no job injected, default accent")
        ),
        "none detail missing:\n{}",
        rows.join("\n")
    );

    // An EMPTY registry never empties the pane — the head survives and the
    // registry emptiness moves inside the section (reworded).
    model.loom_detail = false;
    model.loom_types.clear();
    let (rows, _) = draw(&model);
    assert!(
        rows.iter().any(|row| row.contains("∅ none")),
        "the synthetic default must survive an empty registry:\n{}",
        rows.join("\n")
    );
    assert!(
        rows.iter()
            .any(|row| row.contains("none registered — press n")),
        "the section carries the reworded empty line:\n{}",
        rows.join("\n")
    );
    assert!(
        !rows
            .iter()
            .any(|row| row.contains("no agent types registered")),
        "the old whole-pane empty state must not return:\n{}",
        rows.join("\n")
    );
}

// ---- item A: `p` binds --------------------------------------------------

/// MUTATION CHECK: drop the id from the widened select (or resolve the
/// wrong row / send something for the `none` row's id). Expected RUNTIME
/// failure: the AppRequest assertion, the LiveCommand assertion, or the
/// wire-body agent_type below.
#[test]
fn p_bind_carries_the_exact_id_on_the_wire() {
    let mut model = live_bound_model();
    model.screen = Screen::Loom;
    model.loom_pane = LoomPane::Types;
    model.loom_selection = 1; // @scout
    model.requests.clear();

    model.handle(ctrl(KeyCode::Char('p')));
    assert!(
        model.requests.iter().any(|request| matches!(
            request,
            AppRequest::SelectAgentType { agent_type: Some(id) } if id == "scout"
        )),
        "p must request the bind CARRYING the selected id: {:?}",
        model.requests
    );
    assert_eq!(
        model.flash.as_deref(),
        Some("· binding @scout…"),
        "issuance flash names the type"
    );

    let mut driver = LiveDriver::new("test");
    let commands = drain(&mut driver, &mut model);
    let select = commands
        .into_iter()
        .find(|command| matches!(command, LiveCommand::SelectAgentType { .. }))
        .expect("a SelectAgentType command");
    match request_body(select) {
        RequestBody::SessionSelectAgentType {
            agent_type,
            session_id,
            ..
        } => {
            assert_eq!(agent_type.as_deref(), Some("scout"));
            assert_eq!(session_id, sid(), "bound to the ACTIVE session");
        }
        other => panic!("wrong wire body: {other:?}"),
    }

    // `p` on the `none` row clears — agent_type None on the wire.
    model.identity.agent_type = Some("scout".to_owned());
    model.loom_selection = 0;
    model.requests.clear();
    model.handle(ctrl(KeyCode::Char('p')));
    assert!(
        model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::SelectAgentType { agent_type: None })),
        "none must clear, never bind: {:?}",
        model.requests
    );
    assert_eq!(model.flash.as_deref(), Some("· clearing agent type…"));
    let commands = drain(&mut driver, &mut model);
    let select = commands
        .into_iter()
        .find(|command| matches!(command, LiveCommand::SelectAgentType { .. }))
        .expect("a SelectAgentType command");
    match request_body(select) {
        RequestBody::SessionSelectAgentType { agent_type, .. } => {
            assert_eq!(agent_type, None, "the clear rides None");
        }
        other => panic!("wrong wire body: {other:?}"),
    }

    // Already plain: honest flash, nothing on the wire.
    model.identity.agent_type = None;
    model.requests.clear();
    model.handle(ctrl(KeyCode::Char('p')));
    assert!(model.requests.is_empty(), "already-plain issues nothing");
    assert_eq!(
        model.flash.as_deref(),
        Some("· already plain — no agent type bound")
    );
}

/// MUTATION CHECK: let an unbound or feature-ungated `p` reach the wire.
/// Expected RUNTIME failure: the no-request assertions or the honest flash
/// texts (the stale-daemon note names the fix).
#[test]
fn p_bind_unbound_or_ungated_flashes_honestly() {
    // No bound session.
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.daemon_features = [
        haider_rpc::FEATURE_LOOM_V1.to_owned(),
        haider_rpc::FEATURE_SESSION_AGENT_TYPE_SELECT_V1.to_owned(),
    ]
    .into_iter()
    .collect();
    model.loom_loaded = true;
    model.loom_types = vec![scout()];
    model.screen = Screen::Loom;
    model.loom_pane = LoomPane::Types;
    model.loom_selection = 1;
    model.requests.clear();
    model.handle(ctrl(KeyCode::Char('p')));
    assert!(
        !model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::SelectAgentType { .. })),
        "an unbound p must never reach the wire: {:?}",
        model.requests
    );
    assert_eq!(
        model.flash.as_deref(),
        Some("· bind — no bound session; open a session first"),
        "the flash names the fix"
    );

    // A stale daemon (no session_agent_type_select_v1): the honest note.
    let mut model = live_bound_model();
    model
        .daemon_features
        .remove(haider_rpc::FEATURE_SESSION_AGENT_TYPE_SELECT_V1);
    model.screen = Screen::Loom;
    model.loom_pane = LoomPane::Types;
    model.loom_selection = 1;
    model.requests.clear();
    model.handle(ctrl(KeyCode::Char('p')));
    assert!(
        !model
            .requests
            .iter()
            .any(|request| matches!(request, AppRequest::SelectAgentType { .. })),
        "an ungated p must never reach the wire"
    );
    let flash = model.flash.as_deref().unwrap_or_default();
    assert!(
        flash.contains("agent-type binding"),
        "the stale-daemon note names the surface: {flash}"
    );
}

/// MUTATION CHECK: install identity from the RESPONSE, drop the receipt
/// flash, or swallow the daemon's registry-miss refusal. Expected RUNTIME
/// failure: the identity-untouched assertion, the receipt-flash texts, or
/// the refusal flash carrying the daemon's message.
#[test]
fn bind_receipts_flash_and_refusals_carry_the_daemon_message() {
    let mut model = live_bound_model();
    let mut driver = LiveDriver::new("test");

    // Bind receipt → "· agent type @scout" — and identity does NOT move on
    // the response (the fact is the only identity writer).
    model.requests.push(AppRequest::SelectAgentType {
        agent_type: Some("scout".to_owned()),
    });
    let commands = drain(&mut driver, &mut model);
    let command = commands
        .into_iter()
        .find(|command| matches!(command, LiveCommand::SelectAgentType { .. }))
        .expect("a SelectAgentType command");
    let context = CommandContext::of(&command);
    let replies = map_response(
        &context,
        ResponseBody::SessionSelectAgentType {
            session_id: sid(),
            agent_type: Some("scout".to_owned()),
            selected_seq: 9,
            worker_generation: 7,
        },
    );
    assert_eq!(replies.len(), 1, "one correlated receipt");
    for reply in replies {
        driver.apply(&mut model, reply);
    }
    assert_eq!(model.flash.as_deref(), Some("· agent type @scout"));
    assert_eq!(
        model.identity.agent_type, None,
        "identity moves on the FACT, never the receipt"
    );

    // Clear receipt → the plain flash.
    model
        .requests
        .push(AppRequest::SelectAgentType { agent_type: None });
    let commands = drain(&mut driver, &mut model);
    let LiveCommand::SelectAgentType { command_id, .. } = commands
        .into_iter()
        .find(|command| matches!(command, LiveCommand::SelectAgentType { .. }))
        .expect("a SelectAgentType command")
    else {
        unreachable!()
    };
    driver.apply(&mut model, LiveReply::AgentTypeBound { command_id });
    assert_eq!(model.flash.as_deref(), Some("· agent type cleared — plain"));

    // A registry miss: the daemon's typed refusal reaches the flash.
    model.requests.push(AppRequest::SelectAgentType {
        agent_type: Some("ghost".to_owned()),
    });
    let commands = drain(&mut driver, &mut model);
    let LiveCommand::SelectAgentType { command_id, .. } = commands
        .into_iter()
        .find(|command| matches!(command, LiveCommand::SelectAgentType { .. }))
        .expect("a SelectAgentType command")
    else {
        unreachable!()
    };
    driver.apply(
        &mut model,
        LiveReply::Failed {
            command_id: Some(command_id),
            code: "agent_type_not_registered".to_owned(),
            message: "@ghost is not registered".to_owned(),
            retryable: false,
            presentation: None,
        },
    );
    assert_eq!(
        model.flash.as_deref(),
        Some("· bind @ghost refused — @ghost is not registered"),
        "the refusal flash carries the daemon's message"
    );
}

// ---- item C: the reducer arm (previously UNPINNED) ----------------------

/// MUTATION CHECK: drop the `AgentTypeSelected` arm from
/// `apply_tuning_fact` (or stop clearing on a None fact). Expected RUNTIME
/// failure: `identity.agent_type` never becomes `scout`, or the clearing
/// fact leaves the stale binding standing.
#[test]
fn agent_type_fact_moves_identity_and_a_clearing_fact_reverts() {
    let mut model = live_bound_model();
    let mut driver = LiveDriver::new("test");
    let attachment = AttachmentId::new("agent-type-attachment");
    driver.apply(
        &mut model,
        LiveReply::Attached {
            session: sid(),
            attachment: attachment.clone(),
            worker_generation: 7,
            replay_through_seq: 0,
        },
    );

    let bound = config_fact(
        1,
        &AgentTypeSelected {
            agent_type: Some("scout".to_owned()),
        },
    );
    driver.apply(
        &mut model,
        LiveReply::Event {
            attachment: attachment.clone(),
            session: sid(),
            envelope: Box::new(bound),
        },
    );
    assert_eq!(
        model.identity.agent_type.as_deref(),
        Some("scout"),
        "the committed fact moves identity"
    );

    let cleared = config_fact(2, &AgentTypeSelected { agent_type: None });
    driver.apply(
        &mut model,
        LiveReply::Event {
            attachment,
            session: sid(),
            envelope: Box::new(cleared),
        },
    );
    assert_eq!(
        model.identity.agent_type, None,
        "a clearing fact reverts to plain"
    );
}

// ---- item B: the session accent -----------------------------------------

/// MUTATION CHECK: drop the bound-type accent from the session's identity
/// surfaces (header head-callsign + `{glyph} @{id}` chips), or keep
/// painting it after the binding clears. Expected RUNTIME failure: no
/// #7aa2f7 cell while bound, or a lingering accent cell after the revert.
#[test]
fn session_accent_paints_from_bound_scout_and_reverts() {
    let mut model = live_bound_model();
    model.identity.agent_type = Some("scout".to_owned());
    model.screen = Screen::Session;

    let (rows, colors) = draw(&model);
    let row = rows
        .iter()
        .position(|row| row.contains("@scout"))
        .unwrap_or_else(|| {
            panic!(
                "bound chip missing from the session screen:\n{}",
                rows.join("\n")
            )
        });
    let cell = |byte: usize| rows[row][..byte].chars().count();
    let column = cell(rows[row].find("@scout").expect("column"));
    assert_eq!(
        colors[row][column], SCOUT_ACCENT,
        "the session's identity chip wears the bound type's accent"
    );

    // The revert law: clearing the binding removes the accent EVERYWHERE —
    // no stale paint (default styling exactly).
    model.identity.agent_type = None;
    let (rows, colors) = draw(&model);
    assert!(
        !rows.iter().any(|row| row.contains("@scout")),
        "the chip leaves with the binding:\n{}",
        rows.join("\n")
    );
    assert!(
        !colors.iter().flatten().any(|cell| *cell == SCOUT_ACCENT),
        "no accent cell survives the revert"
    );

    // The snapshot gate: a binding whose id the loom snapshot lacks paints
    // NOTHING (never a fabricated accent).
    model.identity.agent_type = Some("ghost".to_owned());
    let (rows, colors) = draw(&model);
    assert!(
        !rows.iter().any(|row| row.contains("@ghost")),
        "an unknown id renders no chip:\n{}",
        rows.join("\n")
    );
    assert!(
        !colors.iter().flatten().any(|cell| *cell == SCOUT_ACCENT),
        "an unknown id earns no accent"
    );
}

/// MUTATION CHECK: drop the roster join (`SessionSummary.agent_type` →
/// entry → loom color), its feature gate, or the snapshot fallback.
/// Expected RUNTIME failure: the accent-chip assertion for the hydrated
/// row, or the no-chip assertions for the snapshot-less / ungated cases.
#[test]
fn roster_row_accent_joins_the_summary_and_falls_back() {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.daemon_features = [
        haider_rpc::FEATURE_LOOM_V1.to_owned(),
        haider_rpc::FEATURE_SESSION_AGENT_TYPE_SELECT_V1.to_owned(),
    ]
    .into_iter()
    .collect();
    model.loom_loaded = true;
    model.loom_types = vec![scout()];
    model.sessions.clear();
    model.upsert_live_session(&sid());
    let summary = haider_rpc::SessionSummary {
        session_id: sid(),
        head_seq: 4,
        worker_generation: 7,
        run_state: None,
        seen_at_ms: None,
        last_activity_ms: None,
        waiting_why: None,
        needs_input: None,
        metadata: None,
        workspace_cwd: None,
        turn_count: Some(2),
        footprint_tokens: None,
        footprint_truth: None,
        title: None,
        agent_metrics: None,
        last_model: None,
        parent_session_id: None,
        kind: None,
        agent_type: Some("scout".to_owned()),
        effort: None,
        fast: None,
        account_alias: None,
    };
    model.note_summary_counts(&summary);
    assert_eq!(
        model.sessions[0].agent_type.as_deref(),
        Some("scout"),
        "the summary hydrates the roster row"
    );

    model.screen = Screen::Launcher;
    let (rows, colors) = draw(&model);
    let row = rows
        .iter()
        .position(|row| row.contains("@scout"))
        .unwrap_or_else(|| panic!("roster chip missing:\n{}", rows.join("\n")));
    let cell = |byte: usize| rows[row][..byte].chars().count();
    let column = cell(rows[row].find("@scout").expect("column"));
    assert_eq!(
        colors[row][column], SCOUT_ACCENT,
        "the roster chip wears the bound type's accent"
    );
    assert!(
        rows[row].contains('⌖'),
        "the chip carries the glyph: {}",
        rows[row]
    );

    // Snapshot fallback: the loom registry lacking the id renders today's
    // row untouched — no chip, no accent.
    model.loom_types.clear();
    let (rows, colors) = draw(&model);
    assert!(
        !rows.iter().any(|row| row.contains("@scout")),
        "no snapshot entry, no chip:\n{}",
        rows.join("\n")
    );
    assert!(
        !colors.iter().flatten().any(|cell| *cell == SCOUT_ACCENT),
        "no snapshot entry, no accent"
    );

    // Feature gate: an un-gated daemon's summary hydrates nothing.
    let mut ungated = launcher_model();
    ungated.mode = RuntimeMode::Live;
    ungated.sessions.clear();
    ungated.upsert_live_session(&sid());
    ungated.note_summary_counts(&summary);
    assert_eq!(
        ungated.sessions[0].agent_type, None,
        "without session_agent_type_select_v1 the field never hydrates"
    );
}
