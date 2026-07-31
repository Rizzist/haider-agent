//! W3c3 M1 — the raw-envelope session router (report R11 cut 2).
//!
//! The TUI stops consuming a canned script and starts consuming a live
//! attach stream. Everything the swap depends on is pinned here: the STRICT
//! cursor gate (duplicate → no-op, gap → STOP + reattach before any later
//! state mutates), routing by opaque session id, agent-event chip
//! projection, and menu-ownership routing.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::agent::{
    AgentManifest, AgentRole, ChildReport, ChipState, Grant, Placement, ReportVerification,
};
use haider_protocol::envelope::{EventEnvelope, PromptRender, RawEnvelope, RenderTargets};
use haider_protocol::ids::{AgentId, DeviceId, EventId, LeaseId, MenuId, SessionId};
use haider_protocol::menu::{
    AnswerVia, Menu, MenuAnswer, MenuCloseReason, MenuKind, MenuOption, MenuScope,
};
use haider_protocol::state::RunState;
use haider_tui::app::{AppModel, AppRequest};
use haider_tui::identity::{UiGeneration, demo_session_id};
use haider_tui::projection::RawOutcome;
use haider_tui::script::ChipDisplayState;

mod common;
use common::launcher_model;

fn seed_id(n: u64) -> SessionId {
    demo_session_id(UiGeneration::new(n))
}

/// Transcript row count of a BACKGROUND session slot.
fn rows(model: &AppModel, session: &SessionId) -> usize {
    model
        .sessions
        .iter()
        .find(|entry| &entry.id == session)
        .expect("session row")
        .projection
        .entries()
        .len()
}

/// The reattach cursor of a BACKGROUND session slot.
fn cursor(model: &AppModel, session: &SessionId) -> Option<u64> {
    model
        .sessions
        .iter()
        .find(|entry| &entry.id == session)
        .expect("session row")
        .projection
        .last_applied()
}

fn raw(session: &SessionId, seq: u64, payload: &EventPayload) -> RawEnvelope {
    EventEnvelope {
        schema_version: 1,
        event_id: EventId::new(format!("evt-{seq}")),
        seq,
        session_id: session.clone(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("router-device"),
        authority_epoch: 1,
        worker_generation: 1,
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

fn scoped(session: &SessionId, seq: u64, agent: &str, payload: &EventPayload) -> RawEnvelope {
    let mut envelope = raw(session, seq, payload);
    envelope.agent_id = Some(AgentId::new(agent));
    envelope
}

fn user(text: &str) -> EventPayload {
    EventPayload::UserMessage {
        text: text.to_owned(),
        attachments: vec![],
        mode: haider_protocol::DeliveryMode::Steer,
    }
}

fn manifest(agent: &str, parent: Option<&str>) -> AgentManifest {
    AgentManifest {
        agent: AgentId::new(agent),
        role: AgentRole::Subagent,
        task: String::new(),
        callsign: Some("Ammar".to_owned()),
        model_profile: "fable-5".to_owned(),
        grant: Grant {
            tools: vec![],
            effect_ceiling: vec![],
        },
        budget_tokens: None,
        placement: Placement::Local,
        lease: LeaseId::new("lease-1"),
        fencing_epoch: 1,
        attempt: 0,
        parent: parent.map(AgentId::new),
        coordinates: None,
    }
}

fn card(id: &str, scope: MenuScope) -> Menu {
    Menu {
        id: MenuId::new(id),
        kind: MenuKind::Choice,
        scope,
        title: "pick".to_owned(),
        body: vec![],
        options: vec![MenuOption {
            key: "yes".to_owned(),
            label: "yes".to_owned(),
            detail: None,
            decision: None,
        }],
        blocking: true,
        origin: "router".to_owned(),
        ttl_ms: None,
        timeout_option: None,
    }
}

// ---- cursor discipline ------------------------------------------------

#[test]
fn a_gap_stops_reduction_and_requests_a_reattach_before_later_state_mutates() {
    // Report §6.3: "gap stops and emits reattach request before later state
    // mutates". The store is the lag buffer (R9): a client that paints
    // across a hole has invented history nobody committed.
    //
    // MUTATION CHECK: make `SessionProjection::admit`'s gap arm fall
    // through to the cursor advance (restoring the pre-W3c3 behavior) and
    // BOTH the `Reattach` request and the "no later state" assertions fail.
    let mut model = launcher_model();
    let session = seed_id(2);
    // The seeds ship with sim-parity transcripts; the law is about DELTAS.
    let seeded = rows(&model, &session);

    assert_eq!(
        model.route_raw(&raw(&session, 1, &user("first"))),
        RawOutcome::Applied
    );

    // seq 3 with 2 missing: a hole.
    let outcome = model.route_raw(&raw(&session, 3, &user("second")));
    assert_eq!(outcome, RawOutcome::Gap { after_seq: 1 });
    assert_eq!(
        model
            .requests
            .iter()
            .filter(|request| matches!(
                request,
                AppRequest::Reattach { session: s, after_seq: 1 } if *s == session
            ))
            .count(),
        1,
        "the gap emits exactly one reattach, cursored at the last APPLIED seq"
    );

    assert_eq!(
        rows(&model, &session),
        seeded + 1,
        "the post-gap envelope must not have mutated the transcript"
    );
    assert_eq!(
        cursor(&model, &session),
        Some(1),
        "…and the cursor must still name the last fully applied sequence"
    );
}

#[test]
fn a_reattach_replay_lands_the_held_envelopes_and_clears_the_hole() {
    // The other half of the gap law: once the driver reattaches after the
    // reported cursor, the replayed stream applies normally. Without this
    // the strict gate would be a deadlock, not a discipline.
    let mut model = launcher_model();
    let session = seed_id(2);
    let seeded = rows(&model, &session);
    model.route_raw(&raw(&session, 1, &user("first")));
    assert_eq!(
        model.route_raw(&raw(&session, 3, &user("third"))),
        RawOutcome::Gap { after_seq: 1 }
    );
    // The daemon replays strictly after `after_seq`.
    assert_eq!(
        model.route_raw(&raw(&session, 2, &user("second"))),
        RawOutcome::Applied
    );
    assert_eq!(
        model.route_raw(&raw(&session, 3, &user("third"))),
        RawOutcome::Applied
    );
    assert_eq!(rows(&model, &session), seeded + 3, "history is contiguous");
}

#[test]
fn a_duplicate_envelope_is_a_no_op_on_the_attached_session_too() {
    // Delivery is at-least-once on BOTH routes; the attached path reduces
    // through the model's checked-out fields and must dedupe identically.
    //
    // MUTATION CHECK: change `envelope.seq <= last` to `< last` in
    // `SessionProjection::admit` — the second delivery pushes a second row.
    let mut model = launcher_model();
    let session = seed_id(1);
    model.open_session(&session);
    assert_eq!(
        model.route_raw(&raw(&session, 1, &user("hello"))),
        RawOutcome::Applied
    );
    let before = model.projection.entries().len();
    assert_eq!(
        model.route_raw(&raw(&session, 1, &user("hello"))),
        RawOutcome::Duplicate
    );
    assert_eq!(model.projection.entries().len(), before);
}

// ---- routing by opaque id --------------------------------------------

#[test]
fn background_session_envelopes_route_by_opaque_session_id() {
    // Report §6.3: "background-session envelopes route by opaque session
    // ID". Session 3's stream must never touch session 1's transcript,
    // and the attached surface must never absorb a background stream.
    //
    // MUTATION CHECK: drop the `envelope.session_id != self.id` guard in
    // `SessionState::absorb_raw` — the WrongSession assertion below fails.
    let mut model = launcher_model();
    let attached = seed_id(1);
    let background = seed_id(3);
    model.open_session(&attached);
    let attached_rows = model.projection.entries().len();
    let background_rows = model
        .sessions
        .iter()
        .find(|entry| entry.id == background)
        .expect("seed row")
        .projection
        .entries()
        .len();

    assert_eq!(
        model.route_raw(&raw(&background, 1, &user("background work"))),
        RawOutcome::Applied
    );
    assert_eq!(
        model.projection.entries().len(),
        attached_rows,
        "the attached surface must not absorb another session's stream"
    );
    let slot = model
        .sessions
        .iter()
        .find(|entry| entry.id == background)
        .expect("seed row");
    assert_eq!(slot.projection.entries().len(), background_rows + 1);

    // A session this model has never heard of is rejected, not invented.
    assert_eq!(
        model.route_raw(&raw(&SessionId::new("nobody"), 1, &user("ghost"))),
        RawOutcome::WrongSession
    );
    assert_eq!(model.sessions.len(), 3, "no row was fabricated");
}

// ---- agent chip projection -------------------------------------------

#[test]
fn agent_spawn_state_and_report_populate_nested_chips() {
    // Report §6.3: "agent spawn/state/report populate nested chips". The
    // demo grew its subagent tree from `DemoEvent` chip variants; live
    // subagent state can only arrive as envelopes.
    //
    // MUTATION CHECK: delete the `EventPayload::AgentSpawned` arm from
    // `session::apply_agent_payload` and every assertion below fails at
    // the first chip lookup.
    let mut model = launcher_model();
    let session = seed_id(2);
    let seeded = model
        .sessions
        .iter()
        .find(|entry| entry.id == session)
        .expect("seed row")
        .chips
        .len();

    model.route_raw(&raw(
        &session,
        1,
        &EventPayload::AgentSpawned(manifest("agent-parent", None)),
    ));
    model.route_raw(&raw(
        &session,
        2,
        &EventPayload::AgentSpawned(manifest("agent-child", Some("agent-parent"))),
    ));
    model.route_raw(&raw(
        &session,
        3,
        &EventPayload::AgentChipState {
            agent: AgentId::new("agent-child"),
            chip: ChipState::Streaming,
        },
    ));
    model.route_raw(&raw(
        &session,
        4,
        &EventPayload::AgentReport(ChildReport {
            agent: AgentId::new("agent-child"),
            summary: "tests green".to_owned(),
            verified: ReportVerification::Verified,
            workspace_revision: None,
        }),
    ));

    let slot = model
        .sessions
        .iter()
        .find(|entry| entry.id == session)
        .expect("seed row");
    assert_eq!(slot.chips.len(), seeded + 1, "one NEW top-level chip");
    let parent = slot
        .chips
        .iter()
        .find(|chip| chip.agent == "agent-parent")
        .expect("spawned parent");
    assert_eq!(parent.model, "fable-5", "the manifest is the only source");
    assert_eq!(parent.children.len(), 1, "the child nested by manifest");
    let child = &parent.children[0];
    assert_eq!(child.agent, "agent-child");
    assert_eq!(
        child.state,
        ChipDisplayState::Streaming,
        "AgentChipState is the sole chip-state authority"
    );
    assert!(
        child.transcript.entries().iter().any(|entry| matches!(
            entry,
            haider_tui::projection::TranscriptEntry::Note { text }
                if text.contains("tests green") && text.contains("verified")
        )),
        "AgentReport contributes summary + verification content"
    );
}

#[test]
fn a_replayed_agent_spawn_never_doubles_the_chip() {
    // At-least-once delivery plus a reattach replay means the same spawn
    // can arrive twice under different sequences. The tree must be
    // idempotent or every reconnect grows a phantom subagent.
    //
    // MUTATION CHECK: delete the `find_chip_mut(...).is_some()` early
    // return in `session::apply_agent_payload` and the count doubles.
    let mut model = launcher_model();
    let session = seed_id(2);
    let spawn = EventPayload::AgentSpawned(manifest("agent-solo", None));
    model.route_raw(&raw(&session, 1, &spawn));
    let after_first = model
        .sessions
        .iter()
        .find(|entry| entry.id == session)
        .expect("seed row")
        .chips
        .len();
    model.route_raw(&raw(&session, 2, &spawn));
    let after_second = model
        .sessions
        .iter()
        .find(|entry| entry.id == session)
        .expect("seed row")
        .chips
        .len();
    assert_eq!(after_first, after_second, "a replayed spawn is a no-op");
}

// ---- menu ownership ---------------------------------------------------

#[test]
fn a_subagent_menu_answer_never_closes_the_sessions_own_card() {
    // Report R11 cut 2: `MenuOpened` records ownership, and
    // `MenuAnswered`/`MenuClosed` route through that map. Without it a
    // subagent's answer broadcasts and closes the session's blocking card.
    //
    // MUTATION CHECK: make `SessionProjection::apply`'s `MenuAnswered` arm
    // clear `self.menu` unconditionally (drop the `m.id == answer.menu`
    // test) and the session-card assertion below fails.
    //
    // NB: the ownership MAP is not what this test pins — the answer's menu
    // id already protects the session's card. The map's own load-bearing
    // law is the `MenuClosed` test immediately below, where broadcast
    // cannot substitute for it.
    let mut model = launcher_model();
    let session = seed_id(2);
    model.route_raw(&raw(
        &session,
        1,
        &EventPayload::AgentSpawned(manifest("agent-asker", None)),
    ));
    // The session's own blocking card…
    model.route_raw(&raw(
        &session,
        2,
        &EventPayload::MenuOpened(card("session-card", MenuScope::Session)),
    ));
    // …and the subagent's, scoped by the envelope's agent id.
    model.route_raw(&scoped(
        &session,
        3,
        "agent-asker",
        &EventPayload::MenuOpened(card(
            "agent-card",
            MenuScope::Subagent {
                agent: AgentId::new("agent-asker"),
            },
        )),
    ));

    // Answering the SUBAGENT's card must not touch the session's.
    model.route_raw(&raw(
        &session,
        4,
        &EventPayload::MenuAnswered(MenuAnswer {
            menu: MenuId::new("agent-card"),
            option_key: Some("yes".to_owned()),
            option_index: 0,
            value: None,
            via: AnswerVia::Tui,
        }),
    ));

    let slot = model
        .sessions
        .iter()
        .find(|entry| entry.id == session)
        .expect("seed row");
    assert_eq!(
        slot.projection.open_menu().map(|menu| menu.id.as_str()),
        Some("session-card"),
        "the session's blocking card survives a subagent's answer"
    );
    let chip = slot
        .chips
        .iter()
        .find(|chip| chip.agent == "agent-asker")
        .expect("spawned chip");
    assert!(
        chip.transcript.open_menu().is_none(),
        "…and the subagent's own card closed"
    );
}

#[test]
fn a_session_scoped_close_still_reaches_the_subagents_card_that_opened_it() {
    // THE menu-ownership map's load-bearing law (report R11 cut 2). The
    // session reducer broadcasts `MenuAnswered` to chip transcripts, but
    // it does NOT broadcast `MenuClosed` — so a close committed without an
    // agent scope (cancellation, dismissal, recovery-interrupted) can only
    // find the subagent's card through the scope recorded when the card
    // was OPENED. Without the map the chip's question stays open forever
    // and its view keeps a card the daemon already retired.
    //
    // MUTATION CHECK: make the `MenuAnswered`/`MenuClosed` arm of
    // `session::classify` return `Destination::Session` unconditionally
    // (the pre-W3c3 behavior) — the chip's card survives the close and the
    // final assertion fails.
    let mut model = launcher_model();
    let session = seed_id(2);
    model.route_raw(&raw(
        &session,
        1,
        &EventPayload::AgentSpawned(manifest("agent-asker", None)),
    ));
    model.route_raw(&scoped(
        &session,
        2,
        "agent-asker",
        &EventPayload::MenuOpened(card(
            "agent-card",
            MenuScope::Subagent {
                agent: AgentId::new("agent-asker"),
            },
        )),
    ));
    assert!(
        chip_menu(&model, &session, "agent-asker").is_some(),
        "control: the subagent's card opened on its own transcript"
    );

    // The close carries NO agent scope — only the recorded opening knows
    // whose card this was.
    model.route_raw(&raw(
        &session,
        3,
        &EventPayload::MenuClosed {
            menu: MenuId::new("agent-card"),
            reason: MenuCloseReason::Cancelled,
        },
    ));
    assert!(
        chip_menu(&model, &session, "agent-asker").is_none(),
        "the recorded opening scope routed the close to the card that opened"
    );
}

/// The open card on a named chip's transcript.
fn chip_menu(model: &AppModel, session: &SessionId, agent: &str) -> Option<MenuId> {
    let slot = model
        .sessions
        .iter()
        .find(|entry| &entry.id == session)
        .expect("session row");
    haider_tui::app::find_chip(&slot.chips, agent)
        .expect("spawned chip")
        .transcript
        .open_menu()
        .map(|menu| menu.id.clone())
}

// ---- render targets ---------------------------------------------------

#[test]
fn a_non_ui_envelope_advances_the_cursor_without_painting() {
    // §6.1: three surfaces, never conflated. A `render.ui == false`
    // envelope is still part of the sequence — skipping the cursor advance
    // would make the NEXT envelope look like a gap.
    //
    // MUTATION CHECK: return `Admission::Duplicate` instead of
    // `Admission::Skip` for a non-ui envelope and seq 2 below reports a gap.
    let mut model = launcher_model();
    let session = seed_id(2);
    let mut hidden = raw(&session, 1, &EventPayload::RunState(RunState::Thinking));
    hidden.render.ui = false;
    assert_eq!(model.route_raw(&hidden), RawOutcome::Applied);
    let slot = model
        .sessions
        .iter()
        .find(|entry| entry.id == session)
        .expect("seed row");
    assert_eq!(
        slot.projection.badge(),
        "IDLE",
        "a hidden event never paints"
    );
    assert_eq!(slot.projection.last_applied(), Some(1));
    assert_eq!(
        model.route_raw(&raw(&session, 2, &user("next"))),
        RawOutcome::Applied,
        "the sequence continued without a phantom gap"
    );
}
